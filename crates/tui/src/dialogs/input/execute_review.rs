//! Keyboard routing for the single-target review-execution confirmation.
//!
//! Point execution of a finding or a subplan step exposes three keyboard
//! targets — the embedded Coder model list, the directives editor, and the
//! action buttons — and `Tab` cycles through them in visual order. The model
//! list owns bare digits and arrows only while it holds focus, so list
//! navigation and text entry never fight over the same keys (Conventions §6, §7).

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::InputResult;
use crate::rpc::RpcConnection;

/// Routes key events for the review-execution form dialog.
///
/// The dialog is dismissed by dropping the pending confirmation; confirming
/// dispatches `review/execute` with the selected Coder model, the optional
/// directives, and the targeted subplan step.
pub(super) async fn handle_execute_review_dialog(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut target = match st.confirm_execute_review.take() {
        Some(target) if target.is_single_target() => target,
        other => {
            st.confirm_execute_review = other;
            return Ok(None);
        }
    };

    let primary = st.tui_config.input.primary_modifier;
    let meta = st.tui_config.input.meta_modifier;
    let visible_rows = crate::dialogs::render::execute_review_dialog_geometry(
        st.last_frame_area,
        target.selector.len(),
    )
    .model_rows
    .height as usize;

    // The C-M / M-M chord always switches the Coder; the arrows browse the list
    // only while the list owns focus.
    if target
        .selector
        .handle_key(key, primary, meta, !target.focus.is_model_list())
    {
        st.confirm_execute_review = Some(target);
        return Ok(Some(InputResult::Continue));
    }

    // Bare digits, arrows, and paging act on the model list only while it owns
    // focus; elsewhere the directives editor keeps those keys.
    if target.focus.is_model_list() && target.selector.handle_list_key(key, visible_rows) {
        st.confirm_execute_review = Some(target);
        return Ok(Some(InputResult::Continue));
    }

    let is_ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::ALT);

    // Alt+digit picks a Coder by position without leaving the editor.
    if target.focus.is_instructions_editor()
        && !is_ctrl_alt
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT)
    {
        if let KeyCode::Char(c @ '1'..='9') = key.code {
            let index = c as usize - '1' as usize;
            if index < target.selector.len() {
                target.selector.select_index(index);
                st.confirm_execute_review = Some(target);
                return Ok(Some(InputResult::Continue));
            }
        }
    }

    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);

    // C-Enter accelerator unconditionally resolves from any section.
    if matches!(key.code, KeyCode::Enter) && is_cmd {
        super::confirm_execute_review(state, st, server_writer, target).await?;
        return Ok(Some(InputResult::Continue));
    }

    match key.code {
        KeyCode::Esc => {
            st.confirm_button = ConfirmDialogButton::Cancel;
            Ok(Some(InputResult::Continue))
        }
        KeyCode::Char('q') if target.focus.is_actions() => {
            st.confirm_button = ConfirmDialogButton::Cancel;
            Ok(Some(InputResult::Continue))
        }
        // Fast accelerators mirror the shared dialog vocabulary. They are
        // limited to the action bar so bare letters stay literal text inside
        // the directives editor (Conventions §5).
        KeyCode::Char('y') | KeyCode::Char('Y') if target.focus.is_actions() => {
            super::confirm_execute_review(state, st, server_writer, target).await?;
            Ok(Some(InputResult::Continue))
        }
        KeyCode::Char('n') | KeyCode::Char('N') if target.focus.is_actions() => {
            st.confirm_button = ConfirmDialogButton::Cancel;
            Ok(Some(InputResult::Continue))
        }
        KeyCode::Tab => {
            if target.focus.is_actions() && target.confirm_button == ConfirmDialogButton::Cancel {
                target.confirm_button = ConfirmDialogButton::Confirm;
            } else {
                if target.focus.is_actions() {
                    target.confirm_button = ConfirmDialogButton::Cancel;
                }
                target.focus.cycle_next(target.selector.len());
            }
            st.confirm_execute_review = Some(target);
            Ok(Some(InputResult::Continue))
        }
        KeyCode::BackTab => {
            if target.focus.is_actions() && target.confirm_button == ConfirmDialogButton::Confirm {
                target.confirm_button = ConfirmDialogButton::Cancel;
            } else {
                target.focus.cycle_prev(target.selector.len());
                if target.focus.is_actions() {
                    target.confirm_button = ConfirmDialogButton::Confirm;
                }
            }
            st.confirm_execute_review = Some(target);
            Ok(Some(InputResult::Continue))
        }
        // Horizontal stepping moves between the action buttons.
        KeyCode::Left | KeyCode::Right if target.focus.is_actions() => {
            target.confirm_button = match target.confirm_button {
                ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
                ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
            };
            st.confirm_execute_review = Some(target);
            Ok(Some(InputResult::Continue))
        }
        // Execution spends tokens, so it starts only from the focused `Resolve`
        // button; the safe `Cancel` default dismisses the dialog.
        KeyCode::Enter | KeyCode::Char(' ')
            if !has_shift && !has_alt && target.focus.is_actions() =>
        {
            if target.confirm_button == ConfirmDialogButton::Confirm {
                super::confirm_execute_review(state, st, server_writer, target).await?;
            } else {
                st.confirm_button = ConfirmDialogButton::Cancel;
            }
            Ok(Some(InputResult::Continue))
        }
        _ => {
            if target.focus.is_instructions_editor() {
                crate::input::handle_editor_key(&mut target.instructions_editor, key, is_cmd, true);
            }
            st.confirm_execute_review = Some(target);
            Ok(Some(InputResult::Continue))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::Mutex;

    use crate::app::{
        test_state, AppState, ConfirmDialogButton, ConfirmExecuteReviewState, ViewMode,
    };
    use crate::rpc::RpcConnection;
    use crate::views::plans::RefineDialogFocus;

    use tauqe_protocol::{IssueStatus, IssueType, ModelEntry, ModelRef, ReviewIssue};

    fn coder(name: &str) -> ModelEntry {
        ModelEntry::new(ModelRef::openrouter(name), None, None)
    }

    fn finding(id: u32) -> ReviewIssue {
        ReviewIssue {
            id,
            title: format!("Finding {}", id),
            model: None,
            issue_type: IssueType::Bug,
            status: IssueStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            plan: Vec::new(),
            note: None,
        }
    }

    fn state_with_target() -> AppState {
        let mut st = test_state();
        st.view_mode = ViewMode::Review;
        st.confirm_execute_review = Some(ConfirmExecuteReviewState::new(
            Some("rev-1".to_string()),
            "Session rev-1".to_string(),
            "Finding 1".to_string(),
            Some(1),
            Some("step-1".to_string()),
            vec![finding(1)],
            vec![coder("coder-a"), coder("coder-b")],
        ));
        st
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    async fn press(state: &Arc<Mutex<AppState>>, conn: &mut RpcConnection, event: KeyEvent) {
        let guard = state.lock().await;
        super::handle_execute_review_dialog(event, state, guard, conn)
            .await
            .expect("in-memory dispatch never fails");
    }

    async fn target(state: &Arc<Mutex<AppState>>) -> ConfirmExecuteReviewState {
        state
            .lock()
            .await
            .confirm_execute_review
            .clone()
            .expect("dialog stays open")
    }

    async fn set_focus(state: &Arc<Mutex<AppState>>, focus: RefineDialogFocus) {
        let mut guard = state.lock().await;
        if let Some(target) = guard.confirm_execute_review.as_mut() {
            target.focus = focus;
        }
    }

    #[tokio::test]
    async fn ctrl_enter_submits_selected_coder_step_and_directives() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        {
            let mut guard = state.lock().await;
            let target = guard
                .confirm_execute_review
                .as_mut()
                .expect("dialog must be open");
            target.selector.select_index(1);
            target.instructions_editor.insert_str("keep it minimal");
            target.focus = RefineDialogFocus::InstructionsEditor;
        }

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("review/execute"), "{payload}");
        assert!(payload.contains("coder-b"), "{payload}");
        assert!(payload.contains("keep it minimal"), "{payload}");
        assert!(payload.contains("\"item_id\":1"), "{payload}");
        assert!(payload.contains("\"step_id\":\"step-1\""), "{payload}");

        let guard = state.lock().await;
        assert!(guard.confirm_execute_review.is_none());
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }

    #[tokio::test]
    async fn model_switch_chord_cycles_coder_from_the_editor() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::InstructionsEditor).await;

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
        )
        .await;
        let switched = target(&state).await;
        assert_eq!(switched.selector.selected_index(), 1);
        assert_eq!(switched.focus, RefineDialogFocus::InstructionsEditor);
        assert!(
            switched.instructions_editor.is_empty(),
            "the chord must never type into the directives"
        );

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('M'), KeyModifiers::ALT),
        )
        .await;
        assert_eq!(target(&state).await.selector.selected_index(), 0);
    }

    #[tokio::test]
    async fn digits_select_in_list_but_type_in_editor() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::ModelList).await;
        press(&state, &mut conn, key(KeyCode::Char('2'))).await;
        let selected = target(&state).await;
        assert_eq!(selected.selector.selected_index(), 1);
        assert!(selected.instructions_editor.is_empty());

        set_focus(&state, RefineDialogFocus::InstructionsEditor).await;
        press(&state, &mut conn, key(KeyCode::Char('2'))).await;
        let typed = target(&state).await;
        assert_eq!(typed.instructions_editor.get_text(), "2");
        assert_eq!(
            typed.selector.selected_index(),
            1,
            "typing a digit must not switch the coder"
        );
    }

    #[tokio::test]
    async fn alt_digit_selects_coder_from_the_editor() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::InstructionsEditor).await;
        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT),
        )
        .await;

        let picked = target(&state).await;
        assert_eq!(picked.selector.selected_index(), 1);
        assert!(picked.instructions_editor.is_empty());
        assert_eq!(picked.focus, RefineDialogFocus::InstructionsEditor);
    }

    #[tokio::test]
    async fn tab_cycles_focus_through_list_editor_and_actions() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::ModelList).await;

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        assert_eq!(
            target(&state).await.focus,
            RefineDialogFocus::InstructionsEditor
        );

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let actions = target(&state).await;
        assert_eq!(actions.focus, RefineDialogFocus::Actions);
        assert_eq!(actions.confirm_button, ConfirmDialogButton::Cancel);

        // The first Tab inside the action bar focuses `Resolve` instead of
        // leaving the bar, so execution is never started accidentally.
        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let confirm = target(&state).await;
        assert_eq!(confirm.focus, RefineDialogFocus::Actions);
        assert_eq!(confirm.confirm_button, ConfirmDialogButton::Confirm);

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let wrapped = target(&state).await;
        assert_eq!(wrapped.focus, RefineDialogFocus::ModelList);
        assert_eq!(wrapped.confirm_button, ConfirmDialogButton::Cancel);
    }

    #[tokio::test]
    async fn enter_on_default_cancel_dismisses_without_a_request() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, RefineDialogFocus::Actions).await;

        press(&state, &mut conn, key(KeyCode::Enter)).await;

        let guard = state.lock().await;
        assert!(guard.confirm_execute_review.is_none());
        assert!(conn.written().is_empty());
    }

    #[tokio::test]
    async fn y_confirms_from_the_actions() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, RefineDialogFocus::Actions).await;

        press(&state, &mut conn, key(KeyCode::Char('y'))).await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("review/execute"), "{payload}");

        let guard = state.lock().await;
        assert!(guard.confirm_execute_review.is_none());
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }

    #[tokio::test]
    async fn n_cancels_from_the_actions_without_a_request() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, RefineDialogFocus::Actions).await;

        press(&state, &mut conn, key(KeyCode::Char('n'))).await;

        let guard = state.lock().await;
        assert!(guard.confirm_execute_review.is_none());
        assert_eq!(guard.view_mode, ViewMode::Review);
        assert!(conn.written().is_empty());
    }
}
