use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::Mutex;
use tauqe_protocol::{
    methods, ContextAccess, ContextAddParams, ContextAddPatternParams, ContextLayer,
    ContextRemoveParams, ContextSetAccessParams,
};

use crate::app::{AppState, KeyCommand, ViewMode};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

use super::ContextRow;

pub const CONTEXT_COMMANDS: &[KeyCommand] = &[
    // Core Actions
    KeyCommand { key: "e", description: "Add file to context as editable" },
    KeyCommand { key: "r / a", description: "Add file to context as read-only" },
    KeyCommand { key: "t", description: "Toggle access (Editable ↔ Read-Only)" },
    KeyCommand { key: "u", description: "Promote Auto file to persistent User file" },
    KeyCommand { key: "Enter", description: "Promote Auto file or toggle section" },
    // Navigation & Inspection
    KeyCommand { key: "n / p (or j/k, ↑/↓)", description: "Move selection down / up" },
    KeyCommand { key: "Space / Tab", description: "Fold / unfold section" },
    KeyCommand { key: "Esc / q", description: "Return to Develop view" },
    // Auxiliary & Destructive
    KeyCommand { key: "d / x / Del", description: "Remove file from context" },
    KeyCommand { key: "c", description: "Clear all Auto-requested files (Auto)" },
    KeyCommand { key: "C / Shift+C", description: "Clear all persistent User files (User)" },
];

pub async fn handle_context_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    if st.context_view.adding_file {
        match key.code {
            KeyCode::Esc => {
                st.context_view.adding_file = false;
                st.context_view.add_input.clear();
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
                    .cloned()
                {
                    if !candidate.starts_with("[+] Add all matching '") {
                        st.context_view.add_input.clear();
                        st.context_view.add_input.insert_str(&candidate);
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

                let raw_input = st.context_view.add_input.get_text().trim().to_string();
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
            _ => {
                let is_cmd = st.tui_config.input.primary_modifier.matches(key.modifiers);
                if st.context_view.add_input.handle_key(key, is_cmd) {
                    st.context_view.selected_candidate_index = 0;
                    st.update_filtered_candidates();
                }
            }
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
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('p') => {
                if total_rows > 0 {
                    st.context_view.cursor_index =
                        st.context_view.cursor_index.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('n') => {
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
                    st.notify_info(format!("Promoting '{}' to User context...", path));
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
            KeyCode::Char('u') => {
                if let Some(ContextRow::Item(ref item)) = current_row {
                    if item.layer == ContextLayer::Auto {
                        let path = item.path.clone();
                        let access = item.access;
                        st.notify_info(format!("Promoting '{}' to User context...", path));
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
                        st.notify_warning("Only Auto files can be promoted to User context");
                    }
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                // Lowercase `c` clears the Auto layer; uppercase `C` (Shift+C)
                // clears the persistent User layer, mirroring the Auto flow.
                let clear_user = key.code == KeyCode::Char('C')
                    || key.modifiers.contains(KeyModifiers::SHIFT);
                let layer = if clear_user {
                    ContextLayer::User
                } else {
                    ContextLayer::Auto
                };
                let count = st
                    .context
                    .items
                    .iter()
                    .filter(|i| i.layer == layer)
                    .count();
                if count == 0 {
                    if clear_user {
                        st.notify_info("No user files to clear");
                    } else {
                        st.notify_info("No auto files to clear");
                    }
                } else {
                    st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    if clear_user {
                        st.context_view.confirm_clear_user = true;
                    } else {
                        st.context_view.confirm_clear_auto = true;
                    }
                }
            }
            KeyCode::Char('e') => {
                st.context_view.adding_file = true;
                st.context_view.add_access = ContextAccess::Editable;
                st.context_view.add_input.clear();
                st.context_view.selected_candidate_index = 0;
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
                        st.notify_warning(
                            "Pinned files are read-only and cannot be changed",
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
                    let user_count = st
                        .context
                        .items
                        .iter()
                        .filter(|i| i.layer == ContextLayer::User)
                        .count();
                    if user_count == 0 {
                        st.notify_info("No user files to clear");
                    } else {
                        st.context_view.confirm_clear_user = true;
                        st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    Ok(InputResult::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_state;

    fn item(path: &str, layer: ContextLayer) -> tauqe_protocol::ContextItem {
        tauqe_protocol::ContextItem {
            path: path.to_string(),
            access: ContextAccess::ReadOnly,
            size_bytes: 128,
            estimated_tokens: 32,
            layer,
        }
    }

    #[tokio::test]
    async fn test_shift_c_opens_clear_user_confirmation() {
        let mut st = test_state();
        st.view_mode = ViewMode::Context;
        st.context.items.push(item("src/lib.rs", ContextLayer::User));
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT);
        handle_context_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(st.context_view.confirm_clear_user);
        assert!(!st.context_view.confirm_clear_auto);
    }

    #[tokio::test]
    async fn test_lowercase_c_clears_auto_not_user() {
        let mut st = test_state();
        st.view_mode = ViewMode::Context;
        st.context.items.push(item("src/lib.rs", ContextLayer::User));
        st.context.items.push(item("auto.rs", ContextLayer::Auto));
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        handle_context_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(st.context_view.confirm_clear_auto);
        assert!(!st.context_view.confirm_clear_user);
    }

    #[test]
    fn test_command_bindings_are_unique() {
        use std::collections::HashSet;
        let keys: HashSet<&str> = CONTEXT_COMMANDS.iter().map(|c| c.key).collect();
        assert!(!CONTEXT_COMMANDS.is_empty());
        assert_eq!(
            keys.len(),
            CONTEXT_COMMANDS.len(),
            "duplicate key bindings in CONTEXT_COMMANDS"
        );
    }
}
