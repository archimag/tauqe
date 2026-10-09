use std::sync::Arc;
use tokio::sync::Mutex;
use tauqe_protocol::{
    events, ConfigState, ContextState, EditFileDoneEvent, EditFileRetryingEvent,
    EditFileStartedEvent, EditFinishedEvent, EditHunkEvent, Event, GitCommitCreatedEvent,
    GitSquashApplyResult, GitUndoResult, HistoryEntryAddedEvent, ModelDeltaEvent,
    ModelErrorEvent, ModelFinishedEvent, ModelResult, ModelResultEvent, ModelStartedEvent,
    ModelUsageEvent, RepositoryState, ReviewContentDeltaEvent, ReviewErrorEvent,
    ReviewFinishedEvent, ReviewReasoningDeltaEvent,
};

use crate::app::AppState;
use crate::ui::develop::{DevelopView, StreamingFileEdit, StreamingHunk};
use super::responses::messages;

pub fn fail_safe_reject_edits(model: &mut DevelopView, reason: &str) {
    if !model.edits_active || model.edit_final_applied.is_some() {
        return;
    }
    for f in model.files.iter_mut() {
        if f.status == "running" || f.status == "retrying" {
            f.status = "error".to_string();
            f.retry_info = None;
            if f.error.is_none() {
                f.error = Some(reason.to_string());
            }
        }
    }
    model.edit_final_applied = Some(false);
    model.edit_final_error = Some(reason.to_string());
}

pub async fn handle_event(ev: Event, state: &Arc<Mutex<AppState>>, is_reasoning: &mut bool) {
    let mut st = state.lock().await;
    match ev.method.as_str() {
        events::GIT_STATE_CHANGED => {
            if let Some(params) = ev.params {
                if let Some(val) = params.get("repository") {
                    if let Ok(repo) = serde_json::from_value::<RepositoryState>(val.clone()) {
                        st.repo_state = Some(repo);
                    }
                }
            }
        }
        events::GIT_COMMIT_CREATED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<GitCommitCreatedEvent>(params) {
                    st.model.last_commit_hash = Some(data.commit_hash);
                    st.model.last_commit_summary = Some(data.summary);
                    for f in st.model.files.iter_mut() {
                        f.status = "ok".to_string();
                        f.error = None;
                        f.retry_info = None;
                    }
                }
            }
        }
        events::GIT_UNDO_COMPLETED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<GitUndoResult>(params) {
                    st.notify_success(data.message);
                }
            }
        }
        events::GIT_SQUASH_COMPLETED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<GitSquashApplyResult>(params) {
                    st.squash_dialog = None;
                    let first_line = data.message.lines().next().unwrap_or("Squashed commit");
                    st.notify_success(format!(
                        "Squashed commits into {} ('{}')",
                        data.squashed_commit, first_line
                    ));
                    st.model.last_commit_hash = Some(data.squashed_commit);
                    st.model.last_commit_summary = Some(first_line.to_string());
                }
            }
        }
        events::HISTORY_ENTRY_ADDED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<HistoryEntryAddedEvent>(params) {
                    let item_tokens = data.estimated_tokens.unwrap_or_else(|| {
                        data.item.text.chars().count().div_ceil(4) as u64
                    });
                    if data.estimated_tokens.is_some() {
                        st.history_view.estimated_tokens = item_tokens;
                    } else {
                        st.history_view.estimated_tokens += item_tokens;
                    }
                    st.history_view.items.push(data.item);
                    st.history_view.total_count += 1;
                    if st.history_view.auto_scroll {
                        let view_height = st.last_model_height;
                        let total_lines = crate::ui::history::compute_history_items_line_count(&st.history_view.items, None) as u16;
                        st.history_view.scroll = total_lines.saturating_sub(view_height);
                        st.history_view.selected_item_index =
                            st.history_view.items.len().saturating_sub(1);
                    }
                }
            }
        }
        events::CONFIG_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(cfg) = serde_json::from_value::<ConfigState>(params) {
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
        }
        events::CONTEXT_CHANGED => {
            if let Some(params) = ev.params {
                if let Some(val) = params.get("state") {
                    if let Ok(ctx) = serde_json::from_value::<ContextState>(val.clone()) {
                        st.context = ctx;
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
            }
        }
        events::MODEL_STARTED => {
            *is_reasoning = false;
            st.turn_started_at.get_or_insert_with(std::time::Instant::now);
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelStartedEvent>(params) {
                    st.model.operation_id = Some(data.operation_id);
                    st.model.model = Some(data.model);
                    st.model.status = "awaiting".to_string();
                    st.model.edits_active = false;
                    st.model.files.clear();
                    st.model.selected_file_index = 0;
                    st.model.edit_final_applied = None;
                    st.model.edit_final_error = None;
                    st.model.last_commit_hash = None;
                    st.model.last_commit_summary = None;
                    st.model.toolchain_command = None;
                    st.model.toolchain_status = None;
                    st.model.turn_phase = None;
                    st.model.turn_round = None;
                    st.model.turn_max_rounds = None;
                    st.model.turn_phase_detail = None;
                    st.model.auto_scroll = true;
                    st.model.current_cost = Some(0.0);
                }
            }
        }
        events::TURN_PHASE => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::TurnPhaseEvent>(params) {
                    st.model.turn_phase = Some(data.phase);
                    st.model.turn_round = data.round;
                    st.model.turn_max_rounds = data.max_rounds;
                    st.model.turn_phase_detail = data.detail;
                    st.model.status = match data.phase {
                        tauqe_protocol::TurnPhase::Discovery => "discovery".to_string(),
                        tauqe_protocol::TurnPhase::Proposal => {
                            if *is_reasoning {
                                "thinking".to_string()
                            } else {
                                "responding".to_string()
                            }
                        }
                        tauqe_protocol::TurnPhase::Staging => "staging".to_string(),
                        tauqe_protocol::TurnPhase::Verification => "verifying".to_string(),
                        tauqe_protocol::TurnPhase::Healing => "healing".to_string(),
                    };
                }
            }
        }
        events::TOOLCHAIN_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::ToolchainStartedEvent>(params) {
                    st.model.toolchain_command = Some(data.command.clone());
                    st.model.toolchain_status = Some(format!("Running `{}`...", data.command));
                    st.model.status = "verifying".to_string();
                }
            }
        }
        events::TOOLCHAIN_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::ToolchainFinishedEvent>(params) {
                    st.model.toolchain_command = None;
                    st.model.toolchain_status = Some(if data.success {
                        format!("`{}` passed", data.command)
                    } else {
                        data.message.unwrap_or_else(|| format!("`{}` failed", data.command))
                    });
                }
            }
        }
        events::MODEL_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    if !data.delta.is_empty() {
                        st.model.status = "thinking".to_string();
                        if !*is_reasoning {
                            *is_reasoning = true;
                            st.model.reasoning.show = true;
                            if !st.model.reasoning.is_empty() && !st.model.reasoning.text.ends_with("\n\n") {
                                st.model.reasoning.text.push_str("\n\n---\n\n");
                            }
                            let h = st.last_model_height;
                            st.model.clamp_scroll(h);
                        }
                    }
                    st.model.reasoning.append_delta(&data.delta);
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::MODEL_TEXT_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    if !data.delta.is_empty() {
                        if st.model.edits_active {
                            st.model.status = "editing".to_string();
                        } else {
                            st.model.status = "responding".to_string();
                        }
                        if *is_reasoning {
                            *is_reasoning = false;
                            st.model.reasoning.show = false;
                            let h = st.last_model_height;
                            st.model.clamp_scroll(h);
                        }
                    }
                    st.model.text.push_str(&data.delta);
                    st.model.update_markdown();
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::EDIT_STARTED => {
            st.model.edits_active = true;
            st.model.status = "editing".to_string();
        }
        events::EDIT_FILE_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileStartedEvent>(params) {
                    st.model.edits_active = true;
                    st.model.status = "editing".to_string();
                    if let Some(existing) = st.model.files.iter_mut().find(|f| f.path == data.path)
                    {
                        existing.status = "running".to_string();
                        existing.op_type = data.op_type;
                        existing.retry_info = None;
                    } else {
                        st.model.files.push(StreamingFileEdit {
                            path: data.path,
                            op_type: data.op_type,
                            status: "running".to_string(),
                            error: None,
                            hunks: Vec::new(),
                            expanded: false,
                            retry_info: None,
                        });
                        st.model.selected_file_index = st.model.files.len() - 1;
                    }
                }
            }
        }
        events::EDIT_HUNK => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditHunkEvent>(params) {
                    if let Some(f) = st.model.files.iter_mut().find(|f| f.path == data.path) {
                        f.retry_info = None;
                        f.hunks.push(StreamingHunk {
                            hunk_index: data.hunk_index,
                            old_text: data.old_text,
                            new_text: data.new_text,
                        });
                    }
                }
            }
        }
        events::EDIT_FILE_DONE => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileDoneEvent>(params) {
                    if let Some(f) = st.model.files.iter_mut().find(|f| f.path == data.path) {
                        f.status = data.status;
                        f.error = data.error;
                        if f.status == "ok" {
                            f.retry_info = None;
                            f.error = None;
                        }
                    }
                }
            }
        }
        events::EDIT_FILE_RETRYING => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileRetryingEvent>(params) {
                    st.model.status = "editing".to_string();
                    let retry_msg = format!("{}/{} retrying: {}", data.attempt, data.max_retries, data.reason);
                    if let Some(f) = st.model.files.iter_mut().find(|f| f.path == data.path) {
                        f.status = "retrying".to_string();
                        f.retry_info = Some(retry_msg);
                        f.error = None;
                    } else {
                        st.model.files.push(StreamingFileEdit {
                            path: data.path,
                            op_type: "replace".to_string(),
                            status: "retrying".to_string(),
                            error: None,
                            hunks: Vec::new(),
                            expanded: false,
                            retry_info: Some(retry_msg),
                        });
                        st.model.selected_file_index = st.model.files.len() - 1;
                    }
                }
            }
        }
        events::EDIT_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFinishedEvent>(params) {
                    st.model.edit_final_applied = Some(data.applied);
                    st.model.edit_final_error = data.error;
                    if let Some(hash) = data.commit_hash {
                        st.model.last_commit_hash = Some(hash);
                    }
                    if data.applied {
                        for f in st.model.files.iter_mut() {
                            f.status = "ok".to_string();
                            f.error = None;
                            f.retry_info = None;
                        }
                    } else {
                        let fallback_err = st.model.edit_final_error.clone().or_else(|| {
                            Some(messages::EDIT_FAILED_FALLBACK.to_string())
                        });
                        for f in st.model.files.iter_mut() {
                            if f.status == "running" || f.status == "retrying" {
                                f.status = "error".to_string();
                                f.retry_info = None;
                                if f.error.is_none() {
                                    f.error = fallback_err.clone();
                                }
                            }
                        }
                    }
                }
            }
        }
        events::MODEL_USAGE => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelUsageEvent>(params) {
                    st.model.session_total_cost = data.session_total_cost;
                    if let Some(curr) = data.current_cost {
                        st.model.current_cost = Some(curr);
                    }
                    st.model.usage = Some(data);
                }
            }
        }
        events::MODEL_RESULT => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelResultEvent>(params) {
                    match &data.result {
                        ModelResult::Edit {
                            applied: true,
                            commit_hash,
                            summary,
                            ..
                        } => {
                            st.model.edit_final_applied = Some(true);
                            if let Some(hash) = commit_hash {
                                st.model.last_commit_hash = Some(hash.clone());
                            }
                            st.model.last_commit_summary = Some(summary.clone());
                            for f in st.model.files.iter_mut() {
                                f.status = "ok".to_string();
                                f.error = None;
                                f.retry_info = None;
                            }
                        }
                        ModelResult::Edit {
                            applied: false,
                            error,
                            summary,
                            ..
                        } => {
                            st.model.edit_final_applied = Some(false);
                            st.model.edit_final_error = error.clone();
                            st.model.last_commit_summary = Some(summary.clone());
                            for f in st.model.files.iter_mut() {
                                if f.status == "running" || f.status == "retrying" {
                                    f.status = "error".to_string();
                                    f.retry_info = None;
                                    if f.error.is_none() {
                                        f.error = error.clone().or_else(|| {
                                            Some(messages::EDIT_FAILED_FALLBACK.to_string())
                                        });
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    st.model.result = Some(data.result);
                    if let Some(usage) = data.usage {
                        let total_cost = data
                            .session_total_cost
                            .unwrap_or(st.model.session_total_cost);
                        st.model.session_total_cost = total_cost;
                        if let Some(curr) = data.current_cost {
                            st.model.current_cost = Some(curr);
                        }
                        st.model.usage = Some(ModelUsageEvent {
                            operation_id: data.operation_id.clone(),
                            usage,
                            session_total_cost: total_cost,
                            current_cost: data.current_cost,
                        });
                    }
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::MODEL_FINISHED => {
            *is_reasoning = false;
            st.confirm_cancel = false;
            if let Some(started) = st.turn_started_at.take() {
                let elapsed = started.elapsed().as_secs();
                if elapsed >= st.tui_config.notifications.min_duration_seconds {
                    let summary = st.model.last_commit_summary.as_deref().unwrap_or("Turn completed");
                    let focused = st.terminal_focused;
                    crate::terminal::trigger_turn_notification(
                        "TAUQE",
                        summary,
                        &st.tui_config.notifications,
                        focused,
                    );
                }
            }
            if let Some(params) = ev.params {
                if let Ok(_data) = serde_json::from_value::<ModelFinishedEvent>(params) {
                    st.model.status = "done".to_string();
                    st.model.toolchain_command = None;
                    st.model.turn_phase = None;
                    st.model.turn_phase_detail = None;
                    if let Some(c) = st.model.current_cost {
                        st.model.prev_cost = Some(c);
                    }
                    st.model.current_cost = None;
                    if st.model.last_commit_hash.is_some()
                        || st.model.edit_final_applied == Some(true)
                    {
                        for f in st.model.files.iter_mut() {
                            f.status = "ok".to_string();
                            f.error = None;
                            f.retry_info = None;
                        }
                    } else {
                        fail_safe_reject_edits(
                            &mut st.model,
                            "Edit state desynchronized: server finished without sending edit/finished",
                        );
                    }
                    st.model.update_markdown();
                    let h = st.last_model_height;
                    st.model.clamp_scroll(h);
                }
            }
        }
        events::MODEL_CANCELLED => {
            *is_reasoning = false;
            st.confirm_cancel = false;
            st.turn_started_at = None;
            st.model.status = "cancelled".to_string();
            st.model.toolchain_command = None;
            st.model.turn_phase = None;
            st.model.turn_phase_detail = None;
            if let Some(c) = st.model.current_cost {
                if c > 0.0 {
                    st.model.prev_cost = Some(c);
                }
            }
            st.model.current_cost = None;
            fail_safe_reject_edits(
                &mut st.model,
                "Operation cancelled before edits were applied",
            );
            let h = st.last_model_height;
            st.model.clamp_scroll(h);
        }
        events::MODEL_ERROR => {
            *is_reasoning = false;
            st.confirm_cancel = false;
            if let Some(started) = st.turn_started_at.take() {
                let elapsed = started.elapsed().as_secs();
                if elapsed >= st.tui_config.notifications.min_duration_seconds {
                    let focused = st.terminal_focused;
                    crate::terminal::trigger_turn_notification(
                        "TAUQE Error",
                        "Turn failed with error",
                        &st.tui_config.notifications,
                        focused,
                    );
                }
            }
            st.model.toolchain_command = None;
            st.model.turn_phase = None;
            st.model.turn_phase_detail = None;
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelErrorEvent>(params) {
                    st.model.status = "error".to_string();
                    if let Some(c) = st.model.current_cost {
                        if c > 0.0 {
                            st.model.prev_cost = Some(c);
                        }
                    }
                    st.model.current_cost = None;
                    fail_safe_reject_edits(
                        &mut st.model,
                        "Operation failed before edits were applied",
                    );
                    st.model.error = Some(data.message);
                    let h = st.last_model_height;
                    st.model.clamp_scroll(h);
                }
            }
        }
        events::REVIEW_STARTED => {
            st.review.running = true;
            st.turn_started_at = Some(std::time::Instant::now());
        }
        events::REVIEW_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewReasoningDeltaEvent>(params) {
                    st.review.reasoning.append_delta(&data.delta);
                }
            }
        }
        events::REVIEW_CONTENT_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewContentDeltaEvent>(params) {
                    st.review.content.push_str(&data.delta);
                }
            }
        }
        events::REVIEW_FINISHED => {
            let started = st.turn_started_at.take();
            let mut findings_count = 0;
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewFinishedEvent>(params) {
                    if let Some(total) = data.session_total_cost {
                        st.model.session_total_cost = total;
                    }
                    if let Some(cost) = data.current_cost {
                        st.model.prev_cost = Some(cost);
                    }
                    findings_count = data.session.items.len();
                    st.review.set_session(data.session);
                }
            }
            if let Some(started) = started {
                let elapsed = started.elapsed().as_secs();
                if elapsed >= st.tui_config.notifications.min_duration_seconds {
                    let summary = format!("Code review completed ({} findings)", findings_count);
                    let focused = st.terminal_focused;
                    crate::terminal::trigger_turn_notification(
                        "TAUQE Review",
                        &summary,
                        &st.tui_config.notifications,
                        focused,
                    );
                }
            }
        }
        events::REVIEW_CANCELLED => {
            st.turn_started_at = None;
            st.review.running = false;
            st.review.error = Some("Review cancelled".to_string());
        }
        events::REVIEW_ERROR => {
            if let Some(started) = st.turn_started_at.take() {
                let elapsed = started.elapsed().as_secs();
                if elapsed >= st.tui_config.notifications.min_duration_seconds {
                    let focused = st.terminal_focused;
                    crate::terminal::trigger_turn_notification(
                        "TAUQE Review Error",
                        "Code review failed",
                        &st.tui_config.notifications,
                        focused,
                    );
                }
            }
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewErrorEvent>(params) {
                    st.review.running = false;
                    st.review.error = Some(data.message);
                }
            }
        }
        events::PLAN_UPDATED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::PlanUpdatedEvent>(params) {
                    let is_diff = st.plans_view.current_plan.as_ref().map(|p| &p.id) != Some(&data.plan.id);
                    if is_diff {
                        st.plans_view.reset_view_for_new_plan();
                    }
                    st.plans_view.active_plan_id = Some(data.plan.id.clone());
                    st.plans_view.current_plan = Some(data.plan);
                }
            }
        }
        events::PLAN_LIST_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::PlanListChangedEvent>(params) {
                    st.plans_view.plans_list = data.plans;
                    st.plans_view.active_plan_id = data.active_id.clone();
                    if let Some(active) = &data.active_id {
                        if let Some(pos) = st.plans_view.plans_list.iter().position(|p| &p.id == active) {
                            st.plans_view.selected_plan_index = pos;
                        }
                    } else if let Some(first) = st.plans_view.plans_list.first().cloned() {
                        st.plans_view.selected_plan_index = 0;
                        st.plans_view.active_plan_id = Some(first.id.clone());
                    } else {
                        st.plans_view.current_plan = None;
                        st.plans_view.selected_plan_index = 0;
                    }
                }
            }
        }
        _ => {}
    }
}
