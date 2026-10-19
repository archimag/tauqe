use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::methods;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::InputResult;
use crate::rpc::RpcConnection;

/// Routes key events for the plan refine dialog: the embedded Architect model
/// list, the instructions editor, and the confirm/cancel button pair. Focus
/// cycles through the three sections in visual order with `Tab`; the model list
/// owns bare digits and arrows, while the editor keeps text entry and the
/// `Alt+1..9` / `C-M` selection chords (c-embedded-model-list,
/// c-magit-key-navigation, c-unified-dialog-integration).
pub(super) async fn handle_refine_dialog(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut dialog = match st.plans_view.refine_dialog.take() {
        Some(d) => d,
        None => return Ok(None),
    };

    let primary = st.tui_config.input.primary_modifier;
    let meta = st.tui_config.input.meta_modifier;
    let visible_rows =
        crate::dialogs::render::refine_dialog_geometry(st.last_frame_area, dialog.selector.len())
            .model_rows
            .height as usize;

    // The C-M / M-M chord always switches the architect; the arrows browse the
    // list only while the list owns focus, so list navigation and text editing
    // never fight over the same keys (c-magit-key-navigation).
    if dialog
        .selector
        .handle_key(key, primary, meta, !dialog.focus.is_model_list())
    {
        st.plans_view.refine_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    // Bare digits, arrows, and paging act on the model list only while it owns
    // focus; elsewhere the editor keeps those keys.
    if dialog.focus.is_model_list() && dialog.selector.handle_list_key(key, visible_rows) {
        st.plans_view.refine_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    let is_ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::ALT);

    // Alt+digit picks an architect by position without moving the cursor out of
    // the instructions editor (c-magit-key-navigation).
    if dialog.focus.is_instructions_editor()
        && !is_ctrl_alt
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT)
    {
        if let KeyCode::Char(c @ '1'..='9') = key.code {
            let index = c as usize - '1' as usize;
            if index < dialog.selector.len() {
                dialog.selector.select_index(index);
                st.plans_view.refine_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
        }
    }

    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);

    // C-Enter accelerator unconditionally submits from any section.
    if matches!(key.code, KeyCode::Enter) && is_cmd {
        return submit_refine_dialog(state, st, dialog, server_writer).await;
    }

    match key.code {
        KeyCode::Esc => Ok(Some(InputResult::Continue)),
        KeyCode::Char('q') if dialog.focus.is_actions() => Ok(Some(InputResult::Continue)),
        KeyCode::Tab => {
            if dialog.focus.is_actions() && dialog.confirm_button == ConfirmDialogButton::Cancel {
                dialog.confirm_button = ConfirmDialogButton::Confirm;
            } else {
                if dialog.focus.is_actions() {
                    dialog.confirm_button = ConfirmDialogButton::Cancel;
                }
                dialog.focus.cycle_next(dialog.selector.len());
            }
            st.plans_view.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
        KeyCode::BackTab => {
            if dialog.focus.is_actions() && dialog.confirm_button == ConfirmDialogButton::Confirm {
                dialog.confirm_button = ConfirmDialogButton::Cancel;
            } else {
                dialog.focus.cycle_prev(dialog.selector.len());
                if dialog.focus.is_actions() {
                    dialog.confirm_button = ConfirmDialogButton::Confirm;
                }
            }
            st.plans_view.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
        // Horizontal stepping moves between the action buttons.
        KeyCode::Left | KeyCode::Right if dialog.focus.is_actions() => {
            dialog.confirm_button = match dialog.confirm_button {
                ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
                ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
            };
            st.plans_view.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
        // Starting a refinement begins from `Confirm`. Safe `Cancel` dismisses the dialog.
        KeyCode::Enter | KeyCode::Char(' ')
            if !has_shift && !has_alt && dialog.focus.is_actions() =>
        {
            if dialog.confirm_button == ConfirmDialogButton::Confirm {
                submit_refine_dialog(state, st, dialog, server_writer).await
            } else {
                // Safe cancel dismisses without re-attaching the dialog.
                Ok(Some(InputResult::Continue))
            }
        }
        _ => {
            if dialog.focus.is_instructions_editor() {
                crate::input::handle_editor_key(&mut dialog.instructions_editor, key, is_cmd, true);
            }
            st.plans_view.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
    }
}

/// Validates and submits the plan refinement request. Shared by the keyboard
/// handler and the mouse `Start Refinement` button so the ordering stays
/// consistent: resolve the optional instructions and the selected architect,
/// flag the plan as refining, then hand the turn to the Develop view. If the
/// model is already busy the dialog simply stays open.
///
/// The optimistic transition is applied through `start_turn`, so a failed
/// request rolls the refining flag, the view mode, and the turn state back
/// instead of leaving the Plans view stuck with no server turn to clear it
/// (c-5).
pub(crate) async fn submit_refine_dialog(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    dialog: crate::views::plans::PlanRefineDialogState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    if st.model.is_busy() || st.review.running || st.plans_view.refining {
        st.notify_warning(crate::i18n::active_t("notify.model_busy"));
        st.plans_view.refine_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    let instructions_raw = dialog.instructions_editor.get_text().trim().to_string();
    let instructions = if instructions_raw.is_empty() {
        None
    } else {
        Some(instructions_raw)
    };
    let selected_model = dialog
        .selector
        .selected_entry()
        .map(|entry| tauqe_protocol::ModelSelection::Specific(entry.model.clone()));

    let params = tauqe_protocol::PlanRefineParams {
        plan_id: dialog.plan_id.clone(),
        model: selected_model,
        instructions,
    };
    let params = serde_json::to_value(params)?;

    let plan_id = dialog.plan_id.clone();
    super::start_turn(
        state,
        st,
        server_writer,
        methods::PLAN_REFINE,
        params,
        move |st| super::begin_plan_refine_turn(st, plan_id),
    )
    .await;
    Ok(Some(InputResult::Continue))
}

/// Handles the "refine blocked" modal through the shared confirmation contract.
///
/// The modal is rendered by `render_modal_confirm`, so its keyboard contract is
/// the standard one: `Tab` / `←` / `→` move the focused button, `Enter` /
/// `Space` activate it, `y` accepts, and `n` / `Esc` dismiss. Routing the key
/// through `resolve_confirm` keeps a single source of truth for that contract:
/// accepting `Replan Horizon (Y)` triggers horizon replanning with the blocker
/// context, while dismissing simply closes the modal safely.
pub(super) async fn handle_refine_blocked(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let blocked = match st.plans_view.refine_blocked.take() {
        Some(b) => b,
        None => return Ok(None),
    };

    match super::resolve_confirm(&mut st, key) {
        super::ConfirmOutcome::Toggled => {
            st.plans_view.refine_blocked = Some(blocked);
        }
        super::ConfirmOutcome::Dismissed => {}
        super::ConfirmOutcome::Accepted => {
            submit_refine_blocked(state, st, blocked, server_writer).await?;
        }
    }

    Ok(Some(InputResult::Continue))
}

/// Initiates horizon replanning from the "refinement blocked" modal.
///
/// Shared by the keyboard and mouse confirmation paths so a click starts the
/// exact same replanning turn as `Enter`: the blocker reason is folded into the
/// refine instructions, the plan is flagged as refining, and the turn hands off
/// to the Develop view. When the model is already busy the dialog is restored so
/// the developer can retry.
///
/// The optimistic transition is applied through `start_turn`, so a failed
/// request rolls the refining flag, the view mode, and the turn state back
/// instead of leaving the Plans view stuck with no server turn to clear it
/// (c-5).
pub(crate) async fn submit_refine_blocked(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    blocked: crate::views::plans::PlanRefineBlockedState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    if st.model.is_busy() || st.review.running || st.plans_view.refining {
        st.notify_warning(crate::i18n::active_t("notify.model_busy"));
        st.plans_view.refine_blocked = Some(blocked);
        return Ok(());
    }

    let instructions = Some(format!(
        "Replan uncompleted horizon addressing blockers:\n{}",
        blocked.reason
    ));
    let params = tauqe_protocol::PlanRefineParams {
        plan_id: blocked.plan_id.clone(),
        model: None,
        instructions,
    };
    let params = serde_json::to_value(params)?;

    let plan_id = blocked.plan_id.clone();
    super::start_turn(
        state,
        st,
        server_writer,
        methods::PLAN_REFINE,
        params,
        move |st| super::begin_plan_refine_turn(st, plan_id),
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::Mutex;

    use crate::app::{test_state, AppState, ConfirmDialogButton, ViewMode};
    use crate::rpc::RpcConnection;
    use crate::views::plans::PlanRefineBlockedState;

    fn blocked_state() -> AppState {
        let mut st = test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan {
            id: "plan-1".to_string(),
            title: "Test Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
            concept: vec![],
            items: vec![],
        }];
        st.plans_view.refine_blocked = Some(PlanRefineBlockedState {
            plan_id: "plan-1".to_string(),
            plan_title: "Test Plan".to_string(),
            reason: "Blocking contradiction".to_string(),
        });
        st
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    async fn press(state: &Arc<Mutex<AppState>>, conn: &mut RpcConnection, code: KeyCode) {
        let guard = state.lock().await;
        super::handle_refine_blocked(key(code), state, guard, conn)
            .await
            .expect("in-memory dispatch never fails");
    }

    #[tokio::test]
    async fn tab_toggles_focus_and_keeps_the_dialog_open() {
        let state = Arc::new(Mutex::new(blocked_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        press(&state, &mut conn, KeyCode::Tab).await;

        let guard = state.lock().await;
        assert!(
            guard.plans_view.refine_blocked.is_some(),
            "the dialog must stay open while the focus moves"
        );
        assert_eq!(guard.confirm_button, ConfirmDialogButton::Confirm);
    }

    #[tokio::test]
    async fn enter_on_focused_confirm_initiates_replanning() {
        let state = Arc::new(Mutex::new(blocked_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // Focus the `Replan Horizon` button, then activate it.
        press(&state, &mut conn, KeyCode::Tab).await;
        press(&state, &mut conn, KeyCode::Enter).await;

        let guard = state.lock().await;
        assert!(guard.plans_view.refine_blocked.is_none());
        assert!(guard.plans_view.refining);
        assert_eq!(guard.plans_view.refining_plan_id.as_deref(), Some("plan-1"));
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }

    #[tokio::test]
    async fn fast_select_accepts_and_dismisses() {
        // `y` accepts even while the safe `Cancel` button holds focus.
        let accept = Arc::new(Mutex::new(blocked_state()));
        let mut conn_accept = RpcConnection::new(Vec::<u8>::new());
        press(&accept, &mut conn_accept, KeyCode::Char('y')).await;
        let guard = accept.lock().await;
        assert!(guard.plans_view.refining);
        assert!(guard.plans_view.refine_blocked.is_none());

        // `n` dismisses without replanning.
        let dismiss = Arc::new(Mutex::new(blocked_state()));
        let mut conn_dismiss = RpcConnection::new(Vec::<u8>::new());
        press(&dismiss, &mut conn_dismiss, KeyCode::Char('n')).await;
        let guard = dismiss.lock().await;
        assert!(!guard.plans_view.refining);
        assert!(guard.plans_view.refine_blocked.is_none());
    }

    #[tokio::test]
    async fn enter_on_default_cancel_dismisses_safely() {
        let state = Arc::new(Mutex::new(blocked_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // The default focus is the safe `Cancel`; Enter dismisses safely.
        press(&state, &mut conn, KeyCode::Enter).await;

        let guard = state.lock().await;
        assert!(!guard.plans_view.refining);
        assert!(guard.plans_view.refine_blocked.is_none());
    }

    #[tokio::test]
    async fn blocked_dialog_confirm_when_busy_warns_and_keeps_dialog() {
        let mut st = blocked_state();
        st.plans_view.refining = true;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        press(&state, &mut conn, KeyCode::Char('y')).await;

        let guard = state.lock().await;
        assert!(guard.plans_view.refine_blocked.is_some());
        assert_eq!(
            guard.active_notification().map(|n| n.text.as_str()),
            Some("Model is currently busy; wait for operation to finish")
        );
    }
}
