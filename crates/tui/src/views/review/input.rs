use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::{methods, ModelRole, ModelSelection, ReviewItem, ReviewRefineItemParams};
use tokio::sync::Mutex;

use crate::app::{AppState, KeyCommand, ReviewDialogState, ViewMode};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

use super::VisibleReviewRow;

pub const REVIEW_COMMANDS: &[KeyCommand] = &[
    // Core Actions
    KeyCommand { key: "x", description: "Execute target finding or all Todo findings in session (with confirmation)" },
    KeyCommand { key: "X", description: "Execute all Todo findings across all sessions (with confirmation)" },
    KeyCommand { key: "Enter", description: "Execute finding; fold/unfold on session headers" },
    KeyCommand { key: "r", description: "Run code review on current context" },
    KeyCommand { key: "d", description: "Discuss selected finding or entire session in Develop" },
    KeyCommand { key: "R", description: "Decompose selected finding into steps with AI (Architect)" },
    KeyCommand { key: "f", description: "Set discussion focus to selected session or finding without leaving the view" },
    KeyCommand { key: "t / s", description: "Change finding status (Discussion / Todo / InProgress...)" },
    // Navigation & Inspection
    KeyCommand { key: "n / p (or ↑/↓)", description: "Navigate review sessions and findings" },
    KeyCommand { key: "C-n / C-p", description: "Inspect next / previous finding (accordion walk)" },
    KeyCommand { key: "Tab / Space", description: "Fold / unfold session, summary, or finding details" },
    KeyCommand { key: "a", description: "Toggle fold / unfold all sessions and findings" },
    KeyCommand { key: "F", description: "Toggle filter: show all / hide closed (FIXED/REJECTED)" },
    KeyCommand { key: "PgUp / PgDn", description: "Page scroll review view" },
    KeyCommand { key: "Home / End", description: "Select first / last row" },
    KeyCommand { key: "Esc / q", description: "Return to Develop view" },
    // Editing & Clipboard
    KeyCommand { key: "c / y", description: "Copy selected finding or session to clipboard" },
    // Auxiliary & Destructive
    KeyCommand { key: "g", description: "Refresh review sessions from server" },
    KeyCommand { key: "Delete", description: "Delete review session (with confirmation)" },
];

fn execute_review_scope_action(
    st: &mut AppState,
    session_id: &str,
    target_item_id: Option<u32>,
    scope_title: &str,
) {
    if st.model.is_busy() {
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
            tauqe_protocol::ReviewItemStatus::Discussion => {
                st.notify_warning(format!(
                    "Finding #{} is in DISCUSSION status; approve or change status to TODO (t) before executing.",
                    item.id
                ));
            }
            tauqe_protocol::ReviewItemStatus::Fixed => {
                st.notify_info(format!("Finding #{} is already FIXED.", item.id));
            }
            tauqe_protocol::ReviewItemStatus::Skipped => {
                st.notify_info(format!(
                    "Finding #{} is SKIPPED; change status to TODO (t) before executing.",
                    item.id
                ));
            }
            tauqe_protocol::ReviewItemStatus::Rejected => {
                st.notify_warning(format!(
                    "Finding #{} is REJECTED; change status to TODO (t) before executing.",
                    item.id
                ));
            }
            tauqe_protocol::ReviewItemStatus::Todo | tauqe_protocol::ReviewItemStatus::InProgress => {
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState {
                    review_id: Some(session.id.clone()),
                    review_title: if session.title.trim().is_empty() {
                        session.id.clone()
                    } else {
                        session.title.clone()
                    },
                    scope_title: scope_title.to_string(),
                    item_id: Some(item.id),
                    items: vec![item],
                });
            }
        }
    } else {
        let todo_items: Vec<_> = session
            .items
            .iter()
            .filter(|i| {
                i.status == tauqe_protocol::ReviewItemStatus::Todo
                    || i.status == tauqe_protocol::ReviewItemStatus::InProgress
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
            review_id: Some(session.id.clone()),
            review_title: if session.title.trim().is_empty() {
                session.id.clone()
            } else {
                session.title.clone()
            },
            scope_title: format!("All remaining TODO findings ({} items)", todo_items.len()),
            item_id: None,
            items: todo_items,
        });
    }
}

pub(crate) fn execute_all_reviews_action(st: &mut AppState) {
    if st.model.is_busy() {
        st.notify_warning("Model is currently busy; wait for turn to finish or press Esc to cancel");
        return;
    }

    let sessions = st.review.effective_sessions();
    if sessions.is_empty() {
        st.notify_info("No review sessions available.");
        return;
    }

    let mut todo_items = Vec::new();
    let mut total_items = 0;
    let mut discussion_count = 0;
    let mut fixed_count = 0;

    for session in &sessions {
        for item in &session.items {
            total_items += 1;
            match item.status {
                tauqe_protocol::ReviewItemStatus::Todo | tauqe_protocol::ReviewItemStatus::InProgress => {
                    todo_items.push(item.clone());
                }
                tauqe_protocol::ReviewItemStatus::Discussion => {
                    discussion_count += 1;
                }
                tauqe_protocol::ReviewItemStatus::Fixed => {
                    fixed_count += 1;
                }
                _ => {}
            }
        }
    }

    if todo_items.is_empty() {
        if total_items == 0 {
            st.notify_info("No findings in review sessions.");
        } else if discussion_count > 0 {
            st.notify_warning(
                "All remaining findings are in DISCUSSION status; approve or change status to TODO (t) before executing.",
            );
        } else if fixed_count == total_items {
            st.notify_info("All findings across all review sessions are already FIXED.");
        } else {
            st.notify_info("No TODO findings to execute across review sessions.");
        }
        return;
    }

    st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
    st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState {
        review_id: None,
        review_title: "All Review Sessions".to_string(),
        scope_title: format!(
            "All remaining TODO findings across all sessions ({} items)",
            todo_items.len()
        ),
        item_id: None,
        items: todo_items,
    });
}

fn format_item_for_clipboard(item: &ReviewItem) -> String {
    let mut text = format!("[{}] {}", item.severity, item.title);
    if !item.files.is_empty() {
        text.push('\n');
        text.push_str(&item.files.join(", "));
    }
    if let Some(notes) = &item.resolution_notes {
        let trimmed = notes.trim();
        if !trimmed.is_empty() {
            text.push_str("\n\nResolution: ");
            text.push_str(trimmed);
        }
    }
    if !item.body.trim().is_empty() {
        text.push_str("\n\n");
        text.push_str(item.body.trim());
    }
    text
}

/// Decomposes the selected finding into an embedded step plan via the Architect
/// role (`review/refineItem`).
///
/// The Architect either produces a decomposed subplan carried by the updated
/// finding or reports a semantic block; both outcomes arrive as a typed
/// `review/refineItem` response and are applied by the response dispatcher. The
/// developer stays in the Review view, which surfaces the resulting sub-steps.
async fn refine_selected_finding(
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    let params = {
        let mut st = state.lock().await;
        if st.model.is_busy() || st.review.running || st.review.refining.is_some() {
            st.notify_warning("Model is currently busy; wait for operation to finish");
            return Ok(());
        }
        let (review_id, item_id) = match st.review.selected_row() {
            Some(VisibleReviewRow::ReviewItem {
                session_id, item_id, ..
            }) => (session_id, item_id),
            _ => {
                st.notify_info("Select a finding to decompose into steps (R)");
                return Ok(());
            }
        };
        st.review.refining = Some(review_id.clone());
        ReviewRefineItemParams {
            review_id: Some(review_id),
            item_id,
            model: None,
            instructions: None,
        }
    };
    send_request(
        server_writer,
        methods::REVIEW_REFINE_ITEM,
        serde_json::to_value(&params)?,
    )
    .await?;
    state
        .lock()
        .await
        .notify_info("Decomposing finding with the Architect...");
    Ok(())
}

pub async fn handle_review_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

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
                    VisibleReviewRow::SessionSummary { session_id, .. } => {
                        st.review.toggle_summary_expanded(&session_id);
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
                    VisibleReviewRow::SessionSummary { session_id, .. } => {
                        st.review.toggle_summary_expanded(&session_id);
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
        KeyCode::Char('X') => {
            execute_all_reviews_action(&mut st);
        }
        KeyCode::Char('x') => {
            if key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) {
                execute_all_reviews_action(&mut st);
            } else {
                let sel_row = st.review.selected_row();
                if let Some(row) = sel_row {
                    match row {
                        VisibleReviewRow::SessionHeader { session_id, title, .. } => {
                            execute_review_scope_action(&mut st, &session_id, None, &title);
                        }
                        VisibleReviewRow::SessionSummary { session_id, .. } => {
                            execute_review_scope_action(&mut st, &session_id, None, &session_id);
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
                    st.prompt_modal = Some(crate::app::PromptModalState::new(
                        crate::app::PromptModalTarget::Review {
                            review_id: session.id.clone(),
                            review_title: session.title.clone(),
                            focused_items: focused,
                        },
                    ));
                }
            }
        }
        KeyCode::Char('f')
            if !is_ctrl
                && !is_primary
                && !key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) =>
        {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                let session_id = row.session_id().to_string();
                let review_title = st
                    .review
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .map(|s| s.title.clone())
                    .or_else(|| {
                        st.review
                            .session
                            .as_ref()
                            .filter(|s| s.id == session_id)
                            .map(|s| s.title.clone())
                    })
                    .unwrap_or_else(|| session_id.clone());
                let (focused, target_label) = match &row {
                    VisibleReviewRow::ReviewItem { item_id, title, .. } => (
                        vec![item_id.to_string()],
                        Some(format!("finding #{} '{}'", item_id, title)),
                    ),
                    _ => (Vec::new(), None),
                };
                let focus = tauqe_protocol::DiscussionFocus::review(session_id, focused);
                st.discussion_focus = focus.clone();
                let params = tauqe_protocol::DiscussionSetFocusParams { focus };
                let message = match target_label {
                    Some(label) => {
                        format!("Discussion focus set to {} in review '{}'", label, review_title)
                    }
                    None => format!("Discussion focus set to review '{}'", review_title),
                };
                drop(st);
                send_request(
                    server_writer,
                    methods::DISCUSSION_SET_FOCUS,
                    serde_json::to_value(&params)?,
                )
                .await?;
                let mut st = state.lock().await;
                st.notify_info(message);
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
                    tauqe_protocol::ReviewItemStatus::Discussion => 0,
                    tauqe_protocol::ReviewItemStatus::Todo => 1,
                    tauqe_protocol::ReviewItemStatus::InProgress => 2,
                    tauqe_protocol::ReviewItemStatus::Fixed => 3,
                    tauqe_protocol::ReviewItemStatus::Skipped => 4,
                    tauqe_protocol::ReviewItemStatus::Rejected => 5,
                };
                st.status_dialog = Some(crate::app::StatusDialogState::new(
                    crate::app::StatusDialogTarget::ReviewItem {
                        review_id: Some(session_id),
                        item_id,
                        item_title: title,
                        current_status: status,
                    },
                    cur_idx,
                ));
            }
        }
        KeyCode::Char('F') | KeyCode::Char('f')
            if key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) =>
        {
            st.review.toggle_hide_closed();
            st.review.scroll_to_selected();
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            st.review.toggle_fold_all();
            st.review.clamp_selection();
            st.review.scroll_to_selected();
        }
        KeyCode::Delete => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                st.confirm_delete_review = Some(row.session_id().to_string());
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                Some(VisibleReviewRow::SessionSummary { description, .. }) => Some(description),
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
                let reviewer_entries = st
                    .model_choice
                    .roles
                    .as_ref()
                    .and_then(|roles| roles.entries.as_ref())
                    .map(|entries| entries.reviewer.clone())
                    .filter(|entries| !entries.is_empty())
                    .unwrap_or_else(|| {
                        let fallback_model = st
                            .model_choice
                            .resolve(&ModelSelection::Role(ModelRole::Reviewer))
                            .unwrap_or_else(|| st.active_model.clone());
                        let fallback_effort = st
                            .model_choice
                            .roles
                            .as_ref()
                            .and_then(|roles| roles.efforts.as_ref())
                            .and_then(|efforts| efforts.reviewer);
                        vec![tauqe_protocol::ModelEntry::new(
                            fallback_model,
                            fallback_effort,
                            None,
                        )]
                    });
                let files_count = st.context.items.len();
                let estimated_tokens = st.context.total_estimated_tokens as usize;
                st.review.error = None;
                let mut dialog =
                    ReviewDialogState::new(files_count, estimated_tokens, reviewer_entries);
                // Open on the reviewer list when more than one model is
                // configured so it can be picked instantly with the digit
                // shortcuts; a single model leaves focus on the editor
                // (c-magit-key-navigation).
                if dialog.selector.len() > 1 {
                    dialog.focus = crate::app::ReviewDialogFocus::ModelList;
                }
                st.review_dialog = Some(dialog);
            }
        }
        // Decompose the selected finding into an embedded step plan via the
        // Architect role (`review/refineItem`).
        KeyCode::Char('R') => {
            drop(st);
            refine_selected_finding(state, server_writer).await?;
        }
        KeyCode::Char('r') if key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
            drop(st);
            refine_selected_finding(state, server_writer).await?;
        }
        // Refresh review sessions from the server.
        KeyCode::Char('g') | KeyCode::Char('G') => {
            drop(st);
            send_request(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_command_bindings_are_unique() {
        let keys: HashSet<&str> = REVIEW_COMMANDS.iter().map(|c| c.key).collect();
        assert!(!REVIEW_COMMANDS.is_empty());
        assert_eq!(
            keys.len(),
            REVIEW_COMMANDS.len(),
            "duplicate key bindings in REVIEW_COMMANDS"
        );
    }

    #[tokio::test]
    async fn test_review_dialog_uses_configured_reviewer_role() {
        use tauqe_protocol::{ModelRef, ModelRolesEffort, ModelRolesSummary, ReasoningEffort};

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.context.items.push(tauqe_protocol::ContextItem {
            path: "crates/core/src/lib.rs".to_string(),
            access: tauqe_protocol::ContextAccess::ReadOnly,
            size_bytes: 100,
            estimated_tokens: 25,
            layer: tauqe_protocol::ContextLayer::User,
        });

        let rev_model = ModelRef::openrouter("anthropic/claude-3.5-sonnet");
        let rev_entry = tauqe_protocol::ModelEntry::new(
            rev_model.clone(),
            Some(ReasoningEffort::High),
            Some("Claude Auditor".to_string()),
        );
        st.model_choice.roles = Some(ModelRolesSummary {
            architect: ModelRef::openrouter("test-arch"),
            coder: ModelRef::openrouter("test-coder"),
            primary: ModelRef::openrouter("test-pri"),
            reviewer: rev_model.clone(),
            scribe: ModelRef::openrouter("test-scribe"),
            efforts: Some(ModelRolesEffort {
                architect: None,
                coder: None,
                primary: None,
                reviewer: Some(ReasoningEffort::High),
                scribe: None,
            }),
            entries: Some(tauqe_protocol::ModelRolesEntries {
                reviewer: vec![rev_entry],
                ..Default::default()
            }),
        });

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('r'), crossterm::event::KeyModifiers::NONE);
        let res = handle_review_key(key, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        let dialog = st.review_dialog.as_ref().expect("dialog should be opened");
        assert_eq!(dialog.selector.role(), ModelRole::Reviewer);
        let selected = dialog.selected_entry().expect("entry must exist");
        assert_eq!(selected.model, rev_model);
        assert_eq!(selected.effort, Some(ReasoningEffort::High));
        assert_eq!(selected.alias.as_deref(), Some("Claude Auditor"));
    }

    #[test]
    fn test_execute_review_scope_action_defaults_to_safe_cancel() {
        use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSession, ReviewSeverity};

        let mut st = crate::app::test_state();
        st.model.status = crate::views::develop::TurnStatus::Idle;
        // Start on the destructive button to prove the action resets the focus.
        st.confirm_button = crate::app::ConfirmDialogButton::Confirm;

        let session = ReviewSession {
            id: "rev-1".to_string(),
            title: "Session rev-1".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewItem {
                id: 1,
                title: "Finding".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                files: Vec::new(),
                subplan: Vec::new(),
                resolution_notes: None,
                body: String::new(),
            }],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session.clone()];
        st.review.session = Some(session);

        execute_review_scope_action(&mut st, "rev-1", Some(1), "Finding");

        assert_eq!(
            st.confirm_button,
            crate::app::ConfirmDialogButton::Cancel,
            "the confirmation must default to safe cancellation"
        );
        let target = st
            .confirm_execute_review
            .as_ref()
            .expect("confirmation state should be armed");
        assert_eq!(target.review_id, Some("rev-1".to_string()));
        assert_eq!(target.item_id, Some(1));
        assert_eq!(target.items.len(), 1);
    }

    #[test]
    fn test_execute_all_reviews_action_discussion_only_warns() {
        use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSession, ReviewSeverity};

        let mut st = crate::app::test_state();
        let session = ReviewSession {
            id: "rev-disc".to_string(),
            title: "Discussion Only Session".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![
                ReviewItem {
                    id: 1,
                    title: "Finding 1".to_string(),
                    model: None,
                    severity: ReviewSeverity::Warning,
                    status: ReviewItemStatus::Discussion,
                    files: Vec::new(),
                    subplan: Vec::new(),
                    resolution_notes: None,
                    body: String::new(),
                },
                ReviewItem {
                    id: 2,
                    title: "Finding 2".to_string(),
                    model: None,
                    severity: ReviewSeverity::Suggestion,
                    status: ReviewItemStatus::Discussion,
                    files: Vec::new(),
                    subplan: Vec::new(),
                    resolution_notes: None,
                    body: String::new(),
                },
            ],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session];

        execute_all_reviews_action(&mut st);

        assert!(
            st.confirm_execute_review.is_none(),
            "dialog must not open when all findings are in DISCUSSION"
        );
        let notif = st
            .active_notification()
            .expect("notification should be surfaced");
        assert!(
            notif.text.contains("DISCUSSION"),
            "expected notification warning about DISCUSSION status, got: {}",
            notif.text
        );
    }

    #[test]
    fn test_execute_all_reviews_action_with_todo_opens_modal() {
        use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSession, ReviewSeverity};

        let mut st = crate::app::test_state();
        let session1 = ReviewSession {
            id: "rev-1".to_string(),
            title: "Session 1".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![
                ReviewItem {
                    id: 1,
                    title: "Finding 1".to_string(),
                    model: None,
                    severity: ReviewSeverity::Warning,
                    status: ReviewItemStatus::Todo,
                    files: Vec::new(),
                    subplan: Vec::new(),
                    resolution_notes: None,
                    body: String::new(),
                },
                ReviewItem {
                    id: 2,
                    title: "Finding 2".to_string(),
                    model: None,
                    severity: ReviewSeverity::Info,
                    status: ReviewItemStatus::Fixed,
                    files: Vec::new(),
                    subplan: Vec::new(),
                    resolution_notes: None,
                    body: String::new(),
                },
            ],
            raw_markdown: String::new(),
        };
        let session2 = ReviewSession {
            id: "rev-2".to_string(),
            title: "Session 2".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![
                ReviewItem {
                    id: 1,
                    title: "Finding 3".to_string(),
                    model: None,
                    severity: ReviewSeverity::Critical,
                    status: ReviewItemStatus::InProgress,
                    files: Vec::new(),
                    subplan: Vec::new(),
                    resolution_notes: None,
                    body: String::new(),
                },
                ReviewItem {
                    id: 2,
                    title: "Finding 4".to_string(),
                    model: None,
                    severity: ReviewSeverity::Suggestion,
                    status: ReviewItemStatus::Discussion,
                    files: Vec::new(),
                    subplan: Vec::new(),
                    resolution_notes: None,
                    body: String::new(),
                },
            ],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session1, session2];

        execute_all_reviews_action(&mut st);

        let target = st
            .confirm_execute_review
            .as_ref()
            .expect("modal confirmation must be armed");
        assert_eq!(target.review_id, None, "cross-session execution must have review_id: None");
        assert_eq!(target.review_title, "All Review Sessions");
        assert_eq!(st.confirm_button, crate::app::ConfirmDialogButton::Cancel);
        assert_eq!(target.items.len(), 2, "must include exactly the Todo and InProgress items");
        assert_eq!(target.items[0].title, "Finding 1");
        assert_eq!(target.items[1].title, "Finding 3");
    }

    #[test]
    fn test_execute_all_reviews_action_all_fixed_notifies() {
        use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSession, ReviewSeverity};

        let mut st = crate::app::test_state();
        let session = ReviewSession {
            id: "rev-fixed".to_string(),
            title: "All Fixed Session".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewItem {
                id: 1,
                title: "Finding 1".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewItemStatus::Fixed,
                files: Vec::new(),
                subplan: Vec::new(),
                resolution_notes: None,
                body: String::new(),
            }],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session];

        execute_all_reviews_action(&mut st);

        assert!(st.confirm_execute_review.is_none());
        let notif = st.active_notification().expect("notification should be set");
        assert!(notif.text.contains("FIXED"));
    }

    #[tokio::test]
    async fn test_handle_review_key_capital_x_triggers_batch_execute() {
        use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSession, ReviewSeverity};

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        let session = ReviewSession {
            id: "rev-1".to_string(),
            title: "Session 1".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewItem {
                id: 1,
                title: "Todo Finding".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                files: Vec::new(),
                subplan: Vec::new(),
                resolution_notes: None,
                body: String::new(),
            }],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session];

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('X'), crossterm::event::KeyModifiers::NONE);
        let res = handle_review_key(key, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        let target = st.confirm_execute_review.as_ref().expect("modal must open");
        assert_eq!(target.review_id, None);
        assert_eq!(target.items.len(), 1);
    }

    fn todo_session(id: &str, item_id: u32) -> tauqe_protocol::ReviewSession {
        use tauqe_protocol::{ReviewItem, ReviewItemStatus, ReviewSession, ReviewSeverity};

        ReviewSession {
            id: id.to_string(),
            title: format!("Session {id}"),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewItem {
                id: item_id,
                title: "Todo Finding".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                files: Vec::new(),
                subplan: Vec::new(),
                resolution_notes: None,
                body: String::new(),
            }],
            raw_markdown: String::new(),
        }
    }

    #[tokio::test]
    async fn test_capital_r_decomposes_selected_finding() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![todo_session("rev-1", 1)];
        // Row 0 is the session header; row 1 is the finding.
        st.review.selected_index = 1;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('R'), crossterm::event::KeyModifiers::NONE);
        let res = handle_review_key(key, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        assert_eq!(st.review.refining.as_deref(), Some("rev-1"));
        assert_eq!(
            st.active_notification().map(|n| n.text.as_str()),
            Some("Decomposing finding with the Architect...")
        );
    }

    #[tokio::test]
    async fn test_capital_r_on_session_header_requires_a_finding() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![todo_session("rev-1", 1)];
        st.review.selected_index = 0; // session header row

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('R'), crossterm::event::KeyModifiers::NONE);
        handle_review_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(st.review.refining.is_none());
        assert_eq!(
            st.active_notification().map(|n| n.text.as_str()),
            Some("Select a finding to decompose into steps (R)")
        );
    }
}
