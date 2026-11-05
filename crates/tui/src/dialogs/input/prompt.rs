use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::{
    methods, DiscussionFocus, DiscussionRequest, DiscussionTarget, ModelAskParams,
};
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ModalFocus, PromptModalState, PromptModalTarget};
use crate::command::is_bare_press;
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

/// Routes key events for the unified discussion prompt modal. The modal targets
/// either a review session or a plan. A bare `Enter` (or `C-S`) submits the
/// comment as a model request, while `Shift+Enter`, `Alt+Enter` and `C-J` insert
/// a newline; `Tab` toggles between the editor and the action buttons.
pub(super) async fn handle_prompt_modal(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut modal = match st.modals.prompt_modal.take() {
        Some(m) => m,
        None => return Ok(None),
    };

    let primary = st.tui_config.input.primary_modifier;
    let is_ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::ALT);
    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));

    if modal.focus.is_actions() {
        match key.code {
            KeyCode::Esc => {
                return Ok(Some(InputResult::Continue));
            }
            // The dismissal letter is a bare press, so the global quit chord
            // never discards a typed comment (Conventions §3, §5.1).
            KeyCode::Char('q') if is_bare_press(key.modifiers) => {
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Tab | KeyCode::BackTab => {
                modal.focus = ModalFocus::Editor;
                st.modals.prompt_modal = Some(modal);
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Left | KeyCode::Right => {
                modal.actions.toggle();
                st.modals.prompt_modal = Some(modal);
                return Ok(Some(InputResult::Continue));
            }
            // Both fast accelerators are bare presses, so a command chord or the
            // AltGr compose chord never confirms or cancels the card.
            KeyCode::Char('y') | KeyCode::Char('Y') if is_bare_press(key.modifiers) => {
                return submit_prompt_modal(state, st, modal, server_writer).await;
            }
            KeyCode::Char('n') | KeyCode::Char('N') if is_bare_press(key.modifiers) => {
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if modal.actions.is_confirm() {
                    return submit_prompt_modal(state, st, modal, server_writer).await;
                } else {
                    return Ok(Some(InputResult::Continue));
                }
            }
            _ => {
                st.modals.prompt_modal = Some(modal);
                return Ok(Some(InputResult::Continue));
            }
        }
    }

    match key.code {
        KeyCode::Esc => Ok(Some(InputResult::Continue)),
        KeyCode::Tab | KeyCode::BackTab => {
            modal.focus = ModalFocus::Actions;
            modal.actions.reset_to_cancel();
            st.modals.prompt_modal = Some(modal);
            Ok(Some(InputResult::Continue))
        }
        // Unified send policy across every discussion modal: a bare `Enter` sends the
        // comment, while `Shift+Enter`, `Alt+Enter` and `C-J` insert a newline. `C-S`
        // is an explicit fallback, matching the squash dialog.
        KeyCode::Enter
            if !key.modifiers.contains(KeyModifiers::SHIFT)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            return submit_prompt_modal(state, st, modal, server_writer).await;
        }
        KeyCode::Char('s') | KeyCode::Char('S') if is_cmd => {
            return submit_prompt_modal(state, st, modal, server_writer).await;
        }
        _ => {
            if modal.prompt_editor.takes_yank_chord(key, is_cmd) {
                // The paste buffer is a native round trip bounded at
                // NATIVE_CLIPBOARD_TIMEOUT, so it is read with the state guard
                // released and the answer is applied after the re-lock (c-1).
                st.modals.prompt_modal = Some(modal);
                drop(st);
                let clipboard = crate::clipboard::get_from_clipboard_async().await;
                let mut st = state.lock().await;
                if let Some(mut modal) = st.modals.prompt_modal.take() {
                    modal.prompt_editor.yank_with(clipboard);
                    st.modals.prompt_modal = Some(modal);
                }
                return Ok(Some(InputResult::Continue));
            }
            crate::input::handle_editor_key(&mut modal.prompt_editor, key, is_cmd, true);
            st.modals.prompt_modal = Some(modal);
            Ok(Some(InputResult::Continue))
        }
    }
}

/// Resolves the modal target, formats the discussion prompt and dispatches the
/// model request.
///
/// The launch is optimistic: it clears the previous answer buffers, arms the
/// batch flag, and rewrites the discussion anchor before the request leaves the
/// transport. Everything that transition destroys is captured first, so a
/// failed write restores the previous answer, the batch flag, and the anchor,
/// and hands the submitted comment back to its modal instead of losing it.
pub(crate) async fn submit_prompt_modal(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    dialog: PromptModalState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let user_comment = dialog.prompt_editor.get_text().trim().to_string();
    if user_comment.is_empty() {
        st.notify_warning(crate::i18n::active_t("notify.comment_empty"));
        st.modals.prompt_modal = Some(dialog);
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
        // A review group is addressed by the pair of its session and its own
        // identifier, because a group identifier is unique only inside one
        // session (c-3). The dialogue it starts is the sanctioned path that
        // records the architectural decision (c-4).
        PromptModalTarget::ReviewGroup {
            session_id,
            group_id,
            ..
        } => {
            let target = DiscussionTarget::ReviewGroup {
                session_id: session_id.clone(),
                group_id: group_id.clone(),
            };
            let focus = DiscussionFocus::from_target(target.clone(), Vec::new());
            (target, Vec::new(), focus)
        }
    };

    let params = ModelAskParams::Discussion(DiscussionRequest {
        target,
        focused_items,
        comment: user_comment,
    });
    // Serializing the typed payload cannot touch the interface, so it runs
    // before the optimistic transition: a serialization failure leaves the
    // modal and the REPL view exactly as the developer left them.
    let payload = serde_json::to_value(params)?;

    // The launch transition opens a new turn and rewrites the discussion
    // anchor, so the whole launch state is captured and restored when the
    // request never reaches the transport.
    let snapshot = super::TurnLaunchSnapshot::capture(&st);
    super::begin_repl_turn(&mut st);
    st.discussion_focus = focus;
    // The launch transition is complete, so the guard is released before the
    // request leaves the loop and a daemon that stopped draining its stdin can
    // never freeze the interface. A failed write re-acquires the lock to
    // restore the snapshot and hand the comment back to its modal (c-1).
    drop(st);

    if let Err(err) = send_request(server_writer, methods::MODEL_ASK, payload).await {
        let mut st = state.lock().await;
        snapshot.restore(&mut st);
        st.modals.prompt_modal = Some(dialog);
        st.notify_error(
            crate::i18n::active_t("notify.prompt_send_failed").replace("{}", &err.to_string()),
        );
    }

    Ok(Some(InputResult::Continue))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// Discussion modal anchored on a plan: a typed comment and the action bar
    /// focused, which is the section that owns the fast accelerators.
    fn actions_modal() -> PromptModalState {
        let mut modal = PromptModalState::new(PromptModalTarget::Plan {
            plan_id: "plan-1".to_string(),
            plan_title: "Plan 1".to_string(),
            focused_items: Vec::new(),
        });
        modal.prompt_editor.insert_str("keep this comment");
        modal.focus = ModalFocus::Actions;
        modal
    }

    /// Presses one key against the modal and returns the verdict.
    ///
    /// The state guard is created here and consumed by the handler, so the
    /// caller can lock the state again to inspect the outcome.
    async fn press(
        state: &Arc<Mutex<AppState>>,
        event: KeyEvent,
        writer: &mut RpcConnection,
    ) -> Option<InputResult> {
        let guard = state.lock().await;
        handle_prompt_modal(event, state, guard, writer)
            .await
            .expect("an in-memory transport never fails")
    }

    /// A command-modified or AltGr-composed letter reaches neither fast
    /// accelerator nor the dismissal letter: the typed comment survives and no
    /// request leaves the client (Conventions §3, §5.1).
    #[tokio::test]
    async fn command_modified_letters_are_refused_by_every_guarded_arm() {
        for (code, modifiers) in [
            (KeyCode::Char('y'), KeyModifiers::CONTROL),
            (KeyCode::Char('y'), KeyModifiers::ALT),
            (
                KeyCode::Char('y'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            (KeyCode::Char('n'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('n'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            (KeyCode::Char('q'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('q'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        ] {
            let state = Arc::new(Mutex::new(crate::app::test_state()));
            {
                let mut st = state.lock().await;
                st.modals.prompt_modal = Some(actions_modal());
            }
            let mut writer = RpcConnection::new(Vec::<u8>::new());

            let result = press(&state, key(code, modifiers), &mut writer).await;
            assert!(matches!(result, Some(InputResult::Continue)));

            let st = state.lock().await;
            let modal = st
                .modals
                .prompt_modal
                .as_ref()
                .expect("the modal must stay open");
            assert_eq!(modal.prompt_editor.get_text(), "keep this comment");
            assert!(
                writer.written().is_empty(),
                "{code:?} with {modifiers:?} must not launch a turn"
            );
        }
    }

    /// The guard never over-blocks: a bare `n` still cancels the card and `Esc`
    /// stays unconditional (Conventions §3).
    #[tokio::test]
    async fn bare_letters_and_escape_keep_their_actions() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        {
            let mut st = state.lock().await;
            st.modals.prompt_modal = Some(actions_modal());
        }
        let mut writer = RpcConnection::new(Vec::<u8>::new());
        let cancel = key(KeyCode::Char('n'), KeyModifiers::NONE);
        let result = press(&state, cancel, &mut writer).await;
        assert!(matches!(result, Some(InputResult::Continue)));
        assert!(
            state.lock().await.modals.prompt_modal.is_none(),
            "a bare n must still cancel the card"
        );

        let state = Arc::new(Mutex::new(crate::app::test_state()));
        {
            let mut st = state.lock().await;
            st.modals.prompt_modal = Some(actions_modal());
        }
        let escape = key(KeyCode::Esc, KeyModifiers::CONTROL);
        let result = press(&state, escape, &mut writer).await;
        assert!(matches!(result, Some(InputResult::Continue)));
        assert!(
            state.lock().await.modals.prompt_modal.is_none(),
            "Esc stays unconditional while a command modifier is held"
        );
        assert!(writer.written().is_empty());
    }
}
