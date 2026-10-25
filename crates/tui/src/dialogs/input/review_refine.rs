use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::methods;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};
use crate::views::review::RefineTarget;

/// Routes key events for the review-finding decomposition dialog.
///
/// The dialog exposes three keyboard targets — the embedded Architect model
/// list, the instructions editor, and the action buttons — and `Tab` cycles
/// through them in visual order. The model list owns bare digits and arrows
/// only while it holds focus, so list navigation and text entry never fight
/// over the same keys (Conventions §6, §7).
pub(super) async fn handle_review_refine_dialog(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut dialog = match st.review.refine_dialog.take() {
        Some(dialog) => dialog,
        None => return Ok(None),
    };

    let primary = st.tui_config.input.primary_modifier;
    let meta = st.tui_config.input.meta_modifier;
    let visible_rows = crate::dialogs::render::refine::refine_dialog_geometry(
        st.last_frame_area,
        dialog.selector.len(),
    )
    .model_rows
    .height as usize;

    // The C-M / M-M chord always switches the architect; the arrows browse the
    // list only while the list owns focus.
    if dialog
        .selector
        .handle_key(key, primary, meta, !dialog.focus.is_model_list())
    {
        st.review.refine_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    // Bare digits, arrows, and paging act on the model list only while it owns
    // focus; elsewhere the instructions editor keeps those keys.
    if dialog.focus.is_model_list() && dialog.selector.handle_list_key(key, visible_rows) {
        st.review.refine_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    let is_ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::ALT);

    // Alt+digit picks an architect by position without moving the cursor out of
    // the instructions editor.
    if dialog.focus.is_instructions_editor()
        && !is_ctrl_alt
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT)
    {
        if let KeyCode::Char(c @ '1'..='9') = key.code {
            let index = c as usize - '1' as usize;
            if index < dialog.selector.len() {
                dialog.selector.select_index(index);
                st.review.refine_dialog = Some(dialog);
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
        return submit_review_refine_dialog(state, st, dialog, server_writer).await;
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
            st.review.refine_dialog = Some(dialog);
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
            st.review.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
        // Horizontal stepping moves between the action buttons.
        KeyCode::Left | KeyCode::Right if dialog.focus.is_actions() => {
            dialog.confirm_button = match dialog.confirm_button {
                ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
                ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
            };
            st.review.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
        // Decomposition spends tokens, so it starts only from the focused
        // `Confirm` button; the safe `Cancel` default dismisses the dialog.
        KeyCode::Enter | KeyCode::Char(' ')
            if !has_shift && !has_alt && dialog.focus.is_actions() =>
        {
            if dialog.confirm_button == ConfirmDialogButton::Confirm {
                submit_review_refine_dialog(state, st, dialog, server_writer).await
            } else {
                Ok(Some(InputResult::Continue))
            }
        }
        _ => {
            if dialog.focus.is_instructions_editor() {
                crate::input::handle_editor_key(&mut dialog.instructions_editor, key, is_cmd, true);
            }
            st.review.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
    }
}

/// Dispatches `review/refineItem` with the chosen architect and directives.
///
/// Shared by the keyboard and mouse confirmation paths so a click starts the
/// exact same decomposition turn as `Enter`. The finding is optimistically
/// flagged as refining through the existing single-member queue, so the response
/// dispatcher clears the flag through the same path a batch refinement uses.
pub(crate) async fn submit_review_refine_dialog(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    dialog: crate::views::review::ReviewRefineDialogState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    if st.model.is_busy() || st.review.running || st.review.refining.is_some() {
        st.notify_warning(crate::i18n::active_t("notify.model_busy"));
        st.review.refine_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    let instructions_raw = dialog.instructions_editor.get_text().trim().to_string();
    let instructions = if instructions_raw.is_empty() {
        None
    } else {
        Some(instructions_raw)
    };
    // The selected architect rung travels whole: the model together with the
    // reasoning depth it was configured with, so the architect the developer
    // picked is the architect that runs.
    let selected_model = dialog
        .selected_entry()
        .map(|entry| tauqe_protocol::ModelSelection::Tier {
            model: entry.model.clone(),
            effort: entry.effort,
        });

    let params = tauqe_protocol::ReviewRefineItemParams {
        review_id: Some(dialog.review_id.clone()),
        item_id: dialog.item_id,
        model: selected_model,
        instructions,
    };
    let params = serde_json::to_value(params)?;

    let target = RefineTarget {
        review_id: dialog.review_id,
        item_id: dialog.item_id,
    };
    let previous_refining = st.review.refining.clone();
    st.review.begin_refine_batch(vec![target]);
    st.notify_info(crate::i18n::active_t("notify.review_refining"));
    drop(st);

    if let Err(err) = send_request(server_writer, methods::REVIEW_REFINE_ITEM, params).await {
        let mut st = state.lock().await;
        st.review.refining = previous_refining;
        st.review.refine_queue.clear();
        st.notify_error(
            crate::i18n::active_t("notify.turn_start_failed").replace("{}", &err.to_string()),
        );
    }

    Ok(Some(InputResult::Continue))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::Mutex;

    use crate::app::{test_state, AppState, ConfirmDialogButton};
    use crate::rpc::RpcConnection;
    use crate::views::plans::RefineDialogFocus;
    use crate::views::review::ReviewRefineDialogState;

    use tauqe_protocol::{ModelEntry, ModelRef};

    fn architect(name: &str) -> ModelEntry {
        ModelEntry::new(ModelRef::openrouter(name), None)
    }

    fn state_with_dialog() -> AppState {
        let mut st = test_state();
        st.review.refine_dialog = Some(ReviewRefineDialogState::new(
            "rev-1".to_string(),
            "Session rev-1".to_string(),
            2,
            "Missing auth guard".to_string(),
            vec![architect("arch-a"), architect("arch-b")],
        ));
        st
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    async fn press(state: &Arc<Mutex<AppState>>, conn: &mut RpcConnection, event: KeyEvent) {
        let guard = state.lock().await;
        super::handle_review_refine_dialog(event, state, guard, conn)
            .await
            .expect("in-memory dispatch never fails");
    }

    async fn dialog(state: &Arc<Mutex<AppState>>) -> ReviewRefineDialogState {
        state
            .lock()
            .await
            .review
            .refine_dialog
            .clone()
            .expect("dialog stays open")
    }

    async fn set_focus(state: &Arc<Mutex<AppState>>, focus: RefineDialogFocus) {
        let mut guard = state.lock().await;
        if let Some(dialog) = guard.review.refine_dialog.as_mut() {
            dialog.focus = focus;
        }
    }

    #[tokio::test]
    async fn tab_cycles_focus_through_list_editor_and_actions() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::ModelList).await;

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        assert_eq!(
            dialog(&state).await.focus,
            RefineDialogFocus::InstructionsEditor
        );

        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let actions = dialog(&state).await;
        assert_eq!(actions.focus, RefineDialogFocus::Actions);
        assert_eq!(actions.confirm_button, ConfirmDialogButton::Cancel);

        // The first Tab inside the action bar focuses `Confirm` instead of
        // leaving the bar, so decomposition is never started accidentally.
        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let confirm = dialog(&state).await;
        assert_eq!(confirm.focus, RefineDialogFocus::Actions);
        assert_eq!(confirm.confirm_button, ConfirmDialogButton::Confirm);

        // The next Tab restores the safe default and wraps back to the list.
        press(&state, &mut conn, key(KeyCode::Tab)).await;
        let wrapped = dialog(&state).await;
        assert_eq!(wrapped.focus, RefineDialogFocus::ModelList);
        assert_eq!(wrapped.confirm_button, ConfirmDialogButton::Cancel);
    }

    #[tokio::test]
    async fn backtab_walks_focus_backwards_into_the_actions() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::ModelList).await;
        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        )
        .await;

        let backwards = dialog(&state).await;
        assert_eq!(backwards.focus, RefineDialogFocus::Actions);
        assert_eq!(backwards.confirm_button, ConfirmDialogButton::Confirm);
    }

    #[tokio::test]
    async fn horizontal_arrows_toggle_the_action_buttons() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::Actions).await;

        press(&state, &mut conn, key(KeyCode::Right)).await;
        assert_eq!(
            dialog(&state).await.confirm_button,
            ConfirmDialogButton::Confirm
        );

        press(&state, &mut conn, key(KeyCode::Left)).await;
        assert_eq!(
            dialog(&state).await.confirm_button,
            ConfirmDialogButton::Cancel
        );
    }

    #[tokio::test]
    async fn model_switch_chord_cycles_the_architect_from_the_editor() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::InstructionsEditor).await;

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
        )
        .await;
        let switched = dialog(&state).await;
        assert_eq!(switched.selector.selected_index(), 1);
        assert_eq!(switched.focus, RefineDialogFocus::InstructionsEditor);
        assert!(
            switched.instructions_editor.is_empty(),
            "the chord must never type into the instructions"
        );

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('M'), KeyModifiers::ALT),
        )
        .await;
        assert_eq!(dialog(&state).await.selector.selected_index(), 0);
    }

    #[tokio::test]
    async fn digits_select_in_the_list_but_type_in_the_editor() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::ModelList).await;
        press(&state, &mut conn, key(KeyCode::Char('2'))).await;
        let selected = dialog(&state).await;
        assert_eq!(selected.selector.selected_index(), 1);
        assert!(selected.instructions_editor.is_empty());

        set_focus(&state, RefineDialogFocus::InstructionsEditor).await;
        press(&state, &mut conn, key(KeyCode::Char('2'))).await;
        let typed = dialog(&state).await;
        assert_eq!(typed.instructions_editor.get_text(), "2");
        assert_eq!(
            typed.selector.selected_index(),
            1,
            "typing a digit must not switch the architect"
        );
    }

    #[tokio::test]
    async fn alt_digit_selects_the_architect_from_the_editor() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::InstructionsEditor).await;
        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT),
        )
        .await;

        let picked = dialog(&state).await;
        assert_eq!(picked.selector.selected_index(), 1);
        assert!(picked.instructions_editor.is_empty());
        assert_eq!(picked.focus, RefineDialogFocus::InstructionsEditor);
    }

    #[tokio::test]
    async fn bare_q_types_into_the_editor_instead_of_dismissing() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::InstructionsEditor).await;
        press(&state, &mut conn, key(KeyCode::Char('q'))).await;

        let typed = dialog(&state).await;
        assert_eq!(typed.instructions_editor.get_text(), "q");
    }

    #[tokio::test]
    async fn esc_dismisses_without_dispatching_a_request() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        press(&state, &mut conn, key(KeyCode::Esc)).await;

        let guard = state.lock().await;
        assert!(guard.review.refine_dialog.is_none());
        assert!(guard.review.refining.is_none());
        assert!(conn.written().is_empty());
    }

    #[tokio::test]
    async fn ctrl_enter_submits_selected_architect_and_instructions() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        {
            let mut guard = state.lock().await;
            let dialog = guard
                .review
                .refine_dialog
                .as_mut()
                .expect("dialog must be open");
            dialog.selector.select_index(1);
            dialog
                .instructions_editor
                .insert_str("focus on the auth guard");
            dialog.focus = RefineDialogFocus::InstructionsEditor;
        }

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("review/refineItem"), "{payload}");
        assert!(payload.contains("arch-b"), "{payload}");
        assert!(payload.contains("focus on the auth guard"), "{payload}");
        assert!(payload.contains("\"item_id\":2"), "{payload}");

        let guard = state.lock().await;
        assert!(guard.review.refine_dialog.is_none());
        assert_eq!(guard.review.refining.as_deref(), Some("rev-1"));
    }

    #[tokio::test]
    async fn enter_on_default_cancel_dismisses_without_a_request() {
        let state = Arc::new(Mutex::new(state_with_dialog()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        set_focus(&state, RefineDialogFocus::Actions).await;
        press(&state, &mut conn, key(KeyCode::Enter)).await;

        let guard = state.lock().await;
        assert!(guard.review.refine_dialog.is_none());
        assert!(guard.review.refining.is_none());
        assert!(conn.written().is_empty());
    }

    #[tokio::test]
    async fn submit_when_busy_warns_and_keeps_the_dialog() {
        let mut st = state_with_dialog();
        st.review.refining = Some("rev-other".to_string());
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        press(
            &state,
            &mut conn,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        )
        .await;

        let guard = state.lock().await;
        assert!(guard.review.refine_dialog.is_some());
        assert_eq!(guard.review.refining.as_deref(), Some("rev-other"));
        assert_eq!(
            guard.active_notification().map(|n| n.text.as_str()),
            Some("Model is currently busy; wait for operation to finish")
        );
        assert!(conn.written().is_empty());
    }
}
