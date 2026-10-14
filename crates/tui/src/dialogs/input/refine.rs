use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::methods;
use tokio::sync::MutexGuard;

use crate::app::{AppState, ConfirmDialogButton, DiscussPlanDialogState};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

/// Routes key events for the plan refine dialog: the embedded Architect model
/// list, the instructions editor, and the confirm/cancel button pair. Focus
/// cycles through the three sections in visual order with `Tab`; the model list
/// owns bare digits and arrows, while the editor keeps text entry and the
/// `Alt+1..9` / `C-M` selection chords (c-embedded-model-list,
/// c-magit-key-navigation, c-unified-dialog-integration).
pub(super) async fn handle_refine_dialog(
    key: KeyEvent,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut dialog = match st.plans_view.refine_dialog.take() {
        Some(d) => d,
        None => return Ok(None),
    };

    let primary = st.tui_config.input.primary_modifier;
    let visible_rows = crate::dialogs::render::refine_dialog_geometry(
        st.last_frame_area,
        dialog.selector.len(),
    )
    .model_rows
    .height as usize;

    // The C-M / Alt+M chord always switches the architect; the arrows browse the
    // list only while the list owns focus, so list navigation and text editing
    // never fight over the same keys (c-magit-key-navigation).
    if dialog
        .selector
        .handle_key(key, primary, !dialog.focus.is_model_list())
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

    let is_ctrl_alt = key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT);

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

    match key.code {
        KeyCode::Esc => Ok(Some(InputResult::Continue)),
        KeyCode::Tab => {
            dialog.focus.cycle_next();
            st.plans_view.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
        KeyCode::BackTab => {
            dialog.focus.cycle_prev();
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
        // Starting a refinement is a long-running operation: it begins only from
        // the focused `Confirm` button. The safe `Cancel` default dismisses
        // without starting (Conventions §7).
        KeyCode::Enter if !has_shift && !has_alt && dialog.focus.is_actions() => {
            if dialog.confirm_button == ConfirmDialogButton::Confirm {
                return submit_refine_dialog(st, dialog, server_writer).await;
            }
            st.plans_view.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
        _ => {
            if dialog.focus.is_instructions_editor() {
                crate::input::handle_editor_key(
                    &mut dialog.instructions_editor,
                    key,
                    is_cmd,
                    true,
                );
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
pub(crate) async fn submit_refine_dialog(
    mut st: MutexGuard<'_, AppState>,
    dialog: crate::views::plans::PlanRefineDialogState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    if st.model.is_busy() || st.review.running || st.plans_view.refining {
        st.notify_warning("Model is currently busy; wait for operation to finish");
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

    st.plans_view.refining = true;
    st.plans_view.refining_plan_id = Some(dialog.plan_id.clone());
    super::begin_develop_turn(&mut st);

    drop(st);
    send_request(
        server_writer,
        methods::PLAN_REFINE,
        serde_json::to_value(params)?,
    )
    .await?;
    Ok(Some(InputResult::Continue))
}

/// Handles the "refine blocked" modal. `d` opens a discussion dialog seeded with
/// the architect's blocker reason; any other key dismisses the modal.
pub(super) fn handle_refine_blocked(
    key: KeyEvent,
    mut st: MutexGuard<'_, AppState>,
) -> anyhow::Result<Option<InputResult>> {
    let blocked = match st.plans_view.refine_blocked.take() {
        Some(b) => b,
        None => return Ok(None),
    };

    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Ok(Some(InputResult::Continue)),
        KeyCode::Char('d') | KeyCode::Char('D') => {
            let plan = st
                .plans_view
                .plans
                .iter()
                .find(|p| p.id == blocked.plan_id)
                .cloned();
            if let Some(plan) = plan {
                let mut editor = crate::editor::InputEditor::default();
                editor.insert_str(&format!(
                    "The architect reported blockers during plan refinement:\n{}\n\nHow should we resolve these requirements or architecture contradictions?",
                    blocked.reason
                ));
                st.discuss_plan_dialog = Some(DiscussPlanDialogState {
                    plan_id: plan.id,
                    plan_title: plan.title,
                    focused_items: Vec::new(),
                    prompt_editor: editor,
                });
            }
            Ok(Some(InputResult::Continue))
        }
        _ => {
            st.plans_view.refine_blocked = Some(blocked);
            Ok(Some(InputResult::Continue))
        }
    }
}
