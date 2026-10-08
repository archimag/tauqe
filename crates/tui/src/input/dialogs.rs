use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{
    methods, ConfigSetParams, GitSquashApplyParams, GitSquashGenerateMessageParams,
    GitSquashPreviewParams, ReviewStartParams,
};

use crate::app::{AppState, SquashBaseMode, SquashDialogFocus};
use crate::input::InputResult;
use crate::rpc::send_request;
use crate::ui::develop::DevelopView;
use crate::ui::history::HistoryViewState;

pub async fn handle_dialog_event(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<Option<InputResult>> {
    let mut st = state.lock().await;

    // 1. Squash dialog modal
    if let Some(mut dialog) = st.squash_dialog.take() {
        if dialog.custom_input_active {
            match key.code {
                KeyCode::Esc => {
                    dialog.custom_input_active = false;
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Enter => {
                    let custom = dialog.custom_input.trim().to_string();
                    if !custom.is_empty() {
                        dialog.custom_input_active = false;
                        dialog.base_mode = SquashBaseMode::Custom;
                        dialog.base_ref = custom.clone();
                        dialog.loading = true;
                        dialog.status_message = Some(format!("Checking base '{}'...", custom));
                        st.squash_dialog = Some(dialog);
                        drop(st);
                        let params = GitSquashPreviewParams { base_ref: Some(custom) };
                        send_request(server_writer, methods::GIT_SQUASH_PREVIEW, serde_json::to_value(params)?).await?;
                    } else {
                        dialog.custom_input_active = false;
                        st.squash_dialog = Some(dialog);
                    }
                }
                KeyCode::Backspace => {
                    dialog.custom_input.pop();
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Char(c) => {
                    dialog.custom_input.push(c);
                    st.squash_dialog = Some(dialog);
                }
                _ => {
                    st.squash_dialog = Some(dialog);
                }
            }
            return Ok(Some(InputResult::Continue));
        }

        if dialog.focus == SquashDialogFocus::MessageEditor {
            match key.code {
                KeyCode::Esc => {
                    dialog.focus = SquashDialogFocus::FileList;
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) || !dialog.message_buffer.trim().is_empty() => {
                    let msg = dialog.message_buffer.trim().to_string();
                    if msg.is_empty() {
                        dialog.status_message = Some("Commit message cannot be empty".to_string());
                        st.squash_dialog = Some(dialog);
                        return Ok(Some(InputResult::Continue));
                    }
                    let base_ref = dialog.base_ref.clone();
                    st.squash_dialog = None;
                    drop(st);
                    let params = GitSquashApplyParams { base_ref, message: msg };
                    send_request(server_writer, methods::GIT_SQUASH_APPLY, serde_json::to_value(params)?).await?;
                    return Ok(Some(InputResult::Continue));
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    dialog.message_buffer.push('\n');
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.generating_message = true;
                    dialog.status_message = Some("Generating Conventional Commit message with AI...".to_string());
                    let base_ref = dialog.base_ref.clone();
                    st.squash_dialog = Some(dialog);
                    drop(st);
                    let params = GitSquashGenerateMessageParams { base_ref };
                    send_request(server_writer, methods::GIT_SQUASH_GENERATE_MESSAGE, serde_json::to_value(params)?).await?;
                }
                KeyCode::Backspace => {
                    dialog.message_buffer.pop();
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Char(c) => {
                    dialog.message_buffer.push(c);
                    st.squash_dialog = Some(dialog);
                }
                _ => {
                    st.squash_dialog = Some(dialog);
                }
            }
            return Ok(Some(InputResult::Continue));
        }

        match key.code {
            KeyCode::Esc => {
                st.squash_dialog = None;
            }
            KeyCode::Char('1') => {
                dialog.base_mode = SquashBaseMode::Session;
                let base_ref = dialog.session_base.clone().unwrap_or_else(|| "HEAD~1".to_string());
                dialog.loading = true;
                dialog.base_ref = base_ref.clone();
                dialog.status_message = Some("Switching base to Tauqe session...".to_string());
                st.squash_dialog = Some(dialog);
                drop(st);
                let params = GitSquashPreviewParams { base_ref: Some(base_ref) };
                send_request(server_writer, methods::GIT_SQUASH_PREVIEW, serde_json::to_value(params)?).await?;
            }
            KeyCode::Char('2') => {
                dialog.base_mode = SquashBaseMode::Upstream;
                let base_ref = dialog.upstream_base.clone().unwrap_or_else(|| "master".to_string());
                dialog.loading = true;
                dialog.base_ref = base_ref.clone();
                dialog.status_message = Some(format!("Switching base to upstream '{}'...", base_ref));
                st.squash_dialog = Some(dialog);
                drop(st);
                let params = GitSquashPreviewParams { base_ref: Some(base_ref) };
                send_request(server_writer, methods::GIT_SQUASH_PREVIEW, serde_json::to_value(params)?).await?;
            }
            KeyCode::Char('3') => {
                dialog.custom_input_active = true;
                dialog.custom_input = dialog.base_ref.clone();
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Tab => {
                dialog.focus = match dialog.focus {
                    SquashDialogFocus::FileList => SquashDialogFocus::DiffView,
                    SquashDialogFocus::DiffView => SquashDialogFocus::MessageEditor,
                    SquashDialogFocus::MessageEditor => SquashDialogFocus::FileList,
                };
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Char('g') => {
                dialog.generating_message = true;
                dialog.status_message = Some("Generating Conventional Commit message with AI...".to_string());
                let base_ref = dialog.base_ref.clone();
                st.squash_dialog = Some(dialog);
                drop(st);
                let params = GitSquashGenerateMessageParams { base_ref };
                send_request(server_writer, methods::GIT_SQUASH_GENERATE_MESSAGE, serde_json::to_value(params)?).await?;
            }
            KeyCode::Char('e') | KeyCode::Char('m') => {
                dialog.focus = SquashDialogFocus::MessageEditor;
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Char(' ') => {
                if let Some(file) = dialog.files.get_mut(dialog.selected_file_index) {
                    file.expanded = !file.expanded;
                }
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if dialog.focus == SquashDialogFocus::DiffView {
                    dialog.diff_scroll = dialog.diff_scroll.saturating_sub(1);
                } else if dialog.selected_file_index > 0 {
                    dialog.selected_file_index -= 1;
                    dialog.diff_scroll = 0;
                }
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if dialog.focus == SquashDialogFocus::DiffView {
                    dialog.diff_scroll = dialog.diff_scroll.saturating_add(1);
                } else if !dialog.files.is_empty() && dialog.selected_file_index + 1 < dialog.files.len() {
                    dialog.selected_file_index += 1;
                    dialog.diff_scroll = 0;
                }
                st.squash_dialog = Some(dialog);
            }
            KeyCode::PageUp => {
                dialog.diff_scroll = dialog.diff_scroll.saturating_sub(10);
                st.squash_dialog = Some(dialog);
            }
            KeyCode::PageDown => {
                dialog.diff_scroll = dialog.diff_scroll.saturating_add(10);
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Enter => {
                if !dialog.message_buffer.trim().is_empty() {
                    let msg = dialog.message_buffer.trim().to_string();
                    let base_ref = dialog.base_ref.clone();
                    st.squash_dialog = None;
                    drop(st);
                    let params = GitSquashApplyParams { base_ref, message: msg };
                    send_request(server_writer, methods::GIT_SQUASH_APPLY, serde_json::to_value(params)?).await?;
                } else {
                    dialog.focus = SquashDialogFocus::MessageEditor;
                    dialog.status_message = Some("Enter a commit message or press 'g' to generate one.".to_string());
                    st.squash_dialog = Some(dialog);
                }
            }
            _ => {
                st.squash_dialog = Some(dialog);
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 1b. Review launch dialog
    if let Some(mut dialog) = st.review_dialog.take() {
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let prompt = dialog.prompt.trim().to_string();
                let params = ReviewStartParams {
                    model: dialog.models.get(dialog.model_index).cloned(),
                    user_prompt: if prompt.is_empty() { None } else { Some(prompt) },
                };
                st.review.begin();
                drop(st);
                send_request(
                    server_writer,
                    methods::REVIEW_START,
                    serde_json::to_value(params)?,
                )
                .await?;
                return Ok(Some(InputResult::Continue));
            }
            code => {
                let count = dialog.models.len();
                match code {
                    KeyCode::Up | KeyCode::BackTab if count > 0 => {
                        dialog.model_index = (dialog.model_index + count - 1) % count;
                    }
                    KeyCode::Down | KeyCode::Tab if count > 0 => {
                        dialog.model_index = (dialog.model_index + 1) % count;
                    }
                    KeyCode::Backspace => {
                        dialog.prompt.pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        dialog.prompt.push(c);
                    }
                    _ => {}
                }
                st.review_dialog = Some(dialog);
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 2. Cancel confirmation
    if st.confirm_cancel {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                st.confirm_cancel = false;
                drop(st);
                send_request(server_writer, methods::MODEL_CANCEL, serde_json::json!({})).await?;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                st.confirm_cancel = false;
            }
            _ => {}
        }
        return Ok(Some(InputResult::Continue));
    }

    // 3. Undo confirmation
    if st.confirm_undo {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                st.confirm_undo = false;
                drop(st);
                send_request(server_writer, methods::GIT_UNDO, serde_json::json!({})).await?;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                st.confirm_undo = false;
            }
            _ => {}
        }
        return Ok(Some(InputResult::Continue));
    }

    // 4. Clear history confirmation
    if st.confirm_clear_history {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                st.confirm_clear_history = false;
                let cost = st.model.session_total_cost;
                let prev_cost = st.model.prev_cost;
                st.model = DevelopView {
                    session_total_cost: cost,
                    prev_cost,
                    ..Default::default()
                };
                st.history_view = HistoryViewState::default();
                drop(st);
                send_request(server_writer, methods::MODEL_CLEAR_HISTORY, serde_json::json!({})).await?;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                st.confirm_clear_history = false;
            }
            _ => {}
        }
        return Ok(Some(InputResult::Continue));
    }

    // 5. Selection dialog
    if let Some(mut dialog) = st.selection_dialog.take() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                dialog.selected_index = dialog.selected_index.saturating_sub(1);
                st.selection_dialog = Some(dialog);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !dialog.items.is_empty() && dialog.selected_index + 1 < dialog.items.len() {
                    dialog.selected_index += 1;
                }
                st.selection_dialog = Some(dialog);
            }
            KeyCode::Enter => {
                if let Some(chosen) = dialog.items.get(dialog.selected_index).cloned() {
                    st.active_model = chosen.clone();
                    let params = ConfigSetParams {
                        workflow: None,
                        edit_protocol: None,
                        model: Some(chosen),
                    };
                    drop(st);
                    send_request(server_writer, methods::CONFIG_SET, serde_json::to_value(params)?).await?;
                }
            }
            _ => {
                st.selection_dialog = Some(dialog);
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 6. Help popup
    if st.show_help {
        match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                st.show_help = false;
            }
            _ => {}
        }
        return Ok(Some(InputResult::Continue));
    }

    Ok(None)
}
