use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::{methods, DiscussionFocus};
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{ActiveModal, AppState, BatchCancelButton, ViewMode};
use crate::input::{squash, InputResult};
use crate::rpc::{send_request, RpcConnection};
use crate::views::develop::{DevelopView, TurnStatus};
use crate::views::history::HistoryViewState;

mod confirm;
mod prompt;
mod refine;
mod selection;
mod status;

use confirm::{resolve_batch_cancel, resolve_confirm, BatchCancelOutcome, ConfirmOutcome};
pub(crate) use prompt::submit_prompt_modal;
pub(crate) use refine::{submit_refine_blocked, submit_refine_dialog};
pub(crate) use selection::activate_selection_item;
pub(crate) use status::apply_status_dialog;

/// Resets the Develop view and marks the start of a new autonomous turn. Shared by
/// every dialog that launches a model request (prompt, discuss, refine, plan and
/// review execution) so the state transition stays consistent.
fn begin_develop_turn(st: &mut AppState) {
    st.view_mode = ViewMode::Develop;
    // A freshly launched turn starts from a clean batch-cancellation state.
    st.batch_stopping = false;
    // Executing code hands control to the Coder role: detach the discussion
    // focus locally so no anchor survives into the new turn.
    st.discussion_focus = DiscussionFocus::None;
    st.model.reasoning.clear();
    st.model.text.clear();
    st.model.markdown_lines.clear();
    st.model.error = None;
    st.model.result = None;
    st.model.usage = None;
    st.model.scroll = 0;
    st.model.status = TurnStatus::Awaiting;
    st.model.auto_scroll = true;
    st.model.current_cost = Some(0.0);
    st.model.edits_active = false;
    st.model.files.clear();
    st.model.selected_file_index = 0;
    st.model.edit_final_applied = None;
    st.model.edit_final_error = None;
    st.model.last_commit_hash = None;
    st.model.last_commit_summary = None;
    st.model.toolchain = None;
    st.model.copy_flash = None;
    st.model.code_blocks.clear();
    st.turn_started_at = Some(std::time::Instant::now());
}

/// Switches the active view to the REPL and announces a review-execution turn.
///
/// Executing review findings runs the Coder role through a full FSM turn with
/// file edits, Git checkpoints, and toolchain verification. Surfacing the REPL
/// keeps that batch fixing fully transparent.
fn begin_review_execution_turn(st: &mut AppState) {
    begin_develop_turn(st);
    st.model.is_batch = true;
    st.notify_info("Executing review finding(s): switched to REPL for coder execution");
}

/// Snapshot of the optimistic UI state applied when a turn is launched.
///
/// It is captured before the local transition runs and restored if the request
/// cannot be written to the server, so a failed launch never leaves the Develop
/// view or a Review session stuck in a busy state no event can clear.
struct TurnLaunchSnapshot {
    develop_status: TurnStatus,
    turn_started_at: Option<std::time::Instant>,
    review_running: bool,
}

impl TurnLaunchSnapshot {
    fn capture(st: &AppState) -> Self {
        Self {
            develop_status: st.model.status,
            turn_started_at: st.turn_started_at,
            review_running: st.review.running,
        }
    }

    fn restore(self, st: &mut AppState) {
        st.model.status = self.develop_status;
        st.turn_started_at = self.turn_started_at;
        st.review.running = self.review_running;
    }
}

/// Launches an autonomous turn: captures a rollback snapshot, applies the local
/// start transition, dispatches the request, and restores the snapshot with an
/// error notification if the server write fails.
async fn start_turn(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
    method: &str,
    params: serde_json::Value,
    begin: impl FnOnce(&mut AppState),
) {
    let snapshot = TurnLaunchSnapshot::capture(&st);
    begin(&mut st);
    drop(st);

    if let Err(err) = send_request(server_writer, method, params).await {
        let mut st = state.lock().await;
        snapshot.restore(&mut st);
        st.notify_error(format!("Failed to start turn: {}", err));
    }
}

/// Applies the shared start transition for a plan-step batch execution.
///
/// It is the plan-step peer of [`begin_review_execution_turn`]: the Coder role
/// takes over the REPL and `is_batch` marks the turn as part of a batch, so the
/// three-way batch-cancel dialog and per-step notification suppression stay
/// active.
fn begin_scope_execution_turn(st: &mut AppState) {
    begin_develop_turn(st);
    st.model.is_batch = true;
}

/// Launches a review-finding batch execution from a developer confirmation.
///
/// Shared by the keyboard and mouse confirmation paths so a mouse click starts
/// the exact same batch turn as `Enter`: the Coder role takes over, `is_batch`
/// enables the three-way batch-cancel dialog, the discussion focus detaches, and
/// a rollback snapshot protects the launch.
pub(crate) async fn confirm_execute_review(
    state: &Arc<Mutex<AppState>>,
    st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
    target: crate::app::ConfirmExecuteReviewState,
) -> anyhow::Result<()> {
    let params = tauqe_protocol::ReviewExecuteParams {
        review_id: target.review_id,
        item_id: target.item_id,
    };
    let params = serde_json::to_value(params)?;
    start_turn(
        state,
        st,
        server_writer,
        methods::REVIEW_EXECUTE,
        params,
        begin_review_execution_turn,
    )
    .await;
    Ok(())
}

/// Launches a plan-step batch execution from a developer confirmation.
///
/// The plan-step peer of [`confirm_execute_review`], shared by the keyboard and
/// mouse confirmation paths.
pub(crate) async fn confirm_execute_scope(
    state: &Arc<Mutex<AppState>>,
    st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
    target: crate::app::ConfirmExecuteScopeState,
) -> anyhow::Result<()> {
    let params = tauqe_protocol::PlanExecuteParams {
        plan_id: target.plan_id,
        step_id: target.step_id,
    };
    let params = serde_json::to_value(params)?;
    start_turn(
        state,
        st,
        server_writer,
        methods::PLAN_EXECUTE,
        params,
        begin_scope_execution_turn,
    )
    .await;
    Ok(())
}

/// Starts a review session from the review dialog.
///
/// Shared by the keyboard and mouse paths so a failed request rolls the
/// optimistic UI state back identically on both.
pub(crate) async fn confirm_start_review(
    state: &Arc<Mutex<AppState>>,
    st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
    params: serde_json::Value,
) -> anyhow::Result<()> {
    start_turn(
        state,
        st,
        server_writer,
        methods::REVIEW_START,
        params,
        |st| st.review.begin(),
    )
    .await;
    Ok(())
}

/// Clears the conversation history.
///
/// Preserves the running session cost and re-syncs the theme so the reset model
/// and history views keep the active theme mode. Shared by the keyboard and
/// mouse confirmation paths.
pub(crate) async fn confirm_clear_history(
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    let cost = st.model.session_total_cost;
    let prev_cost = st.model.prev_cost;
    st.model = DevelopView {
        session_total_cost: cost,
        prev_cost,
        ..Default::default()
    };
    st.history_view = HistoryViewState::default();
    st.sync_theme_mode();
    drop(st);
    send_request(
        server_writer,
        methods::MODEL_CLEAR_HISTORY,
        serde_json::json!({}),
    )
    .await?;
    Ok(())
}

/// The plan view is refreshed from server responses, so nothing is cleared locally
/// until the server confirms the deletion.
pub(crate) async fn delete_plan(server_writer: &mut RpcConnection, plan_id: String) -> anyhow::Result<()> {
    send_request(
        server_writer,
        methods::PLAN_DELETE,
        serde_json::json!({ "id": plan_id }),
    )
    .await?;
    send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
    send_request(server_writer, methods::PLAN_GET, serde_json::json!({})).await?;
    Ok(())
}

pub(crate) async fn delete_review(server_writer: &mut RpcConnection, review_id: String) -> anyhow::Result<()> {
    send_request(
        server_writer,
        methods::REVIEW_DELETE,
        serde_json::json!({ "id": review_id }),
    )
    .await?;
    send_request(server_writer, methods::REVIEW_LIST, serde_json::json!({})).await?;
    Ok(())
}

/// Applies the action chosen in the batch-cancellation dialog.
///
/// `Continue` keeps the batch running. `Soft Stop` asks the supervisor to
/// finish the current member and stop before the next one. `Hard Stop` aborts
/// the running operation immediately. The dialog always closes and returns to
/// its safe default, so the next interruption starts from `Continue`.
pub(crate) async fn apply_batch_cancel(
    st: &mut AppState,
    choice: BatchCancelButton,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    st.confirm_batch_cancel = false;
    st.batch_cancel_button = BatchCancelButton::Continue;

    match choice {
        BatchCancelButton::Continue => {}
        BatchCancelButton::SoftStop => {
            st.batch_stopping = true;
            // A soft stop is advisory: the operation stays registered so the
            // supervisor can finish the current member and unwind on its own.
            let params = tauqe_protocol::model::ModelCancelParams {
                mode: Some(tauqe_protocol::model::CancelMode::Soft),
            };
            send_request(
                server_writer,
                methods::MODEL_CANCEL,
                serde_json::to_value(params)?,
            )
            .await?;
        }
        BatchCancelButton::HardStop => {
            st.batch_stopping = false;
            let cancel_review = st.review.running;
            let cancel_model = st.model.is_busy();
            if cancel_review {
                let params = tauqe_protocol::ReviewCancelParams {
                    mode: Some(tauqe_protocol::model::CancelMode::Hard),
                };
                send_request(
                    server_writer,
                    methods::REVIEW_CANCEL,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
            if cancel_model || !cancel_review {
                let params = tauqe_protocol::model::ModelCancelParams {
                    mode: Some(tauqe_protocol::model::CancelMode::Hard),
                };
                send_request(
                    server_writer,
                    methods::MODEL_CANCEL,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
    }
    Ok(())
}

pub(crate) async fn handle_dialog_event(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut st = state.lock().await;

    // Dispatch strictly through the canonical modal ordering so the surface that
    // renders is always the surface that receives keystrokes.
    match st.active_modal() {
        Some(ActiveModal::Squash) => {
            return squash::handle_squash_key(key, st, server_writer)
                .await
                .map(Some);
        }
        Some(ActiveModal::Review) => {
            if let Some(mut dialog) = st.review_dialog.take() {
                let primary = st.tui_config.input.primary_modifier;
                let visible_rows = crate::dialogs::render::review_dialog_geometry(
                    st.last_frame_area,
                    dialog.selector.len(),
                )
                .model_rows
                .height as usize;

                // The C-M / Alt+M chord always switches the reviewer; the arrows
                // browse the list only while the list owns focus, so list
                // navigation and text editing never fight over the same keys
                // (c-magit-key-navigation).
                if dialog
                    .selector
                    .handle_key(key, primary, !dialog.focus.is_model_list())
                {
                    st.review_dialog = Some(dialog);
                    return Ok(Some(InputResult::Continue));
                }

                // Bare digits, arrows, and paging act on the model list only
                // while it owns focus; elsewhere the editor keeps those keys.
                if dialog.focus.is_model_list()
                    && dialog.selector.handle_list_key(key, visible_rows)
                {
                    st.review_dialog = Some(dialog);
                    return Ok(Some(InputResult::Continue));
                }

                let is_ctrl_alt = key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::ALT);

                // Alt+digit picks a reviewer by position without moving the
                // cursor out of the instruction editor (c-magit-key-navigation).
                if dialog.focus.is_prompt_editor()
                    && !is_ctrl_alt
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::ALT)
                {
                    if let KeyCode::Char(c @ '1'..='9') = key.code {
                        let index = c as usize - '1' as usize;
                        if index < dialog.selector.len() {
                            dialog.selector.select_index(index);
                            st.review_dialog = Some(dialog);
                            return Ok(Some(InputResult::Continue));
                        }
                    }
                }

                let is_cmd = !is_ctrl_alt
                    && (primary.matches(key.modifiers)
                        || key.modifiers.contains(KeyModifiers::CONTROL));
                let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let has_alt = key.modifiers.contains(KeyModifiers::ALT);

                // C-Enter accelerator unconditionally launches review from any section.
                if matches!(key.code, KeyCode::Enter) && is_cmd {
                    let params = serde_json::to_value(dialog.start_params())?;
                    confirm_start_review(state, st, server_writer, params).await?;
                    return Ok(Some(InputResult::Continue));
                }

                match key.code {
                    // Esc always dismisses the dialog (safe cancellation).
                    KeyCode::Esc => {}
                    KeyCode::Char('q') if dialog.focus.is_actions() => {}
                    KeyCode::Tab => {
                        if dialog.focus.is_actions()
                            && dialog.confirm_button == crate::app::ConfirmDialogButton::Cancel
                        {
                            dialog.confirm_button = crate::app::ConfirmDialogButton::Confirm;
                        } else {
                            if dialog.focus.is_actions() {
                                dialog.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            dialog.focus.cycle_next(dialog.selector.len());
                        }
                        st.review_dialog = Some(dialog);
                    }
                    KeyCode::BackTab => {
                        if dialog.focus.is_actions()
                            && dialog.confirm_button == crate::app::ConfirmDialogButton::Confirm
                        {
                            dialog.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        } else {
                            dialog.focus.cycle_prev(dialog.selector.len());
                            if dialog.focus.is_actions() {
                                dialog.confirm_button = crate::app::ConfirmDialogButton::Confirm;
                            }
                        }
                        st.review_dialog = Some(dialog);
                    }
                    // Horizontal stepping moves between the action buttons.
                    KeyCode::Left | KeyCode::Right if dialog.focus.is_actions() => {
                        dialog.confirm_button = match dialog.confirm_button {
                            crate::app::ConfirmDialogButton::Cancel => {
                                crate::app::ConfirmDialogButton::Confirm
                            }
                            crate::app::ConfirmDialogButton::Confirm => {
                                crate::app::ConfirmDialogButton::Cancel
                            }
                        };
                        st.review_dialog = Some(dialog);
                    }
                    KeyCode::Enter | KeyCode::Char(' ')
                        if !has_shift && !has_alt && dialog.focus.is_actions() =>
                    {
                        // Starting a review is a long-running operation: it
                        // begins only from the focused `Confirm` button. The safe
                        // `Cancel` default dismisses the dialog (Conventions §7).
                        if dialog.confirm_button == crate::app::ConfirmDialogButton::Confirm {
                            let params = serde_json::to_value(dialog.start_params())?;
                            confirm_start_review(state, st, server_writer, params).await?;
                            return Ok(Some(InputResult::Continue));
                        }
                        // Safe cancel dismisses without re-attaching the dialog.
                    }
                    _ => {
                        if dialog.focus.is_prompt_editor() {
                            crate::input::handle_editor_key(
                                &mut dialog.prompt_editor,
                                key,
                                is_cmd,
                                true,
                            );
                        } else if dialog.focus.is_path_editor() {
                            // The target path is a single-line field: newline
                            // insertion and vertical movement stay disabled.
                            crate::input::handle_editor_key(
                                &mut dialog.path_editor,
                                key,
                                is_cmd,
                                false,
                            );
                        }
                        st.review_dialog = Some(dialog);
                    }
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::ConfirmCancel) => {
            match resolve_confirm(&mut st, key) {
                ConfirmOutcome::Toggled => {}
                ConfirmOutcome::Dismissed => st.confirm_cancel = false,
                ConfirmOutcome::Accepted => {
                    st.confirm_cancel = false;
                    let cancel_review = st.review.running;
                    let cancel_model = st.model.is_busy();
                    drop(st);
                    if cancel_review {
                        send_request(
                            server_writer,
                            methods::REVIEW_CANCEL,
                            serde_json::json!({}),
                        )
                        .await?;
                    }
                    if cancel_model || !cancel_review {
                        send_request(
                            server_writer,
                            methods::MODEL_CANCEL,
                            serde_json::json!({}),
                        )
                        .await?;
                    }
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::ConfirmQuit) => {
            match resolve_confirm(&mut st, key) {
                ConfirmOutcome::Toggled => {}
                ConfirmOutcome::Dismissed => st.confirm_quit = false,
                ConfirmOutcome::Accepted => {
                    st.confirm_quit = false;
                    return Ok(Some(InputResult::Exit));
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::BatchCancel) => {
            match resolve_batch_cancel(&mut st, key) {
                BatchCancelOutcome::Toggled => {}
                BatchCancelOutcome::Dismissed => {
                    st.confirm_batch_cancel = false;
                    st.batch_cancel_button = BatchCancelButton::Continue;
                }
                BatchCancelOutcome::Chosen(choice) => {
                    apply_batch_cancel(&mut st, choice, server_writer).await?;
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::ClearAuto) => {
            match resolve_confirm(&mut st, key) {
                ConfirmOutcome::Toggled => {}
                ConfirmOutcome::Dismissed => st.context_view.confirm_clear_auto = false,
                ConfirmOutcome::Accepted => {
                    st.context_view.confirm_clear_auto = false;
                    drop(st);
                    let params = tauqe_protocol::ContextClearParams {
                        layer: Some(tauqe_protocol::ContextLayer::Auto),
                    };
                    send_request(
                        server_writer,
                        methods::CONTEXT_CLEAR,
                        serde_json::to_value(params)?,
                    )
                    .await?;
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::ClearUser) => {
            match resolve_confirm(&mut st, key) {
                ConfirmOutcome::Toggled => {}
                ConfirmOutcome::Dismissed => st.context_view.confirm_clear_user = false,
                ConfirmOutcome::Accepted => {
                    st.context_view.confirm_clear_user = false;
                    drop(st);
                    let params = tauqe_protocol::ContextClearParams {
                        layer: Some(tauqe_protocol::ContextLayer::User),
                    };
                    send_request(
                        server_writer,
                        methods::CONTEXT_CLEAR,
                        serde_json::to_value(params)?,
                    )
                    .await?;
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::ConfirmUndo) => {
            match resolve_confirm(&mut st, key) {
                ConfirmOutcome::Toggled => {}
                ConfirmOutcome::Dismissed => st.confirm_undo = false,
                ConfirmOutcome::Accepted => {
                    st.confirm_undo = false;
                    drop(st);
                    send_request(server_writer, methods::GIT_UNDO, serde_json::json!({})).await?;
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::ConfirmDetachFocus) => {
            match resolve_confirm(&mut st, key) {
                ConfirmOutcome::Toggled => {}
                ConfirmOutcome::Dismissed => st.confirm_detach_focus = false,
                ConfirmOutcome::Accepted => {
                    st.confirm_detach_focus = false;
                    st.discussion_focus = DiscussionFocus::None;
                    drop(st);
                    send_request(
                        server_writer,
                        methods::DISCUSSION_CLEAR_FOCUS,
                        serde_json::json!({}),
                    )
                    .await?;
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::DeletePlan) => {
            if let Some(plan_id) = st.confirm_delete_plan.take() {
                match resolve_confirm(&mut st, key) {
                    ConfirmOutcome::Toggled => st.confirm_delete_plan = Some(plan_id),
                    ConfirmOutcome::Dismissed => {}
                    ConfirmOutcome::Accepted => {
                        drop(st);
                        delete_plan(server_writer, plan_id).await?;
                    }
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::DeleteReview) => {
            if let Some(review_id) = st.confirm_delete_review.take() {
                match resolve_confirm(&mut st, key) {
                    ConfirmOutcome::Toggled => st.confirm_delete_review = Some(review_id),
                    ConfirmOutcome::Dismissed => {}
                    ConfirmOutcome::Accepted => {
                        drop(st);
                        delete_review(server_writer, review_id).await?;
                    }
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::ExecuteReview) => {
            if let Some(target) = st.confirm_execute_review.take() {
                match resolve_confirm(&mut st, key) {
                    ConfirmOutcome::Toggled => st.confirm_execute_review = Some(target),
                    ConfirmOutcome::Dismissed => {}
                    ConfirmOutcome::Accepted => {
                        confirm_execute_review(state, st, server_writer, target).await?;
                    }
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::ExecuteScope) => {
            if let Some(target) = st.confirm_execute_scope.take() {
                match resolve_confirm(&mut st, key) {
                    ConfirmOutcome::Toggled => st.confirm_execute_scope = Some(target),
                    ConfirmOutcome::Dismissed => {}
                    ConfirmOutcome::Accepted => {
                        confirm_execute_scope(state, st, server_writer, target).await?;
                    }
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::RefineDialog) => {
            return refine::handle_refine_dialog(key, st, server_writer).await;
        }
        Some(ActiveModal::RefineBlocked) => {
            return refine::handle_refine_blocked(key, st, server_writer).await;
        }
        Some(ActiveModal::Prompt) => {
            return prompt::handle_prompt_modal(key, st, server_writer).await;
        }
        Some(ActiveModal::ClearHistory) => {
            match resolve_confirm(&mut st, key) {
                ConfirmOutcome::Toggled => {}
                ConfirmOutcome::Dismissed => st.confirm_clear_history = false,
                ConfirmOutcome::Accepted => {
                    st.confirm_clear_history = false;
                    confirm_clear_history(st, server_writer).await?;
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::Status) => {
            if let Some(mut dialog) = st.status_dialog.take() {
                let total_options = dialog.target.options_count();

                if dialog.selector.handle_nav_key(key, total_options) {
                    st.status_dialog = Some(dialog);
                    return Ok(Some(InputResult::Continue));
                }

                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {}
                    KeyCode::Char(c @ '1'..='6') => {
                        let chosen = c as usize - '1' as usize;
                        st.status_dialog = Some(dialog);
                        if chosen < total_options {
                            apply_status_dialog(&mut st, chosen, server_writer).await?;
                        }
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        let chosen = dialog.selector.selected_index();
                        st.status_dialog = Some(dialog);
                        apply_status_dialog(&mut st, chosen, server_writer).await?;
                    }
                    _ => {
                        st.status_dialog = Some(dialog);
                    }
                }
            }
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::Selection) => {
            return selection::handle_selection_dialog_key(key, st, server_writer).await;
        }
        Some(ActiveModal::ThemeSelector) => {
            let visible =
                crate::theme::selector::theme_selector_visible_height(st.last_frame_area);
            crate::theme::selector::handle_theme_selector_key(key, &mut st, visible);
            return Ok(Some(InputResult::Continue));
        }
        Some(ActiveModal::Help) => {
            match key.code {
                KeyCode::F(1) | KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                    st.show_help = false;
                    st.help_scroll = 0;
                }
                KeyCode::Char('h') | KeyCode::Char('H')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    st.show_help = false;
                    st.help_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('n') => {
                    st.help_scroll = st.help_scroll.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('p') => {
                    st.help_scroll = st.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    st.help_scroll = st.help_scroll.saturating_add(10);
                }
                KeyCode::PageUp => {
                    st.help_scroll = st.help_scroll.saturating_sub(10);
                }
                KeyCode::Home => {
                    st.help_scroll = 0;
                }
                _ => {}
            }
            return Ok(Some(InputResult::Continue));
        }
        // `server_disconnected` is handled before dialog dispatch, so it never
        // reaches here; `None` means no modal is active.
        Some(ActiveModal::ServerDisconnected) | None => {}
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSeverity};

    use super::*;
    use crate::app::{ConfirmDialogButton, ConfirmExecuteReviewState};

    fn review_item(id: u32) -> ReviewItem {
        ReviewItem {
            id,
            title: format!("Finding {}", id),
            model: None,
            severity: ReviewSeverity::Warning,
            status: ReviewItemStatus::Todo,
            files: Vec::new(),
            subplan: Vec::new(),
            resolution_notes: None,
            body: String::new(),
        }
    }

    fn execute_review_target() -> ConfirmExecuteReviewState {
        ConfirmExecuteReviewState {
            review_id: Some("rev-1".to_string()),
            review_title: "Session rev-1".to_string(),
            scope_title: "All remaining TODO findings".to_string(),
            item_id: Some(1),
            items: vec![review_item(1)],
        }
    }

    /// Drives the ExecuteReview confirmation through `handle_dialog_event` and
    /// returns the resulting view mode, the active notification text, and
    /// whether a confirmation is still pending.
    async fn launch_execute_review(key: KeyEvent) -> (ViewMode, Option<String>, bool) {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.confirm_execute_review = Some(execute_review_target());
        st.confirm_button = ConfirmDialogButton::Confirm;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_dialog_event(key, &state, &mut conn)
            .await
            .expect("in-memory dispatch never fails");

        let guard = state.lock().await;
        let notification = guard.active_notification().map(|n| n.text.clone());
        (
            guard.view_mode,
            notification,
            guard.confirm_execute_review.is_some(),
        )
    }

    #[tokio::test]
    async fn confirm_button_switches_to_repl_and_notifies() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let (view, notification, pending) = launch_execute_review(key).await;

        assert_eq!(view, ViewMode::Develop);
        assert_eq!(
            notification.as_deref(),
            Some("Executing review finding(s): switched to REPL for coder execution")
        );
        assert!(!pending, "confirmation state must be consumed");
    }

    #[tokio::test]
    async fn confirm_accelerator_switches_to_repl_and_notifies() {
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        let (view, notification, pending) = launch_execute_review(key).await;

        assert_eq!(view, ViewMode::Develop);
        assert_eq!(
            notification.as_deref(),
            Some("Executing review finding(s): switched to REPL for coder execution")
        );
        assert!(!pending, "confirmation state must be consumed");
    }

    #[tokio::test]
    async fn review_dialog_path_editor_captures_typed_text() {
        use crate::app::{ReviewDialogFocus, ReviewDialogState};

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        let mut dialog = ReviewDialogState::new(1, 10, Vec::new());
        dialog.focus = ReviewDialogFocus::PathEditor;
        st.review_dialog = Some(dialog);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_dialog_event(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .expect("in-memory dispatch never fails");

        let guard = state.lock().await;
        let dialog = guard.review_dialog.as_ref().expect("dialog stays open");
        assert_eq!(dialog.path_editor.get_text(), "x");
        assert!(dialog.prompt_editor.is_empty());
    }
}
