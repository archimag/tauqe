//! Keyboard routing for the plan step and scope execution confirmation.
//!
//! Executing a step or a scope hands control to the Coder role, so the dialog
//! exposes two keyboard targets — the embedded Coder model list and the action
//! buttons — and `Tab` cycles through them in visual order. The plan states every
//! step's instructions itself, so the dialog carries no directives editor: the
//! list owns bare digits, arrows, and paging only while it holds focus
//! (c-4, Conventions §6, §7).
//!
//! The dialog belongs to the model-list family, so its keyboard contract lives
//! in the shared core at [`super::model_list`]: the list, the action bar, the
//! `Shift+Tab` spelling, and the bare-press guard of the letter accelerators are
//! resolved once for every dialog of the family. This adapter keeps only what
//! belongs to the target itself — the take guard, the dialog geometry, and the
//! launch it dispatches.

use std::sync::Arc;

use crossterm::event::KeyEvent;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::InputResult;
use crate::rpc::RpcConnection;

use super::model_list::{handle_dialog_key, ModelListKeyOutcome};

/// Routes key events for the plan execution form dialog.
///
/// The dialog is dismissed by dropping the pending confirmation; confirming
/// dispatches `plan/execute` with the selected Coder rung, so the starting
/// ladder step chosen by the developer travels on the wire and the plan stays
/// the single source of truth for each step's instructions (c-tui-selector, c-1).
pub(super) async fn handle_execute_scope_dialog(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut target = match st.modals.confirm_execute_scope.take() {
        Some(target) => target,
        None => return Ok(None),
    };

    let visible_rows = crate::dialogs::render::execute_scope_dialog_geometry(
        st.last_frame_area,
        target.selector.len(),
        target.steps.len() == 1,
    )
    .model_rows
    .height as usize;

    match handle_dialog_key(&mut target, key, &st.tui_config.input, visible_rows) {
        ModelListKeyOutcome::Submit => {
            super::confirm_execute_scope(state, st, server_writer, target).await?;
        }
        // Safe cancellation returns the shared button to its default and drops
        // the dialog without a request.
        ModelListKeyOutcome::Dismiss => {
            st.modals.confirm_button = ConfirmDialogButton::Cancel;
        }
        // The dialog carries no directives editor, so the yank outcome is
        // unreachable; keeping the dialog is the only sensible interpretation of
        // a key it never produces (Conventions §5).
        ModelListKeyOutcome::Consumed | ModelListKeyOutcome::Yank => {
            st.modals.confirm_execute_scope = Some(target);
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
        test_state, AppState, ConfirmDialogButton, ConfirmExecuteScopeState, ExecuteScopeFocus,
        ViewMode,
    };
    use crate::rpc::RpcConnection;

    use tauqe_protocol::{ModelEntry, ModelRef, PlanItem, PlanItemStatus};

    fn coder(name: &str) -> ModelEntry {
        ModelEntry::new(ModelRef::openrouter(name), None)
    }

    fn state_with_target() -> AppState {
        let mut st = test_state();
        st.view_mode = ViewMode::Plans;
        st.modals.confirm_execute_scope = Some(ConfirmExecuteScopeState::new(
            "plan-1".to_string(),
            "Plan One".to_string(),
            "Step One".to_string(),
            Some("1".to_string()),
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step One".to_string(),
                status: PlanItemStatus::Todo,
                ..Default::default()
            }],
            vec![coder("coder-a"), coder("coder-b")],
        ));
        st
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    async fn press(state: &Arc<Mutex<AppState>>, conn: &mut RpcConnection, event: KeyEvent) {
        let guard = state.lock().await;
        super::handle_execute_scope_dialog(event, state, guard, conn)
            .await
            .expect("in-memory dispatch never fails");
    }

    async fn target(state: &Arc<Mutex<AppState>>) -> ConfirmExecuteScopeState {
        state
            .lock()
            .await
            .modals
            .confirm_execute_scope
            .clone()
            .expect("dialog stays open")
    }

    async fn set_focus(state: &Arc<Mutex<AppState>>, focus: ExecuteScopeFocus) {
        let mut guard = state.lock().await;
        if let Some(target) = guard.modals.confirm_execute_scope.as_mut() {
            target.focus = focus;
        }
    }

    #[tokio::test]
    async fn ctrl_enter_submits_selected_coder() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        {
            let mut guard = state.lock().await;
            let target = guard
                .modals
                .confirm_execute_scope
                .as_mut()
                .expect("dialog must be open");
            target.selector.select_index(1);
            target.focus = ExecuteScopeFocus::ModelList;
        }

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("plan/execute"), "{payload}");
        assert!(payload.contains("coder-b"), "{payload}");
        assert!(payload.contains("\"plan_id\":\"plan-1\""), "{payload}");
        assert!(payload.contains("\"step_id\":\"1\""), "{payload}");

        let guard = state.lock().await;
        assert!(guard.modals.confirm_execute_scope.is_none());
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }

    /// The Coder selection is reachable only from the list section: the action
    /// bar never forwards a bare digit to the picker (c-4, Conventions §5).
    #[tokio::test]
    async fn digits_select_in_the_list_and_are_inert_on_the_actions() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, ExecuteScopeFocus::ModelList).await;
        press(&state, &mut conn, key(KeyCode::Char('2'))).await;
        assert_eq!(target(&state).await.selector.selected_index(), 1);

        set_focus(&state, ExecuteScopeFocus::Actions).await;
        press(&state, &mut conn, key(KeyCode::Char('1'))).await;
        let untouched = target(&state).await;
        assert_eq!(
            untouched.selector.selected_index(),
            1,
            "the Coder selection must not change from the action bar"
        );
    }

    /// The dialog owns no text field, so a bare letter, `Enter`, or `Space`
    /// pressed over the Coder list is inert: it neither launches the step nor
    /// dismisses the dialog, and the selection stays untouched (c-4, §5).
    #[tokio::test]
    async fn bare_keys_over_the_list_are_inert() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, ExecuteScopeFocus::ModelList).await;

        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('q'),
            KeyCode::Enter,
            KeyCode::Char(' '),
        ] {
            press(&state, &mut conn, key(code)).await;
        }

        let open = target(&state).await;
        assert_eq!(open.focus, ExecuteScopeFocus::ModelList);
        assert_eq!(open.confirm_button, ConfirmDialogButton::Cancel);
        assert_eq!(open.selector.selected_index(), 0);
        assert!(conn.written().is_empty());
    }

    #[tokio::test]
    async fn tab_cycles_focus_between_the_list_and_the_actions() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, ExecuteScopeFocus::ModelList).await;

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let actions = target(&state).await;
        assert_eq!(actions.focus, ExecuteScopeFocus::Actions);
        assert_eq!(actions.confirm_button, ConfirmDialogButton::Cancel);

        // The first Tab inside the action bar focuses `Execute` instead of
        // leaving the bar, so execution is never started accidentally.
        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let confirm = target(&state).await;
        assert_eq!(confirm.focus, ExecuteScopeFocus::Actions);
        assert_eq!(confirm.confirm_button, ConfirmDialogButton::Confirm);

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let wrapped = target(&state).await;
        assert_eq!(wrapped.focus, ExecuteScopeFocus::ModelList);
        assert_eq!(wrapped.confirm_button, ConfirmDialogButton::Cancel);

        press(&state, &mut conn, key(KeyCode::BackTab)).await;
        let back = target(&state).await;
        assert_eq!(back.focus, ExecuteScopeFocus::Actions);
    }

    #[tokio::test]
    async fn enter_on_default_cancel_dismisses_without_a_request() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, ExecuteScopeFocus::Actions).await;

        press(&state, &mut conn, key(KeyCode::Enter)).await;

        let guard = state.lock().await;
        assert!(guard.modals.confirm_execute_scope.is_none());
        assert!(conn.written().is_empty());
    }

    #[tokio::test]
    async fn y_confirms_from_the_actions() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, ExecuteScopeFocus::Actions).await;

        press(&state, &mut conn, key(KeyCode::Char('y'))).await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("plan/execute"), "{payload}");
        assert!(payload.contains("coder-a"), "{payload}");

        let guard = state.lock().await;
        assert!(guard.modals.confirm_execute_scope.is_none());
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }
    /// The `Shift+Tab` spelling that a terminal reports as `Tab` carrying
    /// `SHIFT` moves the focus backward, exactly as the dedicated `BackTab` code
    /// does, so a shifted press never cycles the focus forward (Conventions §6.2).
    #[tokio::test]
    async fn tab_with_shift_cycles_focus_backward() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, ExecuteScopeFocus::Actions).await;

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        )
        .await;

        let back = target(&state).await;
        assert_eq!(back.focus, ExecuteScopeFocus::ModelList);
        assert_eq!(back.confirm_button, ConfirmDialogButton::Cancel);
        assert!(conn.written().is_empty());
    }

    /// The letter accelerators are bare presses: `Ctrl+Y` is the editor's yank
    /// chord, so it must neither launch a token-spending turn nor dismiss the
    /// dialog that still holds the developer's Coder choice (Conventions §5.2).
    #[tokio::test]
    async fn ctrl_y_from_the_actions_writes_no_request_and_keeps_the_dialog() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, ExecuteScopeFocus::Actions).await;

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
        assert!(guard.modals.confirm_execute_scope.is_some());
        assert_eq!(guard.view_mode, ViewMode::Plans);
    }

    /// `Ctrl+Q` and `Ctrl+N` are command chords, not the dialog's dismissal or
    /// cancellation accelerators, so neither may drop the pending launch.
    #[tokio::test]
    async fn ctrl_q_and_ctrl_n_do_not_dismiss_the_dialog() {
        for character in ['q', 'n'] {
            let state = Arc::new(Mutex::new(state_with_target()));
            let mut conn = RpcConnection::new(Vec::<u8>::new());
            set_focus(&state, ExecuteScopeFocus::Actions).await;

            press(
                &state,
                &mut conn,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
            )
            .await;

            let guard = state.lock().await;
            assert!(
                guard.modals.confirm_execute_scope.is_some(),
                "Ctrl+{character} must not dismiss the dialog"
            );
            drop(guard);
            assert!(conn.written().is_empty());
        }
    }

    /// The AltGr compose chord (`Ctrl+Alt`) belongs to neither command modifier,
    /// so a composed character reaches no accelerator: it neither selects a
    /// Coder in the list nor confirms, cancels, or submits from the action bar
    /// (Conventions §5.2).
    #[tokio::test]
    async fn compose_chord_reaches_neither_the_accelerators_nor_the_list() {
        let state = Arc::new(Mutex::new(state_with_target()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let compose = KeyModifiers::CONTROL | KeyModifiers::ALT;

        set_focus(&state, ExecuteScopeFocus::ModelList).await;
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

        set_focus(&state, ExecuteScopeFocus::Actions).await;
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('q'),
            KeyCode::Enter,
        ] {
            press(&state, &mut conn, KeyEvent::new(code, compose)).await;
        }

        let open = target(&state).await;
        assert_eq!(open.focus, ExecuteScopeFocus::Actions);
        assert_eq!(open.confirm_button, ConfirmDialogButton::Cancel);
        assert!(conn.written().is_empty());
    }
}
