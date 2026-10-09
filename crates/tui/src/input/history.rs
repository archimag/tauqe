use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, HistoryGetParams};

use crate::app::{AppState, KeyCommand, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;

pub async fn request_older_history(
    st: &mut AppState,
    server_writer: &mut ChildStdin,
    select_prev: bool,
) -> anyhow::Result<()> {
    if !st.history_view.has_more || st.history_view.loading {
        return Ok(());
    }
    let Some(first_item) = st.history_view.items.first() else {
        return Ok(());
    };
    let before_id = first_item.id;
    st.history_view.loading = true;
    st.history_view.pending_before_id = Some(before_id);
    st.history_view.select_prev_after_load = select_prev;
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
    Ok(())
}

pub const HISTORY_COMMANDS: &[KeyCommand] = &[
    KeyCommand { key: "Enter / Tab / Space", description: "Fold / unfold active history item" },
    KeyCommand { key: "n / p (or [ / ])", description: "Navigate between history items" },
    KeyCommand { key: "l", description: "Recenter active item (Emacs C-l style)" },
    KeyCommand { key: "c", description: "Copy code block from active item" },
    KeyCommand { key: "y", description: "Copy selected item text to clipboard" },
    KeyCommand { key: "↑/↓ or k/j", description: "Scroll history (fetches older turns)" },
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

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            st.view_mode = ViewMode::Develop;
        }
        KeyCode::Tab | KeyCode::Char(' ') | KeyCode::Enter => {
            if let Some(item) = st.history_view.items.get(st.history_view.selected_item_index) {
                let id = item.id;
                st.history_view.toggle_expanded(id);
                st.history_view.scroll_to_selected_item(view_height, Some(content_width));
            }
        }
        KeyCode::Char('l') | KeyCode::Char('L') => {
            st.history_view.recenter_selected_item(view_height, Some(content_width));
        }
        KeyCode::Char('[') | KeyCode::Char('p') | KeyCode::Char('P') => {
            if !st.history_view.items.is_empty() {
                if st.history_view.selected_item_index > 0 {
                    st.history_view.selected_item_index -= 1;
                    st.history_view.scroll_to_selected_item(view_height, Some(content_width));
                } else {
                    request_older_history(&mut st, server_writer, true).await?;
                }
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
        KeyCode::Char('c') | KeyCode::Char('C') => {
            let sel = st.history_view.selected_item_index;
            let item_info = st
                .history_view
                .items
                .get(sel)
                .map(|item| (item.id, item.text.clone()));
            if let Some((item_id, item_text)) = item_info {
                let maybe_block = st
                    .history_view
                    .code_blocks
                    .iter()
                    .find(|b| b.item_id == item_id)
                    .cloned();
                if let Some(block) = maybe_block {
                    drop(st);
                    let res = crate::clipboard::copy_to_clipboard(&block.code);
                    let mut st = state.lock().await;
                    match res {
                        crate::clipboard::CopyResult::Native => {
                            st.history_view.copy_flash =
                                Some(((block.item_id, block.block_id), std::time::Instant::now()));
                            let lines_count = block.code.lines().count().max(1);
                            st.notify_success(format!(
                                "Copied {} lines of code to clipboard",
                                lines_count
                            ));
                        }
                        crate::clipboard::CopyResult::Osc52Only => {
                            st.history_view.copy_flash =
                                Some(((block.item_id, block.block_id), std::time::Instant::now()));
                            let lines_count = block.code.lines().count().max(1);
                            st.notify_info(format!(
                                "Sent {} lines of code to terminal clipboard (OSC 52)",
                                lines_count
                            ));
                        }
                        crate::clipboard::CopyResult::Failed => {
                            st.notify_error("Failed to copy code to clipboard");
                        }
                    }
                } else if !item_text.is_empty() {
                    drop(st);
                    let res = crate::clipboard::copy_to_clipboard(&item_text);
                    let mut st = state.lock().await;
                    match res {
                        crate::clipboard::CopyResult::Native => {
                            st.notify_success(format!(
                                "History item #{} copied to clipboard",
                                item_id
                            ));
                        }
                        crate::clipboard::CopyResult::Osc52Only => {
                            st.notify_info(format!(
                                "History item #{} sent to terminal clipboard (OSC 52)",
                                item_id
                            ));
                        }
                        crate::clipboard::CopyResult::Failed => {
                            st.notify_error(format!(
                                "Failed to copy history item #{} to clipboard",
                                item_id
                            ));
                        }
                    }
                } else {
                    st.notify_warning(format!(
                        "History item #{} has no text to copy",
                        item_id
                    ));
                }
            }
        }
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let sel = st.history_view.selected_item_index;
            let item_info = st
                .history_view
                .items
                .get(sel)
                .map(|item| (item.id, item.text.clone()));
            if let Some((item_id, item_text)) = item_info {
                if !item_text.is_empty() {
                    drop(st);
                    let res = crate::clipboard::copy_to_clipboard(&item_text);
                    let mut st = state.lock().await;
                    match res {
                        crate::clipboard::CopyResult::Native => {
                            st.notify_success(format!(
                                "History item #{} copied to clipboard",
                                item_id
                            ));
                        }
                        crate::clipboard::CopyResult::Osc52Only => {
                            st.notify_info(format!(
                                "History item #{} sent to terminal clipboard (OSC 52)",
                                item_id
                            ));
                        }
                        crate::clipboard::CopyResult::Failed => {
                            st.notify_error("Failed to copy to clipboard");
                        }
                    }
                } else {
                    st.notify_warning(format!(
                        "History item #{} has no text to copy",
                        item_id
                    ));
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            st.history_view.auto_scroll = false;
            if st.history_view.scroll > 0 {
                st.history_view.scroll = st.history_view.scroll.saturating_sub(1);
                st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            } else {
                request_older_history(&mut st, server_writer, false).await?;
            }
        }
        KeyCode::PageUp => {
            let page = view_height.saturating_sub(2).max(1);
            st.history_view.auto_scroll = false;
            if st.history_view.scroll > 0 {
                st.history_view.scroll = st.history_view.scroll.saturating_sub(page);
                st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            } else {
                request_older_history(&mut st, server_writer, false).await?;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if st.history_view.scroll < max_scroll {
                st.history_view.scroll = st.history_view.scroll.saturating_add(1);
                st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            }
            if st.history_view.scroll >= max_scroll {
                st.history_view.auto_scroll = true;
            }
        }
        KeyCode::PageDown => {
            let page = view_height.saturating_sub(2).max(1);
            st.history_view.scroll = (st.history_view.scroll.saturating_add(page)).min(max_scroll);
            st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            if st.history_view.scroll >= max_scroll {
                st.history_view.auto_scroll = true;
            }
        }
        KeyCode::Home => {
            st.history_view.auto_scroll = false;
            st.history_view.scroll = 0;
            st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            request_older_history(&mut st, server_writer, false).await?;
        }
        KeyCode::End => {
            st.history_view.auto_scroll = true;
            st.history_view.scroll = max_scroll;
            st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
        }
        _ => {}
    }

    Ok(InputResult::Continue)
}
