use std::sync::Arc;
use tokio::sync::Mutex;
use tauqe_protocol::{
    methods, ConfigState, ContextState, GitSquashApplyResult, GitSquashGenerateMessageResult,
    GitSquashPreviewResult, GitUndoResult, HistoryGetResult, ModelRef,
    RepositoryListFilesResult, RequestId, Response, ReviewGetResult, ReviewUpdateItemResult,
};

use crate::app::{AppState, OnboardingStep, ViewMode};
use super::transport::{take_optimistic_rollback, take_pending_request, OptimisticRollback};

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

pub fn apply_system_status_response(st: &mut AppState, val: &serde_json::Value) {
    let ready = val.get("ready").and_then(|v| v.as_bool()).unwrap_or(false);
    let has_git = val.get("has_git").and_then(|v| v.as_bool()).unwrap_or(true);
    let has_config = val.get("has_config").and_then(|v| v.as_bool()).unwrap_or(false);
    let has_api_key = val.get("has_api_key").and_then(|v| v.as_bool()).unwrap_or(false);
    let config_path = val.get("config_path").and_then(|v| v.as_str()).map(|s| s.to_string());
    let credentials_path = val
        .get("credentials_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(m) = val
        .get("model")
        .and_then(|v| serde_json::from_value::<ModelRef>(v.clone()).ok())
    {
        st.active_model = m;
    }
    if let Some(av) = val
        .get("available_models")
        .and_then(|v| serde_json::from_value::<Vec<ModelRef>>(v.clone()).ok())
    {
        if !av.is_empty() {
            st.available_models = av;
        }
    }

    st.onboarding.has_git = has_git;
    st.onboarding.has_config = has_config;
    st.onboarding.has_api_key = has_api_key;
    if config_path.is_some() {
        st.onboarding.config_path = config_path;
    }
    if credentials_path.is_some() {
        st.onboarding.credentials_path = credentials_path;
    }

    if val.get("stub_created").and_then(|v| v.as_bool()).unwrap_or(false) {
        st.onboarding.set_status(messages::stub_credentials_created(
            &st.onboarding.default_credentials_path,
        ));
    } else if val.get("created").and_then(|v| v.as_bool()).unwrap_or(false) {
        st.onboarding.set_status(messages::CONFIG_CREATED.to_string());
        if !has_api_key {
            st.onboarding.step = OnboardingStep::Credentials;
            st.onboarding.selected_index = 0;
        } else {
            st.onboarding.step = OnboardingStep::Ready;
            st.onboarding.selected_index = 0;
        }
    } else if val.get("saved").and_then(|v| v.as_bool()).unwrap_or(false) {
        st.onboarding.set_status(messages::API_KEY_SAVED.to_string());
        st.onboarding.step = OnboardingStep::Ready;
        st.onboarding.selected_index = 0;
    } else if val.get("reloaded").and_then(|v| v.as_bool()).unwrap_or(false) {
        if ready {
            st.onboarding.set_status(messages::CONFIG_RELOADED_OK.to_string());
            if st.view_mode == ViewMode::Onboarding {
                st.onboarding.step = OnboardingStep::Ready;
                st.onboarding.selected_index = 0;
            }
        } else {
            st.onboarding.set_error(messages::API_KEY_NOT_FOUND.to_string());
        }
    }
}

pub async fn handle_response(resp: Response, state: &Arc<Mutex<AppState>>) {
    let method = match &resp.id {
        RequestId::Number(id) => take_pending_request(*id),
        _ => None,
    };

    let mut st = state.lock().await;
    if let Some(err) = resp.error {
        if let RequestId::Number(id) = resp.id {
            if let Some(rollback) = take_optimistic_rollback(id) {
                match rollback {
                    OptimisticRollback::ReviewItemStatus { item_id, prev_status } => {
                        if let Some(session) = st.review.session.as_mut() {
                            if let Some(item) = session.items.iter_mut().find(|i| i.id == item_id) {
                                item.status = prev_status;
                            }
                        }
                    }
                    OptimisticRollback::ReviewItemChecked { item_id } => {
                        if let Some(session) = st.review.session.as_mut() {
                            if let Some(item) = session.items.iter_mut().find(|i| i.id == item_id) {
                                item.is_checked = !item.is_checked;
                            }
                        }
                    }
                    OptimisticRollback::PlanItemStatus { plan_id, item_id, prev_status } => {
                        if let Some(plan) = st.plans_view.current_plan.as_mut() {
                            if plan.id == plan_id {
                                plan.update_item_status(&item_id, prev_status);
                            }
                        }
                    }
                    OptimisticRollback::PlanItemChecked { plan_id, item_id } => {
                        if let Some(plan) = st.plans_view.current_plan.as_mut() {
                            if plan.id == plan_id {
                                plan.toggle_item_checked(&item_id);
                            }
                        }
                    }
                    OptimisticRollback::ActiveModel { prev_model } => {
                        st.active_model = prev_model;
                    }
                }
            }
        }
        let err_text = format!("Error: {}", err.message);
        match method.as_deref() {
            Some(methods::REVIEW_START) => {
                st.review.running = false;
                st.review.error = Some(err.message.clone());
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
            _ => {
                if st.view_mode == ViewMode::Onboarding {
                    st.onboarding.set_error(err.message.clone());
                }
            }
        }
        st.notify_error(err_text);
        return;
    }

    if let RequestId::Number(id) = resp.id {
        take_optimistic_rollback(id);
    }

    if method.as_deref() == Some(methods::PLAN_DELETE) {
        st.notify_success("Plan deleted");
    }

    let Some(val) = resp.result else {
        return;
    };

    if let Some(method_name) = method.as_deref() {
        match method_name {
            methods::GIT_SQUASH_APPLY => {
                if let Ok(applied) = serde_json::from_value::<GitSquashApplyResult>(val) {
                    st.squash_dialog = None;
                    let first_line = applied.message.lines().next().unwrap_or("Squashed commit");
                    st.notify_success(format!(
                        "Squashed commits into {} ('{}')",
                        applied.squashed_commit, first_line
                    ));
                    st.model.last_commit_hash = Some(applied.squashed_commit);
                    st.model.last_commit_summary = Some(first_line.to_string());
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
            methods::GIT_UNDO => {
                if let Ok(undo_res) = serde_json::from_value::<GitUndoResult>(val) {
                    st.notify_success(undo_res.message);
                    st.model.last_commit_hash = None;
                    st.model.last_commit_summary = None;
                    st.model.edit_final_applied = None;
                    st.model.files.clear();
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
                        let added_lines = crate::ui::history::compute_history_items_line_count(
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
                    st.notify_success(format!(
                        "Added {} files (~{} tokens)",
                        pattern_res.added_count, pattern_res.added_tokens
                    ));
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
                            st.notify_info(format!("Removed {} file(s) from context", items_removed));
                        }
                        methods::CONTEXT_CLEAR => {
                            st.notify_success(format!("Cleared {} file(s) from context", items_removed));
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
                    if !st.onboarding.has_config {
                        st.onboarding.step = OnboardingStep::Config;
                        st.onboarding.selected_index = 0;
                    } else if !st.onboarding.has_api_key {
                        st.onboarding.step = OnboardingStep::Credentials;
                        st.onboarding.selected_index = 0;
                    } else {
                        st.onboarding.step = OnboardingStep::Ready;
                        st.onboarding.selected_index = 0;
                    }
                }
            }
            methods::CONFIG_GET | methods::CONFIG_SET => {
                if let Ok(cfg) = serde_json::from_value::<ConfigState>(val) {
                    st.workflow = cfg.workflow;
                    st.active_model = cfg.model;
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
            methods::REVIEW_GET => {
                if let Ok(res) = serde_json::from_value::<ReviewGetResult>(val) {
                    if let Some(session) = res.session {
                        st.review.set_session(session);
                    }
                }
            }
            methods::REVIEW_UPDATE_ITEM => {
                if let Ok(res) = serde_json::from_value::<ReviewUpdateItemResult>(val) {
                    st.review.apply_item(res.item);
                }
            }
            methods::REVIEW_START | methods::REVIEW_CANCEL => {}
            methods::PLAN_LIST => {
                if let Ok(res) = serde_json::from_value::<tauqe_protocol::PlanListResult>(val) {
                    st.plans_view.plans_list = res.plans;
                    let current_is_stale = st.plans_view.current_plan.as_ref().is_some_and(|p| {
                        !st.plans_view.plans_list.iter().any(|l| l.id == p.id)
                    });
                    if current_is_stale {
                        st.plans_view.reset_view_for_new_plan();
                        st.plans_view.current_plan = None;
                    }
                    st.plans_view.active_plan_id = res.active_id.clone();
                    if let Some(active) = &res.active_id {
                        if let Some(pos) = st.plans_view.plans_list.iter().position(|p| &p.id == active) {
                            st.plans_view.selected_plan_index = pos;
                        }
                    }
                }
            }
            methods::PLAN_GET | methods::PLAN_SAVE | methods::PLAN_UPDATE_ITEM => {
                if let Ok(res) = serde_json::from_value::<tauqe_protocol::PlanGetResult>(val.clone()) {
                    if let Some(plan) = res.plan {
                        let is_diff = st.plans_view.current_plan.as_ref().map(|p| &p.id) != Some(&plan.id);
                        if is_diff {
                            st.plans_view.reset_view_for_new_plan();
                        }
                        st.plans_view.active_plan_id = Some(plan.id.clone());
                        if let Some(pos) = st.plans_view.plans_list.iter().position(|p| p.id == plan.id) {
                            st.plans_view.selected_plan_index = pos;
                        }
                        st.plans_view.current_plan = Some(plan);
                    }
                } else if let Ok(res) = serde_json::from_value::<tauqe_protocol::PlanUpdateItemResult>(val) {
                    st.plans_view.current_plan = Some(res.plan);
                }
            }
            methods::PLAN_SET_ACTIVE => {
                if let Ok(res) = serde_json::from_value::<tauqe_protocol::PlanSetActiveResult>(val) {
                    st.plans_view.active_plan_id = res.active_id.clone();
                    if let Some(active) = &res.active_id {
                        if let Some(pos) = st.plans_view.plans_list.iter().position(|p| &p.id == active) {
                            st.plans_view.selected_plan_index = pos;
                        }
                    }
                }
            }
            methods::SYSTEM_STATUS
            | methods::CONFIG_RELOAD
            | methods::CONFIG_CREATE
            | methods::CREDENTIALS_SAVE
            | methods::CREDENTIALS_CREATE_STUB => {
                apply_system_status_response(&mut st, &val);
            }
            _ => {}
        }
    }
}
