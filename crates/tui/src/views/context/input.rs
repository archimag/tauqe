use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::{
    methods, ContextAccess, ContextAddParams, ContextAddPatternParams, ContextLayer,
    ContextRemoveParams, ContextSetAccessParams,
};
use tokio::sync::Mutex;

use crate::app::{AppState, ViewMode};
use crate::command::CommandCategory::{Essential, Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::i18n::engine::active_t;
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
    SelectFirst,
    SelectLast,
    PageUp,
    PageDown,
    FoldToggle,
    FoldAll,
    Return,
    Remove,
    ClearAuto,
    ClearUser,
    Copy,
    Refresh,
}

/// Single source of truth for the Context view keybindings.
///
/// Essential commands mutate the three context layers; Interface commands
/// navigate and fold the section tree; Helpers copy paths, refresh state,
/// remove a file, or clear a whole layer with modal confirmation. The map
/// drives input resolution and the generated Help dialog. Chords are grouped by
/// the canonical category order, and a whole-layer clear is reachable solely
/// through its uppercase `Shift` chord, so declaration order never decides which
/// action a key resolves to.
pub static CONTEXT_COMMAND_MAP: CommandMap<ContextAction> = CommandMap::new(&[
    // Helpers: clipboard copying, refresh, and single-file deletion.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('c')),
        ContextAction::Copy,
        Helpers,
        "cmd.context.copy",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('y')),
        ContextAction::Copy,
        Helpers,
        "cmd.context.copy",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('g')),
        ContextAction::Refresh,
        Helpers,
        "cmd.context.refresh",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('d')),
        ContextAction::Remove,
        Helpers,
        "cmd.context.remove",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Delete),
        ContextAction::Remove,
        Helpers,
        "cmd.context.remove",
    ),
    // Destructive layer clearing: uppercase Shift chords with modal confirmation.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('C')),
        ContextAction::ClearUser,
        Helpers,
        "cmd.context.clear_user",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('c')),
        ContextAction::ClearUser,
        Helpers,
        "cmd.context.clear_user",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('A')),
        ContextAction::ClearAuto,
        Helpers,
        "cmd.context.clear_auto",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('a')),
        ContextAction::ClearAuto,
        Helpers,
        "cmd.context.clear_auto",
    ),
    // Essential: context layer mutations.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('e')),
        ContextAction::AddEditable,
        Essential,
        "cmd.context.add_editable",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('r')),
        ContextAction::AddReadOnly,
        Essential,
        "cmd.context.add_readonly",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('t')),
        ContextAction::ToggleAccess,
        Essential,
        "cmd.context.toggle_access",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('u')),
        ContextAction::PromoteToUser,
        Essential,
        "cmd.context.promote_user",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Enter),
        ContextAction::Activate,
        Essential,
        "cmd.context.activate",
    ),
    // Interface: navigation, folding, and view switching.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        ContextAction::Return,
        Interface,
        "cmd.context.return",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        ContextAction::Return,
        Interface,
        "cmd.context.return",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('a')),
        ContextAction::FoldAll,
        Interface,
        "cmd.context.fold_all",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        ContextAction::FoldToggle,
        Interface,
        "cmd.context.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(' ')),
        ContextAction::FoldToggle,
        Interface,
        "cmd.context.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        ContextAction::NavPrev,
        Interface,
        "cmd.context.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('k')),
        ContextAction::NavPrev,
        Interface,
        "cmd.context.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('p')),
        ContextAction::NavPrev,
        Interface,
        "cmd.context.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        ContextAction::NavNext,
        Interface,
        "cmd.context.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('j')),
        ContextAction::NavNext,
        Interface,
        "cmd.context.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        ContextAction::NavNext,
        Interface,
        "cmd.context.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Home),
        ContextAction::SelectFirst,
        Interface,
        "cmd.context.select_first",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('<')),
        ContextAction::SelectFirst,
        Interface,
        "cmd.context.select_first",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Home),
        ContextAction::SelectFirst,
        Interface,
        "cmd.context.select_first",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::End),
        ContextAction::SelectLast,
        Interface,
        "cmd.context.select_last",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('>')),
        ContextAction::SelectLast,
        Interface,
        "cmd.context.select_last",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::End),
        ContextAction::SelectLast,
        Interface,
        "cmd.context.select_last",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageUp),
        ContextAction::PageUp,
        Interface,
        "cmd.context.page_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageDown),
        ContextAction::PageDown,
        Interface,
        "cmd.context.page_down",
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

                let is_pattern_entry = selected_candidate
                    .as_ref()
                    .is_some_and(|c| c.starts_with("[+] Add all matching '"));

                let raw_input = st.context_view.add_input.get_text().trim().to_string();
                let is_glob_direct =
                    raw_input.contains('*') || raw_input.contains('?') || raw_input.ends_with('/');

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
        ContextAction::SelectFirst => {
            st.context_view.cursor_index = 0;
        }
        ContextAction::SelectLast => {
            if total_rows > 0 {
                st.context_view.cursor_index = total_rows - 1;
            }
        }
        ContextAction::PageUp => {
            let page = if st.context_view.view_height > 2 {
                st.context_view.view_height.saturating_sub(2)
            } else {
                10
            };
            st.context_view.cursor_index = st.context_view.cursor_index.saturating_sub(page);
        }
        ContextAction::PageDown => {
            let page = if st.context_view.view_height > 2 {
                st.context_view.view_height.saturating_sub(2)
            } else {
                10
            };
            if total_rows > 0 {
                st.context_view.cursor_index =
                    (st.context_view.cursor_index + page).min(total_rows - 1);
            }
        }
        ContextAction::FoldToggle => {
            if let Some(ContextRow::Header(layer)) = current_row {
                st.context_view.toggle_section(layer);
                let rows_len = st.context_view.compute_rows(&st.context.items).len();
                if rows_len > 0 && st.context_view.cursor_index >= rows_len {
                    st.context_view.cursor_index = rows_len - 1;
                }
            }
        }
        ContextAction::FoldAll => {
            st.context_view.toggle_fold_all();
            let rows_len = st.context_view.compute_rows(&st.context.items).len();
            if rows_len > 0 && st.context_view.cursor_index >= rows_len {
                st.context_view.cursor_index = rows_len - 1;
            }
        }
        ContextAction::Activate => match current_row {
            Some(ContextRow::Header(layer)) => {
                st.context_view.toggle_section(layer);
                let rows_len = st.context_view.compute_rows(&st.context.items).len();
                if rows_len > 0 && st.context_view.cursor_index >= rows_len {
                    st.context_view.cursor_index = rows_len - 1;
                }
            }
            Some(ContextRow::Item(ref item)) if item.layer == ContextLayer::Auto => {
                let path = item.path.clone();
                let access = item.access;
                st.notify_info(active_t("notify.promoting_to_user").replace("{}", &path));
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
                    st.notify_info(active_t("notify.promoting_to_user").replace("{}", &path));
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
                    st.notify_warning(active_t("notify.only_auto_promotable"));
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
                st.notify_info(active_t("notify.no_auto_files"));
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
                st.notify_info(active_t("notify.no_user_files"));
            } else {
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.context_view.confirm_clear_user = true;
            }
        }
        ContextAction::Copy => {
            let to_copy = match current_row {
                Some(ContextRow::Item(ref item)) => Some(item.path.clone()),
                Some(ContextRow::Header(layer)) => {
                    let paths: Vec<String> = st
                        .context
                        .items
                        .iter()
                        .filter(|i| i.layer == layer)
                        .map(|i| i.path.clone())
                        .collect();
                    if paths.is_empty() {
                        None
                    } else {
                        Some(paths.join("\n"))
                    }
                }
                None => None,
            };

            if let Some(text) = to_copy {
                drop(st);
                let res = crate::clipboard::copy_to_clipboard(&text);
                let mut st = state.lock().await;
                match res {
                    crate::clipboard::CopyResult::Native => {
                        st.notify_success(active_t("notify.context_copied"));
                    }
                    crate::clipboard::CopyResult::Osc52Only => {
                        st.notify_info(active_t("notify.context_copied_osc52"));
                    }
                    crate::clipboard::CopyResult::Failed => {
                        st.notify_error(active_t("notify.copy_failed"));
                    }
                }
            } else if let Some(ContextRow::Header(_)) = current_row {
                st.notify_info(active_t("notify.context_no_files_copy"));
            }
        }
        ContextAction::Refresh => {
            drop(st);
            send_request(server_writer, methods::CONTEXT_GET, serde_json::json!({})).await?;
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
                    st.notify_warning(active_t("notify.pinned_readonly"));
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
                    st.notify_warning(active_t("notify.pinned_protected_remove"));
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
                    st.notify_info(active_t("notify.no_auto_files"));
                } else {
                    st.context_view.confirm_clear_auto = true;
                    st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                }
            }
            Some(ContextRow::Header(ContextLayer::Pinned)) => {
                st.notify_warning(active_t("notify.pinned_protected_section"));
            }
            Some(ContextRow::Header(ContextLayer::User)) => {
                let user_count = st
                    .context
                    .items
                    .iter()
                    .filter(|i| i.layer == ContextLayer::User)
                    .count();
                if user_count == 0 {
                    st.notify_info(active_t("notify.no_user_files"));
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
        st.context
            .items
            .push(item("src/lib.rs", ContextLayer::User));
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('C'), crossterm::event::KeyModifiers::SHIFT);
        handle_context_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(st.context_view.confirm_clear_user);
        assert!(!st.context_view.confirm_clear_auto);
    }

    #[tokio::test]
    async fn test_shift_a_opens_clear_auto_confirmation() {
        let mut st = test_state();
        st.view_mode = ViewMode::Context;
        st.context.items.push(item("auto.rs", ContextLayer::Auto));
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('A'), crossterm::event::KeyModifiers::SHIFT);
        handle_context_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(st.context_view.confirm_clear_auto);
        assert!(!st.context_view.confirm_clear_user);
    }

    #[tokio::test]
    async fn test_lowercase_c_copies_and_does_not_clear() {
        let mut st = test_state();
        st.view_mode = ViewMode::Context;
        st.context
            .items
            .push(item("src/lib.rs", ContextLayer::User));
        st.context.items.push(item("auto.rs", ContextLayer::Auto));
        // Select an item
        st.context_view.cursor_index = 2; // User file item
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::NONE);
        handle_context_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(!st.context_view.confirm_clear_auto);
        assert!(!st.context_view.confirm_clear_user);
    }

    #[tokio::test]
    async fn test_x_does_not_remove_file() {
        let mut st = test_state();
        st.view_mode = ViewMode::Context;
        st.context
            .items
            .push(item("src/lib.rs", ContextLayer::User));
        st.context_view.cursor_index = 2; // User file item
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('x'), crossterm::event::KeyModifiers::NONE);
        let res = handle_context_key(key, &state, &mut conn).await.unwrap();
        assert!(matches!(res, InputResult::Continue));
        // x is unmapped in context view (reserved for execute), so connection buffer is empty
        assert!(conn.written().is_empty());
    }

    #[tokio::test]
    async fn test_a_toggles_fold_all() {
        let mut st = test_state();
        st.view_mode = ViewMode::Context;
        assert!(st.context_view.is_all_expanded());
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('a'), crossterm::event::KeyModifiers::NONE);
        handle_context_key(key, &state, &mut conn).await.unwrap();

        let guard = state.lock().await;
        assert!(!guard.context_view.is_all_expanded());
        assert!(!guard.context_view.pinned_expanded);
        assert!(!guard.context_view.user_expanded);
        assert!(!guard.context_view.auto_expanded);
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
