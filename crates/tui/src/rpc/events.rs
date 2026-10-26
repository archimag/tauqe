use std::sync::Arc;
use tauqe_protocol::{
    events, BatchFinishStatus, BatchFinishSummary, ConfigState, ContextState, EditFileDoneEvent,
    EditFileRetryingEvent, EditFileStartedEvent, EditFinishedEvent, EditHunkEvent, Event,
    GitCommitCreatedEvent, GitSquashApplyResult, HistoryCompactedEvent, HistoryEntryAddedEvent,
    HistoryEntryRolledBackEvent, ModelDeltaEvent, ModelErrorEvent, ModelEscalatedEvent,
    ModelResult, ModelResultEvent, ModelStartedEvent, ModelUsageTotalsEvent,
    PlanBatchFinishedEvent, RepositoryState, ReviewBatchFinishedEvent, ReviewContentDeltaEvent,
    ReviewErrorEvent, ReviewFinishedEvent, ReviewReasoningDeltaEvent, ReviewStartedEvent,
};
use tokio::sync::Mutex;

use super::responses::messages;
use crate::app::{AppState, RunningModel, ViewMode};
use crate::i18n::active_t;
use crate::views::repl::{
    EditOp, FileEditStatus, ReplView, RetryingState, StreamingFileEdit, StreamingHunk,
    ToolchainState, TurnStatus,
};

pub fn fail_safe_reject_edits(model: &mut ReplView, reason: &str) {
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

/// Re-anchors the Develop viewport to the bottom of the streamed turn.
///
/// Streamed file edits and their hunks arrive below the answer block, so a
/// turn that has already filled the viewport would leave the freshly applied
/// edits hidden beneath its lower edge. Every editing event re-anchors the
/// viewport while auto-scroll is enabled, keeping the patch-retry and
/// verification-healing rounds visible (c-autoscroll-edits).
fn scroll_to_stream_end(st: &mut AppState) {
    if st.model.auto_scroll {
        let height = st.last_model_height;
        st.model.scroll = st.model.max_scroll(height);
    }
}

/// Returns every accepted file of the current turn to the retry state.
///
/// Verification is deterministic and authoritative: once the compiler rejects
/// the applied change set, a file marked `Ok` is no longer settled. The healing
/// round regenerates its patches, so the view names the pending regeneration
/// instead of keeping a green mark on code that does not build
/// (c-healing-phase-visual). Files that already carry their own attempt detail
/// keep it, because the patch retry owns that message.
fn mark_files_awaiting_healing(model: &mut ReplView) {
    if model.files.is_empty() {
        return;
    }
    let reason = active_t("repl.edits_retry_healing");
    for file in model.files.iter_mut() {
        if file.status != FileEditStatus::Ok {
            continue;
        }
        file.status = FileEditStatus::Retrying;
        file.retry_info = Some(reason.clone());
        file.error = None;
    }
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
        events::GIT_SQUASH_COMPLETED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<GitSquashApplyResult>(params) {
                    st.squash_dialog = None;
                    let first_line = data
                        .message
                        .lines()
                        .next()
                        .map(str::to_string)
                        .unwrap_or_else(|| active_t("notify.squashed_commit_fallback"));
                    st.notify_success(
                        active_t("notify.squashed_commits")
                            .replacen("{}", &data.squashed_commit, 1)
                            .replacen("{}", &first_line, 1),
                    );
                    st.model.last_commit_hash = Some(data.squashed_commit);
                    st.model.last_commit_summary = Some(first_line);
                }
            }
        }
        events::HISTORY_ENTRY_ADDED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<HistoryEntryAddedEvent>(params) {
                    let item_tokens = data
                        .estimated_tokens
                        .unwrap_or_else(|| data.item.text.chars().count().div_ceil(4) as u64);
                    if data.estimated_tokens.is_some() {
                        st.history_view.estimated_tokens = item_tokens;
                    } else {
                        st.history_view.estimated_tokens += item_tokens;
                    }
                    st.history_view.items.push(data.item);
                    st.history_view.total_count += 1;
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
        }
        events::HISTORY_COMPACTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<HistoryCompactedEvent>(params) {
                    // Compaction is a structural replacement of the summarized
                    // head with a single leading summary, never an append.
                    let view_height = st.last_model_height;
                    st.history_view.apply_compaction(
                        data.summary,
                        &data.tail_ids,
                        data.total_count,
                        data.estimated_tokens,
                        view_height,
                    );
                }
            }
        }
        events::HISTORY_ENTRY_ROLLED_BACK => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<HistoryEntryRolledBackEvent>(params) {
                    // A rollback is neither an append nor a deletion: the turn
                    // failed or was cancelled before its result reached the
                    // model context, so the record stays visible in the
                    // developer's chronicle and its typed status is replaced
                    // in place. Nothing is inferred from surrounding events.
                    let item = data.item;
                    if let Some(existing) =
                        st.history_view.items.iter_mut().find(|it| it.id == item.id)
                    {
                        *existing = item;
                    } else {
                        st.history_view.items.push(item);
                        st.history_view.total_count += 1;
                    }
                    // The turn failed or was cancelled before its prompt reached
                    // the model history, so the editor history is now the only
                    // place the text lives: warn the developer and name the
                    // chords that recall it (c-3).
                    st.notify_prompt_rolled_back();
                }
            }
        }
        events::CONFIG_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(cfg) = serde_json::from_value::<ConfigState>(params) {
                    st.workflow = cfg.workflow;
                    st.active_model = cfg.model;
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
                if let Ok(data) =
                    serde_json::from_value::<tauqe_protocol::ModelChangedEvent>(params)
                {
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
            st.turn_started_at
                .get_or_insert_with(std::time::Instant::now);
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelStartedEvent>(params) {
                    // The display alias is never carried through the turn
                    // state: it is resolved from the provider catalog published
                    // once per frame, so two carriers of one value cannot drift
                    // apart after a configuration change (c-1).
                    st.running_model = Some(RunningModel {
                        model: data.model.clone(),
                        role: data.role,
                        effort: data.effort,
                    });
                    st.model.operation_id = Some(data.operation_id);
                    st.model.model = Some(data.model);
                    // One shared reset releases every per-turn buffer,
                    // including the rendered diff cache, so a batch of
                    // steps can never accumulate the payload of the
                    // previous member.
                    st.model.reset_turn_buffers();
                    st.model.scroll = 0;
                    st.model.status = TurnStatus::Awaiting;
                    st.model.plan_context = data.plan_context;
                    st.model.is_batch = data.is_batch;
                    st.model.auto_scroll = true;
                }
            }
        }
        events::MODEL_ESCALATED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelEscalatedEvent>(params) {
                    // Escalation promotes the turn inside the same role, so
                    // only the tier coordinates change: the header swaps the
                    // running model in place instead of waiting for the turn
                    // to settle. The event's own alias is ignored; the label is
                    // resolved from the published provider catalog (c-1).
                    st.running_model = Some(RunningModel {
                        model: data.model,
                        role: data.role,
                        effort: data.effort,
                    });
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
                    // Entering the healing phase means the compiler rejected
                    // the applied change set, so the accepted files are no
                    // longer settled while their replacements are regenerated
                    // (c-healing-phase-visual).
                    if data.phase == tauqe_protocol::TurnPhase::Healing {
                        mark_files_awaiting_healing(&mut st.model);
                    }
                }
            }
        }
        events::TURN_REPOMAP => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::RepoMapInfoEvent>(params)
                {
                    st.model.repomap_tokens = Some(data.tokens);
                }
            }
        }
        events::TOOLCHAIN_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) =
                    serde_json::from_value::<tauqe_protocol::ToolchainStartedEvent>(params)
                {
                    st.model.toolchain = Some(ToolchainState::Running {
                        command: data.command,
                    });
                    st.model.status = TurnStatus::Verifying;
                }
            }
        }
        events::TOOLCHAIN_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) =
                    serde_json::from_value::<tauqe_protocol::ToolchainFinishedEvent>(params)
                {
                    let failed = !data.success;
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
                    // The compiler is authoritative: an accepted patch that no
                    // longer passes verification is not settled, so the reason
                    // is projected onto the files before the healing round
                    // streams its replacements (c-healing-phase-visual).
                    if failed {
                        mark_files_awaiting_healing(&mut st.model);
                    }
                }
            }
        }
        events::MODEL_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    if !data.delta.is_empty() {
                        st.model.status = TurnStatus::Thinking;
                        // Real output means the pending retry already succeeded.
                        st.model.retrying = None;
                        if !*is_reasoning {
                            *is_reasoning = true;
                            st.model.reasoning.show = true;
                            if !st.model.reasoning.is_empty()
                                && !st.model.reasoning.text.ends_with("\n\n")
                            {
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
                        // Real output means the pending retry already succeeded.
                        st.model.retrying = None;
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
        events::MODEL_CONNECTION_RETRYING => {
            if let Some(params) = ev.params {
                if let Ok(data) =
                    serde_json::from_value::<tauqe_protocol::ModelConnectionRetryingEvent>(params)
                {
                    // The daemon republishes the remaining seconds once per
                    // second, so the view mirrors its countdown instead of
                    // ticking a timer of its own (c-4).
                    st.model.retrying = Some(RetryingState {
                        attempt: data.attempt,
                        max_attempts: data.max_attempts,
                        delay_secs: data.delay_secs,
                    });
                }
            }
        }
        events::MODEL_STREAM_RESET => {
            if let Some(params) = ev.params {
                if serde_json::from_value::<tauqe_protocol::ModelStreamResetEvent>(params).is_ok() {
                    // The connection broke after partial delivery: every
                    // fragment streamed so far belongs to the aborted attempt,
                    // so the view drops them before the retry replays the
                    // answer from its very beginning instead of stitching two
                    // attempts together (c-3).
                    *is_reasoning = false;
                    st.model.text.clear();
                    st.model.markdown_lines.clear();
                    st.model.code_blocks.clear();
                    st.model.reasoning.clear();
                }
            }
        }
        events::EDIT_STARTED => {
            st.model.edits_active = true;
            st.model.status = TurnStatus::Editing;
            scroll_to_stream_end(&mut st);
        }
        events::EDIT_FILE_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileStartedEvent>(params) {
                    st.model.edits_active = true;
                    st.model.status = TurnStatus::Editing;
                    // A repeated start of one file opens a fresh attempt: the
                    // hunks of the previous one were rejected by the patch
                    // retry or invalidated by the compiler during healing, so
                    // their rendered diff must be dropped before the new
                    // stream begins and must never accumulate across attempts
                    // (c-stream-file-reset).
                    st.model.diff_cache.invalidate(&data.path);
                    if let Some(existing) = st.model.files.iter_mut().find(|f| f.path == data.path)
                    {
                        existing.status = FileEditStatus::Running;
                        existing.op_type = data.op_type;
                        existing.error = None;
                        existing.retry_info = None;
                        existing.hunks.clear();
                    } else {
                        st.model.files.push(StreamingFileEdit {
                            path: data.path,
                            op_type: data.op_type,
                            status: FileEditStatus::Running,
                            error: None,
                            hunks: Vec::new(),
                            expanded: false,
                            retry_info: None,
                        });
                        st.model.selected_file_index = st.model.files.len() - 1;
                    }
                    scroll_to_stream_end(&mut st);
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
                    scroll_to_stream_end(&mut st);
                }
            }
        }
        events::EDIT_FILE_DONE => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileDoneEvent>(params) {
                    if let Some(f) = st.model.files.iter_mut().find(|f| f.path == data.path) {
                        f.status = data.status.into();
                        f.error = data.error;
                        if f.status == FileEditStatus::Ok {
                            f.retry_info = None;
                            f.error = None;
                        }
                    }
                    scroll_to_stream_end(&mut st);
                }
            }
        }
        events::EDIT_FILE_RETRYING => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileRetryingEvent>(params) {
                    st.model.status = TurnStatus::Editing;
                    let retry_msg = format!(
                        "{}/{} retrying: {}",
                        data.attempt, data.max_retries, data.reason
                    );
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
                    scroll_to_stream_end(&mut st);
                }
            }
        }
        events::EDIT_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFinishedEvent>(params) {
                    st.model.edit_final_applied = Some(data.outcome.applied);
                    st.model.edit_final_error = data.outcome.error;
                    if let Some(hash) = data.outcome.commit_hash {
                        st.model.last_commit_hash = Some(hash);
                    }
                    if data.outcome.applied {
                        for f in st.model.files.iter_mut() {
                            f.status = FileEditStatus::Ok;
                            f.error = None;
                            f.retry_info = None;
                        }
                    } else {
                        let fallback_err = st
                            .model
                            .edit_final_error
                            .clone()
                            .or_else(|| Some(messages::EDIT_FAILED_FALLBACK.to_string()));
                        for f in st.model.files.iter_mut() {
                            if matches!(
                                f.status,
                                FileEditStatus::Running | FileEditStatus::Retrying
                            ) {
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
        events::MODEL_USAGE_TOTALS => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelUsageTotalsEvent>(params) {
                    // The daemon owns accumulation: the reported snapshot
                    // replaces the mirrored one, so the interface can never
                    // double count an operation (c-1, c-2).
                    st.model.usage_totals = data.snapshot;
                }
            }
        }
        events::MODEL_RESULT => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelResultEvent>(params) {
                    match &data.result {
                        ModelResult::Edit {
                            outcome, summary, ..
                        } if outcome.applied => {
                            st.model.edit_final_applied = Some(true);
                            if let Some(hash) = &outcome.commit_hash {
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
                            outcome, summary, ..
                        } if !outcome.applied => {
                            st.model.edit_final_applied = Some(false);
                            st.model.edit_final_error = outcome.error.clone();
                            st.model.last_commit_summary = Some(summary.clone());
                            for f in st.model.files.iter_mut() {
                                if matches!(
                                    f.status,
                                    FileEditStatus::Running | FileEditStatus::Retrying
                                ) {
                                    f.status = FileEditStatus::Error;
                                    f.retry_info = None;
                                    if f.error.is_none() {
                                        f.error = outcome.error.clone().or_else(|| {
                                            Some(messages::EDIT_FAILED_FALLBACK.to_string())
                                        });
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    st.model.result = Some(data.result);
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::MODEL_FINISHED => {
            // A batch member never settles the shared turn on its own: the
            // supervisor's `plan/batch_finished` / `review/batch_finished`
            // event is the single terminal signal for the whole queue, so the
            // spinner, the status, and the notification stay untouched until
            // the queue actually ends.
            if st.model.is_batch {
                return;
            }
            *is_reasoning = false;
            st.model.reasoning.finalize();
            st.model.reasoning.show = false;
            st.confirm_cancel = false;
            st.running_model = None;
            if let Some(started) = st.turn_started_at.take() {
                let elapsed = started.elapsed().as_secs();
                let focused = st.terminal_focused;
                if st.tui_config.notifications.should_notify(elapsed, focused) {
                    let title = active_t("notify.turn_desktop_title");
                    let summary = st
                        .model
                        .last_commit_summary
                        .clone()
                        .unwrap_or_else(|| active_t("notify.turn_completed"));
                    crate::terminal::trigger_turn_notification(
                        &title,
                        &summary,
                        &st.tui_config.notifications,
                        focused,
                    );
                }
            }
            // The event method is the authoritative lifecycle signal, so the
            // turn settles onto a terminal status even when the payload is
            // absent or malformed. Gating this block on payload parsing would
            // leave the spinner running, `is_busy()` blocking the prompt, and
            // the cancel dialog targeting a turn that has already ended (c-2).
            st.model.status = TurnStatus::Done;
            st.model.toolchain = None;
            st.model.retrying = None;
            st.model.turn_phase = None;
            st.model.turn_phase_detail = None;
            st.model.end_in_flight_usage();
            if st.model.last_commit_hash.is_some() || st.model.edit_final_applied == Some(true) {
                for f in st.model.files.iter_mut() {
                    f.status = FileEditStatus::Ok;
                    f.error = None;
                    f.retry_info = None;
                }
            } else {
                fail_safe_reject_edits(&mut st.model, &active_t("notify.edit_desync"));
            }
            st.model.update_markdown();
            let h = st.last_model_height;
            st.model.clamp_scroll(h);

            if st.model.edit_final_applied == Some(false) {
                if let Some(err) = st.model.edit_final_error.clone() {
                    st.notify_error(err);
                }
            } else {
                let msg = format_turn_completion_message(
                    st.model.last_commit_hash.as_deref(),
                    st.model.last_commit_summary.as_deref(),
                );
                st.notify_success(msg);
            }
        }
        events::MODEL_CANCELLED => {
            *is_reasoning = false;
            st.model.reasoning.finalize();
            st.model.reasoning.show = false;
            st.confirm_cancel = false;
            st.running_model = None;
            st.turn_started_at = None;
            st.model.status = TurnStatus::Cancelled;
            st.model.toolchain = None;
            st.model.retrying = None;
            st.model.turn_phase = None;
            st.model.turn_phase_detail = None;
            st.model.end_in_flight_usage();
            fail_safe_reject_edits(&mut st.model, &active_t("notify.cancelled_before_edits"));
            let h = st.last_model_height;
            st.model.clamp_scroll(h);
        }
        events::MODEL_ERROR => {
            // The failure message is optional enrichment: the event method is
            // the authoritative lifecycle signal, so a missing or malformed
            // payload falls back to a localized diagnostic instead of leaving
            // the turn busy (c-2).
            let mut message = active_t("notify.turn_failed_no_payload");
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelErrorEvent>(params) {
                    message = data.message;
                }
            }
            // A batch member failure is aggregated into the supervisor's
            // summary, so the member records its diagnostic without tearing
            // down the turn the whole queue shares.
            if st.model.is_batch {
                st.model.error = Some(message);
                return;
            }
            *is_reasoning = false;
            st.model.reasoning.finalize();
            st.model.reasoning.show = false;
            st.confirm_cancel = false;
            st.running_model = None;
            if let Some(started) = st.turn_started_at.take() {
                let elapsed = started.elapsed().as_secs();
                let focused = st.terminal_focused;
                if st.tui_config.notifications.should_notify(elapsed, focused) {
                    let title = active_t("notify.turn_error_desktop_title");
                    let body = active_t("notify.turn_error_desktop_body");
                    crate::terminal::trigger_turn_notification(
                        &title,
                        &body,
                        &st.tui_config.notifications,
                        focused,
                    );
                }
            }
            st.model.toolchain = None;
            st.model.retrying = None;
            st.model.turn_phase = None;
            st.model.turn_phase_detail = None;
            st.model.status = TurnStatus::Error;
            st.model.end_in_flight_usage();
            fail_safe_reject_edits(&mut st.model, &active_t("notify.failed_before_edits"));
            st.model.error = Some(message);
            let h = st.last_model_height;
            st.model.clamp_scroll(h);
        }
        events::REVIEW_STARTED => {
            st.review.running = true;
            st.turn_started_at = Some(std::time::Instant::now());
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewStartedEvent>(params) {
                    begin_review_turn(&mut st.model, &data);
                    // A truncated pattern expansion is never silent: the
                    // daemon reports how many matched files its budgets kept
                    // out of the audit, and the interface warns so the
                    // developer knows the coverage boundary.
                    if data.omitted_files > 0 {
                        st.notify_warning(
                            active_t("notify.review_pattern_files_omitted").replacen(
                                "{}",
                                &data.omitted_files.to_string(),
                                1,
                            ),
                        );
                    }
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
                    findings_count = data.session.items.len();
                    st.review.set_session(data.session);
                }
            }
            finish_review_turn(&mut st.model);
            if let Some(started) = started {
                let elapsed = started.elapsed().as_secs();
                let focused = st.terminal_focused;
                if st.tui_config.notifications.should_notify(elapsed, focused) {
                    let title = active_t("notify.review_desktop_title");
                    let summary = active_t("notify.review_completed_desktop_body").replacen(
                        "{}",
                        &findings_count.to_string(),
                        1,
                    );
                    crate::terminal::trigger_turn_notification(
                        &title,
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
            st.review.error = Some(active_t("notify.review_cancelled"));
            end_review_turn(&mut st.model, TurnStatus::Cancelled, None);
        }
        events::REVIEW_ERROR => {
            st.review.running = false;
            if let Some(started) = st.turn_started_at.take() {
                let elapsed = started.elapsed().as_secs();
                let focused = st.terminal_focused;
                if st.tui_config.notifications.should_notify(elapsed, focused) {
                    let title = active_t("notify.review_error_desktop_title");
                    let body = active_t("notify.review_error_desktop_body");
                    crate::terminal::trigger_turn_notification(
                        &title,
                        &body,
                        &st.tui_config.notifications,
                        focused,
                    );
                }
            }
            // The event method is the authoritative lifecycle signal: a missing
            // or malformed payload falls back to a localized diagnostic instead
            // of leaving the review turn busy (c-2).
            let mut message = active_t("notify.review_failed_no_payload");
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewErrorEvent>(params) {
                    message = data.message;
                }
            }
            st.review.error = Some(message.clone());
            end_review_turn(&mut st.model, TurnStatus::Error, Some(message));
        }
        events::PLAN_UPDATED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<tauqe_protocol::PlanUpdatedEvent>(params)
                {
                    let plan_id = data.plan.id.clone();
                    if let Some(pos) = st.plans_view.plans.iter().position(|p| p.id == plan_id) {
                        st.plans_view.plans[pos] = data.plan;
                    } else {
                        st.plans_view.plans.push(data.plan);
                    }
                    // A `save` action creates or fully replaces a plan: switch to
                    // the Plans tab and anchor the cursor on the fresh plan's
                    // header instead of leaving the developer in the active view
                    // (c-4).
                    if data.is_save {
                        st.view_mode = ViewMode::Plans;
                        st.plans_view.focus_plan_header(&plan_id);
                    }
                }
            }
        }
        events::PLAN_LIST_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(data) =
                    serde_json::from_value::<tauqe_protocol::PlanListChangedEvent>(params)
                {
                    // The focus may never anchor a plan that no longer exists:
                    // the daemon clears its own focus on deletion and
                    // broadcasts the change, so this local reconciliation
                    // covers a list update that arrives before (or without)
                    // that event and keeps the header and the REPL from
                    // naming a dead plan (c-2).
                    let focus_is_dead = match st.discussion_focus.target() {
                        Some(tauqe_protocol::DiscussionTarget::Plan { id }) => {
                            !data.plans.iter().any(|plan| &plan.id == id)
                        }
                        _ => false,
                    };
                    if focus_is_dead {
                        st.discussion_focus = tauqe_protocol::DiscussionFocus::None;
                    }
                    st.plans_view.plans = data.plans;
                    st.plans_view.clamp_selection();
                }
            }
        }
        events::REVIEW_UPDATED => {
            if let Some(params) = ev.params {
                if let Ok(data) =
                    serde_json::from_value::<tauqe_protocol::ReviewUpdatedEvent>(params)
                {
                    st.review.update_session(data.session);
                }
            }
        }
        events::REVIEW_LIST_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(data) =
                    serde_json::from_value::<tauqe_protocol::ReviewListChangedEvent>(params)
                {
                    st.review.set_sessions(data.reviews, data.active_id);
                }
            }
        }
        events::PLAN_BATCH_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<PlanBatchFinishedEvent>(params) {
                    // The batch has terminated: clear the batch-scoped state so
                    // a later interruption never targets a finished queue, then
                    // settle the turn the members deliberately left open.
                    st.batch_stopping = false;
                    st.model.is_batch = false;
                    settle_batch_turn(&mut st, data.summary.status);
                    notify_batch_finished(&st, "Plan", &data.summary);
                }
            }
        }
        events::REVIEW_BATCH_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewBatchFinishedEvent>(params) {
                    // Mirror the plan supervisor: the terminal batch event is
                    // the only point that settles the shared turn.
                    st.batch_stopping = false;
                    st.model.is_batch = false;
                    settle_batch_turn(&mut st, data.summary.status);
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
fn begin_review_turn(model: &mut ReplView, data: &ReviewStartedEvent) {
    model.operation_id = Some(data.operation_id.clone());
    model.model = Some(data.model.clone());
    model.reset_turn_buffers();
    model.scroll = 0;
    model.status = TurnStatus::Responding;
    model.plan_context = None;
    model.is_batch = false;
    model.auto_scroll = true;
}

/// Streams a Reviewer reasoning delta into the REPL reasoning buffer.
fn apply_review_reasoning(model: &mut ReplView, delta: &str) {
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
fn apply_review_content(model: &mut ReplView, delta: &str) {
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
fn finish_review_turn(model: &mut ReplView) {
    end_review_turn(model, TurnStatus::Done, None);
    model.update_markdown();
}

/// Settles the REPL buffer into a terminal status and finalizes reasoning.
fn end_review_turn(model: &mut ReplView, status: TurnStatus, error: Option<String>) {
    model.reasoning.finalize();
    model.reasoning.show = false;
    model.status = status;
    if let Some(message) = error {
        model.error = Some(message);
    }
}

/// Settles the REPL buffer when a batch supervisor terminates.
///
/// Batch members leave the turn open (see the `MODEL_FINISHED` and
/// `MODEL_ERROR` handlers): the queue owns one turn from its first member to
/// its last. This is therefore the single point that stops the spinner, clears
/// the phase telemetry, projects the aggregate outcome onto the turn status,
/// and reconciles the streamed file edits, so the interface never stays stuck
/// in a busy state after the queue ends.
fn settle_batch_turn(st: &mut AppState, status: BatchFinishStatus) {
    st.running_model = None;
    st.turn_started_at = None;
    st.confirm_cancel = false;

    let applied = st.model.edit_final_applied == Some(true) || st.model.last_commit_hash.is_some();
    let height = st.last_model_height;

    let model = &mut st.model;
    model.reasoning.finalize();
    model.reasoning.show = false;
    model.status = match status {
        BatchFinishStatus::Completed => TurnStatus::Done,
        BatchFinishStatus::Failed | BatchFinishStatus::Blocked => TurnStatus::Error,
        BatchFinishStatus::Cancelled | BatchFinishStatus::Stopped => TurnStatus::Cancelled,
    };
    model.toolchain = None;
    model.retrying = None;
    model.turn_phase = None;
    model.turn_round = None;
    model.turn_max_rounds = None;
    model.turn_phase_detail = None;
    model.end_in_flight_usage();
    if applied {
        for file in model.files.iter_mut() {
            file.status = FileEditStatus::Ok;
            file.error = None;
            file.retry_info = None;
        }
    } else {
        fail_safe_reject_edits(model, &active_t("notify.edit_desync"));
    }
    model.update_markdown();
    model.clamp_scroll(height);
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
        BatchFinishStatus::Stopped => "stopped",
    };
    let title = active_t("notify.batch_title")
        .replacen("{}", kind, 1)
        .replacen("{}", status_word, 1);
    let mut body = active_t("notify.batch_body")
        .replacen("{}", kind, 1)
        .replacen("{}", &summary.owner_id, 1)
        .replacen("{}", &summary.executed_count.to_string(), 1)
        .replacen("{}", &summary.total_count.to_string(), 1)
        .replacen("{}", &format_elapsed(summary.elapsed_seconds), 1);
    if let Some(err) = summary.error.as_deref() {
        let first_line = err.lines().next().unwrap_or("").trim();
        if !first_line.is_empty() {
            body.push_str(" - ");
            body.push_str(first_line);
        }
    }
    (title, body)
}

/// Formats the in-app completion message for a finished model turn, including
/// the commit hash and summary when a commit was created.
fn format_turn_completion_message(
    commit_hash: Option<&str>,
    commit_summary: Option<&str>,
) -> String {
    let summary = commit_summary.map(str::trim).filter(|s| !s.is_empty());
    match (commit_hash, summary) {
        (Some(hash), Some(summary)) => active_t("notify.turn_completed_hash_summary")
            .replacen("{}", hash, 1)
            .replacen("{}", summary, 1),
        (Some(hash), None) => active_t("notify.turn_completed_hash").replacen("{}", hash, 1),
        (None, Some(summary)) => {
            active_t("notify.turn_completed_summary").replacen("{}", summary, 1)
        }
        (None, None) => active_t("notify.turn_completed"),
    }
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
    use tauqe_protocol::{
        HistoryEntryStatus, ModelCatalogEntry, ModelFinishedEvent, ModelRef, ModelRole,
        ModelRolesEntries, ModelRolesSummary, ReasoningEffort, UiHistoryItem, UiHistoryKind,
    };

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
            model: ModelRef::openrouter("reviewer"),
            files_count: 2,
            estimated_tokens: 128,
            omitted_files: 0,
        }
    }

    #[test]
    fn test_review_started_resets_repl_buffer() {
        let mut model = ReplView::default();
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
        assert_eq!(model.model, Some(ModelRef::openrouter("reviewer")));
        assert_eq!(model.status, TurnStatus::Responding);
        assert!(model.text.is_empty());
        assert!(model.markdown_lines.is_empty());
        assert!(model.error.is_none());
        assert!(model.files.is_empty());
        assert!(!model.edits_active);
        assert!(model.auto_scroll);
    }

    #[tokio::test]
    async fn test_review_started_warns_when_pattern_files_were_omitted() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::REVIEW_STARTED.to_string(),
            params: Some(
                serde_json::to_value(ReviewStartedEvent {
                    operation_id: "rev-op-1".to_string(),
                    model: ModelRef::openrouter("reviewer"),
                    files_count: 2,
                    estimated_tokens: 128,
                    omitted_files: 3,
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        let notif = guard
            .active_notification()
            .expect("an omitted-files warning must be raised");
        assert_eq!(notif.level, crate::app::NotificationLevel::Warning);
        assert!(
            notif.text.contains('3'),
            "the warning must name the omitted count: {}",
            notif.text
        );
    }

    #[test]
    fn test_review_reasoning_delta_streams_into_repl() {
        let mut model = ReplView {
            status: TurnStatus::Responding,
            ..Default::default()
        };

        apply_review_reasoning(&mut model, "weighing the options\n");

        assert_eq!(model.status, TurnStatus::Thinking);
        assert!(model.reasoning.text.contains("weighing the options"));
    }

    #[test]
    fn test_review_content_delta_renders_markdown_in_repl() {
        let mut model = ReplView::default();

        apply_review_content(&mut model, "# Executive Summary\n\nNo critical issues.");

        assert_eq!(model.status, TurnStatus::Responding);
        assert!(model.text.contains("Executive Summary"));
        assert!(!model.markdown_lines.is_empty());
    }

    #[test]
    fn test_review_content_delta_finalizes_and_folds_reasoning() {
        let mut model = ReplView::default();
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
        let mut model = ReplView::default();
        apply_review_reasoning(&mut model, "reasoning");
        apply_review_content(&mut model, "# Summary");
        model.markdown_lines.clear();

        finish_review_turn(&mut model);

        assert_eq!(model.status, TurnStatus::Done);
        assert!(!model.markdown_lines.is_empty());
    }

    #[test]
    fn test_review_terminal_statuses_map_to_repl() {
        let mut cancelled = ReplView::default();
        cancelled.reasoning.show = true;
        end_review_turn(&mut cancelled, TurnStatus::Cancelled, None);
        assert_eq!(cancelled.status, TurnStatus::Cancelled);
        assert!(!cancelled.reasoning.show);

        let mut failed = ReplView::default();
        failed.reasoning.show = true;
        end_review_turn(&mut failed, TurnStatus::Error, Some("boom".to_string()));
        assert_eq!(failed.status, TurnStatus::Error);
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert!(!failed.reasoning.show);
    }

    #[tokio::test]
    async fn test_model_connection_retrying_records_the_countdown() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::MODEL_CONNECTION_RETRYING.to_string(),
            params: Some(
                serde_json::to_value(tauqe_protocol::ModelConnectionRetryingEvent {
                    operation_id: "op-retry".to_string(),
                    attempt: 2,
                    max_attempts: 4,
                    delay_secs: 3,
                    reason: "stream ended prematurely".to_string(),
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        let retrying = guard.model.retrying.expect("the countdown is recorded");
        assert_eq!(retrying.attempt, 2);
        assert_eq!(retrying.max_attempts, 4);
        assert_eq!(retrying.delay_secs, 3);
    }

    #[tokio::test]
    async fn test_model_stream_reset_discards_the_partial_answer() {
        let mut st = crate::app::test_state();
        st.model.text = "Hel".to_string();
        st.model.update_markdown();
        st.model.reasoning.append_delta("drafting the change");
        assert!(!st.model.markdown_lines.is_empty());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = true;
        let ev = Event {
            method: events::MODEL_STREAM_RESET.to_string(),
            params: Some(
                serde_json::to_value(tauqe_protocol::ModelStreamResetEvent {
                    operation_id: "op-reset".to_string(),
                    reason: "connection interrupted mid-stream".to_string(),
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert!(
            guard.model.text.is_empty(),
            "the aborted attempt's fragments must be dropped"
        );
        assert!(guard.model.markdown_lines.is_empty());
        assert!(guard.model.code_blocks.is_empty());
        assert!(
            guard.model.reasoning.is_empty(),
            "the aborted reasoning stream must be dropped as well"
        );
        assert!(!is_reasoning, "the reasoning phase ends with the reset");
    }

    #[tokio::test]
    async fn test_model_escalated_updates_the_running_model() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        let mut is_reasoning = false;

        // The alias is provider-catalog metadata: it reaches the label through
        // the published snapshot, never through the escalation event itself
        // (c-1).
        let summary = ModelRolesSummary {
            scribe: ModelRef::openrouter("scribe"),
            coder: ModelRef::openrouter("stronger-coder"),
            architect: ModelRef::openrouter("architect"),
            primary: ModelRef::openrouter("primary"),
            reviewer: ModelRef::openrouter("reviewer"),
            efforts: None,
            entries: Some(ModelRolesEntries {
                catalog: Some(vec![ModelCatalogEntry {
                    model: ModelRef::openrouter("stronger-coder"),
                    alias: Some("deep".to_string()),
                }]),
                ..Default::default()
            }),
        };
        crate::model_display::install_snapshot(Some(&summary));

        let ev = Event {
            method: events::MODEL_ESCALATED.to_string(),
            params: Some(
                serde_json::to_value(ModelEscalatedEvent {
                    operation_id: "op-escalate".to_string(),
                    model: ModelRef::openrouter("stronger-coder"),
                    role: Some(ModelRole::Coder),
                    alias: Some("ignored-by-the-client".to_string()),
                    effort: Some(ReasoningEffort::High),
                    tier: 1,
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        let running = guard
            .running_model
            .as_ref()
            .expect("the escalated model stays current until the turn settles");
        assert_eq!(running.model, ModelRef::openrouter("stronger-coder"));
        assert!(matches!(running.role, Some(ModelRole::Coder)));
        assert_eq!(
            running.label(),
            "deep [high]",
            "the label comes from the catalog alias plus the reasoning depth"
        );
    }

    #[tokio::test]
    async fn test_model_text_delta_clears_the_reconnect_countdown() {
        let mut st = crate::app::test_state();
        st.model.retrying = Some(RetryingState {
            attempt: 1,
            max_attempts: 3,
            delay_secs: 2,
        });

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::MODEL_TEXT_DELTA.to_string(),
            params: Some(
                serde_json::to_value(ModelDeltaEvent {
                    operation_id: "op-answer".to_string(),
                    delta: "Answer".to_string(),
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert!(
            guard.model.retrying.is_none(),
            "the countdown must not outlive the successful attempt"
        );
    }

    #[tokio::test]
    async fn test_model_error_folds_reasoning_block() {
        let mut st = crate::app::test_state();
        st.model.reasoning.append_delta("Deep thoughts...");
        assert!(st.model.reasoning.show);

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = true;
        let ev = Event {
            method: events::MODEL_ERROR.to_string(),
            params: Some(
                serde_json::to_value(ModelErrorEvent {
                    operation_id: "op-err".to_string(),
                    message: "Generation ended with finish_reason: error".to_string(),
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(guard.model.status, TurnStatus::Error);
        assert!(!guard.model.reasoning.show);
        assert!(!guard.model.reasoning.is_streaming());
    }

    #[tokio::test]
    async fn test_model_finished_emits_in_app_success_notification_with_commit() {
        let mut st = crate::app::test_state();
        st.view_mode = crate::app::ViewMode::Plans;
        st.model.status = TurnStatus::Editing;
        st.model.edits_active = true;
        st.model.edit_final_applied = Some(true);
        st.model.last_commit_hash = Some("a1b2c3d".to_string());
        st.model.last_commit_summary = Some("feat(tui): add notification".to_string());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::MODEL_FINISHED.to_string(),
            params: Some(
                serde_json::to_value(ModelFinishedEvent {
                    operation_id: "op-1".to_string(),
                    full_text: String::new(),
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(guard.model.status, TurnStatus::Done);
        let notif = guard
            .active_notification()
            .expect("completion notification must be set");
        assert_eq!(notif.level, crate::app::NotificationLevel::Success);
        assert_eq!(
            notif.text,
            "Turn completed (a1b2c3d): feat(tui): add notification"
        );
    }

    #[tokio::test]
    async fn test_model_finished_without_edits_emits_completion_notification() {
        let mut st = crate::app::test_state();
        st.view_mode = crate::app::ViewMode::Review;
        st.model.status = TurnStatus::Responding;

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::MODEL_FINISHED.to_string(),
            params: Some(
                serde_json::to_value(ModelFinishedEvent {
                    operation_id: "op-2".to_string(),
                    full_text: String::new(),
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        let notif = guard
            .active_notification()
            .expect("completion notification must be set");
        assert_eq!(notif.level, crate::app::NotificationLevel::Success);
        assert_eq!(notif.text, "Turn completed");
    }

    fn ui_history_item(id: u64, kind: UiHistoryKind, text: &str) -> UiHistoryItem {
        UiHistoryItem {
            id,
            kind,
            text: text.to_string(),
            summary: None,
            commit_hash: None,
            files: Vec::new(),
            status: HistoryEntryStatus::Committed,
        }
    }

    fn compacted_event(
        summary: UiHistoryItem,
        tail_ids: Vec<u64>,
        total_count: usize,
        estimated_tokens: u64,
    ) -> Event {
        Event {
            method: events::HISTORY_COMPACTED.to_string(),
            params: Some(
                serde_json::to_value(HistoryCompactedEvent {
                    summary,
                    tail_ids,
                    total_count,
                    estimated_tokens,
                })
                .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn test_history_compacted_removes_head_and_places_summary_first() {
        let mut st = crate::app::test_state();
        st.last_model_height = 2;
        st.history_view.auto_scroll = true;
        st.history_view.total_count = 5;
        st.history_view.estimated_tokens = 40;
        st.history_view.items = vec![
            ui_history_item(1, UiHistoryKind::User, "q1"),
            ui_history_item(2, UiHistoryKind::Assistant, "a1"),
            ui_history_item(3, UiHistoryKind::User, "q2"),
            ui_history_item(4, UiHistoryKind::Assistant, "a2"),
            ui_history_item(5, UiHistoryKind::User, "q3"),
        ];
        st.history_view.expanded_items.insert(2);

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = compacted_event(
            ui_history_item(3, UiHistoryKind::System, "Session compacted"),
            vec![4, 5],
            3,
            256,
        );

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        let ids: Vec<u64> = guard.history_view.items.iter().map(|it| it.id).collect();
        assert_eq!(
            ids,
            vec![3, 4, 5],
            "the compacted head is dropped and the tail follows"
        );
        assert_eq!(
            guard.history_view.items[0].id, 3,
            "summary leads at index zero"
        );
        assert_eq!(guard.history_view.items[0].kind, UiHistoryKind::System);
        assert_eq!(guard.history_view.total_count, 3);
        assert_eq!(guard.history_view.estimated_tokens, 256);
        assert!(
            !guard.history_view.expanded_items.contains(&2),
            "head fold state is discarded"
        );

        let rendered = guard.history_view.rendered_lines_count;
        let expected_scroll = u16::try_from(rendered)
            .unwrap_or(u16::MAX)
            .saturating_sub(2);
        assert_eq!(guard.history_view.scroll, expected_scroll);
        assert_eq!(
            guard.history_view.selected_item_index,
            guard.history_view.items.len() - 1
        );
    }

    #[tokio::test]
    async fn test_history_compacted_without_auto_scroll_clamps_position() {
        let mut st = crate::app::test_state();
        st.last_model_height = 5;
        st.history_view.auto_scroll = false;
        st.history_view.scroll = u16::MAX;
        st.history_view.selected_item_index = 99;
        st.history_view.items = (1u64..=6)
            .map(|i| ui_history_item(i, UiHistoryKind::User, "line"))
            .collect();

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = compacted_event(
            ui_history_item(3, UiHistoryKind::System, "Compacted"),
            vec![4, 5, 6],
            4,
            96,
        );

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        let ids: Vec<u64> = guard.history_view.items.iter().map(|it| it.id).collect();
        assert_eq!(ids, vec![3, 4, 5, 6]);
        assert_eq!(
            guard.history_view.selected_item_index,
            guard.history_view.items.len() - 1
        );
        let rendered = guard.history_view.rendered_lines_count;
        let expected_scroll = u16::try_from(rendered)
            .unwrap_or(u16::MAX)
            .saturating_sub(5);
        assert_eq!(guard.history_view.scroll, expected_scroll);
    }

    #[tokio::test]
    async fn test_history_entry_rolled_back_replaces_the_record_in_place() {
        let mut st = crate::app::test_state();
        st.view_mode = crate::app::ViewMode::History;
        st.history_view.total_count = 1;
        st.history_view.items = vec![ui_history_item(1, UiHistoryKind::User, "Task")];

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let item = UiHistoryItem {
            status: HistoryEntryStatus::RolledBack,
            summary: Some("Turn cancelled by the developer".to_string()),
            ..ui_history_item(1, UiHistoryKind::User, "Task")
        };
        let ev = Event {
            method: events::HISTORY_ENTRY_ROLLED_BACK.to_string(),
            params: Some(serde_json::to_value(HistoryEntryRolledBackEvent { item }).unwrap()),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(
            guard.history_view.items.len(),
            1,
            "a rollback must never duplicate the record"
        );
        assert_eq!(
            guard.history_view.items[0].status,
            HistoryEntryStatus::RolledBack
        );
        assert_eq!(
            guard.history_view.items[0].summary.as_deref(),
            Some("Turn cancelled by the developer")
        );
        assert_eq!(guard.history_view.total_count, 1);
    }

    #[tokio::test]
    async fn test_history_entry_rolled_back_keeps_an_unseen_record_visible() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        let mut is_reasoning = false;
        let item = UiHistoryItem {
            status: HistoryEntryStatus::RolledBack,
            ..ui_history_item(9, UiHistoryKind::User, "Lost prompt")
        };
        let ev = Event {
            method: events::HISTORY_ENTRY_ROLLED_BACK.to_string(),
            params: Some(serde_json::to_value(HistoryEntryRolledBackEvent { item }).unwrap()),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(guard.history_view.items.len(), 1);
        assert_eq!(
            guard.history_view.items[0].status,
            HistoryEntryStatus::RolledBack
        );
        assert_eq!(guard.history_view.total_count, 1);
    }

    #[tokio::test]
    async fn test_plan_updated_save_switches_to_plans_and_focuses_the_plan() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::PLAN_UPDATED.to_string(),
            params: Some(
                serde_json::to_value(tauqe_protocol::PlanUpdatedEvent {
                    plan: tauqe_protocol::Plan {
                        id: "plan-x".to_string(),
                        title: "Plan X".to_string(),
                        ..Default::default()
                    },
                    is_save: true,
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(guard.view_mode, crate::app::ViewMode::Plans);
        assert!(
            guard.plans_view.is_plan_expanded("plan-x"),
            "a saved plan must be expanded so its header is visible"
        );
    }

    fn plan_with_id(id: &str) -> tauqe_protocol::Plan {
        tauqe_protocol::Plan {
            id: id.to_string(),
            title: format!("Plan {}", id),
            ..Default::default()
        }
    }

    fn plan_list_changed_event(plans: Vec<tauqe_protocol::Plan>) -> Event {
        Event {
            method: events::PLAN_LIST_CHANGED.to_string(),
            params: Some(
                serde_json::to_value(tauqe_protocol::PlanListChangedEvent {
                    plans,
                    active_id: None,
                })
                .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn test_plan_list_changed_clears_a_focus_on_a_deleted_plan() {
        let mut st = crate::app::test_state();
        st.discussion_focus = tauqe_protocol::DiscussionFocus::plan("plan-gone", Vec::new());
        assert!(st.discussion_focus.is_active());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(
            plan_list_changed_event(vec![plan_with_id("plan-kept")]),
            &state,
            &mut is_reasoning,
        )
        .await;

        let guard = state.lock().await;
        assert!(
            guard.discussion_focus.is_none(),
            "a focus anchored on a deleted plan must be dropped locally"
        );
        assert_eq!(
            guard.plans_view.plans.len(),
            1,
            "the refreshed list still replaces the mirrored one"
        );
    }

    #[tokio::test]
    async fn test_plan_list_changed_keeps_a_focus_on_a_surviving_plan() {
        let mut st = crate::app::test_state();
        st.discussion_focus =
            tauqe_protocol::DiscussionFocus::plan("plan-kept", vec!["1".to_string()]);

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(
            plan_list_changed_event(vec![plan_with_id("plan-kept")]),
            &state,
            &mut is_reasoning,
        )
        .await;

        let guard = state.lock().await;
        assert_eq!(
            guard.discussion_focus.target(),
            Some(&tauqe_protocol::DiscussionTarget::Plan {
                id: "plan-kept".to_string()
            }),
            "a surviving plan keeps its anchor"
        );
        assert_eq!(
            guard.discussion_focus.focused_items(),
            ["1".to_string()].as_slice()
        );
    }

    #[tokio::test]
    async fn test_plan_list_changed_leaves_a_review_focus_untouched() {
        let mut st = crate::app::test_state();
        st.discussion_focus = tauqe_protocol::DiscussionFocus::review("rev-1", Vec::new());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(
            plan_list_changed_event(Vec::new()),
            &state,
            &mut is_reasoning,
        )
        .await;

        let guard = state.lock().await;
        assert_eq!(
            guard.discussion_focus.target(),
            Some(&tauqe_protocol::DiscussionTarget::Review {
                id: "rev-1".to_string()
            }),
            "a plan list refresh never touches a review anchor"
        );
    }

    #[tokio::test]
    async fn test_model_finished_for_a_batch_member_keeps_the_turn_open() {
        let mut st = crate::app::test_state();
        st.model.is_batch = true;
        st.model.status = TurnStatus::Responding;
        st.model.reasoning.append_delta("drafting the step");
        st.turn_started_at = Some(std::time::Instant::now());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::MODEL_FINISHED.to_string(),
            params: Some(
                serde_json::to_value(ModelFinishedEvent {
                    operation_id: "op-batch".to_string(),
                    full_text: String::new(),
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(
            guard.model.status,
            TurnStatus::Responding,
            "a batch member must not settle the turn"
        );
        assert!(
            guard.turn_started_at.is_some(),
            "the queue keeps owning the turn started timestamp"
        );
        assert!(guard.model.reasoning.is_streaming());
        assert!(guard.active_notification().is_none());
    }

    #[tokio::test]
    async fn test_plan_batch_finished_settles_the_shared_turn() {
        let mut st = crate::app::test_state();
        st.model.is_batch = true;
        st.model.status = TurnStatus::Responding;
        st.model.edits_active = true;
        st.model.edit_final_applied = Some(true);
        st.model.toolchain = Some(ToolchainState::Running {
            command: "cargo check".to_string(),
        });
        st.model.turn_phase = Some(tauqe_protocol::TurnPhase::Verification);
        st.batch_stopping = true;
        st.turn_started_at = Some(std::time::Instant::now());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::PLAN_BATCH_FINISHED.to_string(),
            params: Some(
                serde_json::to_value(PlanBatchFinishedEvent {
                    operation_id: "op-plan-batch".to_string(),
                    summary: batch_summary(BatchFinishStatus::Completed, 2, 2, 30),
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(guard.model.status, TurnStatus::Done);
        assert!(!guard.model.is_batch);
        assert!(!guard.batch_stopping);
        assert!(guard.model.toolchain.is_none());
        assert!(guard.model.turn_phase.is_none());
        assert!(
            guard.turn_started_at.is_none(),
            "the terminal batch event releases the turn timestamp"
        );
    }

    #[tokio::test]
    async fn test_review_batch_finished_projects_a_failure_onto_the_turn() {
        let mut st = crate::app::test_state();
        st.model.is_batch = true;
        st.model.status = TurnStatus::Verifying;

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::REVIEW_BATCH_FINISHED.to_string(),
            params: Some(
                serde_json::to_value(ReviewBatchFinishedEvent {
                    operation_id: "op-review-batch".to_string(),
                    summary: batch_summary(BatchFinishStatus::Failed, 1, 3, 90),
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(guard.model.status, TurnStatus::Error);
        assert!(!guard.model.is_batch);
    }

    #[tokio::test]
    async fn test_repeated_edit_file_started_clears_the_previous_attempt() {
        let mut st = crate::app::test_state();
        st.model.files.push(StreamingFileEdit {
            path: "crates/tui/src/main.rs".to_string(),
            op_type: EditOp::Replace,
            status: FileEditStatus::Retrying,
            error: Some("hunk did not match the file".to_string()),
            hunks: vec![StreamingHunk {
                hunk_index: 0,
                old_text: "let a = 1;".to_string(),
                new_text: "let a = 2;".to_string(),
            }],
            expanded: true,
            retry_info: Some("1/3 retrying: no match".to_string()),
        });

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::EDIT_FILE_STARTED.to_string(),
            params: Some(
                serde_json::to_value(EditFileStartedEvent {
                    operation_id: "op-edit".to_string(),
                    path: "crates/tui/src/main.rs".to_string(),
                    op_type: EditOp::Replace,
                })
                .unwrap(),
            ),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(
            guard.model.files.len(),
            1,
            "a repeated start must never duplicate the file entry"
        );
        let file = &guard.model.files[0];
        assert!(
            file.hunks.is_empty(),
            "the rejected attempt's hunks must be dropped before the new stream"
        );
        assert_eq!(file.status, FileEditStatus::Running);
        assert!(file.error.is_none());
        assert!(file.retry_info.is_none());
    }

    fn accepted_file(path: &str) -> StreamingFileEdit {
        StreamingFileEdit {
            path: path.to_string(),
            op_type: EditOp::Replace,
            status: FileEditStatus::Ok,
            error: None,
            hunks: Vec::new(),
            expanded: false,
            retry_info: None,
        }
    }

    fn toolchain_finished_event(success: bool) -> Event {
        Event {
            method: events::TOOLCHAIN_FINISHED.to_string(),
            params: Some(
                serde_json::to_value(tauqe_protocol::ToolchainFinishedEvent {
                    operation_id: "op-toolchain".to_string(),
                    command: "cargo check".to_string(),
                    success,
                    message: if success {
                        None
                    } else {
                        Some("error[E0308]: mismatched types".to_string())
                    },
                })
                .unwrap(),
            ),
        }
    }

    fn turn_phase_event(phase: tauqe_protocol::TurnPhase) -> Event {
        Event {
            method: events::TURN_PHASE.to_string(),
            params: Some(
                serde_json::to_value(tauqe_protocol::TurnPhaseEvent {
                    operation_id: "op-turn".to_string(),
                    phase,
                    round: Some(1),
                    max_rounds: Some(3),
                    detail: None,
                })
                .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn test_toolchain_failure_returns_accepted_files_to_retrying() {
        let mut st = crate::app::test_state();
        st.model.files = vec![
            accepted_file("crates/tui/src/main.rs"),
            accepted_file("crates/tui/src/lib.rs"),
        ];

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(toolchain_finished_event(false), &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert!(matches!(
            guard.model.toolchain.as_ref(),
            Some(ToolchainState::Failed { .. })
        ));
        let reason = active_t("repl.edits_retry_healing");
        for file in &guard.model.files {
            assert_eq!(
                file.status,
                FileEditStatus::Retrying,
                "a patch rejected by the compiler must not stay green in the view"
            );
            assert_eq!(file.retry_info.as_deref(), Some(reason.as_str()));
            assert!(file.error.is_none());
        }
    }

    #[tokio::test]
    async fn test_toolchain_success_keeps_accepted_files_settled() {
        let mut st = crate::app::test_state();
        st.model.files = vec![accepted_file("crates/tui/src/main.rs")];

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(toolchain_finished_event(true), &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert!(matches!(
            guard.model.toolchain.as_ref(),
            Some(ToolchainState::Passed { .. })
        ));
        assert_eq!(guard.model.files[0].status, FileEditStatus::Ok);
        assert!(guard.model.files[0].retry_info.is_none());
    }

    #[tokio::test]
    async fn test_healing_phase_returns_accepted_files_to_retrying() {
        let mut st = crate::app::test_state();
        st.model.files = vec![accepted_file("crates/tui/src/main.rs")];
        st.model.status = TurnStatus::Verifying;

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(
            turn_phase_event(tauqe_protocol::TurnPhase::Healing),
            &state,
            &mut is_reasoning,
        )
        .await;

        let guard = state.lock().await;
        assert_eq!(guard.model.status, TurnStatus::Healing);
        assert_eq!(guard.model.files[0].status, FileEditStatus::Retrying);
        assert!(guard.model.files[0].retry_info.is_some());
    }

    #[tokio::test]
    async fn test_healing_phase_keeps_an_in_flight_attempt_message() {
        let mut st = crate::app::test_state();
        st.model.files = vec![StreamingFileEdit {
            retry_info: Some("1/3 retrying: hunk did not match".to_string()),
            status: FileEditStatus::Retrying,
            ..accepted_file("crates/tui/src/stream.rs")
        }];

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(
            turn_phase_event(tauqe_protocol::TurnPhase::Healing),
            &state,
            &mut is_reasoning,
        )
        .await;

        let guard = state.lock().await;
        assert_eq!(
            guard.model.files[0].retry_info.as_deref(),
            Some("1/3 retrying: hunk did not match"),
            "the patch-retry detail owns the message of an unfinished attempt"
        );
    }

    #[tokio::test]
    async fn test_non_healing_phase_keeps_accepted_edits() {
        let mut st = crate::app::test_state();
        st.model.files = vec![accepted_file("crates/tui/src/main.rs")];

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(
            turn_phase_event(tauqe_protocol::TurnPhase::Staging),
            &state,
            &mut is_reasoning,
        )
        .await;

        let guard = state.lock().await;
        assert_eq!(guard.model.files[0].status, FileEditStatus::Ok);
        assert!(guard.model.files[0].retry_info.is_none());
    }

    const STREAMED_PATH: &str = "crates/tui/src/views/repl/state.rs";

    fn edit_file_started_event(path: &str) -> Event {
        Event {
            method: events::EDIT_FILE_STARTED.to_string(),
            params: Some(
                serde_json::to_value(EditFileStartedEvent {
                    operation_id: "op-stream".to_string(),
                    path: path.to_string(),
                    op_type: EditOp::Replace,
                })
                .unwrap(),
            ),
        }
    }

    fn edit_file_done_event(
        path: &str,
        status: tauqe_protocol::EditFileStatus,
        error: Option<&str>,
    ) -> Event {
        Event {
            method: events::EDIT_FILE_DONE.to_string(),
            params: Some(
                serde_json::to_value(EditFileDoneEvent {
                    operation_id: "op-stream".to_string(),
                    path: path.to_string(),
                    status,
                    error: error.map(str::to_string),
                    hunks_count: 1,
                })
                .unwrap(),
            ),
        }
    }

    fn first_hunk_event(path: &str, old_text: &str, new_text: &str) -> Event {
        Event {
            method: events::EDIT_HUNK.to_string(),
            params: Some(
                serde_json::to_value(EditHunkEvent {
                    operation_id: "op-stream".to_string(),
                    path: path.to_string(),
                    hunk_index: 0,
                    old_text: old_text.to_string(),
                    new_text: new_text.to_string(),
                })
                .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn test_edit_hunk_auto_scrolls_the_viewport_to_the_stream_end() {
        let mut st = crate::app::test_state();
        st.last_model_height = 10;
        st.model.auto_scroll = true;
        st.model.scroll = 0;
        st.model.rendered_lines_count = 42;
        st.model.files.push(accepted_file(STREAMED_PATH));

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(
            first_hunk_event(STREAMED_PATH, "let a = 1;", "let a = 2;"),
            &state,
            &mut is_reasoning,
        )
        .await;

        let guard = state.lock().await;
        assert_eq!(
            guard.model.scroll, 32,
            "the streamed hunk must be pulled above the lower viewport edge"
        );
        assert_eq!(guard.model.files[0].hunks.len(), 1);
        assert!(guard.model.auto_scroll);
    }

    #[tokio::test]
    async fn test_edit_stream_keeps_a_manual_scroll_position() {
        let mut st = crate::app::test_state();
        st.last_model_height = 10;
        st.model.auto_scroll = false;
        st.model.scroll = 5;
        st.model.rendered_lines_count = 42;
        st.model.files.push(accepted_file(STREAMED_PATH));

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        handle_event(
            Event {
                method: events::EDIT_STARTED.to_string(),
                params: None,
            },
            &state,
            &mut is_reasoning,
        )
        .await;
        handle_event(
            first_hunk_event(STREAMED_PATH, "let a = 1;", "let a = 2;"),
            &state,
            &mut is_reasoning,
        )
        .await;
        handle_event(
            edit_file_done_event(STREAMED_PATH, tauqe_protocol::EditFileStatus::Ok, None),
            &state,
            &mut is_reasoning,
        )
        .await;

        let guard = state.lock().await;
        assert_eq!(
            guard.model.scroll, 5,
            "a manual scroll survives the whole edit stream"
        );
        assert!(!guard.model.auto_scroll);
    }

    #[tokio::test]
    async fn test_retry_round_never_accumulates_hunks_across_attempts() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        let mut is_reasoning = false;

        // Attempt 1: the file starts, streams one hunk, and the patch retry
        // rejects it before the same file is streamed again.
        handle_event(
            edit_file_started_event(STREAMED_PATH),
            &state,
            &mut is_reasoning,
        )
        .await;
        handle_event(
            first_hunk_event(STREAMED_PATH, "let a = 1;", "let a = 2;"),
            &state,
            &mut is_reasoning,
        )
        .await;
        handle_event(
            edit_file_done_event(
                STREAMED_PATH,
                tauqe_protocol::EditFileStatus::Error,
                Some("hunk did not match the file"),
            ),
            &state,
            &mut is_reasoning,
        )
        .await;
        handle_event(
            Event {
                method: events::EDIT_FILE_RETRYING.to_string(),
                params: Some(
                    serde_json::to_value(EditFileRetryingEvent {
                        operation_id: "op-stream".to_string(),
                        path: STREAMED_PATH.to_string(),
                        attempt: 1,
                        max_retries: 3,
                        reason: "hunk did not match the file".to_string(),
                    })
                    .unwrap(),
                ),
            },
            &state,
            &mut is_reasoning,
        )
        .await;

        // Attempt 2: the restart begins from an empty hunk list.
        handle_event(
            edit_file_started_event(STREAMED_PATH),
            &state,
            &mut is_reasoning,
        )
        .await;
        {
            let guard = state.lock().await;
            assert_eq!(
                guard.model.files.len(),
                1,
                "one path keeps exactly one streamed entry"
            );
            assert!(
                guard.model.files[0].hunks.is_empty(),
                "the rejected attempt's hunks must not survive the restart"
            );
        }
        handle_event(
            first_hunk_event(STREAMED_PATH, "let a = 1;", "let a = 3;"),
            &state,
            &mut is_reasoning,
        )
        .await;

        let guard = state.lock().await;
        assert_eq!(guard.model.files.len(), 1);
        let file = &guard.model.files[0];
        assert_eq!(
            file.hunks.len(),
            1,
            "the retry carries exactly the hunk of its own attempt"
        );
        assert_eq!(file.hunks[0].new_text, "let a = 3;");
        assert_eq!(file.status, FileEditStatus::Running);
        assert!(file.error.is_none());
        assert!(file.retry_info.is_none());
    }

    #[tokio::test]
    async fn test_model_finished_without_payload_still_settles_the_turn() {
        let mut st = crate::app::test_state();
        st.model.status = TurnStatus::Verifying;
        st.model.toolchain = Some(ToolchainState::Running {
            command: "cargo check".to_string(),
        });
        st.model.turn_phase = Some(tauqe_protocol::TurnPhase::Verification);
        st.model.turn_phase_detail = Some("verifying".to_string());
        st.running_model = Some(RunningModel {
            model: ModelRef::openrouter("coder"),
            role: None,
            effort: None,
        });
        st.turn_started_at = Some(std::time::Instant::now());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = true;
        let ev = Event {
            method: events::MODEL_FINISHED.to_string(),
            params: None,
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(guard.model.status, TurnStatus::Done);
        assert!(
            !guard.model.is_busy(),
            "a settled turn must never keep the interface busy"
        );
        assert!(guard.running_model.is_none());
        assert!(
            guard.turn_started_at.is_none(),
            "the cancellation surfaces no longer target the finished turn"
        );
        assert!(!guard.confirm_cancel);
        assert!(!is_reasoning, "the reasoning phase ends with the turn");
        assert!(guard.model.toolchain.is_none());
        assert!(guard.model.turn_phase.is_none());
        assert!(guard.model.turn_phase_detail.is_none());
        let notif = guard
            .active_notification()
            .expect("the settled turn still reports its completion");
        assert_eq!(notif.level, crate::app::NotificationLevel::Success);
    }

    #[tokio::test]
    async fn test_model_finished_with_malformed_payload_still_settles_the_turn() {
        let mut st = crate::app::test_state();
        st.model.status = TurnStatus::Verifying;
        st.model.turn_phase = Some(tauqe_protocol::TurnPhase::Verification);
        st.turn_started_at = Some(std::time::Instant::now());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::MODEL_FINISHED.to_string(),
            params: Some(serde_json::Value::from("truncated frame")),
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(guard.model.status, TurnStatus::Done);
        assert!(!guard.model.is_busy());
        assert!(guard.model.turn_phase.is_none());
        assert!(guard.turn_started_at.is_none());
    }

    #[tokio::test]
    async fn test_model_error_without_payload_still_settles_the_turn() {
        let mut st = crate::app::test_state();
        st.model.status = TurnStatus::Responding;
        st.model.edits_active = true;
        st.model.files.push(StreamingFileEdit {
            status: FileEditStatus::Running,
            ..accepted_file("crates/tui/src/main.rs")
        });
        st.turn_started_at = Some(std::time::Instant::now());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = true;
        let ev = Event {
            method: events::MODEL_ERROR.to_string(),
            params: None,
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert_eq!(guard.model.status, TurnStatus::Error);
        assert!(
            !guard.model.is_busy(),
            "a failed turn must never keep the interface busy"
        );
        assert!(guard.turn_started_at.is_none());
        assert!(!is_reasoning);
        assert_eq!(
            guard.model.files[0].status,
            FileEditStatus::Error,
            "the streamed edits are reconciled even without a payload"
        );
        assert_eq!(
            guard.model.error.as_deref(),
            Some(active_t("notify.turn_failed_no_payload").as_str())
        );
    }

    #[tokio::test]
    async fn test_review_error_without_payload_still_settles_the_repl() {
        let mut st = crate::app::test_state();
        st.review.running = true;
        st.model.status = TurnStatus::Responding;
        st.turn_started_at = Some(std::time::Instant::now());

        let state = Arc::new(Mutex::new(st));
        let mut is_reasoning = false;
        let ev = Event {
            method: events::REVIEW_ERROR.to_string(),
            params: None,
        };

        handle_event(ev, &state, &mut is_reasoning).await;

        let guard = state.lock().await;
        assert!(!guard.review.running);
        assert!(guard.turn_started_at.is_none());
        assert_eq!(guard.model.status, TurnStatus::Error);
        assert!(!guard.model.is_busy());
        let fallback = active_t("notify.review_failed_no_payload");
        assert_eq!(guard.review.error.as_deref(), Some(fallback.as_str()));
        assert_eq!(guard.model.error.as_deref(), Some(fallback.as_str()));
    }
}
