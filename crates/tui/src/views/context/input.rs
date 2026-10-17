use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::Mutex;
use tauqe_protocol::{
    methods, ContextAccess, ContextAddParams, ContextAddPatternParams, ContextLayer,
    ContextRemoveParams, ContextSetAccessParams,
};

use crate::app::{AppState, ViewMode};
use crate::command::CommandCategory::{Essential, Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

use super::ContextRow;

/// Keyboard actions available in the Context view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAction {
    AddEditable,
    AddReadOnly,
    ToggleAccess,
    PromoteToUser,
    Activate,
    NavPrev,
    NavNext,
    FoldToggle,
    Return,
    Remove,
    ClearAuto,
    ClearUser,
}

/// Single source of truth for the Context view keybindings.
///
/// Essential commands mutate the three context layers; Interface commands
/// navigate and fold the section tree; Helpers remove a file or clear a whole
/// layer. The map drives the generated Help dialog. The destructive
/// layer-clearing chords are declared first so a shifted `C` always resolves to
/// clearing the persistent User layer instead of the Auto layer.
pub static CONTEXT_COMMAND_MAP: CommandMap<ContextAction> = CommandMap::new(&[
    // Destructive layer clearing, declared before the bare `c` binding so the
    // shifted chords never fall through to the Auto-layer action.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('C')),
        ContextAction::ClearUser,
        Helpers,
        "Clear all persistent User files",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('c')),
        ContextAction::ClearUser,
        Helpers,
        "Clear all persistent User files",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('c')),
        ContextAction::ClearAuto,
        Helpers,
        "Clear all Auto-requested files",
    ),
    // Essential: context layer mutations.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('e')),
        ContextAction::AddEditable,
        Essential,
        "Add a file to context as editable",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('r')),
        ContextAction::AddReadOnly,
        Essential,
        "Add a file to context as read-only",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('a')),
        ContextAction::AddReadOnly,
        Essential,
        "Add a file to context as read-only",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('t')),
        ContextAction::ToggleAccess,
        Essential,
        "Toggle access between Editable and Read-Only",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('u')),
        ContextAction::PromoteToUser,
        Essential,
        "Promote an Auto file to the persistent User layer",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Enter),
        ContextAction::Activate,
        Essential,
        "Promote an Auto file or fold / unfold the selected section",
    ),
    // Interface: navigation and folding.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        ContextAction::Return,
        Interface,
        "Return to the REPL",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        ContextAction::Return,
        Interface,
        "Return to the REPL",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        ContextAction::NavPrev,
        Interface,
        "Select the previous row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('k')),
        ContextAction::NavPrev,
        Interface,
        "Select the previous row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('p')),
        ContextAction::NavPrev,
        Interface,
        "Select the previous row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        ContextAction::NavNext,
        Interface,
        "Select the next row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('j')),
        ContextAction::NavNext,
        Interface,
        "Select the next row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        ContextAction::NavNext,
        Interface,
        "Select the next row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        ContextAction::FoldToggle,
        Interface,
        "Fold or unfold the selected section",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(' ')),
        ContextAction::FoldToggle,
        Interface,
        "Fold or unfold the selected section",
    ),
    // Helpers: removal.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('d')),
        ContextAction::Remove,
        Helpers,
        "Remove the selected file from context",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('x')),
        ContextAction::Remove,
        Helpers,
        "Remove the selected file from context",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Delete),
        ContextAction::Remove,
        Helpers,
        "Remove the selected file from context",
    ),
]);

pub async fn handle_context_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    // The file picker is a text-input mode: its keystrokes reach the editor
    // directly and are never resolved against the browsing command map, so the
    // unconditional character insertion invariant holds.
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
        return Ok(InputResult::Continue);
    }

    // Resolve the key through the single source of truth so input dispatch and
    // the generated Help never drift (c-1). Case-differentiated chords (Shift+C
    // versus the bare c) are encoded directly in the map.
    let rows = st.context_view.compute_rows(&st.context.items);
    let total_rows = rows.len();
    if st.context_view.cursor_index >= total_rows && total_rows > 0 {
        st.context_view.cursor_index = total_rows - 1;
    }
    let current_row = rows.get(st.context_view.cursor_index).cloned();

    let action = match CONTEXT_COMMAND_MAP.resolve(key, &st.tui_config.input, false) {
        Some(spec) => spec.action,
        None => return Ok(InputResult::Continue),
    };

    match action {
        ContextAction::Return => {
            st.view_mode = ViewMode::Repl;
        }
        ContextAction::NavPrev => {
            if total_rows > 0 {
                st.context_view.cursor_index = st.context_view.cursor_index.saturating_sub(1);
            }
        }
        ContextAction::NavNext => {
            if total_rows > 0 && st.context_view.cursor_index + 1 < total_rows {
                st.context_view.cursor_index += 1;
            }
        }
        ContextAction::FoldToggle => {
            if let Some(ContextRow::Header(layer)) = current_row {
                st.context_view.toggle_section(layer);
            }
        }
        ContextAction::Activate => match current_row {
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
        ContextAction::PromoteToUser => {
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
        ContextAction::ClearAuto => {
            let count = st
                .context
                .items
                .iter()
                .filter(|i| i.layer == ContextLayer::Auto)
                .count();
            if count == 0 {
                st.notify_info("No auto files to clear");
            } else {
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.context_view.confirm_clear_auto = true;
            }
        }
        ContextAction::ClearUser => {
            let count = st
                .context
                .items
                .iter()
                .filter(|i| i.layer == ContextLayer::User)
                .count();
            if count == 0 {
                st.notify_info("No user files to clear");
            } else {
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.context_view.confirm_clear_user = true;
            }
        }
        ContextAction::AddEditable => {
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
        ContextAction::AddReadOnly => {
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
        ContextAction::ToggleAccess => {
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
        ContextAction::Remove => match current_row {
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

        let key = KeyEvent::new(KeyCode::Char('C'), crossterm::event::KeyModifiers::SHIFT);
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

        let key = KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::NONE);
        handle_context_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(st.context_view.confirm_clear_auto);
        assert!(!st.context_view.confirm_clear_user);
    }

    #[test]
    fn test_command_map_chords_are_unique() {
        let chords: Vec<KeyChord> = CONTEXT_COMMAND_MAP
            .specs()
            .iter()
            .map(|spec| spec.chord)
            .collect();
        assert!(!chords.is_empty());
        for (index, chord) in chords.iter().enumerate() {
            for other in chords.iter().skip(index + 1) {
                assert_ne!(chord, other, "duplicate key chord in CONTEXT_COMMAND_MAP");
            }
        }
    }
}
