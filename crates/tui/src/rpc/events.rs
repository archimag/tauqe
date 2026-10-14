use std::sync::Arc;
use tokio::sync::Mutex;
use tauqe_protocol::{
    events, BatchFinishStatus, BatchFinishSummary, ConfigState, ContextState, EditFileDoneEvent,
    EditFileRetryingEvent, EditFileStartedEvent, EditFinishedEvent, EditHunkEvent, Event,
    GitCommitCreatedEvent, GitSquashApplyResult, GitUndoResult, HistoryEntryAddedEvent,
    ModelDeltaEvent, ModelErrorEvent, ModelFinishedEvent, ModelResult, ModelResultEvent,
    ModelStartedEvent, ModelUsageEvent, PlanBatchFinishedEvent, RepositoryState,
    ReviewBatchFinishedEvent, ReviewContentDeltaEvent, ReviewErrorEvent, ReviewFinishedEvent,
    ReviewReasoningDeltaEvent, ReviewStartedEvent,
};

use crate::app::{AppState, RunningModel};
use crate::views::develop::{
    DevelopView, EditOp, FileEditStatus, StreamingFileEdit, StreamingHunk, ToolchainState,
    TurnStatus,
};
use super::responses::messages;

pub fn fail_safe_reject_edits(model: &mut DevelopView, reason: &str) {
    if !model.edits_active || model.edit_final_applied.is_some() {
        return;
    }
    for f in model.files.iter_mut() {
        if matches!(f.status, FileEditStatus::Running | FileEditStatus::Retrying) {
            f.status = FileEditStatus::Error;
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
                        f.status = FileEditStatus::Ok;
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
                        let total_lines = crate::views::history::compute_history_items_line_count(&st.history_view.items, None) as u16;
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
        events::MODEL_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::ModelChangedEvent>(params) {
                    st.active_model = data.effective_model;
                    st.model_choice = crate::app::ModelChoice {
                        selection: data.selection,
                        roles: data.roles,
                    };
                }
            }
        }
        events::DISCUSSION_FOCUS_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(data) =
                    serde_json::from_value::<tauqe_protocol::DiscussionFocusChangedEvent>(params)
                {
                    st.discussion_focus = data.focus;
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
                    st.running_model = Some(RunningModel {
                        model: data.model.clone(),
                        role: data.role,
                        alias: data.alias.clone(),
                        effort: data.effort,
                    });
                    st.model.operation_id = Some(data.operation_id);
                    st.model.model = Some(data.model);
                    st.model.reasoning.clear();
                    st.model.text.clear();
                    st.model.markdown_lines.clear();
                    st.model.error = None;
                    st.model.result = None;
                    st.model.code_blocks.clear();
                    st.model.scroll = 0;
                    st.model.status = TurnStatus::Awaiting;
                    st.model.edits_active = false;
                    st.model.files.clear();
                    st.model.selected_file_index = 0;
                    st.model.edit_final_applied = None;
                    st.model.edit_final_error = None;
                    st.model.last_commit_hash = None;
                    st.model.last_commit_summary = None;
                    st.model.toolchain = None;
                    st.model.turn_phase = None;
                    st.model.turn_round = None;
                    st.model.turn_max_rounds = None;
                    st.model.turn_phase_detail = None;
                    st.model.plan_context = data.plan_context;
                    st.model.is_batch = data.is_batch;
                    st.model.repomap_tokens = None;
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
                        tauqe_protocol::TurnPhase::Discovery => TurnStatus::Discovery,
                        tauqe_protocol::TurnPhase::Proposal => {
                            if *is_reasoning {
                                TurnStatus::Thinking
                            } else {
                                TurnStatus::Responding
                            }
                        }
                        tauqe_protocol::TurnPhase::Staging => TurnStatus::Staging,
                        tauqe_protocol::TurnPhase::Verification => TurnStatus::Verifying,
                        tauqe_protocol::TurnPhase::Healing => TurnStatus::Healing,
                    };
                }
            }
        }
        events::TURN_REPOMAP => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::RepoMapInfoEvent>(params) {
                    st.model.repomap_tokens = Some(data.tokens);
                }
            }
        }
        events::TOOLCHAIN_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::ToolchainStartedEvent>(params) {
                    st.model.toolchain = Some(ToolchainState::Running {
                        command: data.command,
                    });
                    st.model.status = TurnStatus::Verifying;
                }
            }
        }
        events::TOOLCHAIN_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::ToolchainFinishedEvent>(params) {
                    st.model.toolchain = Some(if data.success {
                        ToolchainState::Passed {
                            command: data.command,
                        }
                    } else {
                        ToolchainState::Failed {
                            command: data.command,
                            message: data.message,
                        }
                    });
                }
            }
        }
        events::MODEL_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    if !data.delta.is_empty() {
                        st.model.status = TurnStatus::Thinking;
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
                            st.model.status = TurnStatus::Editing;
                        } else {
                            st.model.status = TurnStatus::Responding;
                        }
                        if *is_reasoning {
                            *is_reasoning = false;
                            st.model.reasoning.finalize();
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
            st.model.status = TurnStatus::Editing;
        }
        events::EDIT_FILE_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileStartedEvent>(params) {
                    st.model.edits_active = true;
                    st.model.status = TurnStatus::Editing;
                    if let Some(existing) = st.model.files.iter_mut().find(|f| f.path == data.path)
                    {
                        existing.status = FileEditStatus::Running;
                        existing.op_type = EditOp::from_wire(&data.op_type);
                        existing.retry_info = None;
                    } else {
                        st.model.files.push(StreamingFileEdit {
                            path: data.path,
                            op_type: EditOp::from_wire(&data.op_type),
                            status: FileEditStatus::Running,
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
                        f.status = FileEditStatus::from_wire(&data.status);
                        f.error = data.error;
                        if f.status == FileEditStatus::Ok {
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
                    st.model.status = TurnStatus::Editing;
                    let retry_msg = format!("{}/{} retrying: {}", data.attempt, data.max_retries, data.reason);
                    if let Some(f) = st.model.files.iter_mut().find(|f| f.path == data.path) {
                        f.status = FileEditStatus::Retrying;
                        f.retry_info = Some(retry_msg);
                        f.error = None;
                    } else {
                        st.model.files.push(StreamingFileEdit {
                            path: data.path,
                            op_type: EditOp::Replace,
                            status: FileEditStatus::Retrying,
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
                            f.status = FileEditStatus::Ok;
                            f.error = None;
                            f.retry_info = None;
                        }
                    } else {
                        let fallback_err = st.model.edit_final_error.clone().or_else(|| {
                            Some(messages::EDIT_FAILED_FALLBACK.to_string())
                        });
                        for f in st.model.files.iter_mut() {
                            if matches!(f.status, FileEditStatus::Running | FileEditStatus::Retrying)
                            {
                                f.status = FileEditStatus::Error;
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
                                f.status = FileEditStatus::Ok;
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
                                if matches!(f.status, FileEditStatus::Running | FileEditStatus::Retrying)
                                {
                                    f.status = FileEditStatus::Error;
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
            st.model.reasoning.finalize();
            st.confirm_cancel = false;
            st.running_model = None;
            if let Some(started) = st.turn_started_at.take() {
                let elapsed = started.elapsed().as_secs();
                let focused = st.terminal_focused;
                // Batch members stay silent: only the supervisor's final event
                // raises a notification for the whole operation.
                if !st.model.is_batch
                    && st.tui_config.notifications.should_notify(elapsed, focused)
                {
                    let summary = st.model.last_commit_summary.as_deref().unwrap_or("Turn completed");
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
                    st.model.status = TurnStatus::Done;
                    st.model.toolchain = None;
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
                            f.status = FileEditStatus::Ok;
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

                    if st.model.edit_final_applied == Some(false) {
                        if let Some(err) = st.model.edit_final_error.clone() {
                            st.notify_error(err);
                        }
                    }
                }
            }
        }
        events::MODEL_CANCELLED => {
            *is_reasoning = false;
            st.model.reasoning.finalize();
            st.confirm_cancel = false;
            st.running_model = None;
            st.turn_started_at = None;
            st.model.status = TurnStatus::Cancelled;
            st.model.toolchain = None;
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
            st.model.reasoning.finalize();
            st.confirm_cancel = false;
            st.running_model = None;
            if let Some(started) = st.turn_started_at.take() {
                let elapsed = started.elapsed().as_secs();
                let focused = st.terminal_focused;
                // Batch member failures are aggregated into the batch summary.
                if !st.model.is_batch
                    && st.tui_config.notifications.should_notify(elapsed, focused)
                {
                    crate::terminal::trigger_turn_notification(
                        "TAUQE Error",
                        "Turn failed with error",
                        &st.tui_config.notifications,
                        focused,
                    );
                }
            }
            st.model.toolchain = None;
            st.model.turn_phase = None;
            st.model.turn_phase_detail = None;
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelErrorEvent>(params) {
                    st.model.status = TurnStatus::Error;
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
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewStartedEvent>(params) {
                    begin_review_turn(&mut st.model, &data);
                }
            }
        }
        events::REVIEW_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewReasoningDeltaEvent>(params) {
                    apply_review_reasoning(&mut st.model, &data.delta);
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::REVIEW_CONTENT_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewContentDeltaEvent>(params) {
                    apply_review_content(&mut st.model, &data.delta);
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::REVIEW_FINISHED => {
            st.review.running = false;
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
            finish_review_turn(&mut st.model);
            if let Some(started) = started {
                let elapsed = started.elapsed().as_secs();
                let focused = st.terminal_focused;
                if st.tui_config.notifications.should_notify(elapsed, focused) {
                    let summary = format!("Code review completed ({} findings)", findings_count);
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
            st.review.running = false;
            st.turn_started_at = None;
            st.review.error = Some("Review cancelled".to_string());
            end_review_turn(&mut st.model, TurnStatus::Cancelled, None);
        }
        events::REVIEW_ERROR => {
            st.review.running = false;
            if let Some(started) = st.turn_started_at.take() {
                let elapsed = started.elapsed().as_secs();
                let focused = st.terminal_focused;
                if st.tui_config.notifications.should_notify(elapsed, focused) {
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
                    st.review.error = Some(data.message.clone());
                    end_review_turn(&mut st.model, TurnStatus::Error, Some(data.message));
                }
            }
        }
        events::PLAN_UPDATED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::PlanUpdatedEvent>(params) {
                    if let Some(pos) = st.plans_view.plans.iter().position(|p| p.id == data.plan.id) {
                        st.plans_view.plans[pos] = data.plan;
                    } else {
                        st.plans_view.plans.push(data.plan);
                    }
                }
            }
        }
        events::PLAN_LIST_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::PlanListChangedEvent>(params) {
                    st.plans_view.plans = data.plans;
                    st.plans_view.clamp_selection();
                }
            }
        }
        events::REVIEW_UPDATED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::ReviewUpdatedEvent>(params) {
                    st.review.update_session(data.session);
                }
            }
        }
        events::REVIEW_LIST_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::ReviewListChangedEvent>(params) {
                    st.review.set_sessions(data.reviews, data.active_id);
                }
            }
        }
        events::PLAN_BATCH_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<PlanBatchFinishedEvent>(params) {
                    notify_batch_finished(&st, "Plan", &data.summary);
                }
            }
        }
        events::REVIEW_BATCH_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewBatchFinishedEvent>(params) {
                    notify_batch_finished(&st, "Review", &data.summary);
                }
            }
        }
        _ => {}
    }
}

/// Resets the REPL buffer so it mirrors a freshly started Reviewer stream.
///
/// The review summary is streamed into the main develop canvas, so the buffer
/// must shed any state left by a prior turn before the reviewer's reasoning and
/// summary begin to arrive.
fn begin_review_turn(model: &mut DevelopView, data: &ReviewStartedEvent) {
    model.operation_id = Some(data.operation_id.clone());
    model.model = Some(data.model.clone());
    model.reasoning.clear();
    model.text.clear();
    model.markdown_lines.clear();
    model.code_blocks.clear();
    model.error = None;
    model.result = None;
    model.scroll = 0;
    model.status = TurnStatus::Responding;
    model.edits_active = false;
    model.files.clear();
    model.selected_file_index = 0;
    model.edit_final_applied = None;
    model.edit_final_error = None;
    model.last_commit_hash = None;
    model.last_commit_summary = None;
    model.toolchain = None;
    model.turn_phase = None;
    model.turn_round = None;
    model.turn_max_rounds = None;
    model.turn_phase_detail = None;
    model.plan_context = None;
    model.is_batch = false;
    model.repomap_tokens = None;
    model.auto_scroll = true;
    model.current_cost = Some(0.0);
}

/// Streams a Reviewer reasoning delta into the REPL reasoning buffer.
fn apply_review_reasoning(model: &mut DevelopView, delta: &str) {
    if !delta.is_empty() {
        model.status = TurnStatus::Thinking;
    }
    model.reasoning.append_delta(delta);
}

/// Streams a Reviewer summary delta into the REPL answer buffer.
///
/// The first non-empty fragment settles the reviewer's reasoning stream exactly
/// once, mirroring `MODEL_TEXT_DELTA`: the live tail window is finalized and the
/// block folds into "▶ Thinking hidden", so the reasoning lifecycle stays
/// identical across ordinary, discussion, and review turns.
fn apply_review_content(model: &mut DevelopView, delta: &str) {
    if delta.is_empty() {
        return;
    }
    if model.reasoning.is_streaming() {
        model.reasoning.finalize();
        model.reasoning.show = false;
    }
    model.status = TurnStatus::Responding;
    model.text.push_str(delta);
    model.update_markdown();
}

/// Finalizes a completed Reviewer stream into the REPL buffer.
fn finish_review_turn(model: &mut DevelopView) {
    end_review_turn(model, TurnStatus::Done, None);
    model.update_markdown();
}

/// Settles the REPL buffer into a terminal status and finalizes reasoning.
fn end_review_turn(model: &mut DevelopView, status: TurnStatus, error: Option<String>) {
    model.reasoning.finalize();
    model.status = status;
    if let Some(message) = error {
        model.error = Some(message);
    }
}

/// Raises exactly one aggregate notification when a batch supervisor finishes.
///
/// Batch members run as a single unit of waiting: their individual turns stay
/// silent, and only this terminal event produces a sound or desktop alert.
fn notify_batch_finished(st: &AppState, kind: &str, summary: &BatchFinishSummary) {
    let focused = st.terminal_focused;
    if !st
        .tui_config
        .notifications
        .should_notify(summary.elapsed_seconds, focused)
    {
        return;
    }
    let (title, body) = format_batch_summary(kind, summary);
    crate::terminal::trigger_turn_notification(
        &title,
        &body,
        &st.tui_config.notifications,
        focused,
    );
}

/// Builds the title and body for a batch completion notification from its
/// aggregate outcome, progress counters, and elapsed time.
fn format_batch_summary(kind: &str, summary: &BatchFinishSummary) -> (String, String) {
    let status_word = match summary.status {
        BatchFinishStatus::Completed => "completed",
        BatchFinishStatus::Failed => "failed",
        BatchFinishStatus::Blocked => "blocked",
        BatchFinishStatus::Cancelled => "cancelled",
    };
    let title = format!("TAUQE {} batch {}", kind, status_word);
    let mut body = format!(
        "{} '{}': {}/{} steps in {}",
        kind,
        summary.owner_id,
        summary.executed_count,
        summary.total_count,
        format_elapsed(summary.elapsed_seconds),
    );
    if let Some(err) = summary.error.as_deref() {
        let first_line = err.lines().next().unwrap_or("").trim();
        if !first_line.is_empty() {
            body.push_str(" - ");
            body.push_str(first_line);
        }
    }
    (title, body)
}

/// Formats an elapsed duration as `Ns` under a minute, otherwise `Nm Ss`.
fn format_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        format!("{}s", seconds)
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch_summary(
        status: BatchFinishStatus,
        executed: usize,
        total: usize,
        elapsed: u64,
    ) -> BatchFinishSummary {
        BatchFinishSummary {
            owner_id: "plan-1".to_string(),
            status,
            executed_count: executed,
            total_count: total,
            elapsed_seconds: elapsed,
            error: None,
        }
    }

    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(60), "1m 0s");
        assert_eq!(format_elapsed(125), "2m 5s");
    }

    #[test]
    fn test_format_batch_summary_completed() {
        let (title, body) = format_batch_summary(
            "Plan",
            &batch_summary(BatchFinishStatus::Completed, 3, 3, 42),
        );
        assert_eq!(title, "TAUQE Plan batch completed");
        assert!(body.contains("3/3"));
        assert!(body.contains("42s"));
    }

    #[test]
    fn test_format_batch_summary_failed_keeps_first_error_line() {
        let mut summary = batch_summary(BatchFinishStatus::Failed, 1, 3, 90);
        summary.error = Some("compiler error\nlong details".to_string());
        let (title, body) = format_batch_summary("Plan", &summary);
        assert_eq!(title, "TAUQE Plan batch failed");
        assert!(body.contains("1/3"));
        assert!(body.contains("1m 30s"));
        assert!(body.contains("compiler error"));
        assert!(!body.contains("long details"));
    }

    fn review_started_event() -> ReviewStartedEvent {
        ReviewStartedEvent {
            operation_id: "rev-op-1".to_string(),
            model: "reviewer".to_string(),
            files_count: 2,
            estimated_tokens: 128,
        }
    }

    #[test]
    fn test_review_started_resets_repl_buffer() {
        let mut model = DevelopView::default();
        apply_review_content(&mut model, "# Stale summary");
        model.error = Some("old error".to_string());
        model.status = TurnStatus::Editing;
        model.edits_active = true;
        model.auto_scroll = false;
        model.files.push(StreamingFileEdit {
            path: "crates/tui/src/lib.rs".to_string(),
            op_type: EditOp::Replace,
            status: FileEditStatus::Running,
            error: None,
            hunks: Vec::new(),
            expanded: false,
            retry_info: None,
        });

        begin_review_turn(&mut model, &review_started_event());

        assert_eq!(model.operation_id.as_deref(), Some("rev-op-1"));
        assert_eq!(model.model.as_deref(), Some("reviewer"));
        assert_eq!(model.status, TurnStatus::Responding);
        assert!(model.text.is_empty());
        assert!(model.markdown_lines.is_empty());
        assert!(model.error.is_none());
        assert!(model.files.is_empty());
        assert!(!model.edits_active);
        assert!(model.auto_scroll);
    }

    #[test]
    fn test_review_reasoning_delta_streams_into_repl() {
        let mut model = DevelopView {
            status: TurnStatus::Responding,
            ..Default::default()
        };

        apply_review_reasoning(&mut model, "weighing the options\n");

        assert_eq!(model.status, TurnStatus::Thinking);
        assert!(model.reasoning.text.contains("weighing the options"));
    }

    #[test]
    fn test_review_content_delta_renders_markdown_in_repl() {
        let mut model = DevelopView::default();

        apply_review_content(&mut model, "# Executive Summary\n\nNo critical issues.");

        assert_eq!(model.status, TurnStatus::Responding);
        assert!(model.text.contains("Executive Summary"));
        assert!(!model.markdown_lines.is_empty());
    }

    #[test]
    fn test_review_content_delta_finalizes_and_folds_reasoning() {
        let mut model = DevelopView::default();
        apply_review_reasoning(&mut model, "weighing the options");
        assert!(model.reasoning.is_streaming());
        assert!(model.reasoning.show);

        apply_review_content(&mut model, "Executive Summary");

        assert!(
            !model.reasoning.is_streaming(),
            "the reasoning stream must finalize when the answer begins"
        );
        assert!(
            !model.reasoning.show,
            "the reasoning block must fold into 'Thinking hidden'"
        );
    }

    #[test]
    fn test_review_finished_rerenders_markdown_and_marks_done() {
        let mut model = DevelopView::default();
        apply_review_reasoning(&mut model, "reasoning");
        apply_review_content(&mut model, "# Summary");
        model.markdown_lines.clear();

        finish_review_turn(&mut model);

        assert_eq!(model.status, TurnStatus::Done);
        assert!(!model.markdown_lines.is_empty());
    }

    #[test]
    fn test_review_terminal_statuses_map_to_repl() {
        let mut cancelled = DevelopView::default();
        end_review_turn(&mut cancelled, TurnStatus::Cancelled, None);
        assert_eq!(cancelled.status, TurnStatus::Cancelled);

        let mut failed = DevelopView::default();
        end_review_turn(&mut failed, TurnStatus::Error, Some("boom".to_string()));
        assert_eq!(failed.status, TurnStatus::Error);
        assert_eq!(failed.error.as_deref(), Some("boom"));
    }
}
