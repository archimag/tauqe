use std::sync::Arc;
use tokio::sync::Mutex;
use tauqe_protocol::{
    methods, ConfigState, ContextState, GitSquashApplyResult, GitSquashGenerateMessageResult,
    GitSquashPreviewResult, HistoryGetResult, RepositoryListFilesResult, RequestId,
    Response, ReviewGetResult, ReviewUpdateItemResult, SystemStatusResult,
};

use crate::app::{AppState, ViewMode};
use crate::i18n::active_t;
use crate::views::repl::TurnStatus;
use super::transport::{OptimisticRollback, RpcConnection};

/// RPC error code returned by the server when the pre-flight toolchain
/// verification blocks a plan or review batch before any step starts.
const PREFLIGHT_VERIFICATION_FAILED: &str = "PREFLIGHT_VERIFICATION_FAILED";

pub mod messages {
    pub fn stub_credentials_created(path: &str) -> String {
        format!("Stub credentials file created at '{}'. Edit it and select 'Check again'.", path)
    }
    pub const CONFIG_CREATED: &str = "tauqe.toml created successfully.";
    pub const API_KEY_SAVED: &str = "OpenRouter API key saved successfully.";
    pub const CONFIG_RELOADED_OK: &str = "Configuration reloaded. All checks passed!";
    pub const API_KEY_NOT_FOUND: &str =
        "API key still not found. Check OPENROUTER_API_KEY environment variable or credentials.toml";
    pub const GIT_INIT_OK: &str = "Git repository initialized successfully.";
    pub const SQUASH_MSG_GENERATED: &str =
        "Message generated via AI. Review or edit, then press Enter to apply.";
    pub const EDIT_FAILED_FALLBACK: &str = "Edit proposal failed to apply";
}

/// How the server produced a [`SystemStatusResult`]. The payload is identical
/// across these operations; the originating method selects the onboarding
/// feedback to surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemStatusAction {
    Query,
    Reloaded,
    Created,
    Saved,
    StubCreated,
}

/// Applies a typed system status payload to the client state.
pub(crate) fn apply_system_status_response(
    st: &mut AppState,
    status: &SystemStatusResult,
    action: SystemStatusAction,
) {
    st.active_model = status.model.clone();
    if let Some(selection) = status.selection.clone() {
        st.model_choice.selection = selection;
    }
    if let Some(roles) = status.roles.clone() {
        st.model_choice.roles = Some(roles);
    }
    if !status.available_models.is_empty() {
        st.available_models = status.available_models.clone();
    }

    st.onboarding.apply_status(status);

    match action {
        SystemStatusAction::StubCreated => {
            st.onboarding.set_status(messages::stub_credentials_created(
                &st.onboarding.default_credentials_path,
            ));
        }
        SystemStatusAction::Created => {
            st.onboarding.set_status(messages::CONFIG_CREATED.to_string());
            st.onboarding.step = st.onboarding.next_step();
            st.onboarding.selected_index = 0;
        }
        SystemStatusAction::Saved => {
            st.onboarding.set_status(messages::API_KEY_SAVED.to_string());
            st.onboarding.step = st.onboarding.next_step();
            st.onboarding.selected_index = 0;
        }
        SystemStatusAction::Reloaded => {
            if status.ready {
                st.onboarding
                    .set_status(messages::CONFIG_RELOADED_OK.to_string());
                if st.view_mode == ViewMode::Onboarding {
                    st.onboarding.step = st.onboarding.next_step();
                    st.onboarding.selected_index = 0;
                }
            } else {
                st.onboarding
                    .set_error(messages::API_KEY_NOT_FOUND.to_string());
            }
        }
        SystemStatusAction::Query => {}
    }

    // A malformed or unreadable `tauqe.toml` falls back to built-in defaults;
    // surface the diagnostic so the user is not silently switched to other
    // models, budgets, and context limits.
    if let Some(err) = status.config_error.as_deref() {
        st.notify_warning(active_t("notify.config_error").replacen("{}", err, 1));
    }
}

pub async fn handle_response(
    resp: Response,
    state: &Arc<Mutex<AppState>>,
    conn: &mut RpcConnection,
) {
    let method = match &resp.id {
        RequestId::Number(id) => conn.take_pending_request(*id),
        _ => None,
    };

    let mut st = state.lock().await;
    if let Some(err) = resp.error {
        if let RequestId::Number(id) = resp.id {
            if let Some(rollback) = conn.take_optimistic_rollback(id) {
                match rollback {
                    OptimisticRollback::ReviewIssueStatus { review_id, item_id, prev_status } => {
                        st.review
                            .set_item_status(review_id.as_deref(), item_id, prev_status);
                    }
                    OptimisticRollback::PlanItemStatus { plan_id, item_id, prev_status } => {
                        if let Some(plan) = st.plans_view.plans.iter_mut().find(|p| p.id == plan_id) {
                            plan.update_item_status(&item_id, prev_status);
                        }
                    }
                    OptimisticRollback::ActiveModel { prev_model, prev_selection } => {
                        st.active_model = prev_model;
                        st.model_choice.selection = prev_selection;
                    }
                }
            }
        }
        if err.code == PREFLIGHT_VERIFICATION_FAILED {
            st.turn_started_at = None;
            st.model.status = TurnStatus::Error;
            st.model.toolchain = None;
            st.model.turn_phase = None;
            st.model.turn_phase_detail = None;
            st.model.error = Some(err.message.clone());
            st.notify_warning(active_t("notify.preflight_failed"));
            return;
        }

        let err_text = active_t("notify.rpc_error").replacen("{}", &err.message, 1);
        match method.as_deref() {
            Some(methods::REVIEW_START) => {
                st.review.running = false;
                st.review.error = Some(err.message.clone());
            }
            Some(methods::PLAN_REFINE) | Some(methods::PLAN_REPLAN) => {
                st.plans_view.refining = false;
                st.plans_view.refining_plan_id = None;
            }
            Some(methods::REVIEW_REFINE_ITEM) => {
                // A failed decomposition aborts the rest of the batch instead of
                // spending further tokens behind a broken call.
                st.review.refining = None;
                st.review.refine_queue.clear();
            }
            Some(methods::GIT_SQUASH_PREVIEW)
            | Some(methods::GIT_SQUASH_GENERATE_MESSAGE)
            | Some(methods::GIT_SQUASH_APPLY) => {
                if let Some(ref mut dialog) = st.squash_dialog {
                    dialog.loading = false;
                    dialog.generating_message = false;
                    dialog.pending_base = None;
                    dialog.applying = false;
                    dialog.status_message = Some(err_text.clone());
                }
            }
            Some(methods::REPOSITORY_INIT)
            | Some(methods::CONFIG_CREATE)
            | Some(methods::CONFIG_RELOAD)
            | Some(methods::CREDENTIALS_SAVE)
            | Some(methods::CREDENTIALS_CREATE_STUB)
            | Some(methods::SYSTEM_STATUS) => {
                st.onboarding.set_error(err.message.clone());
            }
            Some(methods::HISTORY_GET) => {
                st.history_view.loading = false;
            }
            Some(methods::LOCALE_GENERATE) => {
                st.locale_dialog = None;
            }
            _ => {
                if st.view_mode == ViewMode::Onboarding {
                    st.onboarding.set_error(err.message.clone());
                }
            }
        }
        st.notify_error(err_text);
        return;
    }

    // On success the optimistic rollback never applies, but a review status
    // update still needs its target scope so the server's authoritative item
    // replaces the finding in the correct session rather than in whichever
    // session happens to share the numeric finding id.
    let updated_review_id = if let RequestId::Number(id) = resp.id {
        match conn.take_optimistic_rollback(id) {
            Some(OptimisticRollback::ReviewIssueStatus { review_id, .. }) => review_id,
            _ => None,
        }
    } else {
        None
    };

    if method.as_deref() == Some(methods::PLAN_DELETE) {
        st.notify_success(active_t("notify.plan_deleted"));
    }
    if method.as_deref() == Some(methods::REVIEW_DELETE) {
        st.notify_success(active_t("notify.review_deleted"));
    }

    let Some(val) = resp.result else {
        return;
    };

    if let Some(method_name) = method.as_deref() {
        match method_name {
            methods::GIT_SQUASH_APPLY => {
                if let Ok(applied) = serde_json::from_value::<GitSquashApplyResult>(val) {
                    st.squash_dialog = None;
                    let first_line = applied
                        .message
                        .lines()
                        .next()
                        .map(str::to_string)
                        .unwrap_or_else(|| active_t("notify.squashed_commit_fallback"));
                    st.notify_success(
                        active_t("notify.squashed_commits")
                            .replacen("{}", &applied.squashed_commit, 1)
                            .replacen("{}", &first_line, 1),
                    );
                    st.model.last_commit_hash = Some(applied.squashed_commit);
                    st.model.last_commit_summary = Some(first_line);
                }
            }
            methods::GIT_SQUASH_PREVIEW => {
                if let Ok(preview) = serde_json::from_value::<GitSquashPreviewResult>(val) {
                    if let Some(ref mut dialog) = st.squash_dialog {
                        dialog.loading = false;
                        if let Some((mode, _)) = dialog.pending_base.take() {
                            dialog.base_mode = mode;
                        }
                        dialog.base_ref = preview.base_ref;
                        dialog.session_base = preview.session_base;
                        dialog.upstream_base = preview.upstream_base;
                        dialog.commits = preview.commits;
                        dialog.diff_stat = preview.diff_stat;
                        dialog.files = preview
                            .files
                            .into_iter()
                            .map(|f| crate::app::SquashFileItem {
                                path: f.path,
                                diff: f.diff,
                                expanded: true,
                            })
                            .collect();
                        dialog.selected_file_index = 0;
                        dialog.diff_scroll = 0;
                        if dialog.message_editor.is_empty() {
                            if let Some(msg) = preview.suggested_message {
                                dialog.message_editor.insert_str(&msg);
                            }
                        }
                        dialog.status_message = None;
                    }
                }
            }
            methods::GIT_SQUASH_GENERATE_MESSAGE => {
                if let Ok(gen_res) = serde_json::from_value::<GitSquashGenerateMessageResult>(val) {
                    if let Some(total) = gen_res.session_total_cost {
                        st.model.session_total_cost = total;
                    }
                    if let Some(cost) = gen_res.current_cost {
                        st.model.prev_cost = Some(cost);
                    }
                    st.model.current_cost = None;
                    if let Some(ref mut dialog) = st.squash_dialog {
                        dialog.generating_message = false;
                        dialog.message_editor.clear();
                        dialog.message_editor.insert_str(&gen_res.message);
                        dialog.focus = crate::app::SquashDialogFocus::MessageEditor;
                        dialog.status_message = Some(messages::SQUASH_MSG_GENERATED.to_string());
                    }
                }
            }
            methods::HISTORY_GET => {
                if let Ok(history_res) = serde_json::from_value::<HistoryGetResult>(val) {
                    st.history_view.loading = false;
                    st.history_view.has_more = history_res.has_more;
                    st.history_view.total_count = history_res.total_count;
                    st.history_view.estimated_tokens = history_res.estimated_tokens;

                    let select_prev = st.history_view.select_prev_after_load;
                    st.history_view.select_prev_after_load = false;

                    if st.history_view.pending_before_id.take().is_some()
                        && !st.history_view.items.is_empty()
                    {
                        let fetched_count = history_res.items.len();
                        let added_lines = crate::views::history::compute_history_items_line_count(
                            &history_res.items,
                            Some(st.history_view.content_width),
                        );
                        let mut combined = history_res.items;
                        combined.append(&mut st.history_view.items);
                        st.history_view.items = combined;
                        st.history_view.scroll =
                            st.history_view.scroll.saturating_add(added_lines as u16);

                        if select_prev && fetched_count > 0 {
                            st.history_view.selected_item_index = fetched_count - 1;
                            let h = st.last_model_height;
                            let w = st.history_view.content_width;
                            st.history_view.scroll_to_selected_item(h, Some(w));
                        } else {
                            st.history_view.selected_item_index =
                                st.history_view.selected_item_index.saturating_add(fetched_count);
                        }
                    } else {
                        st.history_view.items = history_res.items;
                        st.history_view.auto_scroll = true;
                        if !st.history_view.items.is_empty() {
                            st.history_view.selected_item_index =
                                st.history_view.items.len().saturating_sub(1);
                        }
                    }
                }
            }
            methods::CONTEXT_ADD_PATTERN => {
                if let Ok(pattern_res) =
                    serde_json::from_value::<tauqe_protocol::ContextAddPatternResult>(val)
                {
                    st.context = pattern_res.state;
                    if pattern_res.truncated {
                        st.notify_warning(
                            active_t("notify.context_added_truncated")
                                .replacen("{}", &pattern_res.added_count.to_string(), 1)
                                .replacen("{}", &pattern_res.added_tokens.to_string(), 1),
                        );
                    } else {
                        st.notify_success(
                            active_t("notify.context_added")
                                .replacen("{}", &pattern_res.added_count.to_string(), 1)
                                .replacen("{}", &pattern_res.added_tokens.to_string(), 1),
                        );
                    }
                    if st.context_view.adding_file {
                        st.update_filtered_candidates();
                    }
                }
            }
            methods::REPOSITORY_LIST_FILES => {
                if let Ok(file_res) = serde_json::from_value::<RepositoryListFilesResult>(val) {
                    st.all_repo_files = file_res.files;
                    if st.context_view.adding_file {
                        st.update_filtered_candidates();
                    }
                }
            }
            methods::CONTEXT_GET
            | methods::CONTEXT_ADD
            | methods::CONTEXT_REMOVE
            | methods::CONTEXT_SET_ACCESS
            | methods::CONTEXT_CLEAR => {
                if let Ok(ctx) = serde_json::from_value::<ContextState>(val) {
                    let items_before = st.context.items.len();
                    st.context = ctx;
                    let items_removed = items_before.saturating_sub(st.context.items.len());
                    match method_name {
                        methods::CONTEXT_REMOVE => {
                            st.notify_info(
                                active_t("notify.context_removed")
                                    .replacen("{}", &items_removed.to_string(), 1),
                            );
                        }
                        methods::CONTEXT_CLEAR => {
                            st.notify_success(
                                active_t("notify.context_cleared")
                                    .replacen("{}", &items_removed.to_string(), 1),
                            );
                        }
                        _ => {}
                    }
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
            }
            methods::REPOSITORY_INIT => {
                if let Ok(init_res) =
                    serde_json::from_value::<tauqe_protocol::RepositoryInitResult>(val)
                {
                    st.repo_state = Some(init_res.repository);
                    st.onboarding.has_git = true;
                    st.onboarding.set_status(messages::GIT_INIT_OK.to_string());
                    st.onboarding.step = st.onboarding.next_step();
                    st.onboarding.selected_index = 0;
                }
            }
            methods::CONFIG_GET | methods::CONFIG_SET => {
                if let Ok(cfg) = serde_json::from_value::<ConfigState>(val) {
                    st.workflow = cfg.workflow;
                    st.active_model = cfg.model;
                    if let Some(sel) = cfg.selection {
                        st.model_choice.selection = sel;
                    }
                    if cfg.roles.is_some() {
                        st.model_choice.roles = cfg.roles;
                    }
                    if !cfg.available_models.is_empty() {
                        st.available_models = cfg.available_models;
                    }
                    st.edit_protocol = cfg.edit_protocol;
                    if !cfg.available_workflows.is_empty() {
                        st.available_workflows = cfg.available_workflows;
                    }
                    if !cfg.available_edit_protocols.is_empty() {
                        st.available_edit_protocols = cfg.available_edit_protocols;
                    }
                }
            }
            methods::MODEL_CURRENT => {
                if let Ok(res) = serde_json::from_value::<tauqe_protocol::ModelCurrentResult>(val) {
                    st.active_model = res.effective_model;
                    st.model_choice = crate::app::ModelChoice {
                        selection: res.selection,
                        roles: res.roles,
                    };
                    if !res.available_models.is_empty() {
                        st.available_models = res.available_models;
                    }
                }
            }
            methods::MODEL_SELECT => {
                if let Ok(res) = serde_json::from_value::<tauqe_protocol::ModelSelectResult>(val) {
                    st.active_model = res.effective_model;
                    st.model_choice = crate::app::ModelChoice {
                        selection: res.selection,
                        roles: res.roles,
                    };
                }
            }
            methods::REVIEW_GET => {
                if let Ok(res) = serde_json::from_value::<ReviewGetResult>(val) {
                    if let Some(session) = res.session {
                        st.review.set_session(session);
                    }
                }
            }
            methods::REVIEW_UPDATE_ITEM => {
                if let Ok(res) = serde_json::from_value::<ReviewUpdateItemResult>(val) {
                    st.review
                        .apply_item(updated_review_id.as_deref(), res.item);
                }
            }
            methods::REVIEW_LIST => {
                if let Ok(res) = serde_json::from_value::<tauqe_protocol::ReviewListResult>(val) {
                    st.review.set_sessions(res.reviews, res.active_id);
                }
            }
            methods::REVIEW_START | methods::REVIEW_CANCEL | methods::REVIEW_DELETE => {}
            methods::REVIEW_REFINE_ITEM => {
                let review_id = st.review.refining.take();
                if let Ok(res) =
                    serde_json::from_value::<tauqe_protocol::ReviewRefineItemResult>(val)
                {
                    match res {
                        tauqe_protocol::ReviewRefineItemResult::Success { item, explanation } => {
                            let item_id = item.id;
                            let target_review_id = review_id
                                .clone()
                                .or_else(|| st.review.session.as_ref().map(|s| s.id.clone()));
                            if let Some(ref rid) = target_review_id {
                                st.review.expanded_items.insert(format!("{}:{}", rid, item_id));
                                st.review.collapsed_sessions.remove(rid);
                            }
                            st.review.apply_item(review_id.as_deref(), item);
                            let msg = match explanation {
                                Some(exp) if !exp.trim().is_empty() => {
                                    active_t("notify.finding_decomposed").replacen("{}", &exp, 1)
                                }
                                _ => active_t("notify.finding_decomposed_default"),
                            };
                            st.notify_success(msg);
                            st.review.clamp_selection();
                            st.review.scroll_to_selected();
                        }
                        tauqe_protocol::ReviewRefineItemResult::Blocked { reason } => {
                            // A refusal is a semantic stop, not a transient
                            // failure: draining the queue would only repeat it.
                            st.review.refine_queue.clear();
                            st.notify_warning(
                                active_t("notify.finding_decompose_blocked")
                                    .replacen("{}", &reason, 1),
                            );
                        }
                    }
                }
            }
            methods::PLAN_LIST => {
                if let Ok(res) = serde_json::from_value::<tauqe_protocol::PlanListResult>(val) {
                    st.plans_view.plans = res.plans;
                    st.plans_view.clamp_selection();
                }
            }
            methods::PLAN_GET | methods::PLAN_UPDATE_ITEM => {
                if let Ok(res) = serde_json::from_value::<tauqe_protocol::PlanGetResult>(val.clone()) {
                    if let Some(plan) = res.plan {
                        if let Some(pos) = st.plans_view.plans.iter().position(|p| p.id == plan.id) {
                            st.plans_view.plans[pos] = plan;
                        } else {
                            st.plans_view.plans.push(plan);
                        }
                    }
                } else if let Ok(res) = serde_json::from_value::<tauqe_protocol::PlanUpdateItemResult>(val) {
                    if let Some(pos) = st.plans_view.plans.iter().position(|p| p.id == res.plan.id) {
                        st.plans_view.plans[pos] = res.plan;
                    } else {
                        st.plans_view.plans.push(res.plan);
                    }
                }
            }
            methods::PLAN_SAVE => {
                // A save creates or fully replaces a plan. Its body and cursor
                // anchor arrive through the preceding `plan/updated` event, so
                // the response only guarantees the developer lands on the Plans
                // tab instead of the view the save was triggered from (c-4).
                st.view_mode = ViewMode::Plans;
            }
            methods::PLAN_SET_ACTIVE => {}
            methods::PLAN_REFINE | methods::PLAN_REPLAN => {
                st.plans_view.refining = false;
                let refining_plan_id = st.plans_view.refining_plan_id.take();
                if let Ok(refine_res) = serde_json::from_value::<tauqe_protocol::PlanRefineResult>(val) {
                    match refine_res {
                        tauqe_protocol::PlanRefineResult::Success { plan, explanation } => {
                            let plan_title = plan.title.clone();
                            let plan_id = plan.id.clone();
                            let (total_items, completed_items) = plan.count_stats();
                            if let Some(pos) = st.plans_view.plans.iter().position(|p| p.id == plan.id) {
                                st.plans_view.plans[pos] = plan;
                            } else {
                                st.plans_view.plans.push(plan);
                            }
                            st.plans_view.expanded_plans.insert(plan_id.clone());
                            st.plans_view.collapsed_sections.remove(&format!("impl:{}", plan_id));
                            st.plans_view.clamp_selection();
                            st.plans_view.scroll_to_selected();
                            let msg = match explanation {
                                Some(exp) if !exp.trim().is_empty() => {
                                    active_t("notify.plan_horizon_updated")
                                        .replacen("{}", &plan_title, 1)
                                        .replacen("{}", &completed_items.to_string(), 1)
                                        .replacen("{}", &total_items.to_string(), 1)
                                        .replacen("{}", &exp, 1)
                                }
                                _ => active_t("notify.plan_horizon_updated_default")
                                    .replacen("{}", &plan_title, 1)
                                    .replacen("{}", &completed_items.to_string(), 1)
                                    .replacen("{}", &total_items.to_string(), 1),
                            };
                            st.notify_success(msg);
                        }
                        tauqe_protocol::PlanRefineResult::Blocked { reason } => {
                            let target_plan_id = refining_plan_id.unwrap_or_else(|| {
                                st.plans_view.selected_row().map(|r| r.plan_id().to_string()).unwrap_or_default()
                            });
                            let plan_title = st.plans_view.plans.iter()
                                .find(|p| p.id == target_plan_id)
                                .map(|p| p.title.clone())
                                .unwrap_or_else(|| "Plan".to_string());
                            st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            st.plans_view.refine_blocked = Some(crate::views::plans::PlanRefineBlockedState {
                                plan_id: target_plan_id,
                                plan_title,
                                reason,
                            });
                            st.view_mode = ViewMode::Plans;
                            st.notify_warning(active_t("notify.plan_horizon_blocked"));
                        }
                    }
                }
            }
            methods::LOCALE_GENERATE => {
                st.locale_dialog = None;
                if let Ok(result) =
                    serde_json::from_value::<tauqe_protocol::LocaleGenerateResult>(val)
                {
                    let language = result.language.clone();
                    let locale =
                        crate::i18n::CustomLocale::new(language.clone(), result.translations);
                    // Hot-swap first so the interface reacts immediately, then
                    // persist the single custom locale (c-4, c-5). A failed write
                    // degrades to a session-only swap rather than a fatal error.
                    st.apply_custom_locale(locale.clone());
                    match crate::i18n::save_custom_locale(&locale) {
                        Ok(_) => st.notify_success(
                            active_t("notify.locale_switched").replacen("{}", &language, 1),
                        ),
                        Err(err) => st.notify_warning(
                            active_t("notify.locale_switched_unsaved")
                                .replacen("{}", &language, 1)
                                .replacen("{}", &err.to_string(), 1),
                        ),
                    }
                }
            }
            methods::SYSTEM_STATUS
            | methods::CONFIG_RELOAD
            | methods::CONFIG_CREATE
            | methods::CREDENTIALS_SAVE
            | methods::CREDENTIALS_CREATE_STUB => {
                if let Ok(status) = serde_json::from_value::<SystemStatusResult>(val) {
                    let action = match method_name {
                        methods::CONFIG_RELOAD => SystemStatusAction::Reloaded,
                        methods::CONFIG_CREATE => SystemStatusAction::Created,
                        methods::CREDENTIALS_SAVE => SystemStatusAction::Saved,
                        methods::CREDENTIALS_CREATE_STUB => SystemStatusAction::StubCreated,
                        _ => SystemStatusAction::Query,
                    };
                    apply_system_status_response(&mut st, &status, action);
                }
            }
            _ => {}
        }
    }

    // A batch decomposition drains one finding at a time: after each settled
    // `review/refineItem` response, dispatch the next queued finding, or drop the
    // busy marker once the queue is exhausted. The state lock is released before
    // writing the request so the UI never blocks on transport I/O.
    if method.as_deref() == Some(methods::REVIEW_REFINE_ITEM) {
        let next = st.review.next_refine_target();
        drop(st);
        if let Some(target) = next {
            let params = tauqe_protocol::ReviewRefineItemParams {
                review_id: Some(target.review_id),
                item_id: target.item_id,
                model: None,
                instructions: None,
            };
            if let Ok(value) = serde_json::to_value(&params) {
                let _ = super::transport::send_request(conn, methods::REVIEW_REFINE_ITEM, value)
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::NotificationLevel;
    use tauqe_protocol::{
        IssueStatus, IssueType, ResponseError, ReviewIssue, ReviewItemStatus, ReviewSession,
    };

    fn test_state() -> Arc<Mutex<AppState>> {
        Arc::new(Mutex::new(crate::app::test_state()))
    }

    #[tokio::test]
    async fn test_preflight_failure_unsticks_model_and_warns() {
        let state = test_state();
        {
            let mut st = state.lock().await;
            st.model.status = TurnStatus::Awaiting;
            st.turn_started_at = Some(std::time::Instant::now());
            assert!(st.model.is_busy());
        }

        let resp = Response {
            id: RequestId::Number(1),
            result: None,
            error: Some(ResponseError::new(
                PREFLIGHT_VERIFICATION_FAILED,
                "Pre-flight verification failed: the repository does not build cleanly.",
            )),
        };
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_response(resp, &state, &mut conn).await;

        let st = state.lock().await;
        assert_eq!(st.model.status, TurnStatus::Error);
        assert!(!st.model.is_busy());
        assert!(st.turn_started_at.is_none());
        assert!(st
            .model
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Pre-flight verification failed")));
        let notif = st
            .active_notification()
            .expect("pre-flight failure should raise a notification");
        assert_eq!(notif.level, NotificationLevel::Warning);
    }

    #[tokio::test]
    async fn test_plan_refine_response_updates_horizon_and_notifies() {
        let state = test_state();
        {
            let mut st = state.lock().await;
            st.plans_view.refining = true;
            st.plans_view.refining_plan_id = Some("p1".to_string());
            st.plans_view.plans = vec![tauqe_protocol::Plan {
                id: "p1".to_string(),
                title: "Plan 1".to_string(),
                ..Default::default()
            }];
        }

        let plan = tauqe_protocol::Plan {
            id: "p1".to_string(),
            title: "Plan 1".to_string(),
            items: vec![tauqe_protocol::PlanItem {
                id: "s1".to_string(),
                title: "Step 1".to_string(),
                status: tauqe_protocol::PlanItemStatus::Todo,
                ..Default::default()
            }],
            ..Default::default()
        };
        let resp = Response {
            id: RequestId::Number(1),
            result: Some(
                serde_json::to_value(tauqe_protocol::PlanRefineResult::Success {
                    plan,
                    explanation: Some("New horizon synthesized".to_string()),
                })
                .unwrap(),
            ),
            error: None,
        };
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        crate::rpc::transport::send_request_with_id(
            &mut conn,
            1,
            methods::PLAN_REFINE,
            serde_json::json!({}),
        )
        .await
        .unwrap();

        handle_response(resp, &state, &mut conn).await;

        let st = state.lock().await;
        assert!(!st.plans_view.refining);
        assert!(st.plans_view.refining_plan_id.is_none());
        assert_eq!(st.plans_view.plans[0].items.len(), 1);
        let notif = st.active_notification().expect("notification expected");
        assert!(notif.text.contains("Plan 'Plan 1' horizon updated"));
    }

    #[tokio::test]
    async fn test_plan_refine_response_blocked_surfaces_dialog() {
        let state = test_state();
        {
            let mut st = state.lock().await;
            st.plans_view.refining = true;
            st.plans_view.refining_plan_id = Some("p1".to_string());
            st.plans_view.plans = vec![tauqe_protocol::Plan {
                id: "p1".to_string(),
                title: "Plan 1".to_string(),
                ..Default::default()
            }];
        }

        let resp = Response {
            id: RequestId::Number(2),
            result: Some(
                serde_json::to_value(tauqe_protocol::PlanRefineResult::Blocked {
                    reason: "Contradiction found".to_string(),
                })
                .unwrap(),
            ),
            error: None,
        };
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        crate::rpc::transport::send_request_with_id(
            &mut conn,
            2,
            methods::PLAN_REFINE,
            serde_json::json!({}),
        )
        .await
        .unwrap();

        handle_response(resp, &state, &mut conn).await;

        let st = state.lock().await;
        assert!(!st.plans_view.refining);
        assert_eq!(st.view_mode, ViewMode::Plans);
        let blocked = st.plans_view.refine_blocked.as_ref().expect("refine_blocked expected");
        assert_eq!(blocked.plan_id, "p1");
        assert_eq!(blocked.reason, "Contradiction found");
    }

    fn review_item(id: u32, status: ReviewItemStatus) -> ReviewIssue {
        ReviewIssue {
            id,
            title: format!("Finding {id}"),
            model: None,
            issue_type: IssueType::Quality,
            status,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            plan: Vec::new(),
            note: None,
        }
    }

    fn review_session(id: &str, status: ReviewItemStatus) -> ReviewSession {
        ReviewSession {
            id: id.to_string(),
            title: format!("Session {id}"),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![review_item(1, status).into()],
            raw_markdown: String::new(),
        }
    }

    #[tokio::test]
    async fn test_review_status_rollback_is_scoped_by_review_id() {
        let state = test_state();
        {
            let mut st = state.lock().await;
            st.review = crate::views::review::ReviewViewState {
                sessions: vec![
                    review_session("rev-1", ReviewItemStatus::InProgress),
                    review_session("rev-2", ReviewItemStatus::InProgress),
                ],
                session: Some(review_session("rev-1", ReviewItemStatus::InProgress)),
                ..Default::default()
            };
        }

        let mut conn = RpcConnection::new(Vec::<u8>::new());
        crate::rpc::transport::send_request_with_id(
            &mut conn,
            42,
            methods::REVIEW_UPDATE_ITEM,
            serde_json::json!({}),
        )
        .await
        .unwrap();
        conn.record_optimistic_rollback(
            42,
            OptimisticRollback::ReviewIssueStatus {
                review_id: Some("rev-2".to_string()),
                item_id: 1,
                prev_status: IssueStatus::Todo,
            },
        );

        let resp = Response {
            id: RequestId::Number(42),
            result: None,
            error: Some(ResponseError::new("REVIEW_UPDATE_FAILED", "boom")),
        };
        handle_response(resp, &state, &mut conn).await;

        let st = state.lock().await;
        // Only rev-2 rolls back; rev-1 keeps its optimistic value.
        assert_eq!(
            st.review.sessions[0].items[0].status,
            ReviewItemStatus::InProgress
        );
        assert_eq!(st.review.sessions[1].items[0].status, ReviewItemStatus::Todo);
        assert_eq!(
            st.review.session.as_ref().map(|s| s.items[0].status),
            Some(ReviewItemStatus::InProgress)
        );
    }

    #[tokio::test]
    async fn test_review_update_item_response_is_scoped_by_review_id() {
        let state = test_state();
        {
            let mut st = state.lock().await;
            st.review = crate::views::review::ReviewViewState {
                sessions: vec![
                    review_session("rev-1", ReviewItemStatus::Todo),
                    review_session("rev-2", ReviewItemStatus::InProgress),
                ],
                session: Some(review_session("rev-1", ReviewItemStatus::Todo)),
                ..Default::default()
            };
        }

        let mut conn = RpcConnection::new(Vec::<u8>::new());
        crate::rpc::transport::send_request_with_id(
            &mut conn,
            43,
            methods::REVIEW_UPDATE_ITEM,
            serde_json::json!({}),
        )
        .await
        .unwrap();
        conn.record_optimistic_rollback(
            43,
            OptimisticRollback::ReviewIssueStatus {
                review_id: Some("rev-2".to_string()),
                item_id: 1,
                prev_status: IssueStatus::Todo,
            },
        );

        let resp = Response {
            id: RequestId::Number(43),
            result: Some(
                serde_json::to_value(ReviewUpdateItemResult {
                    item: review_item(1, ReviewItemStatus::Fixed),
                })
                .unwrap(),
            ),
            error: None,
        };
        handle_response(resp, &state, &mut conn).await;

        let st = state.lock().await;
        // The server's authoritative item lands in rev-2 only.
        assert_eq!(st.review.sessions[0].items[0].status, ReviewItemStatus::Todo);
        assert_eq!(st.review.sessions[1].items[0].status, ReviewItemStatus::Fixed);
        assert_eq!(
            st.review.session.as_ref().map(|s| s.items[0].status),
            Some(ReviewItemStatus::Todo)
        );
    }
}
