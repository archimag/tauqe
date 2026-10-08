use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{
    methods, ContextAccess, ContextAddParams, ContextAddPatternParams, ContextLayer,
    ContextRemoveParams, ContextSetAccessParams,
};

use crate::app::{AppState, KeyCommand, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;
use crate::ui::context::ContextRow;

pub const CONTEXT_COMMANDS: &[KeyCommand] = &[
    KeyCommand { key: "e", description: "Add file to context as editable" },
    KeyCommand { key: "r / a", description: "Add file to context as read-only" },
    KeyCommand { key: "t", description: "Toggle access (Editable ↔ Read-Only)" },
    KeyCommand { key: "p / u", description: "Promote Auto file to persistent User file" },
    KeyCommand { key: "c / C", description: "Clear all auto-requested files (Auto)" },
    KeyCommand { key: "d / x / Del", description: "Remove file from context" },
    KeyCommand { key: "Space / Tab", description: "Fold / unfold section" },
    KeyCommand { key: "↑/↓ or k/j", description: "Move selection up / down" },
    KeyCommand { key: "Enter", description: "Promote Auto file or toggle section" },
    KeyCommand { key: "Esc / q", description: "Return to Develop view" },
];

pub async fn handle_context_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    if st.context_view.adding_file {
        if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('w') => {
                    crate::input::pop_word_backward(&mut st.context_view.add_input);
                    st.context_view.selected_candidate_index = 0;
                    st.update_filtered_candidates();
                    return Ok(InputResult::Continue);
                }
                KeyCode::Char('u') => {
                    st.context_view.add_input.clear();
                    st.context_view.selected_candidate_index = 0;
                    st.update_filtered_candidates();
                    return Ok(InputResult::Continue);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                st.context_view.adding_file = false;
                st.context_view.add_input.clear();
                st.context_view.status_message = None;
            }
            KeyCode::Up => {
                if !st.context_view.filtered_candidates.is_empty() {
                    st.context_view.selected_candidate_index =
                        st.context_view.selected_candidate_index.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if !st.context_view.filtered_candidates.is_empty()
                    && st.context_view.selected_candidate_index + 1
                        < st.context_view.filtered_candidates.len()
                {
                    st.context_view.selected_candidate_index += 1;
                }
            }
            KeyCode::Tab => {
                if let Some(candidate) = st
                    .context_view
                    .filtered_candidates
                    .get(st.context_view.selected_candidate_index)
                {
                    if !candidate.starts_with("[+] Add all matching '") {
                        st.context_view.add_input = candidate.clone();
                        st.update_filtered_candidates();
                    }
                }
            }
            KeyCode::Enter => {
                let selected_candidate = st
                    .context_view
                    .filtered_candidates
                    .get(st.context_view.selected_candidate_index)
                    .cloned();

                let is_pattern_entry = selected_candidate.as_ref().is_some_and(|c| {
                    c.starts_with("[+] Add all matching '")
                });

                let raw_input = st.context_view.add_input.trim().to_string();
                let is_glob_direct = raw_input.contains('*')
                    || raw_input.contains('?')
                    || raw_input.ends_with('/');

                let access = st.context_view.add_access;
                if is_pattern_entry || is_glob_direct {
                    st.context_view.adding_file = false;
                    st.context_view.add_input.clear();
                    drop(st);

                    let params = ContextAddPatternParams {
                        pattern: raw_input,
                        access,
                    };
                    send_request(
                        server_writer,
                        methods::CONTEXT_ADD_PATTERN,
                        serde_json::to_value(params)?,
                    )
                    .await?;
                } else {
                    let target_path = if let Some(candidate) = selected_candidate {
                        candidate
                    } else {
                        raw_input
                    };

                    if !target_path.is_empty() {
                        st.context_view.adding_file = false;
                        st.context_view.add_input.clear();
                        drop(st);
                        let params = ContextAddParams {
                            path: target_path,
                            access,
                            layer: Some(ContextLayer::User),
                        };
                        send_request(
                            server_writer,
                            methods::CONTEXT_ADD,
                            serde_json::to_value(params)?,
                        )
                        .await?;
                    }
                }
            }
            KeyCode::Char(c) if crate::input::is_char_typing(key.modifiers) => {
                st.context_view.add_input.push(c);
                st.context_view.selected_candidate_index = 0;
                st.update_filtered_candidates();
            }
            KeyCode::Backspace => {
                st.context_view.add_input.pop();
                st.context_view.selected_candidate_index = 0;
                st.update_filtered_candidates();
            }
            _ => {}
        }
    } else {
        let rows = st.context_view.compute_rows(&st.context.items);
        let total_rows = rows.len();
        if st.context_view.cursor_index >= total_rows && total_rows > 0 {
            st.context_view.cursor_index = total_rows - 1;
        }
        let current_row = rows.get(st.context_view.cursor_index).cloned();

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                st.view_mode = ViewMode::Develop;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if total_rows > 0 {
                    st.context_view.cursor_index =
                        st.context_view.cursor_index.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if total_rows > 0 && st.context_view.cursor_index + 1 < total_rows {
                    st.context_view.cursor_index += 1;
                }
            }
            KeyCode::Tab | KeyCode::Char(' ') => {
                if let Some(ContextRow::Header(layer)) = current_row {
                    st.context_view.toggle_section(layer);
                }
            }
            KeyCode::Enter => match current_row {
                Some(ContextRow::Header(layer)) => {
                    st.context_view.toggle_section(layer);
                }
                Some(ContextRow::Item(ref item)) if item.layer == ContextLayer::Auto => {
                    let path = item.path.clone();
                    let access = item.access;
                    st.context_view.status_message =
                        Some(format!("Promoting '{}' to User context...", path));
                    drop(st);
                    let params = ContextAddParams {
                        path,
                        access,
                        layer: Some(ContextLayer::User),
                    };
                    send_request(
                        server_writer,
                        methods::CONTEXT_ADD,
                        serde_json::to_value(params)?,
                    )
                    .await?;
                }
                _ => {}
            },
            KeyCode::Char('p') | KeyCode::Char('u') => {
                if let Some(ContextRow::Item(ref item)) = current_row {
                    if item.layer == ContextLayer::Auto {
                        let path = item.path.clone();
                        let access = item.access;
                        st.context_view.status_message =
                            Some(format!("Promoting '{}' to User context...", path));
                        drop(st);
                        let params = ContextAddParams {
                            path,
                            access,
                            layer: Some(ContextLayer::User),
                        };
                        send_request(
                            server_writer,
                            methods::CONTEXT_ADD,
                            serde_json::to_value(params)?,
                        )
                        .await?;
                    } else {
                        st.context_view.status_message =
                            Some("Only Auto files can be promoted to User context".to_string());
                    }
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                let auto_count = st
                    .context
                    .items
                    .iter()
                    .filter(|i| i.layer == ContextLayer::Auto)
                    .count();
                if auto_count == 0 {
                    st.notify_info("No auto files to clear");
                } else {
                    st.context_view.confirm_clear_auto = true;
                    st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                }
            }
            KeyCode::Char('e') => {
                st.context_view.adding_file = true;
                st.context_view.add_access = ContextAccess::Editable;
                st.context_view.add_input.clear();
                st.context_view.selected_candidate_index = 0;
                st.context_view.status_message = None;
                st.update_filtered_candidates();

                drop(st);
                send_request(
                    server_writer,
                    methods::REPOSITORY_LIST_FILES,
                    serde_json::json!({}),
                )
                .await?;
            }
            KeyCode::Char('r') | KeyCode::Char('a') => {
                st.context_view.adding_file = true;
                st.context_view.add_access = ContextAccess::ReadOnly;
                st.context_view.add_input.clear();
                st.context_view.selected_candidate_index = 0;
                st.context_view.status_message = None;
                st.update_filtered_candidates();

                drop(st);
                send_request(
                    server_writer,
                    methods::REPOSITORY_LIST_FILES,
                    serde_json::json!({}),
                )
                .await?;
            }
            KeyCode::Char('t') => {
                if let Some(ContextRow::Item(ref item)) = current_row {
                    if item.layer == ContextLayer::Pinned {
                        st.context_view.status_message = Some(
                            "Pinned files are read-only and cannot be changed".to_string(),
                        );
                    } else {
                        let path = item.path.clone();
                        let next_access = match item.access {
                            ContextAccess::Editable => ContextAccess::ReadOnly,
                            ContextAccess::ReadOnly => ContextAccess::Editable,
                        };
                        drop(st);
                        let params = ContextSetAccessParams {
                            path,
                            access: next_access,
                        };
                        send_request(
                            server_writer,
                            methods::CONTEXT_SET_ACCESS,
                            serde_json::to_value(params)?,
                        )
                        .await?;
                    }
                }
            }
            KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => match current_row {
                Some(ContextRow::Item(ref item)) => {
                    if item.layer == ContextLayer::Pinned {
                        st.notify_warning(
                            "Pinned files are protected and cannot be removed",
                        );
                    } else {
                        let path = item.path.clone();
                        if st.context_view.cursor_index > 0
                            && st.context_view.cursor_index >= total_rows.saturating_sub(1)
                        {
                            st.context_view.cursor_index -= 1;
                        }
                        st.notify_info(format!("Removed '{}' from context", path));
                        drop(st);
                        let params = ContextRemoveParams { path };
                        send_request(
                            server_writer,
                            methods::CONTEXT_REMOVE,
                            serde_json::to_value(params)?,
                        )
                        .await?;
                    }
                }
                Some(ContextRow::Header(ContextLayer::Auto)) => {
                    let auto_count = st
                        .context
                        .items
                        .iter()
                        .filter(|i| i.layer == ContextLayer::Auto)
                        .count();
                    if auto_count == 0 {
                        st.notify_info("No auto files to clear");
                    } else {
                        st.context_view.confirm_clear_auto = true;
                        st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                }
                Some(ContextRow::Header(ContextLayer::Pinned)) => {
                    st.notify_warning(
                        "Pinned section is protected and cannot be removed",
                    );
                }
                Some(ContextRow::Header(ContextLayer::User)) => {
                    st.notify_info("To remove a User file, select the file and press 'd'");
                }
                _ => {}
            },
            _ => {}
        }
    }

    Ok(InputResult::Continue)
}
