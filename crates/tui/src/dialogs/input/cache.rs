//! Keyboard contract and request dispatch of the external documentation cache
//! browser.
//!
//! The browser is a modal surface of the Context view: it reads the durable
//! cache inventory, folds the logical tree, and guards every removal behind the
//! shared confirmation card. The chords are declared once here, so the card the
//! developer reads and the keys it intercepts cannot drift apart
//! (Conventions §3, §5, §6).

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::{methods, OuterDocsCacheRemoveParams};
use tokio::sync::MutexGuard;

use crate::app::{AppState, ConfirmDialogButton};
use crate::command::CommandCategory::{Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

use super::confirm::{resolve_confirm, ConfirmOutcome};

/// Keyboard actions of the cache browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheAction {
    Close,
    Prev,
    Next,
    PageUp,
    PageDown,
    First,
    Last,
    FoldToggle,
    FoldAll,
    Remove,
}

/// Single source of truth for the cache browser keybindings.
pub static CACHE_COMMAND_MAP: CommandMap<CacheAction> = CommandMap::new(&[
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        CacheAction::Close,
        Interface,
        "cmd.outer_docs_cache.close",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        CacheAction::Close,
        Interface,
        "cmd.outer_docs_cache.close",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('g')),
        CacheAction::Close,
        Interface,
        "cmd.outer_docs_cache.close",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        CacheAction::Prev,
        Interface,
        "cmd.outer_docs_cache.prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('k')),
        CacheAction::Prev,
        Interface,
        "cmd.outer_docs_cache.prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('p')),
        CacheAction::Prev,
        Interface,
        "cmd.outer_docs_cache.prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        CacheAction::Next,
        Interface,
        "cmd.outer_docs_cache.next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('j')),
        CacheAction::Next,
        Interface,
        "cmd.outer_docs_cache.next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        CacheAction::Next,
        Interface,
        "cmd.outer_docs_cache.next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageUp),
        CacheAction::PageUp,
        Interface,
        "cmd.outer_docs_cache.page_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageDown),
        CacheAction::PageDown,
        Interface,
        "cmd.outer_docs_cache.page_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Home),
        CacheAction::First,
        Interface,
        "cmd.outer_docs_cache.first",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('<')),
        CacheAction::First,
        Interface,
        "cmd.outer_docs_cache.first",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Home),
        CacheAction::First,
        Interface,
        "cmd.outer_docs_cache.first",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::End),
        CacheAction::Last,
        Interface,
        "cmd.outer_docs_cache.last",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('>')),
        CacheAction::Last,
        Interface,
        "cmd.outer_docs_cache.last",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::End),
        CacheAction::Last,
        Interface,
        "cmd.outer_docs_cache.last",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        CacheAction::FoldToggle,
        Interface,
        "cmd.outer_docs_cache.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(' ')),
        CacheAction::FoldToggle,
        Interface,
        "cmd.outer_docs_cache.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('a')),
        CacheAction::FoldAll,
        Interface,
        "cmd.outer_docs_cache.fold_all",
    ),
    // Removing an entry edits the durable cache, so it stays on the canonical
    // `Delete` reservation and always raises the confirmation card.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Delete),
        CacheAction::Remove,
        Helpers,
        "cmd.outer_docs_cache.remove",
    ),
]);

/// Routes one key press inside the cache browser.
pub(crate) async fn handle_outer_docs_cache_key(
    key: KeyEvent,
    mut st: MutexGuard<'_, AppState>,
) -> anyhow::Result<Option<InputResult>> {
    // The visible window and the resolved command are read before the dialog
    // borrows the state, so the borrow ends before an arm mutates another field.
    let visible = crate::dialogs::render::outer_docs_cache_visible_rows(st.last_frame_area);
    let action = CACHE_COMMAND_MAP
        .resolve(key, &st.tui_config.input, false)
        .map(|spec| spec.action);

    let Some(action) = action else {
        return Ok(Some(InputResult::Continue));
    };

    if action == CacheAction::Close {
        st.modals.outer_docs_cache = None;
        return Ok(Some(InputResult::Continue));
    }

    let page = isize::try_from(visible).unwrap_or(isize::MAX);
    let Some(dialog) = st.modals.outer_docs_cache.as_mut() else {
        return Ok(None);
    };

    let mut removal = None;
    match action {
        CacheAction::Close => {}
        CacheAction::Prev => dialog.move_cursor(-1),
        CacheAction::Next => dialog.move_cursor(1),
        CacheAction::PageUp => dialog.move_cursor(-page),
        CacheAction::PageDown => dialog.move_cursor(page),
        CacheAction::First => dialog.select_first(),
        CacheAction::Last => dialog.select_last(),
        CacheAction::FoldToggle => dialog.toggle_fold(),
        CacheAction::FoldAll => dialog.toggle_fold_all(),
        CacheAction::Remove => {
            if let Some(row) = dialog.rows().get(dialog.cursor) {
                removal = Some(row.path.clone());
            }
        }
    }
    dialog.scroll_to_cursor(visible);

    if let Some(path) = removal {
        st.modals.confirm_button = ConfirmDialogButton::Cancel;
        st.modals.confirm_outer_docs_cache_remove = Some(path);
    }
    Ok(Some(InputResult::Continue))
}

/// Routes one key press inside the cache removal confirmation.
pub(crate) async fn handle_outer_docs_cache_remove_key(
    key: KeyEvent,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let Some(path) = st.modals.confirm_outer_docs_cache_remove.take() else {
        return Ok(None);
    };

    match resolve_confirm(&mut st, key) {
        ConfirmOutcome::Toggled | ConfirmOutcome::Ignored => {
            st.modals.confirm_outer_docs_cache_remove = Some(path)
        }
        ConfirmOutcome::Dismissed => {
            st.modals.confirm_button = ConfirmDialogButton::Cancel;
        }
        ConfirmOutcome::Accepted => {
            st.modals.confirm_button = ConfirmDialogButton::Cancel;
            // The card states that the tree is reloading while the removal
            // travels, so a stale node never reads as a refused deletion.
            if let Some(dialog) = st.modals.outer_docs_cache.as_mut() {
                dialog.loading = true;
            }
            drop(st);
            let params = OuterDocsCacheRemoveParams { path };
            send_request(
                server_writer,
                methods::OUTER_DOCS_CACHE_REMOVE,
                serde_json::to_value(params)?,
            )
            .await?;
        }
    }
    Ok(Some(InputResult::Continue))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use tokio::sync::Mutex;

    use super::*;
    use crate::app::{ActiveModal, OuterDocsCacheDialogState, ViewMode};
    use crate::i18n::catalog::canonical_catalog;
    use crate::rpc::RpcConnection;

    fn entry(
        path: &str,
        size_bytes: u64,
        estimated_tokens: u64,
    ) -> tauqe_protocol::OuterDocsCacheEntry {
        tauqe_protocol::OuterDocsCacheEntry {
            path: path.to_string(),
            size_bytes,
            estimated_tokens,
        }
    }

    /// Browser over two documents of one identifier, with the highlight on the
    /// first document row: the projection puts the directory first.
    fn cache_state() -> Arc<Mutex<AppState>> {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Context;
        st.last_frame_area = Rect::new(0, 0, 120, 40);
        st.modals.outer_docs_cache = Some(OuterDocsCacheDialogState {
            entries: vec![
                entry("openrouter/chat-completion", 2048, 512),
                entry("openrouter/models", 1024, 256),
            ],
            cursor: 1,
            ..Default::default()
        });
        Arc::new(Mutex::new(st))
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn cache_command_map_declares_unique_chords() {
        let chords: Vec<KeyChord> = CACHE_COMMAND_MAP
            .specs()
            .iter()
            .map(|spec| spec.chord)
            .collect();
        assert!(!chords.is_empty());
        for (index, chord) in chords.iter().enumerate() {
            for other in chords.iter().skip(index + 1) {
                assert_ne!(chord, other, "duplicate key chord in CACHE_COMMAND_MAP");
            }
        }
    }

    #[test]
    fn every_cache_command_description_is_a_catalog_key() {
        let catalog: HashSet<String> = canonical_catalog()
            .into_iter()
            .map(|item| item.key)
            .collect();
        for spec in CACHE_COMMAND_MAP.specs() {
            assert!(
                catalog.contains(spec.description_key),
                "description key '{}' is missing from the canonical catalog",
                spec.description_key
            );
        }
    }

    #[tokio::test]
    async fn delete_raises_the_removal_confirmation_with_the_safe_default() {
        let state = cache_state();
        let conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        handle_outer_docs_cache_key(press(KeyCode::Delete), guard)
            .await
            .expect("in-memory dispatch never fails");

        assert!(
            conn.written().is_empty(),
            "the removal waits for the confirmation card"
        );
        let guard = state.lock().await;
        assert_eq!(
            guard.modals.confirm_outer_docs_cache_remove.as_deref(),
            Some("openrouter/chat-completion")
        );
        assert_eq!(guard.modals.confirm_button, ConfirmDialogButton::Cancel);
    }

    #[tokio::test]
    async fn a_confirmed_removal_sends_the_logical_path() {
        let state = cache_state();
        {
            let mut guard = state.lock().await;
            guard.modals.confirm_outer_docs_cache_remove = Some("openrouter/models".to_string());
            guard.modals.confirm_button = ConfirmDialogButton::Confirm;
        }
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        handle_outer_docs_cache_remove_key(press(KeyCode::Enter), guard, &mut conn)
            .await
            .expect("in-memory dispatch never fails");

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(
            payload.contains(methods::OUTER_DOCS_CACHE_REMOVE),
            "{payload}"
        );
        assert!(payload.contains("openrouter/models"), "{payload}");
        assert!(
            !payload.contains(".tauqe"),
            "the disk prefix never reaches the wire: {payload}"
        );

        let guard = state.lock().await;
        assert!(guard.modals.confirm_outer_docs_cache_remove.is_none());
        assert_eq!(guard.modals.confirm_button, ConfirmDialogButton::Cancel);
        assert!(guard
            .modals
            .outer_docs_cache
            .as_ref()
            .is_some_and(|dialog| dialog.loading));
    }

    #[tokio::test]
    async fn dismissing_the_removal_card_sends_nothing() {
        let state = cache_state();
        {
            let mut guard = state.lock().await;
            guard.modals.confirm_outer_docs_cache_remove = Some("openrouter/models".to_string());
            guard.modals.confirm_button = ConfirmDialogButton::Confirm;
        }
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        handle_outer_docs_cache_remove_key(press(KeyCode::Esc), guard, &mut conn)
            .await
            .expect("in-memory dispatch never fails");

        assert!(conn.written().is_empty());
        let guard = state.lock().await;
        assert!(guard.modals.confirm_outer_docs_cache_remove.is_none());
    }

    #[tokio::test]
    async fn the_browser_captures_no_text() {
        let state = cache_state();
        let guard = state.lock().await;
        assert_eq!(guard.active_modal(), Some(ActiveModal::OuterDocsCache));
        assert!(guard.has_active_modal());
        assert!(!guard.text_input_capturing());
    }

    #[tokio::test]
    async fn folding_the_highlighted_directory_hides_its_documents() {
        let state = cache_state();
        {
            let mut guard = state.lock().await;
            let dialog = guard
                .modals
                .outer_docs_cache
                .as_mut()
                .expect("the browser is open");
            dialog.cursor = 0;
        }
        let guard = state.lock().await;
        handle_outer_docs_cache_key(press(KeyCode::Tab), guard)
            .await
            .expect("in-memory dispatch never fails");

        let guard = state.lock().await;
        let dialog = guard
            .modals
            .outer_docs_cache
            .as_ref()
            .expect("the browser stays open");
        let rows = dialog.rows();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].collapsed);
    }
}
