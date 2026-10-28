//! Keyboard routing for the plan step and scope execution confirmation.
//!
//! Executing a step or a scope hands control to the Coder role, so the dialog
//! exposes two keyboard targets — the embedded Coder model list and the action
//! buttons — and `Tab` cycles through them in visual order. The plan states every
//! step's instructions itself, so the dialog carries no directives editor: the
//! list owns bare digits, arrows, and paging only while it holds focus
//! (c-4, Conventions §6, §7).

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::InputResult;
use crate::rpc::RpcConnection;

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
    let mut target = match st.confirm_execute_scope.take() {
        Some(target) => target,
        None => return Ok(None),
    };

    let primary = st.tui_config.input.primary_modifier;
    let meta = st.tui_config.input.meta_modifier;
    let visible_rows = crate::dialogs::render::execute_scope_dialog_geometry(
        st.last_frame_area,
        target.selector.len(),
        target.steps.len() == 1,
    )
    .model_rows
    .height as usize;

    // The C-M / M-M chord always switches the Coder; the arrows browse the list
    // only while the list owns focus.
    if target
        .selector
        .handle_key(key, primary, meta, !target.focus.is_model_list())
    {
        st.confirm_execute_scope = Some(target);
        return Ok(Some(InputResult::Continue));
    }

    // Bare digits, arrows, and paging act on the model list only while it owns
    // focus, so the action bar never forwards a bare digit to the picker.
    if target.focus.is_model_list() && target.selector.handle_list_key(key, visible_rows) {
        st.confirm_execute_scope = Some(target);
        return Ok(Some(InputResult::Continue));
    }

    let is_ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::ALT);

    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);

    // C-Enter accelerator unconditionally executes from any section.
    if matches!(key.code, KeyCode::Enter) && is_cmd {
        super::confirm_execute_scope(state, st, server_writer, target).await?;
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
        // limited to the action bar, so a bare letter pressed over the Coder
        // list never confirms or cancels the launch (Conventions §5).
        KeyCode::Char('y') | KeyCode::Char('Y') if target.focus.is_actions() => {
            super::confirm_execute_scope(state, st, server_writer, target).await?;
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
            st.confirm_execute_scope = Some(target);
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
            st.confirm_execute_scope = Some(target);
            Ok(Some(InputResult::Continue))
        }
        // Horizontal stepping moves between the action buttons.
        KeyCode::Left | KeyCode::Right if target.focus.is_actions() => {
            target.confirm_button = match target.confirm_button {
                ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
                ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
            };
            st.confirm_execute_scope = Some(target);
            Ok(Some(InputResult::Continue))
        }
        // Execution spends tokens, so it starts only from the focused `Execute`
        // button; the safe `Cancel` default dismisses the dialog.
        KeyCode::Enter | KeyCode::Char(' ')
            if !has_shift && !has_alt && target.focus.is_actions() =>
        {
            if target.confirm_button == ConfirmDialogButton::Confirm {
                super::confirm_execute_scope(state, st, server_writer, target).await?;
            } else {
                st.confirm_button = ConfirmDialogButton::Cancel;
            }
            Ok(Some(InputResult::Continue))
        }
        _ => {
            st.confirm_execute_scope = Some(target);
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
        st.confirm_execute_scope = Some(ConfirmExecuteScopeState::new(
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
            .confirm_execute_scope
            .clone()
            .expect("dialog stays open")
    }

    async fn set_focus(state: &Arc<Mutex<AppState>>, focus: ExecuteScopeFocus) {
        let mut guard = state.lock().await;
        if let Some(target) = guard.confirm_execute_scope.as_mut() {
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
        assert!(guard.confirm_execute_scope.is_none());
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
        assert!(guard.confirm_execute_scope.is_none());
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
        assert!(guard.confirm_execute_scope.is_none());
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }
}
