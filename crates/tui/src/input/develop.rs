use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, GitSquashPreviewParams, ModelAskParams};

use crate::app::AppState;
use crate::input::InputResult;
use crate::rpc::send_request;

pub async fn handle_develop_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    // 1. Control shortcuts in Develop view
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        let mut st = state.lock().await;
        match key.code {
            KeyCode::Char('a') => {
                st.input_editor.move_beginning_of_line();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('e') => {
                st.input_editor.move_end_of_line();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('k') => {
                st.input_editor.kill_line();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('u') => {
                st.input_editor.kill_to_beginning_of_line();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('y') => {
                st.input_editor.yank();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('d') => {
                st.input_editor.delete_forward();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('b') => {
                st.input_editor.move_backward();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('f') => {
                st.input_editor.move_forward();
                return Ok(InputResult::Continue);
            }
            KeyCode::Left => {
                st.input_editor.move_word_backward();
                return Ok(InputResult::Continue);
            }
            KeyCode::Right => {
                st.input_editor.move_word_forward();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('s') => {
                if st.model.is_busy() {
                    st.model.git_notification = Some(
                        "Cannot squash commits while model is generating".to_string(),
                    );
                } else {
                    st.squash_dialog = Some(crate::app::SquashDialogState::default());
                    drop(st);
                    let params = GitSquashPreviewParams { base_ref: None };
                    send_request(
                        server_writer,
                        methods::GIT_SQUASH_PREVIEW,
                        serde_json::to_value(params)?,
                    )
                    .await?;
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Enter => {
                if let Some(prompt) = st.take_prompt() {
                    drop(st);
                    let params = ModelAskParams { prompt };
                    send_request(server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await?;
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('j') => {
                st.input_editor.insert_char('\n');
                return Ok(InputResult::Continue);
            }
            _ => {}
        }
    }

    // 2. Alt shortcuts in Develop view
    if key.modifiers.contains(KeyModifiers::ALT) {
        let mut st = state.lock().await;
        match key.code {
            KeyCode::Up => {
                st.model.auto_scroll = false;
                st.model.scroll = st.model.scroll.saturating_sub(1);
                return Ok(InputResult::Continue);
            }
            KeyCode::Down => {
                let view_height = st.last_model_height;
                let max = st.model.max_scroll(view_height);
                st.model.scroll = (st.model.scroll.saturating_add(1)).min(max);
                if st.model.scroll >= max {
                    st.model.auto_scroll = true;
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('b') | KeyCode::Left => {
                st.input_editor.move_word_backward();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('f') | KeyCode::Right => {
                st.input_editor.move_word_forward();
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('d') => {
                st.input_editor.kill_word_forward();
                return Ok(InputResult::Continue);
            }
            KeyCode::Backspace => {
                st.input_editor.kill_word_backward();
                return Ok(InputResult::Continue);
            }
            KeyCode::Enter => {
                if let Some(prompt) = st.take_prompt() {
                    drop(st);
                    let params = ModelAskParams { prompt };
                    send_request(server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await?;
                }
                return Ok(InputResult::Continue);
            }
            _ => {}
        }
    }

    // 3. Normal keys in Develop view
    let mut st = state.lock().await;
    let view_height = st.last_model_height;
    match key.code {
        KeyCode::Esc => {
            if st.model.is_busy() {
                st.confirm_cancel = true;
            } else if !st.input_editor.is_empty() {
                st.input_editor.clear();
            }
        }
        KeyCode::Char('u') if st.input_editor.is_empty() => {
            if st.model.is_busy() {
                st.model.git_notification = Some(
                    "Cannot undo while model is generating".to_string(),
                );
            } else {
                st.confirm_undo = true;
            }
        }
        KeyCode::Char('s') if st.input_editor.is_empty() => {
            if st.model.is_busy() {
                st.model.git_notification = Some(
                    "Cannot squash commits while model is generating".to_string(),
                );
            } else {
                st.squash_dialog = Some(crate::app::SquashDialogState::default());
                drop(st);
                let params = GitSquashPreviewParams { base_ref: None };
                send_request(
                    server_writer,
                    methods::GIT_SQUASH_PREVIEW,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
        KeyCode::Char('[') if st.input_editor.is_empty() => {
            if !st.model.files.is_empty() {
                st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                st.model.scroll_to_selected_file(view_height);
            }
        }
        KeyCode::Char(']') if st.input_editor.is_empty() => {
            if !st.model.files.is_empty() && st.model.selected_file_index + 1 < st.model.files.len() {
                st.model.selected_file_index += 1;
                st.model.scroll_to_selected_file(view_height);
            }
        }
        KeyCode::Char(' ') if st.input_editor.is_empty() => {
            let sel_idx = st.model.selected_file_index;
            if let Some(file) = st.model.files.get_mut(sel_idx) {
                file.expanded = !file.expanded;
                st.model.scroll_to_selected_file(view_height);
            }
        }
        KeyCode::Tab => {
            if st.input_editor.is_empty() {
                if !st.model.files.is_empty() {
                    let sel_idx = st.model.selected_file_index;
                    if let Some(file) = st.model.files.get_mut(sel_idx) {
                        file.expanded = !file.expanded;
                        let h = st.last_model_height;
                        st.model.clamp_scroll(h);
                    }
                }
            } else {
                st.input_editor.insert_str("  ");
            }
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            st.input_editor.insert_char('\n');
        }
        KeyCode::Enter => {
            if let Some(prompt) = st.take_prompt() {
                drop(st);
                let params = ModelAskParams { prompt };
                send_request(server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await?;
            } else if !st.model.files.is_empty() {
                let sel_idx = st.model.selected_file_index;
                if let Some(file) = st.model.files.get_mut(sel_idx) {
                    file.expanded = !file.expanded;
                    st.model.scroll_to_selected_file(view_height);
                }
            }
        }
        KeyCode::Char(c) => {
            st.input_editor.insert_char(c);
        }
        KeyCode::Backspace => {
            st.input_editor.delete_backward();
        }
        KeyCode::Delete => {
            st.input_editor.delete_forward();
        }
        KeyCode::Left => {
            st.input_editor.move_backward();
        }
        KeyCode::Right => {
            st.input_editor.move_forward();
        }
        KeyCode::Home => {
            if !st.input_editor.is_empty() {
                st.input_editor.move_beginning_of_line();
            } else {
                st.model.auto_scroll = false;
                st.model.scroll = 0;
            }
        }
        KeyCode::End => {
            if !st.input_editor.is_empty() {
                st.input_editor.move_end_of_line();
            } else {
                st.model.auto_scroll = true;
                st.model.scroll = st.model.max_scroll(view_height);
            }
        }
        KeyCode::PageUp => {
            st.model.auto_scroll = false;
            st.model.scroll = st.model.scroll.saturating_sub(10);
        }
        KeyCode::PageDown => {
            let max = st.model.max_scroll(view_height);
            st.model.scroll = (st.model.scroll.saturating_add(10)).min(max);
            if st.model.scroll >= max {
                st.model.auto_scroll = true;
            }
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if st.input_editor.is_empty() {
                st.model.auto_scroll = false;
                st.model.scroll = st.model.scroll.saturating_sub(1);
            }
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if st.input_editor.is_empty() {
                let max = st.model.max_scroll(view_height);
                st.model.scroll = (st.model.scroll.saturating_add(1)).min(max);
                if st.model.scroll >= max {
                    st.model.auto_scroll = true;
                }
            }
        }
        KeyCode::Up => {
            if !st.input_editor.is_empty() {
                st.input_editor.move_line_up();
            } else if !st.model.files.is_empty() {
                st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                st.model.scroll_to_selected_file(view_height);
            } else {
                st.model.auto_scroll = false;
                st.model.scroll = st.model.scroll.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if !st.input_editor.is_empty() {
                st.input_editor.move_line_down();
            } else if !st.model.files.is_empty() {
                if st.model.selected_file_index + 1 < st.model.files.len() {
                    st.model.selected_file_index += 1;
                    st.model.scroll_to_selected_file(view_height);
                }
            } else {
                let max = st.model.max_scroll(view_height);
                st.model.scroll = (st.model.scroll.saturating_add(1)).min(max);
                if st.model.scroll >= max {
                    st.model.auto_scroll = true;
                }
            }
        }
        _ => {}
    }

    Ok(InputResult::Continue)
}
