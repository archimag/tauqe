use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, HistoryGetParams};

use crate::app::{AppState, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;

pub async fn handle_history_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;
    let view_height = st.last_model_height;
    let total_lines = st.history_view.rendered_lines_count as u16;
    let max_scroll = total_lines.saturating_sub(view_height);

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            st.view_mode = ViewMode::Develop;
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
