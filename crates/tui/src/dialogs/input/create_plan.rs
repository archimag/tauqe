//! Keyboard routing for the plan creation form.
//!
//! The form belongs to the plan-synthesis dialog family, so its keyboard
//! contract lives in the shared core at [`super::plan_form`]: the Planner
//! ladder, the Critic ladder with its disable switch, the directives editor,
//! the action bar, the `Shift+Tab` spelling, and the bare-press guard of the
//! letter accelerators are resolved once for every dialog of the family. This
//! adapter keeps only what belongs to the creation target itself — the take
//! guard, the dialog geometry, the hoisted clipboard read, and the launch it
//! dispatches.

use std::sync::Arc;

use crossterm::event::KeyEvent;
use tauqe_protocol::methods;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::InputResult;
use crate::rpc::RpcConnection;

use super::plan_form::{handle_plan_form_key, hoisted_plan_form_yank, PlanFormKeyOutcome};

/// Routes key events for the plan creation form.
pub(super) async fn handle_create_plan_dialog(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut dialog = match st.modals.create_dialog.take() {
        Some(dialog) => dialog,
        None => return Ok(None),
    };

    let geometry = crate::dialogs::render::create_plan_dialog_geometry(
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
            return submit_create_dialog(state, st, dialog, server_writer).await;
        }
        // The paste buffer is a native round trip bounded at
        // NATIVE_CLIPBOARD_TIMEOUT, so the shared helper releases the state
        // guard around it and applies the answer after the re-lock (c-1).
        PlanFormKeyOutcome::Yank => {
            hoisted_plan_form_yank(
                state,
                st,
                dialog,
                |st, dialog| st.modals.create_dialog = Some(dialog),
                |st| st.modals.create_dialog.take(),
            )
            .await;
        }
        // Safe cancellation returns the shared button to its default and drops
        // the dialog without a request.
        PlanFormKeyOutcome::Dismiss => {
            st.modals.confirm_button = ConfirmDialogButton::Cancel;
        }
        PlanFormKeyOutcome::Consumed => {
            st.modals.create_dialog = Some(dialog);
        }
    }
    Ok(Some(InputResult::Continue))
}

/// Validates and submits the two-phase plan creation request.
///
/// The engineering directives are mandatory: a request without them has
/// nothing to plan, so the handler refuses it while the form still holds the
/// developer's text instead of letting the server reject an empty pipeline.
/// The selected Planner and Critic rungs travel whole — the model together with
/// the reasoning depth it was configured with — so the architects the developer
/// picked are the architects that run; a skipped critique phase sends no critic
/// at all (c-typed-plan-creation-dialog).
///
/// The optimistic transition is applied through `start_turn`, so a failed
/// request rolls the refining flag, the view mode, and the turn state back
/// instead of leaving the Plans view stuck with no server turn to clear it
/// (c-5).
pub(crate) async fn submit_create_dialog(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    dialog: crate::views::plans::PlanCreateDialogState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    if st.model.is_busy() || st.review.running || st.plans_view.refining {
        st.notify_warning(crate::i18n::active_t("notify.model_busy"));
        st.modals.create_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    let directives = dialog.directives_editor.get_text().trim().to_string();
    if directives.is_empty() {
        st.notify_warning(crate::i18n::active_t("notify.plan_create_directives_empty"));
        st.modals.create_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    let planner_model =
        dialog
            .planner
            .selected_entry()
            .map(|entry| tauqe_protocol::ModelSelection::Tier {
                model: entry.model.clone(),
                effort: entry.effort,
            });
    let skip_critique = dialog.skip_critique;
    let critic_model = if skip_critique {
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
    let params = tauqe_protocol::PlanCreatePipelineParams {
        directives,
        planner_model,
        critic_model,
        skip_critique,
    };
    let params = serde_json::to_value(params)?;

    super::start_turn(
        state,
        st,
        server_writer,
        methods::PLAN_CREATE_PIPELINE,
        params,
        super::begin_plan_create_turn,
    )
    .await;
    Ok(Some(InputResult::Continue))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tauqe_protocol::{ModelEntry, ModelRef};
    use tokio::sync::Mutex;

    use crate::app::{test_state, AppState, ConfirmDialogButton, ViewMode};
    use crate::rpc::RpcConnection;
    use crate::views::plans::{PlanCreateDialogState, PlanFormFocus};

    fn architect(name: &str) -> ModelEntry {
        ModelEntry::new(ModelRef::openrouter(name), None)
    }

    fn state_with_form() -> AppState {
        let mut st = test_state();
        st.view_mode = ViewMode::Plans;
        st.modals.create_dialog = Some(PlanCreateDialogState::new(vec![
            architect("architect-a"),
            architect("architect-b"),
        ]));
        st
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    async fn press(state: &Arc<Mutex<AppState>>, conn: &mut RpcConnection, event: KeyEvent) {
        let guard = state.lock().await;
        super::handle_create_plan_dialog(event, state, guard, conn)
            .await
            .expect("in-memory dispatch never fails");
    }

    async fn open_dialog(state: &Arc<Mutex<AppState>>) -> PlanCreateDialogState {
        state
            .lock()
            .await
            .modals
            .create_dialog
            .clone()
            .expect("dialog stays open")
    }

    async fn set_focus(state: &Arc<Mutex<AppState>>, focus: PlanFormFocus) {
        let mut guard = state.lock().await;
        if let Some(dialog) = guard.modals.create_dialog.as_mut() {
            dialog.focus = focus;
        }
    }

    #[tokio::test]
    async fn ctrl_enter_submits_the_typed_directives() {
        let state = Arc::new(Mutex::new(state_with_form()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        {
            let mut guard = state.lock().await;
            let dialog = guard.modals.create_dialog.as_mut().expect("dialog");
            dialog
                .directives_editor
                .insert_str("build the two-phase pipeline");
            dialog.planner.select_index(0);
            dialog.critic.select_index(1);
        }

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("plan/createPipeline"), "{payload}");
        assert!(
            payload.contains("build the two-phase pipeline"),
            "{payload}"
        );
        assert!(payload.contains("architect-b"), "{payload}");
        assert!(payload.contains("\"skip_critique\":false"), "{payload}");

        let guard = state.lock().await;
        assert!(guard.modals.create_dialog.is_none());
        assert!(guard.plans_view.refining);
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }

    #[tokio::test]
    async fn empty_directives_keep_the_form_open() {
        let state = Arc::new(Mutex::new(state_with_form()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        assert!(conn.written().is_empty());
        let guard = state.lock().await;
        assert!(guard.modals.create_dialog.is_some());
        assert!(!guard.plans_view.refining);
        let notification = guard.active_notification().expect("warning");
        assert!(notification.text.contains("engineering directives"));
    }

    #[tokio::test]
    async fn tab_walks_planner_critic_editor_and_actions() {
        let state = Arc::new(Mutex::new(state_with_form()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        assert_eq!(
            open_dialog(&state).await.focus,
            PlanFormFocus::InstructionsEditor
        );

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let actions = open_dialog(&state).await;
        assert_eq!(actions.focus, PlanFormFocus::Actions);
        assert_eq!(actions.confirm_button, ConfirmDialogButton::Cancel);

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let confirm = open_dialog(&state).await;
        assert_eq!(confirm.focus, PlanFormFocus::Actions);
        assert_eq!(confirm.confirm_button, ConfirmDialogButton::Confirm);

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        assert_eq!(open_dialog(&state).await.focus, PlanFormFocus::PlannerList);
        press(&state, &mut conn, key(KeyCode::Tab)).await;
        assert_eq!(open_dialog(&state).await.focus, PlanFormFocus::CriticList);
        press(&state, &mut conn, key(KeyCode::Tab)).await;
        assert_eq!(
            open_dialog(&state).await.focus,
            PlanFormFocus::InstructionsEditor
        );
    }

    #[tokio::test]
    async fn space_in_the_critic_section_toggles_the_critique() {
        let state = Arc::new(Mutex::new(state_with_form()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, PlanFormFocus::CriticList).await;

        press(&state, &mut conn, key(KeyCode::Char(' '))).await;
        assert!(open_dialog(&state).await.skip_critique);
        press(&state, &mut conn, key(KeyCode::Enter)).await;
        assert!(!open_dialog(&state).await.skip_critique);
        assert!(conn.written().is_empty());
    }

    #[tokio::test]
    async fn esc_dismisses_and_resets_the_confirm_button() {
        let state = Arc::new(Mutex::new(state_with_form()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        set_focus(&state, PlanFormFocus::Actions).await;
        state.lock().await.modals.confirm_button = ConfirmDialogButton::Confirm;

        press(&state, &mut conn, key(KeyCode::Esc)).await;

        let guard = state.lock().await;
        assert!(guard.modals.create_dialog.is_none());
        assert_eq!(guard.modals.confirm_button, ConfirmDialogButton::Cancel);
        drop(guard);
        assert!(conn.written().is_empty());
    }
}
