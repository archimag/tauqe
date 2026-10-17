use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::{methods, DiscussionFocus, DiscussionRequest, DiscussionTarget, ModelAskParams};
use tokio::sync::MutexGuard;

use crate::app::{AppState, ModalFocus, PromptModalState, PromptModalTarget};
use crate::input::{is_char_typing, InputResult};
use crate::rpc::{send_request, RpcConnection};

/// Routes key events for the unified discussion prompt modal. The modal targets
/// either a review session or a plan. A bare `Enter` (or `C-S`) submits the
/// comment as a model request, while `Shift+Enter`, `Alt+Enter` and `C-J` insert
/// a newline; `Tab` toggles between the editor and the action buttons.
pub(super) async fn handle_prompt_modal(
    key: KeyEvent,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut modal = match st.prompt_modal.take() {
        Some(m) => m,
        None => return Ok(None),
    };

    let primary = st.tui_config.input.primary_modifier;
    let is_ctrl_alt = key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT);
    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));

    if modal.focus.is_actions() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Tab | KeyCode::BackTab => {
                modal.focus = ModalFocus::Editor;
                st.prompt_modal = Some(modal);
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Left | KeyCode::Right => {
                modal.actions.toggle();
                st.prompt_modal = Some(modal);
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if is_char_typing(key.modifiers) => {
                return submit_prompt_modal(st, modal, server_writer).await;
            }
            KeyCode::Char('n') | KeyCode::Char('N') if is_char_typing(key.modifiers) => {
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if modal.actions.is_confirm() {
                    return submit_prompt_modal(st, modal, server_writer).await;
                } else {
                    return Ok(Some(InputResult::Continue));
                }
            }
            _ => {
                st.prompt_modal = Some(modal);
                return Ok(Some(InputResult::Continue));
            }
        }
    }

    match key.code {
        KeyCode::Esc => {
            Ok(Some(InputResult::Continue))
        }
        KeyCode::Tab | KeyCode::BackTab => {
            modal.focus = ModalFocus::Actions;
            modal.actions.reset_to_cancel();
            st.prompt_modal = Some(modal);
            Ok(Some(InputResult::Continue))
        }
        // Unified send policy across every discussion modal: a bare `Enter` sends the
        // comment, while `Shift+Enter`, `Alt+Enter` and `C-J` insert a newline. `C-S`
        // is an explicit fallback, matching the squash dialog.
        KeyCode::Enter
            if !key.modifiers.contains(KeyModifiers::SHIFT)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            return submit_prompt_modal(st, modal, server_writer).await;
        }
        KeyCode::Char('s') | KeyCode::Char('S') if is_cmd => {
            return submit_prompt_modal(st, modal, server_writer).await;
        }
        _ => {
            crate::input::handle_editor_key(&mut modal.prompt_editor, key, is_cmd, true);
            st.prompt_modal = Some(modal);
            Ok(Some(InputResult::Continue))
        }
    }
}

/// Resolves the modal target, formats the discussion prompt and dispatches the
/// model request. The modal is restored when the comment is empty or the target
/// cannot be found so the user can correct the input.
pub(crate) async fn submit_prompt_modal(
    mut st: MutexGuard<'_, AppState>,
    dialog: PromptModalState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let user_comment = dialog.prompt_editor.get_text().trim().to_string();
    if user_comment.is_empty() {
        st.notify_warning("Comment is empty");
        st.prompt_modal = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    let (target, focused_items, focus) = match &dialog.target {
        PromptModalTarget::Review {
            review_id,
            focused_items,
            ..
        } => {
            let ids = focused_items
                .iter()
                .map(|(id, _)| id.to_string())
                .collect::<Vec<String>>();
            (
                DiscussionTarget::Review {
                    id: review_id.clone(),
                },
                ids.clone(),
                DiscussionFocus::review(review_id.clone(), ids),
            )
        }
        PromptModalTarget::Plan {
            plan_id,
            focused_items,
            ..
        } => {
            let ids = focused_items
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<String>>();
            (
                DiscussionTarget::Plan {
                    id: plan_id.clone(),
                },
                ids.clone(),
                DiscussionFocus::plan(plan_id.clone(), ids),
            )
        }
    };

    super::begin_repl_turn(&mut st);
    st.discussion_focus = focus;
    drop(st);
    let params = ModelAskParams::Discussion(DiscussionRequest {
        target,
        focused_items,
        comment: user_comment,
    });
    send_request(
        server_writer,
        methods::MODEL_ASK,
        serde_json::to_value(params)?,
    )
    .await?;
    Ok(Some(InputResult::Continue))
}
