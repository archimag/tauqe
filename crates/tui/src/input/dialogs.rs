use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::{methods, ConfigSetParams, ModelRef, ReviewStartParams};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::{is_char_typing, squash, InputResult};
use crate::rpc::{
    allocate_request_id, record_optimistic_rollback, send_request, send_request_with_id,
    OptimisticRollback,
};
use crate::ui::develop::DevelopView;
use crate::ui::history::HistoryViewState;

enum ConfirmOutcome {
    Toggled,
    Accepted,
    Dismissed,
}

/// Standard key handling for `[ Confirm ] / [ Cancel ]` modals (§7 Conventions):
/// arrows/Tab cycle buttons, `Enter`/`Space` activates the focused button,
/// `y`/`Y` confirms immediately, `n`/`N`/`Esc`/`q` dismisses safely.
fn resolve_confirm(st: &mut AppState, key: KeyEvent) -> ConfirmOutcome {
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
            st.confirm_button = match st.confirm_button {
                ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
                ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
            };
            ConfirmOutcome::Toggled
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let accepted = st.confirm_button == ConfirmDialogButton::Confirm;
            st.confirm_button = ConfirmDialogButton::Cancel;
            if accepted {
                ConfirmOutcome::Accepted
            } else {
                ConfirmOutcome::Dismissed
            }
        }
        KeyCode::Char('y') | KeyCode::Char('Y') if is_char_typing(key.modifiers) => {
            st.confirm_button = ConfirmDialogButton::Cancel;
            ConfirmOutcome::Accepted
        }
        KeyCode::Char('n') | KeyCode::Char('N') if is_char_typing(key.modifiers) => {
            st.confirm_button = ConfirmDialogButton::Cancel;
            ConfirmOutcome::Dismissed
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            st.confirm_button = ConfirmDialogButton::Cancel;
            ConfirmOutcome::Dismissed
        }
        _ => ConfirmOutcome::Toggled,
    }
}

/// Applies a model choice optimistically and registers a rollback so that a failed
/// `config/set` restores the previously active model (shared by keyboard and mouse).
pub(super) async fn apply_model_selection(
    mut st: MutexGuard<'_, AppState>,
    chosen: ModelRef,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<()> {
    let prev_model = std::mem::replace(&mut st.active_model, chosen.clone());
    st.selection_dialog = None;
    let req_id = allocate_request_id();
    record_optimistic_rollback(req_id, OptimisticRollback::ActiveModel { prev_model });
    drop(st);
    let params = ConfigSetParams {
        model: Some(chosen),
        ..Default::default()
    };
    send_request_with_id(
        server_writer,
        req_id,
        methods::CONFIG_SET,
        serde_json::to_value(params)?,
    )
    .await
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

pub(crate) async fn apply_status_dialog(
    st: &mut AppState,
    chosen_idx: usize,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<()> {
    let dialog = match st.status_dialog.take() {
        Some(d) => d,
        None => return Ok(()),
    };

    match dialog.target {
        crate::app::StatusDialogTarget::PlanItem {
            plan_id,
            item_id,
            current_status,
            ..
        } => {
            let statuses = [
                tauqe_protocol::PlanItemStatus::Todo,
                tauqe_protocol::PlanItemStatus::InProgress,
                tauqe_protocol::PlanItemStatus::Done,
                tauqe_protocol::PlanItemStatus::Cancelled,
            ];
            let new_status = match statuses.get(chosen_idx).copied() {
                Some(s) => s,
                None => return Ok(()),
            };

            if new_status != current_status {
                if let Some(plan) = st.plans_view.current_plan.as_mut() {
                    plan.update_item_status(&item_id, new_status);
                }
                let req_id = crate::rpc::allocate_request_id();
                crate::rpc::record_optimistic_rollback(
                    req_id,
                    crate::rpc::OptimisticRollback::PlanItemStatus {
                        plan_id: plan_id.clone(),
                        item_id: item_id.clone(),
                        prev_status: current_status,
                    },
                );
                let params = tauqe_protocol::PlanUpdateItemParams {
                    plan_id,
                    item_id,
                    status: Some(new_status),
                    checked: None,
                };
                crate::rpc::send_request_with_id(
                    server_writer,
                    req_id,
                    methods::PLAN_UPDATE_ITEM,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
        crate::app::StatusDialogTarget::ReviewItem {
            item_id,
            current_status,
            ..
        } => {
            let statuses = [
                tauqe_protocol::ReviewStatus::Todo,
                tauqe_protocol::ReviewStatus::Done,
                tauqe_protocol::ReviewStatus::Rejected,
            ];
            let new_status = match statuses.get(chosen_idx).copied() {
                Some(s) => s,
                None => return Ok(()),
            };

            if new_status != current_status {
                if let Some(session) = st.review.session.as_mut() {
                    if let Some(item) = session.items.iter_mut().find(|i| i.id == item_id) {
                        item.status = new_status;
                    }
                }
                if st.review.hide_closed {
                    st.review.clamp_selection();
                    st.review.scroll_to_selected();
                }
                let req_id = crate::rpc::allocate_request_id();
                crate::rpc::record_optimistic_rollback(
                    req_id,
                    crate::rpc::OptimisticRollback::ReviewItemStatus {
                        item_id,
                        prev_status: current_status,
                    },
                );
                let params = tauqe_protocol::ReviewUpdateItemParams {
                    item_id,
                    status: Some(new_status),
                    is_checked: None,
                };
                crate::rpc::send_request_with_id(
                    server_writer,
                    req_id,
                    methods::REVIEW_UPDATE_ITEM,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
    }
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
            KeyCode::Enter => {
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
                    false,
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
            crate::app::StatusDialogTarget::PlanItem { .. } => 4,
            crate::app::StatusDialogTarget::ReviewItem { .. } => 3,
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
                if let Some(chosen) = dialog.items.get(dialog.selected_index).cloned() {
                    apply_model_selection(st, chosen, server_writer).await?;
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
