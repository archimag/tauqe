use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::{methods, ReviewItem, ReviewUpdateItemParams};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

use crate::app::{AppState, KeyCommand, ReviewDialogState, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;

pub const REVIEW_COMMANDS: &[KeyCommand] = &[
    KeyCommand { key: "r", description: "Run code review on the current context" },
    KeyCommand { key: "s", description: "Toggle filter: show all / hide closed (DONE/REJECTED)" },
    KeyCommand { key: "Ctrl+R", description: "Fold / unfold thinking (reasoning) stream" },
    KeyCommand { key: "↑/↓ or k/j", description: "Select previous / next finding" },
    KeyCommand { key: "Tab / Space", description: "Fold / unfold selected finding" },
    KeyCommand { key: "x", description: "Check / uncheck finding for Develop" },
    KeyCommand { key: "t", description: "Cycle status: TODO → DONE → REJECTED" },
    KeyCommand { key: "c / y / Enter", description: "Copy selected finding to clipboard" },
    KeyCommand { key: "PgUp / PgDn", description: "Page scroll review view" },
    KeyCommand { key: "Home / End", description: "Select first / last finding" },
    KeyCommand { key: "Esc", description: "Cancel running review / return to Develop" },
];

fn format_item_for_clipboard(item: &ReviewItem) -> String {
    let mut text = format!("[{}] {}", item.severity, item.title);
    if let Some(path) = &item.file_path {
        text.push('\n');
        text.push_str(path);
        if let Some((start, end)) = item.line_range {
            text.push_str(&format!(":{}-{}", start, end));
        }
    }
    if !item.body.trim().is_empty() {
        text.push_str("\n\n");
        text.push_str(item.body.trim());
    }
    text
}

fn mutate_selected(
    st: &mut AppState,
    f: impl FnOnce(&mut ReviewItem) -> ReviewUpdateItemParams,
) -> Option<ReviewUpdateItemParams> {
    let sel = st.review.selected_item_index()?;
    st.review
        .session
        .as_mut()
        .and_then(|s| s.items.get_mut(sel))
        .map(f)
}

pub async fn handle_review_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
    {
        st.review.reasoning.toggle_fold();
        return Ok(InputResult::Continue);
    }

    if st.review.running {
        if key.code == KeyCode::Esc {
            drop(st);
            send_request(server_writer, methods::REVIEW_CANCEL, serde_json::json!({})).await?;
        }
        return Ok(InputResult::Continue);
    }

    let items_len = st.review.visible_item_indices().len();
    let page = st.review.view_height.max(1);

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            st.view_mode = ViewMode::Develop;
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            st.review.toggle_hide_closed();
            st.review.scroll_to_selected();
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            if st.model.is_busy() {
                st.review.error =
                    Some("Cannot start a review while the model is generating".to_string());
            } else if st.context.items.is_empty() {
                st.review.error = Some(
                    "Context is empty: add files in the Context tab (Ctrl+2) first".to_string(),
                );
            } else {
                let models = st.available_models.clone();
                let model_index = models
                    .iter()
                    .position(|m| m == &st.active_model)
                    .unwrap_or(0);
                let files_count = st.context.items.len();
                let estimated_tokens = st.context.total_estimated_tokens as usize;
                st.review.error = None;
                st.review_dialog = Some(ReviewDialogState {
                    files_count,
                    estimated_tokens,
                    models,
                    model_index,
                    prompt: String::new(),
                });
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if st.review.selected_index > 0 {
                st.review.selected_index -= 1;
                st.review.scroll_to_selected();
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if st.review.selected_index + 1 < items_len {
                st.review.selected_index += 1;
                st.review.scroll_to_selected();
            }
        }
        KeyCode::Home => {
            st.review.selected_index = 0;
            st.review.scroll_to_selected();
        }
        KeyCode::End => {
            st.review.selected_index = items_len.saturating_sub(1);
            st.review.scroll_to_selected();
        }
        KeyCode::PageUp => {
            st.review.scroll = st.review.scroll.saturating_sub(page);
        }
        KeyCode::PageDown => {
            let max = (st.review.rendered_lines as u16).saturating_sub(page);
            st.review.scroll = st.review.scroll.saturating_add(page).min(max);
        }
        KeyCode::Tab | KeyCode::Char(' ') => {
            let id = st
                .review
                .selected_item_index()
                .and_then(|idx| st.review.session.as_ref()?.items.get(idx))
                .map(|i| i.id);
            if let Some(id) = id {
                st.review.toggle_expanded(id);
                st.review.scroll_to_selected();
            }
        }
        KeyCode::Char('x') | KeyCode::Char('X') => {
            let update = mutate_selected(&mut st, |item| {
                item.is_checked = !item.is_checked;
                ReviewUpdateItemParams {
                    item_id: item.id,
                    status: None,
                    is_checked: Some(item.is_checked),
                }
            });
            if let Some(params) = update {
                drop(st);
                send_request(
                    server_writer,
                    methods::REVIEW_UPDATE_ITEM,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
        KeyCode::Char('t') | KeyCode::Char('T') => {
            let update = mutate_selected(&mut st, |item| {
                item.status = item.status.next();
                ReviewUpdateItemParams {
                    item_id: item.id,
                    status: Some(item.status),
                    is_checked: None,
                }
            });
            if st.review.hide_closed {
                st.review.clamp_selection();
                st.review.scroll_to_selected();
            }
            if let Some(params) = update {
                drop(st);
                send_request(
                    server_writer,
                    methods::REVIEW_UPDATE_ITEM,
                    serde_json::to_value(params)?,
                )
                .await?;
            }
        }
        KeyCode::Char('c')
        | KeyCode::Char('C')
        | KeyCode::Char('y')
        | KeyCode::Char('Y')
        | KeyCode::Enter => {
            let text = st
                .review
                .selected_item_index()
                .and_then(|idx| st.review.session.as_ref()?.items.get(idx))
                .map(format_item_for_clipboard);
            if let Some(text) = text {
                let msg = if crate::clipboard::copy_to_clipboard(&text) {
                    "Review finding copied to clipboard"
                } else {
                    "Failed to copy to clipboard"
                };
                st.model.copy_notification = Some((msg.to_string(), std::time::Instant::now()));
            }
        }
        _ => {}
    }

    Ok(InputResult::Continue)
}
