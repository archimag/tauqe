use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::methods;
use tokio::sync::MutexGuard;

use crate::app::{AppState, ConfirmDialogButton, DiscussPlanDialogState};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

/// Routes key events for the plan refine dialog: model picker navigation, the
/// instructions editor, and the confirm/cancel button pair. Refinement only
/// starts when the confirm button holds focus.
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
    let is_ctrl_alt = key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT);
    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);

    // Model switching is unified through the shared selector: the C-M / Alt+M
    // chord always switches, and the arrows browse the architect list while the
    // instructions editor is empty, so vertical editor navigation stays intact
    // once the editor holds text (c-1, c-3).
    let editor_focused = !dialog.instructions_editor.is_empty();
    if dialog.selector.handle_key(key, primary, editor_focused) {
        st.plans_view.refine_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    match key.code {
        KeyCode::Esc => Ok(Some(InputResult::Continue)),
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
            dialog.confirm_button = match dialog.confirm_button {
                ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
                ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
            };
            st.plans_view.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
        KeyCode::Enter if !has_shift && !has_alt => {
            if dialog.confirm_button == ConfirmDialogButton::Cancel {
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
        _ => {
            crate::input::handle_editor_key(&mut dialog.instructions_editor, key, is_cmd, true);
            st.plans_view.refine_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
    }
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
