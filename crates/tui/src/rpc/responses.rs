//! Handlers for the responses the daemon sends back to the client.
//!
//! Every tracked request is answered here: the pending method is resolved
//! through the connection tracker, so a payload is applied to the state of the
//! request that produced it, and a rejected request settles the turn or the
//! dialog it started instead of leaving a spinner that no event will ever stop
//! (c-3).

use std::sync::Arc;

use tauqe_protocol::review::ReviewPreviewResult;
use tauqe_protocol::{
    methods, ConfigState, ContextAddPatternResult, ContextState, GitSquashGenerateMessageResult,
    GitSquashPreviewResult, HistoryGetResult, ModelCurrentResult, ModelRef, ModelRolesSummary,
    ModelSelectResult, ModelSelection, OuterDocsCacheListResult, OuterDocsCacheRemoveResult,
    PlanClearHorizonResult, PlanListResult, PlanRefineResult, RepositoryInitResult,
    RepositoryListFilesResult, RequestId, Response, ReviewAnalyzeResult, ReviewGetResult,
    ReviewListResult, SystemStatusResult,
};
use tokio::sync::Mutex;

use super::events::fail_safe_reject_edits;
use super::transport::{send_request, OptimisticRollback, RpcConnection};
use crate::app::{
    AppState, ConfirmDialogButton, ConfirmReviewTruncationState, ModelChoice, PendingReviewLaunch,
    SquashFileItem,
};
use crate::i18n::{active_t, CustomLocale};
use crate::views::repl::TurnStatus;
/// A request the client dispatches once the applied response is fully handled.
enum FollowUp {
    /// Fire-and-forget request carrying the given parameters.
    Request(&'static str, serde_json::Value),
    /// The audit of a review launch whose projected window was not truncated.
    ReviewLaunch(Box<PendingReviewLaunch>),
    /// A synthesized locale awaiting persistence outside the state lock.
    PersistLocale(CustomLocale),
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
        Some(methods::GIT_GET_DIFF) => {
            if let Ok(diff) = serde_json::from_value::<tauqe_protocol::GitDiffResult>(value) {
                apply_commit_audit(st, &diff.diff);
            }
        }
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
                st.onboarding
                    .set_status(active_t("onboarding.status.config_reloaded"));
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
                st.set_repo_files(files.files);
                if st.context_view.adding_file {
                    st.update_filtered_candidates();
                }
            }
        }
        Some(methods::REPOSITORY_INIT) => {
            if let Ok(initialized) = serde_json::from_value::<RepositoryInitResult>(value) {
                st.repo_state = Some(initialized.repository);
                st.onboarding.has_git = true;
                st.onboarding
                    .set_status(active_t("onboarding.status.git_initialized"));
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
            st.onboarding
                .set_status(active_t("onboarding.status.config_created"));
            st.onboarding.step = st.onboarding.next_step();
            st.onboarding.selected_index = 0;
        }
        Some(methods::CREDENTIALS_SAVE) => {
            st.onboarding.has_api_key = true;
            st.onboarding
                .set_status(active_t("onboarding.status.api_key_saved"));
            st.onboarding.selected_index = 0;
        }
        Some(methods::CREDENTIALS_CREATE_STUB) => {
            let path = st.onboarding.default_credentials_path.clone();
            let template = active_t("onboarding.status.stub_credentials_created");
            let confirmation = template.replacen("{}", &path, 1);
            st.onboarding.set_status(confirmation);
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
                let view_height = st.last_model_height;
                st.history_view.apply_page(history, view_height);
            }
        }
        Some(methods::OUTER_DOCS_CACHE_LIST) => {
            if let Ok(listed) = serde_json::from_value::<OuterDocsCacheListResult>(value) {
                if let Some(dialog) = st.modals.outer_docs_cache.as_mut() {
                    dialog.entries = listed.entries;
                    dialog.loading = false;
                    dialog.clamp_cursor();
                }
            }
        }
        Some(methods::OUTER_DOCS_CACHE_REMOVE) => {
            if let Ok(removed) = serde_json::from_value::<OuterDocsCacheRemoveResult>(value) {
                st.notify_success(
                    active_t("notify.outer_docs_cache_removed")
                        .replace("{}", &removed.removed_files.to_string()),
                );
            }
            // The tree is rebuilt from the authoritative inventory, so the
            // removed node leaves the pane as soon as the daemon answers
            // (c-outer-docs-cache-tui-deletion).
            if let Some(dialog) = st.modals.outer_docs_cache.as_mut() {
                dialog.loading = true;
                follow_ups.push(FollowUp::Request(
                    methods::OUTER_DOCS_CACHE_LIST,
                    serde_json::json!({}),
                ));
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
        Some(methods::PLAN_REFINE)
        | Some(methods::PLAN_REPLAN)
        | Some(methods::PLAN_CREATE_PIPELINE) => {
            // The creation pipeline answers with the same result shape as a
            // directed modification, so the notification below names the issued
            // plan as created, and the form that requested it is settled here
            // (c-two-phase-planning-engine).
            let created = method == Some(methods::PLAN_CREATE_PIPELINE);
            st.plans_view.refining = false;
            st.plans_view.refining_plan_id = None;
            st.modals.refine_dialog = None;
            st.modals.create_dialog = None;
            match serde_json::from_value::<PlanRefineResult>(value) {
                Ok(PlanRefineResult::Success { plan, explanation }) => {
                    let (done, total) = step_progress(&plan);
                    let explanation = explanation
                        .as_deref()
                        .map(str::trim)
                        .filter(|text| !text.is_empty());
                    let message = match (created, explanation) {
                        (true, Some(explanation)) => active_t("notify.plan_created")
                            .replacen("{}", &plan.id, 1)
                            .replacen("{}", &done.to_string(), 1)
                            .replacen("{}", &total.to_string(), 1)
                            .replacen("{}", explanation, 1),
                        (true, None) => active_t("notify.plan_created_default")
                            .replacen("{}", &plan.id, 1)
                            .replacen("{}", &done.to_string(), 1)
                            .replacen("{}", &total.to_string(), 1),
                        (false, Some(explanation)) => active_t("notify.plan_horizon_updated")
                            .replacen("{}", &plan.id, 1)
                            .replacen("{}", &done.to_string(), 1)
                            .replacen("{}", &total.to_string(), 1)
                            .replacen("{}", explanation, 1),
                        (false, None) => active_t("notify.plan_horizon_updated_default")
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
        Some(methods::REVIEW_PREVIEW) => apply_review_preview(st, value, follow_ups),
        Some(methods::REVIEW_DELETE) => {
            st.notify_info(active_t("notify.review_deleted"));
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
                if let Some(dialog) = st.modals.squash_dialog.as_mut() {
                    dialog.generating_message = false;
                    dialog.message_editor.clear();
                    dialog.message_editor.insert_str(&generated.message);
                    dialog.status_message = None;
                }
            }
        }
        Some(methods::LOCALE_GENERATE) => handle_locale_response(st, value, follow_ups),
        _ => {}
    }
}

/// Applies the audit-window projection to the review launch that requested it.
///
/// The answer is the consent gate of the launch: an untruncated window starts the
/// audit immediately through the shared launch helper, while a truncated one
/// opens the confirmation card with the full counters and sends nothing until
/// the developer confirms it (c-3). A projection that cannot be read returns the
/// launch dialog with its typed fields instead of starting an audit nobody
/// described.
fn apply_review_preview(
    st: &mut AppState,
    value: serde_json::Value,
    follow_ups: &mut Vec<FollowUp>,
) {
    let Some(pending) = st.modals.pending_review_launch.take() else {
        return;
    };
    let preview = match serde_json::from_value::<ReviewPreviewResult>(value) {
        Ok(preview) => preview,
        Err(error) => {
            st.modals.review_dialog = Some(pending.dialog);
            st.notify_error(
                active_t("notify.review_preview_failed").replace("{}", &error.to_string()),
            );
            return;
        }
    };
    if preview.window.is_truncated() {
        st.modals.confirm_review_truncation = Some(ConfirmReviewTruncationState {
            pending,
            window: preview.window,
        });
        st.modals.confirm_button = ConfirmDialogButton::Cancel;
    } else {
        follow_ups.push(FollowUp::ReviewLaunch(Box::new(pending)));
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
        Some(methods::REVIEW_PREVIEW) => {
            // A refused projection settles the launch it started: the dialog
            // returns with its typed fields and no operation opens (c-3).
            if let Some(pending) = st.modals.pending_review_launch.take() {
                st.modals.review_dialog = Some(pending.dialog);
            }
            st.modals.confirm_button = ConfirmDialogButton::Cancel;
            st.notify_error(active_t("notify.review_preview_failed").replace("{}", message));
        }
        Some(methods::REVIEW_ANALYZE) => {
            st.review.analyzing = false;
            st.review.error = Some(message.to_string());
            settle_failed_turn(st, message);
        }
        Some(methods::PLAN_REFINE)
        | Some(methods::PLAN_REPLAN)
        | Some(methods::PLAN_CREATE_PIPELINE) => {
            st.modals.refine_dialog = None;
            st.modals.create_dialog = None;
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
            settle_squash_failure(st, message);
        }
        Some(methods::LOCALE_GENERATE) => {
            if let Some(dialog) = st.modals.locale_dialog.as_mut() {
                dialog.generating = false;
            }
            st.notify_error(active_t("notify.locale_generate_failed").replace("{}", message));
        }
        Some(methods::OUTER_DOCS_CACHE_REMOVE) => {
            // A refused removal settles the browser's loading state, so the tree
            // stays readable and the failure is reported (c-3).
            if let Some(dialog) = st.modals.outer_docs_cache.as_mut() {
                dialog.loading = false;
            }
            st.notify_error(active_t("notify.rpc_error").replace("{}", message));
        }
        Some(methods::OUTER_DOCS_CACHE_LIST) => {
            // A refused inventory settles the browser's loading state instead of
            // leaving a spinner no answer will ever stop (c-3).
            if let Some(dialog) = st.modals.outer_docs_cache.as_mut() {
                dialog.loading = false;
            }
            st.notify_error(active_t("notify.outer_docs_cache_list_failed").replace("{}", message));
        }
        Some(methods::HISTORY_GET) => {
            // A refused page settles the handshake it armed: no answer arrives
            // for it, and a stuck `loading` flag would silently disable every
            // later fetch for the rest of the session (c-3).
            st.history_view.cancel_page_fetch();
            st.notify_error(active_t("notify.rpc_error").replace("{}", message));
        }
        _ => st.notify_error(active_t("notify.rpc_error").replace("{}", message)),
    }
}

/// Settles the squash dialog a request that failed left behind.
///
/// The dialog stays open carrying its commit message, so neither a request
/// the daemon refused nor a transport write that never reached it may keep
/// the in-flight flags its press raised: `applying` alone makes `handle_key`
/// ignore every later key, which would wedge the card for good (c-3). Both
/// paths report the failure through the same notification, so a closed pipe
/// reads exactly like a refusal.
pub(crate) fn settle_squash_failure(st: &mut AppState, message: &str) {
    if let Some(dialog) = st.modals.squash_dialog.as_mut() {
        dialog.loading = false;
        dialog.generating_message = false;
        dialog.applying = false;
        dialog.pending_base = None;
    }
    st.notify_error(active_t("notify.rpc_error").replace("{}", message));
}

/// Projects a failed turn onto the interface: the status becomes an error, the
/// spinner stops, the running model is cleared, the reasoning stream is folded,
/// and the staged edits are reconciled.
fn settle_failed_turn(st: &mut AppState, message: &str) {
    st.running_model = None;
    st.turn_started_at = None;
    st.modals.confirm_cancel = false;
    st.modals.confirm_batch_cancel = false;
    st.modals.batch_stopping = false;
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
///
/// This is the single implementation of "undo one optimistic mutation": the
/// error-response path and the transport-write-failure path both call it, so a
/// request the daemon refused and a request that never left the client are
/// reverted by the same code (c-3).
pub(crate) fn apply_optimistic_rollback(st: &mut AppState, rollback: OptimisticRollback) {
    match rollback {
        OptimisticRollback::ActiveModel {
            prev_model,
            prev_selection,
        } => {
            st.active_model = prev_model;
            st.model_choice.selection = prev_selection;
            st.mark_model_choice_changed();
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
                let items = &mut plan.implementation.items;
                if let Some(item) = items.iter_mut().find(|item| item.id == item_id) {
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
        st.mark_model_choice_changed();
    }
    if let Some(roles) = config.roles {
        st.model_choice.roles = Some(roles);
        st.mark_model_choice_changed();
    }
    st.review_presets = config.review_presets.unwrap_or_default();
}

/// Applies the system status reported by the daemon and advances the setup.
fn apply_system_status(st: &mut AppState, status: SystemStatusResult, reloaded: bool) {
    st.onboarding.apply_status(&status);
    st.active_model = status.model;
    if let Some(selection) = status.selection {
        st.model_choice.selection = selection;
        st.mark_model_choice_changed();
    }
    if let Some(roles) = status.roles {
        st.model_choice.roles = Some(roles);
        st.mark_model_choice_changed();
    }
    if reloaded {
        if status.has_api_key {
            st.onboarding
                .set_status(active_t("onboarding.status.config_reloaded"));
        } else {
            st.onboarding
                .set_error(active_t("onboarding.status.api_key_not_found").as_str());
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
    st.set_model_choice(ModelChoice { selection, roles });
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
    let items = &plan.implementation.items;
    let total = items.len();
    let done = items
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
    let Some(dialog) = st.modals.squash_dialog.as_mut() else {
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

/// Applies a synthesized locale in memory and defers its persistence.
///
/// The write itself is blocking filesystem work, so it is handed to the
/// follow-up queue and performed with the state lock released (c-1).
fn handle_locale_response(
    st: &mut AppState,
    value: serde_json::Value,
    follow_ups: &mut Vec<FollowUp>,
) {
    if let Some(dialog) = st.modals.locale_dialog.as_mut() {
        dialog.generating = false;
    }
    let Ok(result) = serde_json::from_value::<tauqe_protocol::locale::LocaleGenerateResult>(value)
    else {
        return;
    };
    let locale = CustomLocale::new(result.language, result.translations);
    st.apply_custom_locale(locale.clone());
    st.modals.locale_dialog = None;
    follow_ups.push(FollowUp::PersistLocale(locale));
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
            FollowUp::ReviewLaunch(pending) => {
                // The audit starts through the same launch helper the consent
                // card uses, so an untruncated projection and a confirmed
                // truncation dispatch the identical request (c-3).
                let pending = *pending;
                let st = state.lock().await;
                if let Err(error) =
                    crate::dialogs::input::launch_review(state, st, server_writer, pending.params)
                        .await
                {
                    let mut st = state.lock().await;
                    st.notify_error(active_t("notify.rpc_error").replace("{}", &error.to_string()));
                }
            }
            FollowUp::PersistLocale(locale) => {
                let language = locale.language.clone();
                // The write is blocking filesystem work: it runs on the blocking
                // pool with no lock held, and the state is locked again only to
                // report the outcome (c-1).
                let saved = match tokio::task::spawn_blocking(move || {
                    crate::i18n::storage::save_custom_locale(&locale)
                })
                .await
                {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                let mut st = state.lock().await;
                match saved {
                    Ok(_) => st.notify_success(
                        active_t("notify.locale_generated").replace("{}", &language),
                    ),
                    Err(error) => {
                        let message = active_t("notify.locale_switched_unsaved")
                            .replacen("{}", &language, 1)
                            .replacen("{}", &error, 1);
                        st.notify_warning(message);
                    }
                }
            }
        }
    }
}

/// Header that opens the block of one file inside a unified diff.
const DIFF_FILE_HEADER: &str = "diff --git ";

/// Opens the fullscreen audit over the change set of one commit.
///
/// The inspector renders the staged change set of the Develop viewport, so the
/// finished change set of a plan step is imported into exactly that
/// representation: one entry per file, carrying the hunks the renderer already
/// draws. The staged set of the running turn is never touched (c-3).
fn apply_commit_audit(st: &mut AppState, diff: &str) {
    let files = parse_commit_diff(diff);
    if files.is_empty() {
        st.notify_info(active_t("notify.plan_commit_diff_empty"));
        return;
    }
    st.modals.diff_audit = Some(crate::app::CommitAudit {
        files,
        cache: crate::views::repl::DiffRenderCache::default(),
    });
    st.modals.diff_inspector = Some(crate::app::DiffInspectorState {
        selected_file: 0,
        diff_scroll: 0,
        focus: crate::app::DiffInspectorFocus::Files,
    });
}

/// Builds the audited files of one commit from its unified diff.
fn parse_commit_diff(diff: &str) -> Vec<crate::views::repl::StreamingFileEdit> {
    let mut files = Vec::new();
    let mut current: Option<CommitFile> = None;
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix(DIFF_FILE_HEADER) {
            if let Some(file) = current.take() {
                files.extend(file.finish());
            }
            current = Some(CommitFile::new(header));
            continue;
        }
        if let Some(file) = current.as_mut() {
            file.push(line);
        }
    }
    if let Some(file) = current.take() {
        files.extend(file.finish());
    }
    files
}

/// The coordinates a unified diff hunk header states.
///
/// Git prints `@@ -old_start,old_lines +new_start,new_lines @@ hint`, omitting a
/// count that equals one and appending the section the hunk sits in — usually
/// the enclosing function signature — after the closing `@@`. Reading those
/// numbers lets the audit name the real lines of the file instead of counting
/// the rows of the patch (c-unified-diff-header-parsing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HunkHeader {
    old_start: usize,
    old_lines: usize,
    new_start: usize,
    new_lines: usize,
    context_hint: Option<String>,
}

/// Parses a canonical unified diff hunk header.
///
/// A combined header of a merge diff (`@@@ -a,b -c,d +e,f @@@`) carries one
/// range per parent, so it names no single pair of ranges and is refused
/// instead of being read as an ordinary hunk.
fn parse_hunk_header(line: &str) -> Option<HunkHeader> {
    let (ranges, tail) = line.strip_prefix("@@ ")?.split_once(" @@")?;
    let mut parts = ranges.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let (old_start, old_lines) = parse_hunk_range(old, '-')?;
    let (new_start, new_lines) = parse_hunk_range(new, '+')?;
    let hint = tail.trim();
    let context_hint = if hint.is_empty() {
        None
    } else {
        Some(hint.to_string())
    };
    Some(HunkHeader {
        old_start,
        old_lines,
        new_start,
        new_lines,
        context_hint,
    })
}

/// Parses one `-start[,lines]` (or `+start[,lines]`) range of a hunk header.
///
/// Git omits the count when a range covers exactly one line.
fn parse_hunk_range(spec: &str, marker: char) -> Option<(usize, usize)> {
    let body = spec.strip_prefix(marker)?;
    match body.split_once(',') {
        Some((start, lines)) => Some((start.parse().ok()?, lines.parse().ok()?)),
        None => Some((body.parse().ok()?, 1)),
    }
}

/// One file block of a unified diff being converted into a staged file edit.
struct CommitFile {
    path: String,
    op: tauqe_protocol::EditFileOp,
    hunks: Vec<crate::views::repl::StreamingHunk>,
}

impl CommitFile {
    /// Starts a file block from the path pair of its `diff --git` header.
    fn new(header: &str) -> Self {
        let path = header
            .rsplit_once(" b/")
            .map(|(_, path)| path.to_string())
            .unwrap_or_else(|| header.trim().to_string());
        Self {
            path,
            op: tauqe_protocol::EditFileOp::Replace,
            hunks: Vec::new(),
        }
    }

    /// Appends one diff line of the block this file is built from.
    fn push(&mut self, line: &str) {
        if line.starts_with("new file mode") {
            self.op = tauqe_protocol::EditFileOp::Create;
            return;
        }
        if line.starts_with("deleted file mode") {
            self.op = tauqe_protocol::EditFileOp::Delete;
            return;
        }
        if let Some(renamed) = line.strip_prefix("rename to ") {
            self.path = renamed.trim().to_string();
            self.op = tauqe_protocol::EditFileOp::Move;
            return;
        }
        if line.starts_with("@@") {
            // The header states the ranges and the section of the hunk, so the
            // audit can name the real lines of the file. A header that names no
            // single pair of ranges keeps its body with unknown coordinates
            // (`0`), never an invented range (c-unified-diff-header-parsing).
            let header = parse_hunk_header(line).unwrap_or_default();
            self.hunks.push(crate::views::repl::StreamingHunk {
                hunk_index: self.hunks.len(),
                old_start: header.old_start,
                old_lines: header.old_lines,
                new_start: header.new_start,
                new_lines: header.new_lines,
                context_hint: header.context_hint,
                old_text: String::new(),
                new_text: String::new(),
            });
            return;
        }
        if line.starts_with("\\ No newline") {
            return;
        }
        let Some(hunk) = self.hunks.last_mut() else {
            return;
        };
        let mut chars = line.chars();
        let marker = chars.next();
        let rest = chars.as_str();
        match marker {
            Some(' ') => {
                push_diff_line(&mut hunk.old_text, rest);
                push_diff_line(&mut hunk.new_text, rest);
            }
            Some('-') => push_diff_line(&mut hunk.old_text, rest),
            Some('+') => push_diff_line(&mut hunk.new_text, rest),
            _ => {}
        }
    }

    /// Resolves the block into the staged file the inspector audits.
    fn finish(self) -> Option<crate::views::repl::StreamingFileEdit> {
        if self.hunks.is_empty() {
            return None;
        }
        Some(crate::views::repl::StreamingFileEdit {
            path: self.path,
            op_type: self.op,
            status: crate::views::repl::FileEditStatus::Ok,
            error: None,
            hunks: self.hunks,
            retry_info: None,
        })
    }
}

/// Appends one diff body line, keeping its trailing newline.
fn push_diff_line(target: &mut String, text: &str) {
    target.push_str(text);
    target.push('\n');
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tauqe_protocol::review::{ReviewPreviewResult, ReviewPreviewWindow};
    use tauqe_protocol::{methods, RequestId, Response};
    use tokio::sync::Mutex;

    use super::*;
    use crate::app::{
        ActiveModal, ConfirmDialogButton, NotificationLevel, PendingReviewLaunch,
        ReviewDialogState, ViewMode,
    };

    /// Answers a pending review projection through the response handler.
    async fn deliver_preview(
        state: &Arc<Mutex<AppState>>,
        conn: &mut RpcConnection,
        result: ReviewPreviewResult,
    ) {
        let id = send_request(conn, methods::REVIEW_PREVIEW, serde_json::json!({}))
            .await
            .expect("an in-memory sink never fails to write");
        handle_response(
            Response::ok_typed(RequestId::Number(id), &result),
            state,
            conn,
        )
        .await;
    }

    /// Builds the pending launch a confirmed launch dialog leaves behind.
    fn pending_launch(path: &str) -> PendingReviewLaunch {
        let mut dialog = ReviewDialogState::new(1, 10, Vec::new(), Vec::new());
        dialog.path_inputs[0].insert_str(path);
        PendingReviewLaunch {
            dialog,
            params: tauqe_protocol::ReviewStartParams {
                path_patterns: vec![path.to_string()],
                ..Default::default()
            },
        }
    }

    /// A window the budgets leave intact launches the audit immediately, so an
    /// ordinary launch never pays for an extra confirmation step (c-3).
    #[tokio::test]
    async fn an_untruncated_projection_launches_the_audit_immediately() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.modals.pending_review_launch = Some(pending_launch("src/"));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        deliver_preview(
            &state,
            &mut conn,
            ReviewPreviewResult {
                window: ReviewPreviewWindow {
                    matched_files: 2,
                    selected_files: 2,
                    selected_tokens: 1_000,
                    files_token_budget: 60_000,
                    ..Default::default()
                },
                groups: Vec::new(),
            },
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("review/start"), "{payload}");
        assert!(payload.contains("src/"), "{payload}");

        let guard = state.lock().await;
        assert!(guard.modals.pending_review_launch.is_none());
        assert!(guard.modals.confirm_review_truncation.is_none());
        assert!(guard.review.running);
    }

    /// A truncated window asks for consent with the full counters and opens no
    /// audit until the developer answers (c-3).
    #[tokio::test]
    async fn a_truncated_projection_opens_the_consent_card() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.modals.pending_review_launch = Some(pending_launch("crates/"));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        deliver_preview(
            &state,
            &mut conn,
            ReviewPreviewResult {
                window: ReviewPreviewWindow {
                    matched_files: 9,
                    selected_files: 4,
                    selected_tokens: 40_000,
                    omitted_files: 5,
                    omitted_by_budget: 5,
                    files_token_budget: 60_000,
                    ..Default::default()
                },
                groups: Vec::new(),
            },
        )
        .await;

        let guard = state.lock().await;
        let target = guard
            .modals
            .confirm_review_truncation
            .as_ref()
            .expect("the truncated window asks for consent");
        assert_eq!(target.window.omitted_files, 5);
        assert_eq!(target.window.omitted_by_budget, 5);
        assert_eq!(target.window.files_token_budget, 60_000);
        assert!(guard.modals.pending_review_launch.is_none());
        assert_eq!(guard.active_modal(), Some(ActiveModal::ReviewTruncation));
        assert_eq!(guard.modals.confirm_button, ConfirmDialogButton::Cancel);
        assert!(!guard.review.running, "a pending consent opens no audit");
    }

    /// A refused projection restores the launch dialog with its typed fields
    /// instead of leaving the launch pending forever (c-3).
    #[tokio::test]
    async fn a_refused_projection_restores_the_launch_dialog() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.modals.pending_review_launch = Some(pending_launch("srx/"));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let id = send_request(&mut conn, methods::REVIEW_PREVIEW, serde_json::json!({}))
            .await
            .expect("an in-memory sink never fails to write");
        handle_response(
            Response::err(
                RequestId::Number(id),
                tauqe_protocol::errors::INVALID_PARAMS,
                "pattern selects no file",
            ),
            &state,
            &mut conn,
        )
        .await;

        let guard = state.lock().await;
        assert!(guard.modals.pending_review_launch.is_none());
        assert!(guard.modals.confirm_review_truncation.is_none());
        let dialog = guard
            .modals
            .review_dialog
            .as_ref()
            .expect("the launch dialog returns with its fields");
        assert_eq!(dialog.path_inputs[0].get_text(), "srx/");
        assert!(!guard.review.running);
        assert_eq!(
            guard.active_notification().map(|n| n.level),
            Some(NotificationLevel::Error)
        );
    }
    /// The unified diff of a commit is split into the files the inspector lists
    /// and the hunks it renders, so the audit presents the commit exactly as the
    /// renderer knows how to draw it (c-3).
    #[test]
    fn a_commit_diff_is_imported_into_the_audited_change_set() {
        let diff = concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "index 111..222 100644\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -1,2 +1,2 @@ fn main() {\n",
            "-let a = 1;\n",
            "+let a = 2;\n",
            " fn main() {}\n",
            "diff --git a/src/b.rs b/src/b.rs\n",
            "new file mode 100644\n",
            "index 000..333\n",
            "--- /dev/null\n",
            "+++ b/src/b.rs\n",
            "@@ -0,0 +1 @@\n",
            "+fn added() {}\n",
        );

        let files = parse_commit_diff(diff);

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].op_type, tauqe_protocol::EditFileOp::Replace);
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].old_text, "let a = 1;\nfn main() {}\n");
        assert_eq!(files[0].hunks[0].new_text, "let a = 2;\nfn main() {}\n");
        // The header names the ranges of both versions and the section of the
        // change, and every number reaches the staged hunk unchanged.
        let hunk = &files[0].hunks[0];
        assert_eq!(
            (
                hunk.old_start,
                hunk.old_lines,
                hunk.new_start,
                hunk.new_lines
            ),
            (1, 2, 1, 2)
        );
        assert_eq!(hunk.context_hint.as_deref(), Some("fn main() {"));
        assert_eq!(files[1].path, "src/b.rs");
        assert_eq!(files[1].op_type, tauqe_protocol::EditFileOp::Create);
        assert_eq!(files[1].hunks[0].new_text, "fn added() {}\n");
        // A created file has no source range: git states `-0,0`, and the hunk
        // keeps that fact instead of inventing a source line.
        assert_eq!(
            (
                files[1].hunks[0].old_start,
                files[1].hunks[0].old_lines,
                files[1].hunks[0].new_start,
                files[1].hunks[0].new_lines,
            ),
            (0, 0, 1, 1)
        );
        assert!(files[1].hunks[0].context_hint.is_none());
    }
    /// The header states both ranges of the hunk and the section it sits in, so
    /// the audit can name the real lines of both versions of the file
    /// (c-unified-diff-header-parsing).
    #[test]
    fn a_hunk_header_yields_its_ranges_and_its_section() {
        assert_eq!(
            parse_hunk_header("@@ -12,7 +14,8 @@ fn main() {"),
            Some(HunkHeader {
                old_start: 12,
                old_lines: 7,
                new_start: 14,
                new_lines: 8,
                context_hint: Some("fn main() {".to_string()),
            })
        );
        // Git omits a count of one, and a header may carry no section at all.
        assert_eq!(
            parse_hunk_header("@@ -3 +5 @@"),
            Some(HunkHeader {
                old_start: 3,
                old_lines: 1,
                new_start: 5,
                new_lines: 1,
                context_hint: None,
            })
        );
    }

    /// A combined header belongs to a merge diff and names one range per parent,
    /// so it is refused instead of being read as a single pair of ranges.
    #[test]
    fn a_combined_merge_header_is_refused() {
        assert_eq!(
            parse_hunk_header("@@@ -1,4 -1,4 +1,5 @@@ fn main() {"),
            None
        );
        assert_eq!(parse_hunk_header("@@ -1,2 @@"), None);
        assert_eq!(parse_hunk_header("@@ -x,2 +1,2 @@"), None);
        assert_eq!(parse_hunk_header("not a header"), None);
    }
    /// The cache inventory answer fills the browser, and a removal refreshes it
    /// through the same request, so the tree always mirrors the daemon
    /// (c-outer-docs-cache-tui-browser, c-outer-docs-cache-tui-deletion).
    #[tokio::test]
    async fn a_cache_removal_refreshes_the_browser_inventory() {
        let mut st = crate::app::test_state();
        st.modals.outer_docs_cache = Some(crate::app::OuterDocsCacheDialogState {
            loading: true,
            ..Default::default()
        });

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let id = send_request(
            &mut conn,
            methods::OUTER_DOCS_CACHE_LIST,
            serde_json::json!({}),
        )
        .await
        .expect("an in-memory sink never fails to write");
        handle_response(
            Response::ok_typed(
                RequestId::Number(id),
                &tauqe_protocol::OuterDocsCacheListResult {
                    entries: vec![tauqe_protocol::OuterDocsCacheEntry {
                        path: "openrouter/models".to_string(),
                        size_bytes: 1024,
                        estimated_tokens: 256,
                    }],
                },
            ),
            &state,
            &mut conn,
        )
        .await;
        {
            let guard = state.lock().await;
            let dialog = guard
                .modals
                .outer_docs_cache
                .as_ref()
                .expect("the browser stays open");
            assert_eq!(dialog.entries.len(), 1);
            assert!(!dialog.loading);
        }

        let id = send_request(
            &mut conn,
            methods::OUTER_DOCS_CACHE_REMOVE,
            serde_json::json!({ "path": "openrouter/models" }),
        )
        .await
        .expect("an in-memory sink never fails to write");
        handle_response(
            Response::ok_typed(
                RequestId::Number(id),
                &tauqe_protocol::OuterDocsCacheRemoveResult {
                    removed_files: 1,
                    removed_bytes: 1024,
                },
            ),
            &state,
            &mut conn,
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert_eq!(
            payload.matches(methods::OUTER_DOCS_CACHE_LIST).count(),
            2,
            "the removal re-reads the inventory: {payload}"
        );
        let guard = state.lock().await;
        assert!(guard
            .modals
            .outer_docs_cache
            .as_ref()
            .is_some_and(|dialog| dialog.loading));
        assert_eq!(
            guard.active_notification().map(|n| n.level),
            Some(NotificationLevel::Success)
        );
    }
}
