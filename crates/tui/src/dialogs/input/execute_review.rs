//! Keyboard routing for the single-target review-execution confirmation.
//!
//! The dialog belongs to the model-list family, so its keyboard contract lives
//! in the shared core at [`super::model_list`]: the embedded Coder list, the
//! directives editor, and the action buttons are resolved once for every dialog
//! of the family, which keeps the letter accelerators on bare presses and both
//! `Shift+Tab` spellings moving the focus backward. This adapter keeps only what
//! belongs to the review target itself — the single-target guard, the dialog
//! geometry, and the launch it dispatches.

use std::sync::Arc;

use crossterm::event::KeyEvent;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::InputResult;
use crate::rpc::RpcConnection;

use super::model_list::{handle_dialog_key, hoisted_editor_yank, ModelListKeyOutcome};

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

    let visible_rows = crate::dialogs::render::execute_review_dialog_geometry(
        st.last_frame_area,
        target.selector.len(),
    )
    .model_rows
    .height as usize;

    match handle_dialog_key(&mut target, key, &st.tui_config.input, visible_rows) {
        ModelListKeyOutcome::Submit => {
            super::confirm_execute_review(state, st, server_writer, target).await?;
        }
        // Safe cancellation returns the shared button to its default and drops
        // the dialog without a request.
        ModelListKeyOutcome::Dismiss => {
            st.confirm_button = ConfirmDialogButton::Cancel;
        }
        // The clipboard read is a native round trip, so it runs inside the
        // shared helper with the state guard released (c-1).
        ModelListKeyOutcome::Yank => {
            hoisted_editor_yank(
                state,
                st,
                target,
                |st, target| st.confirm_execute_review = Some(target),
                |st| st.confirm_execute_review.take(),
            )
            .await;
        }
        ModelListKeyOutcome::Consumed => {
            st.confirm_execute_review = Some(target);
        }
    }
    Ok(Some(InputResult::Continue))
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
        ModelEntry::new(ModelRef::openrouter(name), None)
    }

    fn finding(id: u32) -> ReviewIssue {
        ReviewIssue {
            id,
            title: format!("Finding {}", id),
            uuid: None,
            commit: None,
            group: None,
            model: None,
            issue_type: IssueType::Bug,
            status: IssueStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            note: None,
        }
    }

    /// Finding-level point target: it names the finding itself, so it never
    /// carries an implementation step — a step id travels only with a group
    /// target (c-4, c-5).
    fn state_with_target() -> AppState {
        let mut st = test_state();
        st.view_mode = ViewMode::Review;
        st.confirm_execute_review = Some(ConfirmExecuteReviewState::new(
            Some("rev-1".to_string()),
            "Session rev-1".to_string(),
            "Finding 1".to_string(),
            Some(1),
            // A finding-level target names the finding itself; an implementation
            // step travels only together with a group target (c-4, c-5).
            None,
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
    async fn ctrl_enter_submits_selected_coder_and_directives() {
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
        assert!(
            !payload.contains("\"step_id\""),
            "a finding-level target never carries an implementation step: {payload}"
        );

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

    /// A step of a group plan travels as `group_id` + `step_id` with the
    /// developer's Coder rung and directives, and never as a finding queue
    /// (c-1, c-4, c-5).
    #[tokio::test]
    async fn ctrl_enter_submits_the_targeted_group_step() {
        let mut st = test_state();
        st.view_mode = ViewMode::Review;
        st.confirm_execute_review = Some(
            ConfirmExecuteReviewState::new(
                Some("rev-1".to_string()),
                "Session rev-1".to_string(),
                "Unified auth guard".to_string(),
                None,
                Some("1.1".to_string()),
                Vec::new(),
                vec![coder("coder-a"), coder("coder-b")],
            )
            .with_group("arch-auth".to_string(), "Unified auth guard".to_string()),
        );
        {
            let target = st
                .confirm_execute_review
                .as_mut()
                .expect("dialog must be open");
            target.selector.select_index(1);
            target
                .instructions_editor
                .insert_str("keep the guard unified");
        }

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("review/execute"), "{payload}");
        assert!(payload.contains("\"group_id\":\"arch-auth\""), "{payload}");
        assert!(payload.contains("\"step_id\":\"1.1\""), "{payload}");
        assert!(payload.contains("coder-b"), "{payload}");
        assert!(payload.contains("keep the guard unified"), "{payload}");
        assert!(
            !payload.contains("item_ids"),
            "a group step never queues findings: {payload}"
        );

        let guard = state.lock().await;
        assert!(guard.confirm_execute_review.is_none());
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }
    /// The letter accelerators are bare presses: `Ctrl+Y` is the editor's yank
    /// chord, so it must neither launch a token-spending turn nor dismiss the
    /// dialog that still holds the developer's directives (Conventions §5.2).
    #[tokio::test]
    async fn ctrl_y_from_the_actions_writes_no_request_and_keeps_the_dialog() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, RefineDialogFocus::Actions).await;

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
        )
        .await;

        assert!(
            conn.written().is_empty(),
            "a command chord must never launch a turn"
        );
        let guard = state.lock().await;
        assert!(
            guard.confirm_execute_review.is_some(),
            "the dialog stays open with its directives"
        );
        assert_eq!(guard.view_mode, ViewMode::Review);
    }

    /// `Ctrl+Q` and `Ctrl+N` are command chords, not the dialog's dismissal or
    /// cancellation accelerators, so neither may discard typed directives.
    #[tokio::test]
    async fn ctrl_q_and_ctrl_n_do_not_dismiss_the_dialog() {
        for character in ['q', 'n'] {
            let state = Arc::new(Mutex::new(state_with_target()));
            let mut conn = RpcConnection::new(Vec::<u8>::new());
            set_focus(&state, RefineDialogFocus::Actions).await;

            press(
                &state,
                &mut conn,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
            )
            .await;

            let guard = state.lock().await;
            assert!(
                guard.confirm_execute_review.is_some(),
                "Ctrl+{character} must not dismiss the dialog"
            );
            drop(guard);
            assert!(conn.written().is_empty());
        }
    }

    /// A terminal that reports `Shift+Tab` as `Tab` carrying `SHIFT` must move
    /// the focus backward, exactly as the dedicated `BackTab` code does, so the
    /// shifted press never cycles the focus forward (Conventions §6.2).
    #[tokio::test]
    async fn tab_with_shift_cycles_focus_backward() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, RefineDialogFocus::ModelList).await;

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        )
        .await;

        assert_eq!(target(&state).await.focus, RefineDialogFocus::Actions);
    }

    /// The AltGr compose chord (`Ctrl+Alt`) belongs to neither command modifier,
    /// so a composed character reaches no accelerator: it neither selects a
    /// Coder in the list nor confirms, cancels, or submits from the action bar,
    /// and with the action bar focused it never reaches the editor either
    /// (Conventions §5.2).
    #[tokio::test]
    async fn compose_chord_reaches_neither_the_accelerators_nor_the_editor() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let compose = KeyModifiers::CONTROL | KeyModifiers::ALT;

        set_focus(&state, RefineDialogFocus::ModelList).await;
        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('2'), compose),
        )
        .await;
        assert_eq!(
            target(&state).await.selector.selected_index(),
            0,
            "a composed digit must not select a Coder"
        );

        set_focus(&state, RefineDialogFocus::Actions).await;
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('q'),
            KeyCode::Enter,
        ] {
            press(&state, &mut conn, KeyEvent::new(code, compose)).await;
        }

        let open = target(&state).await;
        assert_eq!(open.focus, RefineDialogFocus::Actions);
        assert_eq!(open.confirm_button, ConfirmDialogButton::Cancel);
        assert!(open.instructions_editor.is_empty());
        assert!(conn.written().is_empty());
    }
}
