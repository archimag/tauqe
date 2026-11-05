//! Keyboard routing for the two-phase plan-synthesis dialogs.
//!
//! The plan refinement form and the plan creation form stack the same sections
//! — the Planner ladder, the Critic ladder with its disable switch, the
//! instructions editor, and the action bar — so their keyboard contract lives
//! in the shared core at [`super::plan_form`]. This adapter keeps only what
//! belongs to the refinement target itself: the take guard, the dialog
//! geometry, the hoisted clipboard read, and the launch it dispatches.

use std::sync::Arc;

use crossterm::event::KeyEvent;
use tauqe_protocol::methods;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::InputResult;
use crate::rpc::RpcConnection;

use super::plan_form::{handle_plan_form_key, hoisted_plan_form_yank, PlanFormKeyOutcome};

/// Routes key events for the plan refinement form.
pub(super) async fn handle_refine_dialog(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut dialog = match st.modals.refine_dialog.take() {
        Some(dialog) => dialog,
        None => return Ok(None),
    };

    let geometry = crate::dialogs::render::refine_dialog_geometry(
        st.last_frame_area,
        dialog.planner.len(),
        dialog.critic.len(),
    );

    match handle_plan_form_key(
        &mut dialog,
        key,
        &st.tui_config.input,
        geometry.planner_rows.height as usize,
        geometry.critic_rows.height as usize,
    ) {
        PlanFormKeyOutcome::Submit => {
            return submit_refine_dialog(state, st, dialog, server_writer).await;
        }
        // The paste buffer is a native round trip bounded at
        // NATIVE_CLIPBOARD_TIMEOUT, so the shared helper releases the state
        // guard around it and applies the answer after the re-lock (c-1).
        PlanFormKeyOutcome::Yank => {
            hoisted_plan_form_yank(
                state,
                st,
                dialog,
                |st, dialog| st.modals.refine_dialog = Some(dialog),
                |st| st.modals.refine_dialog.take(),
            )
            .await;
        }
        // Safe cancellation returns the shared button to its default and drops
        // the dialog without a request.
        PlanFormKeyOutcome::Dismiss => {
            st.modals.confirm_button = ConfirmDialogButton::Cancel;
        }
        PlanFormKeyOutcome::Consumed => {
            st.modals.refine_dialog = Some(dialog);
        }
    }
    Ok(Some(InputResult::Continue))
}

/// Validates and submits the directed plan modification request. Shared by the
/// keyboard handler and the mouse `Start Refinement` button so the ordering
/// stays consistent: resolve the optional instructions, the selected Planner,
/// and the selected Critic, flag the plan as refining, then hand the turn to
/// the REPL view. If the model is already busy the dialog simply stays open.
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
        st.modals.refine_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    let instructions_raw = dialog.instructions_editor.get_text().trim().to_string();
    let instructions = if instructions_raw.is_empty() {
        None
    } else {
        Some(instructions_raw)
    };
    // The selected Planner and Critic rungs travel whole: the model together
    // with the reasoning depth it was configured with, so the architects the
    // developer picked are the architects that run. A skipped critique phase
    // sends no critic at all (c-typed-plan-mutation-dialog).
    let selected_planner =
        dialog
            .planner
            .selected_entry()
            .map(|entry| tauqe_protocol::ModelSelection::Tier {
                model: entry.model.clone(),
                effort: entry.effort,
            });
    let skip_critique = dialog.skip_critique;
    let selected_critic = if skip_critique {
        None
    } else {
        dialog
            .critic
            .selected_entry()
            .map(|entry| tauqe_protocol::ModelSelection::Tier {
                model: entry.model.clone(),
                effort: entry.effort,
            })
    };

    let params = tauqe_protocol::PlanRefineParams {
        plan_id: dialog.plan_id.clone(),
        model: selected_planner,
        critic_model: selected_critic,
        skip_critique,
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
    let blocked = match st.modals.refine_blocked.take() {
        Some(b) => b,
        None => return Ok(None),
    };

    match super::resolve_confirm(&mut st, key) {
        super::ConfirmOutcome::Toggled | super::ConfirmOutcome::Ignored => {
            st.modals.refine_blocked = Some(blocked);
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
/// to the REPL view. When the model is already busy the dialog is restored so
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
        st.modals.refine_blocked = Some(blocked);
        return Ok(());
    }

    let instructions = Some(format!(
        "Replan uncompleted horizon addressing blockers:\n{}",
        blocked.reason
    ));
    let params = tauqe_protocol::PlanRefineParams {
        plan_id: blocked.plan_id.clone(),
        model: None,
        critic_model: None,
        skip_critique: false,
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
    use tauqe_protocol::{ModelEntry, ModelRef};
    use tokio::sync::Mutex;

    use crate::app::{test_state, AppState, ConfirmDialogButton, ViewMode};
    use crate::rpc::RpcConnection;
    use crate::views::plans::{PlanFormFocus, PlanRefineBlockedState, PlanRefineDialogState};
    fn blocked_state() -> AppState {
        let mut st = test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan::from_parts(
            "plan-1".to_string(),
            "Test Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        )];
        st.modals.refine_blocked = Some(PlanRefineBlockedState {
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
            guard.modals.refine_blocked.is_some(),
            "the dialog must stay open while the focus moves"
        );
        assert_eq!(guard.modals.confirm_button, ConfirmDialogButton::Confirm);
    }

    #[tokio::test]
    async fn enter_on_focused_confirm_initiates_replanning() {
        let state = Arc::new(Mutex::new(blocked_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // Focus the `Replan Horizon` button, then activate it.
        press(&state, &mut conn, KeyCode::Tab).await;
        press(&state, &mut conn, KeyCode::Enter).await;

        let guard = state.lock().await;
        assert!(guard.modals.refine_blocked.is_none());
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
        assert!(guard.modals.refine_blocked.is_none());

        // `n` dismisses without replanning.
        let dismiss = Arc::new(Mutex::new(blocked_state()));
        let mut conn_dismiss = RpcConnection::new(Vec::<u8>::new());
        press(&dismiss, &mut conn_dismiss, KeyCode::Char('n')).await;
        let guard = dismiss.lock().await;
        assert!(!guard.plans_view.refining);
        assert!(guard.modals.refine_blocked.is_none());
    }

    #[tokio::test]
    async fn enter_on_default_cancel_dismisses_safely() {
        let state = Arc::new(Mutex::new(blocked_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // The default focus is the safe `Cancel`; Enter dismisses safely.
        press(&state, &mut conn, KeyCode::Enter).await;

        let guard = state.lock().await;
        assert!(!guard.plans_view.refining);
        assert!(guard.modals.refine_blocked.is_none());
    }

    #[tokio::test]
    async fn blocked_dialog_confirm_when_busy_warns_and_keeps_dialog() {
        let mut st = blocked_state();
        st.plans_view.refining = true;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        press(&state, &mut conn, KeyCode::Char('y')).await;

        let guard = state.lock().await;
        assert!(guard.modals.refine_blocked.is_some());
        assert_eq!(
            guard.active_notification().map(|n| n.text.as_str()),
            Some("Model is currently busy; wait for operation to finish")
        );
    }
    fn architect(name: &str) -> ModelEntry {
        ModelEntry::new(ModelRef::openrouter(name), None)
    }

    /// A refinement form over two configured Architect entries, so the focus
    /// walk and the model-switch chord are both live (c-3, c-4).
    fn refine_state() -> AppState {
        let mut st = test_state();
        st.view_mode = ViewMode::Plans;
        st.modals.refine_dialog = Some(PlanRefineDialogState::new(
            "plan-1".to_string(),
            "Plan One".to_string(),
            vec![architect("architect-a"), architect("architect-b")],
        ));
        st
    }

    async fn press_dialog(state: &Arc<Mutex<AppState>>, conn: &mut RpcConnection, key: KeyEvent) {
        let guard = state.lock().await;
        super::handle_refine_dialog(key, state, guard, conn)
            .await
            .expect("in-memory dispatch never fails");
    }

    async fn open_dialog(state: &Arc<Mutex<AppState>>) -> PlanRefineDialogState {
        state
            .lock()
            .await
            .modals
            .refine_dialog
            .clone()
            .expect("dialog stays open")
    }

    async fn set_focus(state: &Arc<Mutex<AppState>>, focus: PlanFormFocus) {
        let mut guard = state.lock().await;
        if let Some(dialog) = guard.modals.refine_dialog.as_mut() {
            dialog.focus = focus;
        }
    }

    async fn set_confirm_button(state: &Arc<Mutex<AppState>>, button: ConfirmDialogButton) {
        let mut guard = state.lock().await;
        if let Some(dialog) = guard.modals.refine_dialog.as_mut() {
            dialog.confirm_button = button;
        }
    }

    /// `C-Enter` submits the refinement from the editor that owns the keyboard,
    /// so the developer never has to walk the focus to the action bar.
    #[tokio::test]
    async fn ctrl_enter_submits_the_refinement_from_the_editor() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        {
            let mut guard = state.lock().await;
            let dialog = guard
                .modals
                .refine_dialog
                .as_mut()
                .expect("dialog must be open");
            assert_eq!(dialog.focus, PlanFormFocus::InstructionsEditor);
            dialog
                .instructions_editor
                .insert_str("split the first step");
        }

        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("plan/refine"), "{payload}");
        assert!(payload.contains("plan-1"), "{payload}");
        assert!(payload.contains("split the first step"), "{payload}");

        let guard = state.lock().await;
        assert!(guard.modals.refine_dialog.is_none());
        assert!(guard.plans_view.refining);
        assert_eq!(guard.plans_view.refining_plan_id.as_deref(), Some("plan-1"));
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }

    /// The shared action-bar vocabulary reaches the refinement form: `y`
    /// confirms and `n` cancels without a request.
    #[tokio::test]
    async fn y_confirms_from_the_actions_and_n_cancels() {
        let confirm = Arc::new(Mutex::new(refine_state()));
        let mut confirm_conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&confirm, PlanFormFocus::Actions).await;
        press_dialog(
            &confirm,
            &mut confirm_conn,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        )
        .await;
        let payload = String::from_utf8(confirm_conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("plan/refine"), "{payload}");
        let guard = confirm.lock().await;
        assert!(guard.modals.refine_dialog.is_none());
        assert!(guard.plans_view.refining);
        drop(guard);

        let cancel = Arc::new(Mutex::new(refine_state()));
        let mut cancel_conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&cancel, PlanFormFocus::Actions).await;
        press_dialog(
            &cancel,
            &mut cancel_conn,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        )
        .await;
        let guard = cancel.lock().await;
        assert!(guard.modals.refine_dialog.is_none());
        assert!(!guard.plans_view.refining);
        assert_eq!(guard.view_mode, ViewMode::Plans);
        drop(guard);
        assert!(cancel_conn.written().is_empty());
    }

    /// Dismissal returns the shared confirm button to its safe default, so the
    /// next dialog opens on `Cancel` instead of inheriting `Refine`.
    #[tokio::test]
    async fn esc_dismisses_and_resets_the_shared_confirm_button() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, PlanFormFocus::Actions).await;
        set_confirm_button(&state, ConfirmDialogButton::Confirm).await;

        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await;

        let guard = state.lock().await;
        assert!(guard.modals.refine_dialog.is_none());
        assert_eq!(guard.modals.confirm_button, ConfirmDialogButton::Cancel);
        assert!(!guard.plans_view.refining);
        drop(guard);
        assert!(conn.written().is_empty());
    }

    /// The letter accelerators are bare presses: `Ctrl+Y` is the editor's yank
    /// chord, so it must neither start a token-spending refinement nor dismiss
    /// the dialog that still holds the developer's instructions
    /// (Conventions §5.2).
    #[tokio::test]
    async fn ctrl_y_from_the_actions_writes_no_request_and_keeps_the_dialog() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, PlanFormFocus::Actions).await;

        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
        )
        .await;

        assert!(
            conn.written().is_empty(),
            "a command chord must never start a refinement"
        );
        let open = open_dialog(&state).await;
        assert_eq!(open.focus, PlanFormFocus::Actions);
        let guard = state.lock().await;
        assert!(!guard.plans_view.refining);
        assert_eq!(guard.view_mode, ViewMode::Plans);
    }

    /// `Ctrl+Q` and `Ctrl+N` are command chords, not the dialog's dismissal or
    /// cancellation accelerators, so neither may discard typed instructions.
    #[tokio::test]
    async fn ctrl_q_and_ctrl_n_do_not_dismiss_the_dialog() {
        for character in ['q', 'n'] {
            let state = Arc::new(Mutex::new(refine_state()));
            let mut conn = RpcConnection::new(Vec::<u8>::new());
            set_focus(&state, PlanFormFocus::Actions).await;

            press_dialog(
                &state,
                &mut conn,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
            )
            .await;

            assert!(
                state.lock().await.modals.refine_dialog.is_some(),
                "Ctrl+{character} must not dismiss the dialog"
            );
            assert!(conn.written().is_empty());
        }
    }

    /// The `Shift+Tab` spelling a terminal reports as `Tab` carrying `SHIFT`
    /// walks the focus backward, exactly as the dedicated `BackTab` code does
    /// (Conventions §6.2).
    #[tokio::test]
    async fn tab_with_shift_cycles_focus_backward() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, PlanFormFocus::InstructionsEditor).await;

        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        )
        .await;

        let open = open_dialog(&state).await;
        assert_eq!(open.focus, PlanFormFocus::CriticList);
        assert!(conn.written().is_empty());
    }

    /// A bare letter typed into the editor stays literal text, so the
    /// accelerators never swallow the developer's instructions (Conventions §5).
    #[tokio::test]
    async fn q_n_and_y_typed_in_the_editor_stay_literal_text() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        for character in ['q', 'n', 'y'] {
            press_dialog(
                &state,
                &mut conn,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            )
            .await;
        }

        let open = open_dialog(&state).await;
        assert_eq!(open.instructions_editor.get_text(), "qny");
        assert_eq!(open.focus, PlanFormFocus::InstructionsEditor);
        assert!(conn.written().is_empty());
    }

    /// A bare digit selects an Architect rung only from the ladder; in the
    /// editor it is literal text (c-4, Conventions §5).
    #[tokio::test]
    async fn digits_select_in_the_list_but_type_in_the_editor() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, PlanFormFocus::PlannerList).await;
        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
        )
        .await;
        let selected = open_dialog(&state).await;
        assert_eq!(selected.planner.selected_index(), 1);
        assert!(selected.instructions_editor.is_empty());

        set_focus(&state, PlanFormFocus::InstructionsEditor).await;
        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
        )
        .await;
        let typed = open_dialog(&state).await;
        assert_eq!(typed.instructions_editor.get_text(), "2");
        assert_eq!(
            typed.planner.selected_index(),
            1,
            "typing a digit must not switch the architect"
        );
    }

    /// The AltGr compose chord (`Ctrl+Alt`) belongs to neither command modifier,
    /// so a composed character reaches no accelerator: it neither selects an
    /// Architect in the ladder nor confirms, cancels, or submits from the action
    /// bar (Conventions §5.2).
    #[tokio::test]
    async fn compose_chord_reaches_neither_the_accelerators_nor_the_ladder() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let compose = KeyModifiers::CONTROL | KeyModifiers::ALT;

        set_focus(&state, PlanFormFocus::PlannerList).await;
        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('2'), compose),
        )
        .await;
        assert_eq!(
            open_dialog(&state).await.planner.selected_index(),
            0,
            "a composed digit must not select an Architect"
        );

        set_focus(&state, PlanFormFocus::Actions).await;
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('q'),
            KeyCode::Enter,
        ] {
            press_dialog(&state, &mut conn, KeyEvent::new(code, compose)).await;
        }

        let open = open_dialog(&state).await;
        assert_eq!(open.focus, PlanFormFocus::Actions);
        assert_eq!(open.confirm_button, ConfirmDialogButton::Cancel);
        assert!(!state.lock().await.plans_view.refining);
        assert!(conn.written().is_empty());
    }
    /// The selected Planner and Critic travel on the wire with the skip flag,
    /// so the refinement dialog configures both phases of the synthesis
    /// (c-typed-plan-mutation-dialog).
    #[tokio::test]
    async fn ctrl_enter_sends_the_planner_critic_and_skip_flag() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        {
            let mut guard = state.lock().await;
            let dialog = guard.modals.refine_dialog.as_mut().expect("dialog");
            dialog.planner.select_index(1);
            dialog.critic.select_index(1);
        }

        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("plan/refine"), "{payload}");
        assert!(payload.contains("architect-b"), "{payload}");
        assert!(payload.contains("\"skip_critique\":false"), "{payload}");
    }

    /// Disabling the critique phase drops the critic selection from the request
    /// instead of sending a contradictory pair (c-typed-plan-mutation-dialog).
    #[tokio::test]
    async fn skipping_the_critique_sends_no_critic_model() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        {
            let mut guard = state.lock().await;
            let dialog = guard.modals.refine_dialog.as_mut().expect("dialog");
            dialog.skip_critique = true;
        }

        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("plan/refine"), "{payload}");
        assert!(payload.contains("\"skip_critique\":true"), "{payload}");
        assert!(!payload.contains("critic_model"), "{payload}");
    }

    /// The critic ladder is a first-class section: a bare digit selects its rung
    /// while it owns the keyboard, and Space toggles the independent audit
    /// phase instead of launching the turn (Conventions §5, §6).
    #[tokio::test]
    async fn critic_section_selects_and_toggles_the_critique() {
        let state = Arc::new(Mutex::new(refine_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, PlanFormFocus::CriticList).await;

        press_dialog(&state, &mut conn, key(KeyCode::Char('2'))).await;
        let selected = open_dialog(&state).await;
        assert_eq!(selected.critic.selected_index(), 1);
        assert!(!selected.skip_critique);

        press_dialog(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        )
        .await;
        let toggled = open_dialog(&state).await;
        assert!(toggled.skip_critique);
        assert_eq!(toggled.critic.selected_index(), 1);
        assert!(conn.written().is_empty());
    }
}
