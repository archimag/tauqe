use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::{methods, ReviewStartParams};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

use crate::app::{AppState, SelectionDialogKind, ViewMode};
use crate::input::{squash, InputResult};
use crate::rpc::send_request;
use crate::ui::develop::DevelopView;
use crate::ui::history::HistoryViewState;

mod confirm;
mod discuss;
mod prompt;
mod refine;
mod selection;
mod status;

use confirm::{resolve_confirm, ConfirmOutcome};
pub(crate) use selection::activate_selection_item;
pub(crate) use status::apply_status_dialog;

/// Resets the Develop view and marks the start of a new autonomous turn. Shared by
/// every dialog that launches a model request (prompt, discuss, refine, plan and
/// review execution) so the state transition stays consistent.
fn begin_develop_turn(st: &mut AppState) {
    st.view_mode = ViewMode::Develop;
    st.model.reasoning.clear();
    st.model.text.clear();
    st.model.markdown_lines.clear();
    st.model.error = None;
    st.model.result = None;
    st.model.usage = None;
    st.model.scroll = 0;
    st.model.status = "awaiting".to_string();
    st.model.auto_scroll = true;
    st.model.current_cost = Some(0.0);
    st.model.edits_active = false;
    st.model.files.clear();
    st.model.selected_file_index = 0;
    st.model.edit_final_applied = None;
    st.model.edit_final_error = None;
    st.model.last_commit_hash = None;
    st.model.last_commit_summary = None;
    st.model.toolchain_command = None;
    st.model.toolchain_status = None;
    st.model.copy_flash = None;
    st.model.code_blocks.clear();
    st.turn_started_at = Some(std::time::Instant::now());
}

/// The plan view is refreshed from server responses, so nothing is cleared locally
/// until the server confirms the deletion.
pub(crate) async fn delete_plan(server_writer: &mut ChildStdin, plan_id: String) -> anyhow::Result<()> {
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

pub(crate) async fn delete_review(server_writer: &mut ChildStdin, review_id: String) -> anyhow::Result<()> {
    send_request(
        server_writer,
        methods::REVIEW_DELETE,
        serde_json::json!({ "id": review_id }),
    )
    .await?;
    send_request(server_writer, methods::REVIEW_LIST, serde_json::json!({})).await?;
    Ok(())
}

pub async fn handle_dialog_event(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<Option<InputResult>> {
    let mut st = state.lock().await;

    // 1. Squash dialog modal
    if st.squash_dialog.is_some() {
        return squash::handle_squash_key(key, st, server_writer)
            .await
            .map(Some);
    }

    // 1b. Review launch dialog
    if let Some(mut dialog) = st.review_dialog.take() {
        let primary = st.tui_config.input.primary_modifier;
        let is_ctrl_alt = key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT);
        let is_cmd = !is_ctrl_alt
            && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));

        match key.code {
            KeyCode::Esc => {
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) && !key.modifiers.contains(KeyModifiers::ALT) => {
                let prompt = dialog.prompt_editor.get_text().trim().to_string();
                let params = ReviewStartParams {
                    model: dialog.models.get(dialog.model_index).cloned(),
                    user_prompt: if prompt.is_empty() { None } else { Some(prompt) },
                };
                st.review.begin();
                drop(st);
                send_request(
                    server_writer,
                    methods::REVIEW_START,
                    serde_json::to_value(params)?,
                )
                .await?;
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Tab => {
                let count = dialog.models.len();
                if count > 0 {
                    dialog.model_index = (dialog.model_index + 1) % count;
                }
                st.review_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::BackTab => {
                let count = dialog.models.len();
                if count > 0 {
                    dialog.model_index = (dialog.model_index + count - 1) % count;
                }
                st.review_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Up if dialog.prompt_editor.is_empty() => {
                let count = dialog.models.len();
                if count > 0 {
                    dialog.model_index = (dialog.model_index + count - 1) % count;
                }
                st.review_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Down if dialog.prompt_editor.is_empty() => {
                let count = dialog.models.len();
                if count > 0 {
                    dialog.model_index = (dialog.model_index + 1) % count;
                }
                st.review_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
            _ => {
                crate::input::handle_editor_key(
                    &mut dialog.prompt_editor,
                    key,
                    is_cmd,
                    true,
                );
                st.review_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
        }
    }

    // 2. Cancel confirmation
    if st.confirm_cancel {
        match resolve_confirm(&mut st, key) {
            ConfirmOutcome::Toggled => {}
            ConfirmOutcome::Dismissed => st.confirm_cancel = false,
            ConfirmOutcome::Accepted => {
                st.confirm_cancel = false;
                let cancel_review = st.review.running;
                let cancel_model = st.model.is_busy();
                drop(st);
                if cancel_review {
                    send_request(server_writer, methods::REVIEW_CANCEL, serde_json::json!({}))
                        .await?;
                }
                if cancel_model || !cancel_review {
                    send_request(server_writer, methods::MODEL_CANCEL, serde_json::json!({}))
                        .await?;
                }
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 2a. Quit confirmation
    if st.confirm_quit {
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

    // 2b. Context clear auto confirmation (success is reported on the server response)
    if st.context_view.confirm_clear_auto {
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

    // 3. Undo confirmation
    if st.confirm_undo {
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

    // 4a. Delete plan confirmation
    if let Some(plan_id) = st.confirm_delete_plan.take() {
        match resolve_confirm(&mut st, key) {
            ConfirmOutcome::Toggled => st.confirm_delete_plan = Some(plan_id),
            ConfirmOutcome::Dismissed => {}
            ConfirmOutcome::Accepted => {
                drop(st);
                delete_plan(server_writer, plan_id).await?;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 4a-review-delete. Delete review confirmation
    if let Some(review_id) = st.confirm_delete_review.take() {
        match resolve_confirm(&mut st, key) {
            ConfirmOutcome::Toggled => st.confirm_delete_review = Some(review_id),
            ConfirmOutcome::Dismissed => {}
            ConfirmOutcome::Accepted => {
                drop(st);
                delete_review(server_writer, review_id).await?;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 4a-review-execute. Execute review scope confirmation (single item or session batch)
    if let Some(target) = st.confirm_execute_review.take() {
        match resolve_confirm(&mut st, key) {
            ConfirmOutcome::Toggled => st.confirm_execute_review = Some(target),
            ConfirmOutcome::Dismissed => {}
            ConfirmOutcome::Accepted => {
                begin_develop_turn(&mut st);
                drop(st);
                let params = tauqe_protocol::ReviewExecuteParams {
                    review_id: target.review_id,
                    item_id: target.item_id,
                };
                send_request(
                    server_writer,
                    methods::REVIEW_EXECUTE,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 4a-2. Execute plan scope confirmation (single step, group, or entire plan)
    if let Some(target) = st.confirm_execute_scope.take() {
        match resolve_confirm(&mut st, key) {
            ConfirmOutcome::Toggled => st.confirm_execute_scope = Some(target),
            ConfirmOutcome::Dismissed => {}
            ConfirmOutcome::Accepted => {
                begin_develop_turn(&mut st);
                drop(st);
                let params = tauqe_protocol::PlanExecuteParams {
                    plan_id: target.plan_id,
                    step_id: target.step_id,
                };
                send_request(
                    server_writer,
                    methods::PLAN_EXECUTE,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 4a-3. Discuss plan dialog
    if st.discuss_plan_dialog.is_some() {
        return discuss::handle_discuss_plan(key, st, server_writer).await;
    }

    // 4a-3b. Plan refine dialog
    if st.plans_view.refine_dialog.is_some() {
        return refine::handle_refine_dialog(key, st, server_writer).await;
    }

    // 4a-3c. Plan refine blocked modal
    if st.plans_view.refine_blocked.is_some() {
        return refine::handle_refine_blocked(key, st);
    }

    // 4a-4. Prompt modal dialog (unified discussion)
    if st.prompt_modal.is_some() {
        return prompt::handle_prompt_modal(key, st, server_writer).await;
    }

    // 4. Clear history confirmation
    if st.confirm_clear_history {
        match resolve_confirm(&mut st, key) {
            ConfirmOutcome::Toggled => {}
            ConfirmOutcome::Dismissed => st.confirm_clear_history = false,
            ConfirmOutcome::Accepted => {
                st.confirm_clear_history = false;
                let cost = st.model.session_total_cost;
                let prev_cost = st.model.prev_cost;
                st.model = DevelopView {
                    session_total_cost: cost,
                    prev_cost,
                    ..Default::default()
                };
                st.history_view = HistoryViewState::default();
                drop(st);
                send_request(
                    server_writer,
                    methods::MODEL_CLEAR_HISTORY,
                    serde_json::json!({}),
                )
                .await?;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 4b. Status dialog (for Plan and Review items)
    if let Some(mut dialog) = st.status_dialog.take() {
        let total_options = match &dialog.target {
            crate::app::StatusDialogTarget::PlanItem { .. } => 6,
            crate::app::StatusDialogTarget::ReviewItem { .. } => 5,
        };

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('p') => {
                dialog.selected_index = dialog.selected_index.saturating_sub(1);
                st.status_dialog = Some(dialog);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('n') => {
                if dialog.selected_index + 1 < total_options {
                    dialog.selected_index += 1;
                }
                st.status_dialog = Some(dialog);
            }
            KeyCode::Home => {
                dialog.selected_index = 0;
                st.status_dialog = Some(dialog);
            }
            KeyCode::End => {
                dialog.selected_index = total_options - 1;
                st.status_dialog = Some(dialog);
            }
            KeyCode::Char('1') => {
                st.status_dialog = Some(dialog);
                apply_status_dialog(&mut st, 0, server_writer).await?;
            }
            KeyCode::Char('2') => {
                st.status_dialog = Some(dialog);
                apply_status_dialog(&mut st, 1, server_writer).await?;
            }
            KeyCode::Char('3') => {
                st.status_dialog = Some(dialog);
                apply_status_dialog(&mut st, 2, server_writer).await?;
            }
            KeyCode::Char('4') if total_options >= 4 => {
                st.status_dialog = Some(dialog);
                apply_status_dialog(&mut st, 3, server_writer).await?;
            }
            KeyCode::Char('5') if total_options >= 5 => {
                st.status_dialog = Some(dialog);
                apply_status_dialog(&mut st, 4, server_writer).await?;
            }
            KeyCode::Char('6') if total_options >= 6 => {
                st.status_dialog = Some(dialog);
                apply_status_dialog(&mut st, 5, server_writer).await?;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let chosen = dialog.selected_index;
                st.status_dialog = Some(dialog);
                apply_status_dialog(&mut st, chosen, server_writer).await?;
            }
            _ => {
                st.status_dialog = Some(dialog);
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 5. Selection dialog
    if let Some(mut dialog) = st.selection_dialog.take() {
        let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
        let area = crate::ui::selection_dialog_area(ratatui::layout::Rect::new(0, 0, term_w, term_h), dialog.items.len());
        let visible_height = crate::ui::dialogs::selection_dialog_visible_height(area);

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('p') => {
                dialog.select_prev(visible_height);
                st.selection_dialog = Some(dialog);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('n') => {
                dialog.select_next(visible_height);
                st.selection_dialog = Some(dialog);
            }
            KeyCode::PageUp => {
                for _ in 0..visible_height {
                    dialog.select_prev(visible_height);
                }
                st.selection_dialog = Some(dialog);
            }
            KeyCode::PageDown => {
                for _ in 0..visible_height {
                    dialog.select_next(visible_height);
                }
                st.selection_dialog = Some(dialog);
            }
            KeyCode::Home => {
                dialog.selected_index = 0;
                dialog.ensure_visible(visible_height);
                st.selection_dialog = Some(dialog);
            }
            KeyCode::End => {
                if !dialog.items.is_empty() {
                    dialog.selected_index = dialog.items.len() - 1;
                    dialog.ensure_visible(visible_height);
                }
                st.selection_dialog = Some(dialog);
            }
            KeyCode::Enter => {
                if let Some(item) = dialog.items.get(dialog.selected_index).cloned() {
                    activate_selection_item(st, item, server_writer).await?;
                }
            }
            KeyCode::Char(c @ '1'..='6') if dialog.kind == SelectionDialogKind::ModelRoles => {
                if let Some(item) = dialog.items.get(c as usize - '1' as usize).cloned() {
                    activate_selection_item(st, item, server_writer).await?;
                }
            }
            _ => {
                st.selection_dialog = Some(dialog);
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 6. Help popup
    if st.show_help {
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

    Ok(None)
}
