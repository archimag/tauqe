use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::methods;
use tokio::process::ChildStdin;
use tokio::sync::MutexGuard;

use crate::app::{AppState, ModalFocus, PromptModalState, PromptModalTarget};
use crate::input::{is_char_typing, InputResult};
use crate::rpc::send_request;

/// Routes key events for the unified discussion prompt modal. The modal targets
/// either a review session or a plan; `Enter` submits the comment as a model
/// request, `Tab` toggles between the editor and the action buttons.
pub(super) async fn handle_prompt_modal(
    key: KeyEvent,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut ChildStdin,
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
        KeyCode::Enter if is_cmd => {
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
async fn submit_prompt_modal(
    mut st: MutexGuard<'_, AppState>,
    dialog: PromptModalState,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<Option<InputResult>> {
    let user_comment = dialog.prompt_editor.get_text().trim().to_string();
    if user_comment.is_empty() {
        st.notify_warning("Comment is empty");
        st.prompt_modal = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    match &dialog.target {
        PromptModalTarget::Review {
            review_id,
            focused_items,
            ..
        } => {
            let session = st
                .review
                .sessions
                .iter()
                .find(|s| &s.id == review_id)
                .cloned()
                .or_else(|| {
                    st.review
                        .session
                        .as_ref()
                        .filter(|s| &s.id == review_id)
                        .cloned()
                });

            if let Some(session) = session {
                let focused_ids: Vec<u32> = focused_items.iter().map(|(id, _)| *id).collect();
                let prompt = tauqe_protocol::review::format_review_discussion_prompt_items(
                    &session,
                    &focused_ids,
                    &user_comment,
                );

                super::begin_develop_turn(&mut st);
                drop(st);
                send_request(
                    server_writer,
                    methods::MODEL_ASK,
                    serde_json::json!({ "prompt": prompt }),
                )
                .await?;
                Ok(Some(InputResult::Continue))
            } else {
                st.notify_error(format!("Review session '{}' not found", review_id));
                st.prompt_modal = Some(dialog);
                Ok(Some(InputResult::Continue))
            }
        }
        PromptModalTarget::Plan {
            plan_id,
            focused_items,
            ..
        } => {
            let plan = st
                .plans_view
                .plans
                .iter()
                .find(|p| &p.id == plan_id)
                .cloned();
            if let Some(plan) = plan {
                let focused_ids: Vec<String> =
                    focused_items.iter().map(|(id, _)| id.clone()).collect();
                let prompt = tauqe_protocol::plan::format_plan_discussion_prompt(
                    &plan,
                    &focused_ids,
                    &user_comment,
                );

                super::begin_develop_turn(&mut st);
                drop(st);
                send_request(
                    server_writer,
                    methods::MODEL_ASK,
                    serde_json::json!({ "prompt": prompt }),
                )
                .await?;
                Ok(Some(InputResult::Continue))
            } else {
                st.notify_error(format!("Plan '{}' not found", plan_id));
                st.prompt_modal = Some(dialog);
                Ok(Some(InputResult::Continue))
            }
        }
    }
}
