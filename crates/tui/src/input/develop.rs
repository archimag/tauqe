use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, ModelAskParams};

use crate::app::{AppState, KeyCommand};
use crate::input::InputResult;
use crate::rpc::send_request;

pub const DEVELOP_COMMANDS: &[KeyCommand] = &[
    KeyCommand { key: "Enter / C-Enter", description: "Send prompt" },
    KeyCommand { key: "Shift+Enter / Alt+Enter / C-J", description: "Insert newline into prompt" },
    KeyCommand { key: "↑ / ↓", description: "Navigate prompt lines or prompt history (scroll response when empty)" },
    KeyCommand { key: "C-Left / C-Right / Alt+B / Alt+F", description: "Move cursor word backward / forward" },
    KeyCommand { key: "C-A / C-E / Home / End", description: "Move cursor to beginning / end of line or response" },
    KeyCommand { key: "C-K / C-U", description: "Kill line to end / beginning into kill ring" },
    KeyCommand { key: "C-W / Alt+Backspace", description: "Kill word backward into kill ring" },
    KeyCommand { key: "Alt+D", description: "Kill word forward into kill ring" },
    KeyCommand { key: "C-D / Delete", description: "Delete character forward" },
    KeyCommand { key: "C-B / C-F / Left / Right", description: "Move cursor character backward / forward" },
    KeyCommand { key: "C-Y", description: "Yank (restore) text from kill ring" },
    KeyCommand { key: "Esc", description: "Interrupt generation or clear prompt (saves to kill ring)" },
    KeyCommand { key: "C-Z / Alt+U", description: "Undo last AI commit" },
    KeyCommand { key: "F6 / C-S", description: "Squash commits dialog" },
    KeyCommand { key: "C-[ / C-] / Alt+[/]", description: "Select previous / next modified file" },
    KeyCommand { key: "Tab / C-Space / Alt+Space", description: "Fold / unfold selected file diff (when prompt is empty)" },
    KeyCommand { key: "Alt+C / Alt+Y", description: "Copy model response to clipboard" },
    KeyCommand { key: "PgUp / PgDn", description: "Page scroll response view" },
    KeyCommand { key: "Alt+↑ / Alt+↓", description: "Scroll model response up / down" },
];

pub async fn handle_develop_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let primary_mod = { state.lock().await.tui_config.input.primary_modifier };
    let is_primary = primary_mod.matches(key.modifiers);

    // 1. Primary command modifier shortcuts (C-x in Emacs style)
    if is_primary {
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
            KeyCode::Char('w') => {
                st.input_editor.kill_word_backward();
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
                crate::input::open_squash_dialog(&mut st, server_writer).await?;
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('z') => {
                if st.model.is_busy() {
                    st.notify_warning("Cannot undo while model is generating");
                } else if st.model.last_commit_hash.is_none() {
                    st.notify_warning("No AI commit to undo");
                } else {
                    st.confirm_undo = true;
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Enter => {
                if let Some(prompt) = st.take_prompt() {
                    drop(st);
                    let params = ModelAskParams { prompt: prompt.clone() };
                    if let Err(err) = send_request(server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await {
                        let mut st = state.lock().await;
                        st.input_editor.clear();
                        st.input_editor.insert_str(&prompt);
                        st.model.status = "error".to_string();
                        st.notify_error(format!("Failed to send prompt: {}", err));
                    }
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('j') => {
                st.input_editor.insert_char('\n');
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('[') => {
                if !st.model.files.is_empty() {
                    st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                    let h = st.last_model_height;
                    st.model.scroll_to_selected_file(h);
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Char(']') => {
                if !st.model.files.is_empty() && st.model.selected_file_index + 1 < st.model.files.len() {
                    st.model.selected_file_index += 1;
                    let h = st.last_model_height;
                    st.model.scroll_to_selected_file(h);
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Char(' ') => {
                let sel_idx = st.model.selected_file_index;
                let h = st.last_model_height;
                if let Some(file) = st.model.files.get_mut(sel_idx) {
                    file.expanded = !file.expanded;
                    st.model.scroll_to_selected_file(h);
                }
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
            KeyCode::Char('u') | KeyCode::Char('U') => {
                if st.model.is_busy() {
                    st.notify_warning("Cannot undo while model is generating");
                } else if st.model.last_commit_hash.is_none() {
                    st.notify_warning("No AI commit to undo");
                } else {
                    st.confirm_undo = true;
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('[') => {
                if !st.model.files.is_empty() {
                    st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                    let h = st.last_model_height;
                    st.model.scroll_to_selected_file(h);
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Char(']') => {
                if !st.model.files.is_empty() && st.model.selected_file_index + 1 < st.model.files.len() {
                    st.model.selected_file_index += 1;
                    let h = st.last_model_height;
                    st.model.scroll_to_selected_file(h);
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Char(' ') => {
                let sel_idx = st.model.selected_file_index;
                let h = st.last_model_height;
                if let Some(file) = st.model.files.get_mut(sel_idx) {
                    file.expanded = !file.expanded;
                    st.model.scroll_to_selected_file(h);
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Enter => {
                st.input_editor.insert_char('\n');
                return Ok(InputResult::Continue);
            }
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                if st.model.is_busy() {
                    st.notify_warning("Cannot copy while model is generating");
                } else if st.model.text.trim().is_empty() {
                    st.notify_warning("No model response to copy");
                } else {
                    let text = st.model.text.clone();
                    match crate::clipboard::copy_to_clipboard(&text) {
                        crate::clipboard::CopyResult::Native => {
                            st.notify_success("Model response copied to clipboard");
                        }
                        crate::clipboard::CopyResult::Osc52Only => {
                            st.notify_success("Model response copied via terminal (OSC 52)");
                        }
                        crate::clipboard::CopyResult::Failed => {
                            st.notify_error("Failed to copy to clipboard");
                        }
                    }
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
                st.input_editor.clear_saving();
                st.notify_info("Prompt cleared (C-Y to restore)");
            }
        }
        KeyCode::F(6) => {
            crate::input::open_squash_dialog(&mut st, server_writer).await?;
        }
        KeyCode::Tab => {
            if st.input_editor.is_empty() && !st.model.files.is_empty() {
                let sel_idx = st.model.selected_file_index;
                let h = st.last_model_height;
                if let Some(file) = st.model.files.get_mut(sel_idx) {
                    file.expanded = !file.expanded;
                    st.model.scroll_to_selected_file(h);
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
                let params = ModelAskParams { prompt: prompt.clone() };
                if let Err(err) = send_request(server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await {
                    let mut st = state.lock().await;
                    st.input_editor.clear();
                    st.input_editor.insert_str(&prompt);
                    st.model.status = "error".to_string();
                    st.notify_error(format!("Failed to send prompt: {}", err));
                }
            }
        }
        KeyCode::Char(c) if crate::input::is_char_typing(key.modifiers) => {
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
            let page = view_height.saturating_sub(2).max(1);
            st.model.auto_scroll = false;
            st.model.scroll = st.model.scroll.saturating_sub(page);
        }
        KeyCode::PageDown => {
            let page = view_height.saturating_sub(2).max(1);
            let max = st.model.max_scroll(view_height);
            st.model.scroll = (st.model.scroll.saturating_add(page)).min(max);
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
            if st.input_editor.history_index.is_some() {
                if !st.input_editor.move_line_up() {
                    st.input_editor.history_prev();
                }
            } else if st.input_editor.is_empty() {
                if st.input_editor.history_prev() {
                    return Ok(InputResult::Continue);
                }
                if !st.model.files.is_empty() {
                    st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                    st.model.scroll_to_selected_file(view_height);
                } else {
                    st.model.auto_scroll = false;
                    st.model.scroll = st.model.scroll.saturating_sub(1);
                }
            } else if !st.input_editor.move_line_up() && st.input_editor.line_count() == 1 {
                if st.input_editor.history_prev() {
                    return Ok(InputResult::Continue);
                }
            } else if !st.model.files.is_empty() {
                st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                st.model.scroll_to_selected_file(view_height);
            } else {
                st.model.auto_scroll = false;
                st.model.scroll = st.model.scroll.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if st.input_editor.history_index.is_some() {
                if !st.input_editor.move_line_down() {
                    st.input_editor.history_next();
                }
            } else if !st.input_editor.is_empty() {
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
