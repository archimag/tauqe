//! Handlers for the responses the daemon sends back to the client.
//!
//! Every tracked request is answered here: the pending method is resolved
//! through the connection tracker, so a payload is applied to the state of the
//! request that produced it, and a rejected request settles the turn or the
//! dialog it started instead of leaving a spinner that no event will ever stop
//! (c-3).

use std::sync::Arc;

use tauqe_protocol::{
    methods, ConfigState, ContextAddPatternResult, ContextState, GitSquashGenerateMessageResult,
    GitSquashPreviewResult, HistoryGetResult, ModelCurrentResult, ModelRef, ModelRolesSummary,
    ModelSelectResult, ModelSelection, PlanClearHorizonResult, PlanListResult, PlanRefineResult,
    RepositoryInitResult, RepositoryListFilesResult, RequestId, Response, ReviewAnalyzeResult,
    ReviewGetResult, ReviewListResult, ReviewRefineGroupResult, ReviewRefineItemParams,
    ReviewRefineItemResult, SystemStatusResult,
};
use tokio::sync::Mutex;

use super::events::fail_safe_reject_edits;
use super::transport::{send_request, OptimisticRollback, RpcConnection};
use crate::app::{AppState, ModelChoice, SquashFileItem};
use crate::i18n::active_t;
use crate::views::repl::TurnStatus;
use crate::views::review::RefineTarget;

/// Onboarding confirmations raised by daemon responses.
///
/// Setup runs before any interface locale can exist, so these confirmations are
/// rendered in English; every other user-facing text resolves through the
/// localization catalog.
pub mod messages {
    /// Fallback edit failure recorded when the daemon omits its own reason.
    pub const EDIT_FAILED_FALLBACK: &str = "Edit failed on the server";

    /// The project configuration file was created.
    pub const CONFIG_CREATED: &str = "tauqe.toml created successfully.";

    /// The OpenRouter API key was stored.
    pub const API_KEY_SAVED: &str = "OpenRouter API key saved successfully.";

    /// Every check passed after the configuration reload.
    pub const CONFIG_RELOADED_OK: &str = "Configuration reloaded. All checks passed!";

    /// The reload left the system without an API key.
    pub const API_KEY_NOT_FOUND: &str =
        "API key still not found. Enter it or create a stub credentials file.";

    /// The repository was initialized.
    pub const GIT_INIT_OK: &str = "Git repository initialized successfully.";

    /// Confirmation raised after a stub credentials file was created.
    pub fn stub_credentials_created(path: &str) -> String {
        format!("Stub credentials file created at '{path}'. Edit it and select 'Check again'.")
    }
}

/// A request the client dispatches once the applied response is fully handled.
enum FollowUp {
    /// Fire-and-forget request carrying the given parameters.
    Request(&'static str, serde_json::Value),
    /// The next finding of a running architectural decomposition queue.
    ReviewRefine(RefineTarget),
}

/// Applies one daemon response to the shared application state.
pub async fn handle_response(
    resp: Response,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) {
    let Some(request_id) = response_request_id(&resp.id) else {
        return;
    };
    let method = server_writer.take_pending_request(request_id);
    let rollback = server_writer.take_optimistic_rollback(request_id);

    if let Some(error) = resp.error {
        let mut st = state.lock().await;
        if let Some(rollback) = rollback {
            apply_optimistic_rollback(&mut st, rollback);
        }
        reject_response(&mut st, method.as_deref(), &error.message);
        return;
    }

    let Some(result) = resp.result else {
        return;
    };

    let mut follow_ups = Vec::new();
    {
        let mut st = state.lock().await;
        apply_response(&mut st, method.as_deref(), result, &mut follow_ups);
    }
    run_follow_ups(state, server_writer, follow_ups).await;
}

/// Extracts the numeric identifier of a response, if the request used one.
fn response_request_id(id: &RequestId) -> Option<u64> {
    match id {
        RequestId::Number(value) => Some(*value),
        _ => None,
    }
}

/// Applies a successful payload to the state of the request that produced it.
fn apply_response(
    st: &mut AppState,
    method: Option<&str>,
    value: serde_json::Value,
    follow_ups: &mut Vec<FollowUp>,
) {
    match method {
        Some(methods::CONFIG_GET) => {
            if let Ok(config) = serde_json::from_value::<ConfigState>(value) {
                apply_config(st, config);
            }
        }
        Some(methods::CONFIG_RELOAD) => {
            if let Ok(status) = serde_json::from_value::<SystemStatusResult>(value.clone()) {
                apply_system_status(st, status, true);
            } else if let Ok(config) = serde_json::from_value::<ConfigState>(value) {
                apply_config(st, config);
                st.onboarding.set_status(messages::CONFIG_RELOADED_OK);
                st.onboarding.step = st.onboarding.next_step();
                st.onboarding.selected_index = 0;
            }
        }
        Some(methods::SYSTEM_STATUS) => {
            if let Ok(status) = serde_json::from_value::<SystemStatusResult>(value) {
                apply_system_status(st, status, false);
            }
        }
        Some(methods::MODEL_CURRENT) => {
            if let Ok(current) = serde_json::from_value::<ModelCurrentResult>(value) {
                apply_model(
                    st,
                    current.effective_model,
                    current.selection,
                    current.roles,
                );
            }
        }
        Some(methods::MODEL_SELECT) => {
            if let Ok(selected) = serde_json::from_value::<ModelSelectResult>(value) {
                apply_model(
                    st,
                    selected.effective_model,
                    selected.selection,
                    selected.roles,
                );
            }
        }
        Some(methods::REPOSITORY_LIST_FILES) => {
            if let Ok(files) = serde_json::from_value::<RepositoryListFilesResult>(value) {
                st.all_repo_files = files.files;
                if st.context_view.adding_file {
                    st.update_filtered_candidates();
                }
            }
        }
        Some(methods::REPOSITORY_INIT) => {
            if let Ok(initialized) = serde_json::from_value::<RepositoryInitResult>(value) {
                st.repo_state = Some(initialized.repository);
                st.onboarding.has_git = true;
                st.onboarding.set_status(messages::GIT_INIT_OK);
                st.onboarding.step = st.onboarding.next_step();
                st.onboarding.selected_index = 0;
                follow_ups.push(FollowUp::Request(
                    methods::REPOSITORY_LIST_FILES,
                    serde_json::json!({}),
                ));
            }
        }
        Some(methods::CONFIG_CREATE) => {
            st.onboarding.has_config = true;
            st.onboarding.set_status(messages::CONFIG_CREATED);
            st.onboarding.step = st.onboarding.next_step();
            st.onboarding.selected_index = 0;
        }
        Some(methods::CREDENTIALS_SAVE) => {
            st.onboarding.has_api_key = true;
            st.onboarding.set_status(messages::API_KEY_SAVED);
            st.onboarding.step = st.onboarding.next_step();
            st.onboarding.selected_index = 0;
        }
        Some(methods::CREDENTIALS_CREATE_STUB) => {
            let path = st.onboarding.default_credentials_path.clone();
            st.onboarding
                .set_status(messages::stub_credentials_created(&path));
        }
        Some(methods::CONTEXT_GET)
        | Some(methods::CONTEXT_ADD)
        | Some(methods::CONTEXT_REMOVE)
        | Some(methods::CONTEXT_CLEAR)
        | Some(methods::CONTEXT_SET_ACCESS) => {
            if let Ok(context) = serde_json::from_value::<ContextState>(value) {
                apply_context(st, context);
            }
        }
        Some(methods::CONTEXT_ADD_PATTERN) => {
            if let Ok(added) = serde_json::from_value::<ContextAddPatternResult>(value) {
                apply_context(st, added.state);
                let template = if added.truncated {
                    active_t("notify.context_added_truncated")
                } else {
                    active_t("notify.context_added")
                };
                st.notify_success(
                    template
                        .replacen("{}", &added.added_count.to_string(), 1)
                        .replacen("{}", &added.added_tokens.to_string(), 1),
                );
            }
        }
        Some(methods::HISTORY_GET) => {
            if let Ok(history) = serde_json::from_value::<HistoryGetResult>(value) {
                st.history_view.items = history.items;
                st.history_view.total_count = history.total_count;
                st.history_view.estimated_tokens = history.estimated_tokens;
                if st.history_view.auto_scroll {
                    let view_height = st.last_model_height;
                    let total_lines = crate::views::history::compute_history_items_line_count(
                        &st.history_view.items,
                        None,
                    ) as u16;
                    st.history_view.scroll = total_lines.saturating_sub(view_height);
                    st.history_view.selected_item_index =
                        st.history_view.items.len().saturating_sub(1);
                }
            }
        }
        Some(methods::PLAN_LIST) => {
            if let Ok(list) = serde_json::from_value::<PlanListResult>(value) {
                st.plans_view.plans = list.plans;
                st.plans_view.clamp_selection();
            }
        }
        Some(methods::PLAN_DELETE) => {
            st.notify_info(active_t("notify.plan_deleted"));
        }
        Some(methods::PLAN_CLEAR_HORIZON) => {
            if let Ok(cleared) = serde_json::from_value::<PlanClearHorizonResult>(value) {
                upsert_plan(st, cleared.plan);
            }
            st.notify_success(active_t("notify.plan_horizon_cleared"));
        }
        Some(methods::PLAN_REFINE) | Some(methods::PLAN_REPLAN) => {
            st.plans_view.refining = false;
            st.plans_view.refining_plan_id = None;
            st.plans_view.refine_dialog = None;
            match serde_json::from_value::<PlanRefineResult>(value) {
                Ok(PlanRefineResult::Success { plan, explanation }) => {
                    let (done, total) = step_progress(&plan);
                    let explanation = explanation
                        .as_deref()
                        .map(str::trim)
                        .filter(|text| !text.is_empty());
                    let message = match explanation {
                        Some(explanation) => active_t("notify.plan_horizon_updated")
                            .replacen("{}", &plan.id, 1)
                            .replacen("{}", &done.to_string(), 1)
                            .replacen("{}", &total.to_string(), 1)
                            .replacen("{}", explanation, 1),
                        None => active_t("notify.plan_horizon_updated_default")
                            .replacen("{}", &plan.id, 1)
                            .replacen("{}", &done.to_string(), 1)
                            .replacen("{}", &total.to_string(), 1),
                    };
                    upsert_plan(st, plan);
                    st.notify_success(message);
                }
                Ok(PlanRefineResult::Blocked { reason }) => st.notify_error(reason),
                Err(_) => {}
            }
        }
        Some(methods::REVIEW_LIST) => {
            if let Ok(list) = serde_json::from_value::<ReviewListResult>(value) {
                st.review.set_sessions(list.reviews, list.active_id);
            }
        }
        Some(methods::REVIEW_GET) => {
            if let Ok(fetched) = serde_json::from_value::<ReviewGetResult>(value) {
                if let Some(session) = fetched.session {
                    st.review.set_session(session);
                }
            }
        }
        Some(methods::REVIEW_DELETE) => {
            st.notify_info(active_t("notify.review_deleted"));
        }
        Some(methods::REVIEW_REFINE_ITEM) => {
            st.review.refine_dialog = None;
            match serde_json::from_value::<ReviewRefineItemResult>(value) {
                Ok(ReviewRefineItemResult::Success { explanation, .. }) => {
                    st.review.error = None;
                    let message = match explanation
                        .as_deref()
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                    {
                        Some(text) => active_t("notify.finding_decomposed").replacen("{}", text, 1),
                        None => active_t("notify.finding_decomposed_default"),
                    };
                    st.notify_success(message);
                    match st.review.next_refine_target() {
                        Some(target) => {
                            st.review.refining = Some(target.review_id.clone());
                            follow_ups.push(FollowUp::ReviewRefine(target));
                        }
                        None => st.review.refining = None,
                    }
                }
                Ok(ReviewRefineItemResult::Blocked { reason }) => {
                    st.review.refining = None;
                    st.review.refine_queue.clear();
                    st.notify_warning(
                        active_t("notify.finding_decompose_blocked").replace("{}", &reason),
                    );
                    st.review.error = Some(reason);
                }
                Err(_) => {
                    st.review.refining = None;
                    st.review.refine_queue.clear();
                }
            }
        }
        Some(methods::REVIEW_REFINE_GROUP) => {
            st.review.refine_dialog = None;
            st.review.refining = None;
            match serde_json::from_value::<ReviewRefineGroupResult>(value) {
                Ok(ReviewRefineGroupResult::Success { explanation, .. }) => {
                    st.review.error = None;
                    let message = match explanation
                        .as_deref()
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                    {
                        Some(text) => active_t("notify.finding_decomposed").replacen("{}", text, 1),
                        None => active_t("notify.finding_decomposed_default"),
                    };
                    st.notify_success(message);
                }
                Ok(ReviewRefineGroupResult::Blocked { reason }) => {
                    st.notify_warning(
                        active_t("notify.finding_decompose_blocked").replace("{}", &reason),
                    );
                    st.review.error = Some(reason);
                }
                Err(_) => {}
            }
        }
        Some(methods::REVIEW_ANALYZE) => {
            st.review.analyzing = false;
            st.review.error = None;
            if let Ok(analyzed) = serde_json::from_value::<ReviewAnalyzeResult>(value) {
                st.review.update_session(analyzed.session);
            }
        }
        Some(methods::GIT_SQUASH_PREVIEW) => {
            apply_squash_preview(st, value);
        }
        Some(methods::GIT_SQUASH_GENERATE_MESSAGE) => {
            if let Ok(generated) = serde_json::from_value::<GitSquashGenerateMessageResult>(value) {
                if let Some(dialog) = st.squash_dialog.as_mut() {
                    dialog.generating_message = false;
                    dialog.message_editor.clear();
                    dialog.message_editor.insert_str(&generated.message);
                    dialog.status_message = None;
                }
            }
        }
        Some(methods::LOCALE_GENERATE) => handle_locale_response(st, value),
        _ => {}
    }
}

/// Settles the state a rejected request started.
///
/// A request that the daemon refuses never produces the events that normally
/// settle a turn, so the status, the running model, the spinner and the
/// reasoning block are restored here instead of waiting for an event that will
/// never arrive (c-3).
fn reject_response(st: &mut AppState, method: Option<&str>, message: &str) {
    match method {
        Some(methods::REVIEW_ANALYZE) => {
            st.review.analyzing = false;
            st.review.error = Some(message.to_string());
            settle_failed_turn(st, message);
        }
        Some(methods::REVIEW_REFINE_ITEM) | Some(methods::REVIEW_REFINE_GROUP) => {
            st.review.refine_dialog = None;
            st.review.refining = None;
            st.review.refine_queue.clear();
            st.review.error = Some(message.to_string());
            st.notify_error(active_t("notify.rpc_error").replace("{}", message));
        }
        Some(methods::PLAN_REFINE) | Some(methods::PLAN_REPLAN) => {
            st.plans_view.refine_dialog = None;
            st.plans_view.refining = false;
            st.plans_view.refining_plan_id = None;
            st.notify_error(active_t("notify.rpc_error").replace("{}", message));
        }
        Some(methods::MODEL_ASK)
        | Some(methods::PLAN_EXECUTE)
        | Some(methods::PLAN_EXECUTE_PIPELINE)
        | Some(methods::REVIEW_EXECUTE)
        | Some(methods::REVIEW_START) => settle_failed_turn(st, message),
        Some(methods::GIT_SQUASH_PREVIEW)
        | Some(methods::GIT_SQUASH_GENERATE_MESSAGE)
        | Some(methods::GIT_SQUASH_APPLY) => {
            if let Some(dialog) = st.squash_dialog.as_mut() {
                dialog.loading = false;
                dialog.generating_message = false;
                dialog.applying = false;
                dialog.pending_base = None;
            }
            st.notify_error(active_t("notify.rpc_error").replace("{}", message));
        }
        Some(methods::LOCALE_GENERATE) => {
            if let Some(dialog) = st.locale_dialog.as_mut() {
                dialog.generating = false;
            }
            st.notify_error(active_t("notify.locale_generate_failed").replace("{}", message));
        }
        _ => st.notify_error(active_t("notify.rpc_error").replace("{}", message)),
    }
}

/// Projects a failed turn onto the interface: the status becomes an error, the
/// spinner stops, the running model is cleared, the reasoning stream is folded,
/// and the staged edits are reconciled.
fn settle_failed_turn(st: &mut AppState, message: &str) {
    st.running_model = None;
    st.turn_started_at = None;
    st.confirm_cancel = false;
    st.confirm_batch_cancel = false;
    st.batch_stopping = false;
    st.review.running = false;
    st.review.analyzing = false;

    let height = st.last_model_height;
    let model = &mut st.model;
    model.is_batch = false;
    model.reasoning.finalize();
    model.reasoning.show = false;
    model.status = TurnStatus::Error;
    model.toolchain = None;
    model.retrying = None;
    model.turn_phase = None;
    model.turn_phase_detail = None;
    model.turn_round = None;
    model.turn_max_rounds = None;
    model.error = Some(message.to_string());
    model.end_in_flight_usage();
    fail_safe_reject_edits(model, message);
    model.update_markdown();
    model.clamp_scroll(height);

    st.notify_error(active_t("notify.rpc_error").replace("{}", message));
}

/// Restores the optimistic state a rejected request had already replaced.
fn apply_optimistic_rollback(st: &mut AppState, rollback: OptimisticRollback) {
    match rollback {
        OptimisticRollback::ActiveModel {
            prev_model,
            prev_selection,
        } => {
            st.active_model = prev_model;
            st.model_choice.selection = prev_selection;
        }
        OptimisticRollback::PlanItemStatus {
            plan_id,
            item_id,
            prev_status,
        } => {
            if let Some(plan) = st
                .plans_view
                .plans
                .iter_mut()
                .find(|plan| plan.id == plan_id)
            {
                if let Some(item) = plan.items.iter_mut().find(|item| item.id == item_id) {
                    item.status = prev_status;
                }
            }
        }
        OptimisticRollback::ReviewIssueStatus {
            review_id,
            item_id,
            prev_status,
        } => {
            let session = match review_id.as_deref() {
                Some(id) => st
                    .review
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == id),
                None => None,
            };
            let session = match session {
                Some(session) => Some(session),
                None => st.review.session.as_mut(),
            };
            if let Some(session) = session {
                if let Some(item) = session.items.iter_mut().find(|item| item.id == item_id) {
                    item.status = prev_status;
                }
            }
        }
    }
}

/// Applies the configuration snapshot reported by the daemon.
fn apply_config(st: &mut AppState, config: ConfigState) {
    st.workflow = config.workflow;
    st.edit_protocol = config.edit_protocol;
    st.active_model = config.model;
    if !config.available_workflows.is_empty() {
        st.available_workflows = config.available_workflows;
    }
    if !config.available_edit_protocols.is_empty() {
        st.available_edit_protocols = config.available_edit_protocols;
    }
    if let Some(selection) = config.selection {
        st.model_choice.selection = selection;
    }
    if let Some(roles) = config.roles {
        st.model_choice.roles = Some(roles);
    }
    st.review_presets = config.review_presets.unwrap_or_default();
}

/// Applies the system status reported by the daemon and advances the setup.
fn apply_system_status(st: &mut AppState, status: SystemStatusResult, reloaded: bool) {
    st.onboarding.apply_status(&status);
    st.active_model = status.model;
    if let Some(selection) = status.selection {
        st.model_choice.selection = selection;
    }
    if let Some(roles) = status.roles {
        st.model_choice.roles = Some(roles);
    }
    if reloaded {
        if status.has_api_key {
            st.onboarding.set_status(messages::CONFIG_RELOADED_OK);
        } else {
            st.onboarding.set_error(messages::API_KEY_NOT_FOUND);
        }
    }
    st.onboarding.step = st.onboarding.next_step();
    st.onboarding.selected_index = 0;
}

/// Applies the model selection reported by the daemon.
fn apply_model(
    st: &mut AppState,
    effective_model: ModelRef,
    selection: ModelSelection,
    roles: Option<ModelRolesSummary>,
) {
    st.active_model = effective_model;
    st.model_choice = ModelChoice { selection, roles };
}

/// Applies the authoritative context state and keeps the cursor in range.
fn apply_context(st: &mut AppState, context: ContextState) {
    st.context = context;
    let rows = st.context_view.compute_rows(&st.context.items);
    if rows.is_empty() {
        st.context_view.cursor_index = 0;
    } else if st.context_view.cursor_index >= rows.len() {
        st.context_view.cursor_index = rows.len() - 1;
    }
    if st.context_view.adding_file {
        st.update_filtered_candidates();
    }
}

/// Replaces a plan in the mirrored list or appends it when it is new.
fn upsert_plan(st: &mut AppState, plan: tauqe_protocol::Plan) {
    match st
        .plans_view
        .plans
        .iter()
        .position(|existing| existing.id == plan.id)
    {
        Some(index) => st.plans_view.plans[index] = plan,
        None => st.plans_view.plans.push(plan),
    }
    st.plans_view.clamp_selection();
}

/// Counts the completed and total implementation steps of a plan.
fn step_progress(plan: &tauqe_protocol::Plan) -> (usize, usize) {
    let total = plan.items.len();
    let done = plan
        .items
        .iter()
        .filter(|item| item.status == tauqe_protocol::PlanItemStatus::Done)
        .count();
    (done, total)
}

/// Applies the squash preview and leaves the custom base input mode.
fn apply_squash_preview(st: &mut AppState, value: serde_json::Value) {
    let Ok(preview) = serde_json::from_value::<GitSquashPreviewResult>(value) else {
        return;
    };
    let Some(dialog) = st.squash_dialog.as_mut() else {
        return;
    };
    dialog.loading = false;
    dialog.base_ref = preview.base_ref;
    dialog.session_base = preview.session_base;
    dialog.upstream_base = preview.upstream_base;
    dialog.commits = preview.commits;
    dialog.diff_stat = preview.diff_stat;
    dialog.files = preview
        .files
        .into_iter()
        .map(|file| SquashFileItem {
            path: file.path,
            diff: file.diff,
            expanded: false,
        })
        .collect();
    dialog.selected_file_index = 0;
    dialog.diff_scroll = 0;
    if let Some((mode, _)) = dialog.pending_base.take() {
        dialog.base_mode = mode;
        dialog.custom_input_active = false;
    }
    if let Some(message) = preview.suggested_message {
        if dialog.message_editor.is_empty() {
            dialog.message_editor.insert_str(&message);
        }
    }
    dialog.status_message = None;
}

/// Applies a synthesized locale and persists it for the next session.
fn handle_locale_response(st: &mut AppState, value: serde_json::Value) {
    if let Some(dialog) = st.locale_dialog.as_mut() {
        dialog.generating = false;
    }
    let Ok(result) = serde_json::from_value::<tauqe_protocol::locale::LocaleGenerateResult>(value)
    else {
        return;
    };
    let language = result.language;
    let locale = crate::i18n::CustomLocale::new(language.clone(), result.translations);
    let saved = crate::i18n::storage::save_custom_locale(&locale);
    st.apply_custom_locale(locale);
    st.locale_dialog = None;
    match saved {
        Ok(_) => st.notify_success(active_t("notify.locale_generated").replace("{}", &language)),
        Err(error) => {
            let message = active_t("notify.locale_switched_unsaved")
                .replacen("{}", &language, 1)
                .replacen("{}", &error.to_string(), 1);
            st.notify_warning(message);
        }
    }
}

/// Dispatches the requests the applied response deferred.
async fn run_follow_ups(
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
    follow_ups: Vec<FollowUp>,
) {
    for follow_up in follow_ups {
        match follow_up {
            FollowUp::Request(method, params) => {
                if let Err(error) = send_request(server_writer, method, params).await {
                    let mut st = state.lock().await;
                    st.notify_error(active_t("notify.rpc_error").replace("{}", &error.to_string()));
                }
            }
            FollowUp::ReviewRefine(target) => {
                let params = ReviewRefineItemParams {
                    review_id: Some(target.review_id),
                    item_id: target.item_id,
                    model: None,
                    instructions: None,
                };
                let params = match serde_json::to_value(params) {
                    Ok(params) => params,
                    Err(error) => {
                        let mut st = state.lock().await;
                        st.review.refining = None;
                        st.notify_error(
                            active_t("notify.rpc_error").replace("{}", &error.to_string()),
                        );
                        continue;
                    }
                };
                if let Err(error) =
                    send_request(server_writer, methods::REVIEW_REFINE_ITEM, params).await
                {
                    let mut st = state.lock().await;
                    st.review.refining = None;
                    st.review.refine_queue.clear();
                    st.notify_error(active_t("notify.rpc_error").replace("{}", &error.to_string()));
                } else {
                    let mut st = state.lock().await;
                    st.notify_info(active_t("notify.review_refining"));
                }
            }
        }
    }
}
