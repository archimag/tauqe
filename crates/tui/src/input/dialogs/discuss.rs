use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::methods;
use tokio::process::ChildStdin;
use tokio::sync::MutexGuard;

use crate::app::AppState;
use crate::input::InputResult;
use crate::rpc::send_request;

/// Routes key events for the plan discussion dialog. `Enter` submits the comment as
/// a model request, any other text key edits the prompt, `Esc` closes the dialog.
pub(super) async fn handle_discuss_plan(
    key: KeyEvent,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<Option<InputResult>> {
    let mut dialog = match st.discuss_plan_dialog.take() {
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

    match key.code {
        KeyCode::Esc => Ok(Some(InputResult::Continue)),
        KeyCode::Enter if !has_shift && !has_alt => {
            let user_comment = dialog.prompt_editor.get_text().trim().to_string();
            if user_comment.is_empty() {
                st.notify_warning("Comment is empty");
                st.discuss_plan_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }

            let plan = st
                .plans_view
                .plans
                .iter()
                .find(|p| p.id == dialog.plan_id)
                .cloned();
            if let Some(plan) = plan {
                let focused_ids: Vec<String> =
                    dialog.focused_items.iter().map(|(id, _)| id.clone()).collect();
                let prompt = tauqe_protocol::plan::format_plan_discussion_prompt(
                    &plan,
                    &focused_ids,
                    &user_comment,
                );

                super::begin_develop_turn(&mut st);
                drop(st);
                send_request(
                    server_writer,
                    methods::MODEL_ASK,
                    serde_json::json!({ "prompt": prompt }),
                )
                .await?;
                Ok(Some(InputResult::Continue))
            } else {
                st.notify_error(format!("Plan '{}' not found", dialog.plan_id));
                st.discuss_plan_dialog = Some(dialog);
                Ok(Some(InputResult::Continue))
            }
        }
        _ => {
            crate::input::handle_editor_key(&mut dialog.prompt_editor, key, is_cmd, true);
            st.discuss_plan_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
    }
}
