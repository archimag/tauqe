use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, HistoryGetParams};

use crate::app::{AppState, KeyCommand, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;

pub const HISTORY_COMMANDS: &[KeyCommand] = &[
    KeyCommand { key: "Tab / Space", description: "Fold / unfold active history item" },
    KeyCommand { key: "[ / ] or p / n", description: "Navigate between history items" },
    KeyCommand { key: "c / y / Enter", description: "Copy selected item text to clipboard" },
    KeyCommand { key: "↑/↓ or k/j", description: "Scroll history (fetches older turns)" },
    KeyCommand { key: "Alt+↑ / Alt+↓", description: "Select previous / next item" },
    KeyCommand { key: "PgUp / PgDn", description: "Page scroll history view" },
    KeyCommand { key: "Home / End", description: "Jump to oldest / newest message" },
    KeyCommand { key: "Esc / q", description: "Return to Develop view" },
];

pub async fn handle_history_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;
    let view_height = st.last_model_height;
    let content_width = st.history_view.content_width;
    let total_lines = st.history_view.rendered_lines_count as u16;
    let max_scroll = total_lines.saturating_sub(view_height);

    if key.modifiers.contains(crossterm::event::KeyModifiers::ALT) {
        match key.code {
            KeyCode::Up => {
                if !st.history_view.items.is_empty() {
                    st.history_view.selected_item_index =
                        st.history_view.selected_item_index.saturating_sub(1);
                    st.history_view.scroll_to_selected_item(view_height, Some(content_width));
                }
                return Ok(InputResult::Continue);
            }
            KeyCode::Down => {
                if !st.history_view.items.is_empty()
                    && st.history_view.selected_item_index + 1 < st.history_view.items.len()
                {
                    st.history_view.selected_item_index += 1;
                    st.history_view.scroll_to_selected_item(view_height, Some(content_width));
                }
                return Ok(InputResult::Continue);
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            st.view_mode = ViewMode::Develop;
        }
        KeyCode::Tab | KeyCode::Char(' ') => {
            if let Some(item) = st.history_view.items.get(st.history_view.selected_item_index) {
                let id = item.id;
                st.history_view.toggle_expanded(id);
                st.history_view.scroll_to_selected_item(view_height, Some(content_width));
            }
        }
        KeyCode::Char('[') | KeyCode::Char('p') | KeyCode::Char('P') => {
            if !st.history_view.items.is_empty() {
                st.history_view.selected_item_index =
                    st.history_view.selected_item_index.saturating_sub(1);
                st.history_view.scroll_to_selected_item(view_height, Some(content_width));
            }
        }
        KeyCode::Char(']') | KeyCode::Char('n') | KeyCode::Char('N') => {
            if !st.history_view.items.is_empty()
                && st.history_view.selected_item_index + 1 < st.history_view.items.len()
            {
                st.history_view.selected_item_index += 1;
                st.history_view.scroll_to_selected_item(view_height, Some(content_width));
            }
        }
        KeyCode::BackTab => {
            if !st.history_view.items.is_empty() {
                if st.history_view.selected_item_index > 0 {
                    st.history_view.selected_item_index -= 1;
                } else {
                    st.history_view.selected_item_index =
                        st.history_view.items.len().saturating_sub(1);
                }
                st.history_view.scroll_to_selected_item(view_height, Some(content_width));
            }
        }
        KeyCode::Char('c')
        | KeyCode::Char('C')
        | KeyCode::Char('y')
        | KeyCode::Char('Y')
        | KeyCode::Enter => {
            let sel = st.history_view.selected_item_index;
            if let Some(item) = st.history_view.items.get(sel) {
                if !item.text.is_empty() {
                    if crate::clipboard::copy_to_clipboard(&item.text) {
                        st.model.copy_notification = Some((
                            format!("History item #{} copied to clipboard", item.id),
                            std::time::Instant::now(),
                        ));
                    } else {
                        st.model.copy_notification = Some((
                            "Failed to copy to clipboard".to_string(),
                            std::time::Instant::now(),
                        ));
                    }
                } else {
                    st.model.copy_notification = Some((
                        format!("History item #{} has no text to copy", item.id),
                        std::time::Instant::now(),
                    ));
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            st.history_view.auto_scroll = false;
            if st.history_view.scroll > 0 {
                st.history_view.scroll = st.history_view.scroll.saturating_sub(1);
            } else if st.history_view.has_more && !st.history_view.loading {
                if let Some(first_item) = st.history_view.items.first() {
                    let before_id = first_item.id;
                    st.history_view.loading = true;
                    st.history_view.pending_before_id = Some(before_id);
                    drop(st);
                    let params = HistoryGetParams {
                        limit: Some(10),
                        before_id: Some(before_id),
                    };
                    send_request(
                        server_writer,
                        methods::HISTORY_GET,
                        serde_json::to_value(params)?,
                    )
                    .await?;
                }
            }
        }
        KeyCode::PageUp => {
            st.history_view.auto_scroll = false;
            if st.history_view.scroll > 0 {
                st.history_view.scroll = st.history_view.scroll.saturating_sub(10);
            } else if st.history_view.has_more && !st.history_view.loading {
                if let Some(first_item) = st.history_view.items.first() {
                    let before_id = first_item.id;
                    st.history_view.loading = true;
                    st.history_view.pending_before_id = Some(before_id);
                    drop(st);
                    let params = HistoryGetParams {
                        limit: Some(10),
                        before_id: Some(before_id),
                    };
                    send_request(
                        server_writer,
                        methods::HISTORY_GET,
                        serde_json::to_value(params)?,
                    )
                    .await?;
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if st.history_view.scroll < max_scroll {
                st.history_view.scroll = st.history_view.scroll.saturating_add(1);
            }
            if st.history_view.scroll >= max_scroll {
                st.history_view.auto_scroll = true;
            }
        }
        KeyCode::PageDown => {
            st.history_view.scroll = (st.history_view.scroll.saturating_add(10)).min(max_scroll);
            if st.history_view.scroll >= max_scroll {
                st.history_view.auto_scroll = true;
            }
        }
        KeyCode::Home => {
            st.history_view.auto_scroll = false;
            st.history_view.scroll = 0;
            if st.history_view.has_more && !st.history_view.loading {
                if let Some(first_item) = st.history_view.items.first() {
                    let before_id = first_item.id;
                    st.history_view.loading = true;
                    st.history_view.pending_before_id = Some(before_id);
                    drop(st);
                    let params = HistoryGetParams {
                        limit: Some(10),
                        before_id: Some(before_id),
                    };
                    send_request(
                        server_writer,
                        methods::HISTORY_GET,
                        serde_json::to_value(params)?,
                    )
                    .await?;
                }
            }
        }
        KeyCode::End => {
            st.history_view.auto_scroll = true;
            st.history_view.scroll = max_scroll;
        }
        _ => {}
    }

    Ok(InputResult::Continue)
}
