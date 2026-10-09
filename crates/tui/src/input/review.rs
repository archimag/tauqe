use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::{methods, ReviewItem};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

use crate::app::{AppState, KeyCommand, ReviewDialogState, ViewMode};
use crate::input::InputResult;
use crate::ui::review::VisibleReviewRow;

pub const REVIEW_COMMANDS: &[KeyCommand] = &[
    KeyCommand { key: "n / p (or ↑/↓)", description: "Navigate review sessions and findings" },
    KeyCommand { key: "C-n / C-p", description: "Inspect next / previous finding (accordion walk)" },
    KeyCommand { key: "Tab / Space", description: "Fold / unfold session or finding details" },
    KeyCommand { key: "x", description: "Execute target finding or all Todo findings in session (with confirmation)" },
    KeyCommand { key: "Enter", description: "Execute finding; fold/unfold on session headers" },
    KeyCommand { key: "d", description: "Discuss selected finding or entire session in Develop" },
    KeyCommand { key: "t / s", description: "Change finding status (Discussion / Todo / InProgress...)" },
    KeyCommand { key: "f", description: "Toggle filter: show all / hide closed (FIXED/REJECTED)" },
    KeyCommand { key: "a", description: "Toggle fold / unfold all sessions and findings" },
    KeyCommand { key: "r", description: "Run code review on current context" },
    KeyCommand { key: "Shift+R", description: "Refresh review sessions from server" },
    KeyCommand { key: "c / y", description: "Copy selected finding or session to clipboard" },
    KeyCommand { key: "Delete", description: "Delete review session (with confirmation)" },
    KeyCommand { key: "Ctrl+R", description: "Fold / unfold thinking stream" },
    KeyCommand { key: "PgUp / PgDn", description: "Page scroll review view" },
    KeyCommand { key: "Home / End", description: "Select first / last row" },
    KeyCommand { key: "Esc / q", description: "Return to Develop view" },
];

fn execute_review_scope_action(
    st: &mut AppState,
    session_id: &str,
    target_item_id: Option<u32>,
    scope_title: &str,
) {
    if st.model.is_busy() || st.review.running {
        st.notify_warning("Model is currently busy; wait for turn to finish or press Esc to cancel");
        return;
    }

    let session = match st
        .review
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .cloned()
        .or_else(|| {
            st.review
                .session
                .as_ref()
                .filter(|s| s.id == session_id)
                .cloned()
        }) {
        Some(s) => s,
        None => {
            st.notify_error(format!("Review session '{}' not found", session_id));
            return;
        }
    };

    if let Some(item_id) = target_item_id {
        let item = match session.find_item(item_id) {
            Some(i) => i.clone(),
            None => {
                st.notify_error(format!("Review item #{} not found", item_id));
                return;
            }
        };

        match item.status {
            tauqe_protocol::ReviewStatus::Discussion => {
                st.notify_warning(format!(
                    "Finding #{} is in DISCUSSION status; approve or change status to TODO (t) before executing.",
                    item.id
                ));
            }
            tauqe_protocol::ReviewStatus::Fixed => {
                st.notify_info(format!("Finding #{} is already FIXED.", item.id));
            }
            tauqe_protocol::ReviewStatus::Rejected => {
                st.notify_warning(format!(
                    "Finding #{} is REJECTED; change status to TODO (t) before executing.",
                    item.id
                ));
            }
            tauqe_protocol::ReviewStatus::Todo | tauqe_protocol::ReviewStatus::InProgress => {
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState {
                    review_id: session.id.clone(),
                    review_title: if session.title.trim().is_empty() {
                        session.id.clone()
                    } else {
                        session.title.clone()
                    },
                    scope_title: scope_title.to_string(),
                    items: vec![item],
                });
            }
        }
    } else {
        let discussion_items: Vec<_> = session
            .items
            .iter()
            .filter(|i| i.status == tauqe_protocol::ReviewStatus::Discussion)
            .collect();

        if !discussion_items.is_empty() {
            let preview = if discussion_items.len() <= 2 {
                discussion_items
                    .iter()
                    .map(|i| format!("#{}", i.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                format!(
                    "{}, and {} more",
                    discussion_items[..2]
                        .iter()
                        .map(|i| format!("#{}", i.id))
                        .collect::<Vec<_>>()
                        .join(", "),
                    discussion_items.len() - 2
                )
            };
            st.notify_warning(format!(
                "Review session contains {} item(s) in DISCUSSION ({}). Review or approve (t) before executing batch.",
                discussion_items.len(),
                preview
            ));
            return;
        }

        let todo_items: Vec<_> = session
            .items
            .iter()
            .filter(|i| {
                i.status == tauqe_protocol::ReviewStatus::Todo
                    || i.status == tauqe_protocol::ReviewStatus::InProgress
            })
            .cloned()
            .collect();

        if todo_items.is_empty() {
            let (total, fixed) = session.stats();
            if fixed == total && total > 0 {
                st.notify_info("All findings in this review session are already FIXED.");
            } else {
                st.notify_info("No TODO findings to execute in this review session.");
            }
            return;
        }

        st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
        st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState {
            review_id: session.id.clone(),
            review_title: if session.title.trim().is_empty() {
                session.id.clone()
            } else {
                session.title.clone()
            },
            scope_title: format!("All remaining TODO findings ({} items)", todo_items.len()),
            items: todo_items,
        });
    }
}

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
        if key.code == KeyCode::Esc || key.code == KeyCode::Char('q') {
            st.confirm_cancel = true;
            st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
        }
        return Ok(InputResult::Continue);
    }

    let primary_mod = st.tui_config.input.primary_modifier;
    let is_ctrl_alt = key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
        && key.modifiers.contains(crossterm::event::KeyModifiers::ALT);
    let is_primary = !is_ctrl_alt && primary_mod.matches(key.modifiers);
    let is_ctrl = key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);

    let is_ctrl_n = !is_ctrl_alt
        && (is_ctrl || is_primary)
        && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'));
    let is_ctrl_p = !is_ctrl_alt
        && (is_ctrl || is_primary)
        && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'));

    if is_ctrl_n {
        st.review.accordion_navigate(true);
        return Ok(InputResult::Continue);
    }
    if is_ctrl_p {
        st.review.accordion_navigate(false);
        return Ok(InputResult::Continue);
    }

    let rows_len = st.review.flatten_rows().len();
    let page = st.review.view_height.saturating_sub(2).max(1);

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            st.view_mode = ViewMode::Develop;
        }
        KeyCode::Up | KeyCode::Char('p') | KeyCode::Char('P') if !is_ctrl && !is_primary => {
            if st.review.selected_index > 0 {
                st.review.selected_index -= 1;
                st.review.scroll_to_selected();
            }
        }
        KeyCode::Down | KeyCode::Char('n') | KeyCode::Char('N') if !is_ctrl && !is_primary => {
            if rows_len > 0 && st.review.selected_index + 1 < rows_len {
                st.review.selected_index += 1;
                st.review.scroll_to_selected();
            }
        }
        KeyCode::Home => {
            st.review.selected_index = 0;
            st.review.scroll_to_selected();
        }
        KeyCode::End => {
            if rows_len > 0 {
                st.review.selected_index = rows_len - 1;
                st.review.scroll_to_selected();
            }
        }
        KeyCode::PageUp => {
            st.review.scroll = st.review.scroll.saturating_sub(page);
        }
        KeyCode::PageDown => {
            let max = (st.review.rendered_lines as u16).saturating_sub(page);
            st.review.scroll = st.review.scroll.saturating_add(page).min(max);
        }
        KeyCode::Tab | KeyCode::Char(' ') => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisibleReviewRow::SessionHeader { session_id, .. } => {
                        st.review.toggle_session_expanded(&session_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                    VisibleReviewRow::ReviewItem {
                        session_id,
                        item_id,
                        ..
                    } => {
                        st.review.toggle_item_expanded(&session_id, item_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                }
            }
        }
        KeyCode::Enter => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisibleReviewRow::SessionHeader { session_id, .. } => {
                        st.review.toggle_session_expanded(&session_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                    VisibleReviewRow::ReviewItem {
                        session_id,
                        item_id,
                        title,
                        ..
                    } => {
                        execute_review_scope_action(&mut st, &session_id, Some(item_id), &title);
                    }
                }
            }
        }
        KeyCode::Char('x') | KeyCode::Char('X') => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisibleReviewRow::SessionHeader { session_id, title, .. } => {
                        execute_review_scope_action(&mut st, &session_id, None, &title);
                    }
                    VisibleReviewRow::ReviewItem {
                        session_id,
                        item_id,
                        title,
                        ..
                    } => {
                        execute_review_scope_action(&mut st, &session_id, Some(item_id), &title);
                    }
                }
            }
        }
        KeyCode::Char('d') | KeyCode::Char('D') => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                let session_id = row.session_id().to_string();
                let session_opt = st
                    .review
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .cloned()
                    .or_else(|| {
                        st.review
                            .session
                            .as_ref()
                            .filter(|s| s.id == session_id)
                            .cloned()
                    });

                if let Some(session) = session_opt {
                    let mut focused = Vec::new();
                    if let VisibleReviewRow::ReviewItem {
                        item_id,
                        title,
                        ..
                    } = row
                    {
                        focused.push((item_id, title));
                    }
                    st.discuss_review_dialog = Some(crate::app::DiscussReviewDialogState {
                        review_id: session.id.clone(),
                        review_title: session.title.clone(),
                        focused_items: focused,
                        prompt_editor: crate::editor::InputEditor::default(),
                    });
                }
            }
        }
        KeyCode::Char('t') | KeyCode::Char('T') | KeyCode::Char('s') | KeyCode::Char('S') => {
            let sel_row = st.review.selected_row();
            if let Some(VisibleReviewRow::ReviewItem {
                session_id,
                item_id,
                title,
                status,
                ..
            }) = sel_row
            {
                let cur_idx = match status {
                    tauqe_protocol::ReviewStatus::Discussion => 0,
                    tauqe_protocol::ReviewStatus::Todo => 1,
                    tauqe_protocol::ReviewStatus::InProgress => 2,
                    tauqe_protocol::ReviewStatus::Fixed => 3,
                    tauqe_protocol::ReviewStatus::Rejected => 4,
                };
                st.status_dialog = Some(crate::app::StatusDialogState {
                    target: crate::app::StatusDialogTarget::ReviewItem {
                        review_id: Some(session_id),
                        item_id,
                        item_title: title,
                        current_status: status,
                    },
                    selected_index: cur_idx,
                });
            }
        }
        KeyCode::Char('f') | KeyCode::Char('F') => {
            st.review.toggle_hide_closed();
            st.review.scroll_to_selected();
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            if st.review.collapsed_sessions.is_empty() && st.review.expanded_items.is_empty() {
                st.review.collapse_all();
            } else {
                st.review.expand_all();
            }
            st.review.clamp_selection();
            st.review.scroll_to_selected();
        }
        KeyCode::Delete => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                st.confirm_delete_review = Some(row.session_id().to_string());
            }
        }
        KeyCode::Char('c')
        | KeyCode::Char('C')
        | KeyCode::Char('y')
        | KeyCode::Char('Y') => {
            let sel_row = st.review.selected_row();
            let text = match sel_row {
                Some(VisibleReviewRow::ReviewItem {
                    session_id,
                    item_id,
                    ..
                }) => {
                    let s_opt = st.review.sessions.iter().find(|s| s.id == session_id).or(st.review.session.as_ref());
                    s_opt.and_then(|s| s.items.iter().find(|i| i.id == item_id)).map(format_item_for_clipboard)
                }
                Some(VisibleReviewRow::SessionHeader { session_id, .. }) => {
                    let s_opt = st.review.sessions.iter().find(|s| s.id == session_id).or(st.review.session.as_ref());
                    s_opt.map(|s| s.raw_markdown.clone())
                }
                None => None,
            };

            if let Some(text) = text {
                drop(st);
                let res = crate::clipboard::copy_to_clipboard(&text);
                let mut st = state.lock().await;
                match res {
                    crate::clipboard::CopyResult::Native => {
                        st.notify_success("Review findings copied to clipboard");
                    }
                    crate::clipboard::CopyResult::Osc52Only => {
                        st.notify_info("Review findings sent to terminal clipboard (OSC 52)");
                    }
                    crate::clipboard::CopyResult::Failed => {
                        st.notify_error("Failed to copy to clipboard");
                    }
                }
            }
        }
        KeyCode::Char('r') if !key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
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
                    prompt_editor: crate::editor::InputEditor::default(),
                });
            }
        }
        KeyCode::Char('R') | KeyCode::Char('r') if key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
            drop(st);
            crate::rpc::send_request(
                server_writer,
                methods::REVIEW_LIST,
                serde_json::json!({}),
            )
            .await?;
        }
        _ => {}
    }

    Ok(InputResult::Continue)
}
