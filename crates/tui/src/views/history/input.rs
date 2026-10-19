use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::Mutex;
use tauqe_protocol::{methods, HistoryGetParams};

use crate::app::{AppState, ViewMode};
use crate::command::CommandCategory::{Essential, Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::i18n::engine::active_t;
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

pub async fn request_older_history(
    st: &mut AppState,
    server_writer: &mut RpcConnection,
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

/// Keyboard actions available in the History view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAction {
    Return,
    FoldToggle,
    NavPrev,
    NavNext,
    Recenter,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    SelectFirst,
    SelectLast,
    CopyBlock,
    CopyItem,
    ClearHistory,
}

/// Single source of truth for the History view keybindings.
///
/// Essential commands fold and copy an entry; Interface commands navigate and
/// scroll; Helpers clear the whole conversation. The map drives the generated
/// Help dialog. Chords are grouped by the canonical category order, and the
/// destructiveness of a command lives in its chord, never in its position, so
/// the `Shift+C` clear chord cannot shadow the bare copy bindings.
pub static HISTORY_COMMAND_MAP: CommandMap<HistoryAction> = CommandMap::new(&[
    // Essential: folding and copying the active entry.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Enter),
        HistoryAction::FoldToggle,
        Essential,
        "cmd.history.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        HistoryAction::FoldToggle,
        Essential,
        "cmd.history.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(' ')),
        HistoryAction::FoldToggle,
        Essential,
        "cmd.history.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('c')),
        HistoryAction::CopyBlock,
        Essential,
        "cmd.history.copy_block",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('y')),
        HistoryAction::CopyItem,
        Essential,
        "cmd.history.copy_item",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('Y')),
        HistoryAction::CopyItem,
        Essential,
        "cmd.history.copy_item",
    ),
    // Interface: navigation and scrolling.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        HistoryAction::Return,
        Interface,
        "cmd.history.return",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        HistoryAction::Return,
        Interface,
        "cmd.history.return",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('p')),
        HistoryAction::NavPrev,
        Interface,
        "cmd.history.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('P')),
        HistoryAction::NavPrev,
        Interface,
        "cmd.history.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('[')),
        HistoryAction::NavPrev,
        Interface,
        "cmd.history.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::BackTab),
        HistoryAction::NavPrev,
        Interface,
        "cmd.history.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::BackTab),
        HistoryAction::NavPrev,
        Interface,
        "cmd.history.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        HistoryAction::NavNext,
        Interface,
        "cmd.history.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('N')),
        HistoryAction::NavNext,
        Interface,
        "cmd.history.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(']')),
        HistoryAction::NavNext,
        Interface,
        "cmd.history.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('l')),
        HistoryAction::Recenter,
        Interface,
        "cmd.history.recenter",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('L')),
        HistoryAction::Recenter,
        Interface,
        "cmd.history.recenter",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        HistoryAction::ScrollUp,
        Interface,
        "cmd.history.scroll_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('k')),
        HistoryAction::ScrollUp,
        Interface,
        "cmd.history.scroll_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        HistoryAction::ScrollDown,
        Interface,
        "cmd.history.scroll_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('j')),
        HistoryAction::ScrollDown,
        Interface,
        "cmd.history.scroll_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageUp),
        HistoryAction::PageUp,
        Interface,
        "cmd.history.page_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageDown),
        HistoryAction::PageDown,
        Interface,
        "cmd.history.page_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Home),
        HistoryAction::SelectFirst,
        Interface,
        "cmd.history.select_first",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('<')),
        HistoryAction::SelectFirst,
        Interface,
        "cmd.history.select_first",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Home),
        HistoryAction::SelectFirst,
        Interface,
        "cmd.history.select_first",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::End),
        HistoryAction::SelectLast,
        Interface,
        "cmd.history.select_last",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('>')),
        HistoryAction::SelectLast,
        Interface,
        "cmd.history.select_last",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::End),
        HistoryAction::SelectLast,
        Interface,
        "cmd.history.select_last",
    ),
    // Helpers: destructive clearing of the whole conversation.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('C')),
        HistoryAction::ClearHistory,
        Helpers,
        "cmd.history.clear",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('c')),
        HistoryAction::ClearHistory,
        Helpers,
        "cmd.history.clear",
    ),
]);

pub async fn handle_history_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;
    let view_height = st.last_model_height;
    let content_width = st.history_view.content_width;
    let total_lines = st.history_view.rendered_lines_count as u16;
    let max_scroll = total_lines.saturating_sub(view_height);

    // Resolve the key through the single source of truth so input dispatch and
    // the generated Help never drift (c-1). Clearing the whole conversation
    // history is destructive and isolated to this view: it is bound to Shift+C
    // and arms the existing confirmation dialog, so it can never fire from a
    // bare key or a background view (c-history-clear-isolation). `C-g` is the
    // canonical keyboard-quit chord: normalized to Esc globally, it simply
    // leaves History for Develop exactly like the `Esc` action below.
    let action = match HISTORY_COMMAND_MAP.resolve(key, &st.tui_config.input, false) {
        Some(spec) => spec.action,
        None => return Ok(InputResult::Continue),
    };

    match action {
        HistoryAction::ClearHistory => {
            if st.model.is_busy() {
                st.notify_warning(active_t("notify.history_clear_busy"));
            } else {
                st.confirm_clear_history = true;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        HistoryAction::Return => {
            st.view_mode = ViewMode::Repl;
        }
        HistoryAction::FoldToggle => {
            if let Some(item) = st.history_view.items.get(st.history_view.selected_item_index) {
                let id = item.id;
                st.history_view.toggle_expanded(id);
                st.history_view.scroll_to_selected_item(view_height, Some(content_width));
            }
        }
        HistoryAction::Recenter => {
            st.history_view.recenter_selected_item(view_height, Some(content_width));
        }
        HistoryAction::NavPrev => {
            if !st.history_view.items.is_empty() {
                if st.history_view.selected_item_index > 0 {
                    st.history_view.selected_item_index -= 1;
                    st.history_view.scroll_to_selected_item(view_height, Some(content_width));
                } else {
                    request_older_history(&mut st, server_writer, true).await?;
                }
            }
        }
        HistoryAction::NavNext => {
            if !st.history_view.items.is_empty()
                && st.history_view.selected_item_index + 1 < st.history_view.items.len()
            {
                st.history_view.selected_item_index += 1;
                st.history_view.scroll_to_selected_item(view_height, Some(content_width));
                if st.history_view.selected_item_index + 1 >= st.history_view.items.len() {
                    st.history_view.auto_scroll = true;
                }
            }
        }
        HistoryAction::CopyBlock => {
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
                            st.notify_success(
                                active_t("notify.code_copied")
                                    .replace("{}", &lines_count.to_string()),
                            );
                        }
                        crate::clipboard::CopyResult::Osc52Only => {
                            st.history_view.copy_flash =
                                Some(((block.item_id, block.block_id), std::time::Instant::now()));
                            let lines_count = block.code.lines().count().max(1);
                            st.notify_info(
                                active_t("notify.code_copied_osc52")
                                    .replace("{}", &lines_count.to_string()),
                            );
                        }
                        crate::clipboard::CopyResult::Failed => {
                            st.notify_error(active_t("notify.code_copy_failed"));
                        }
                    }
                } else if !item_text.is_empty() {
                    drop(st);
                    let res = crate::clipboard::copy_to_clipboard(&item_text);
                    let mut st = state.lock().await;
                    match res {
                        crate::clipboard::CopyResult::Native => {
                            st.notify_success(
                                active_t("notify.history_item_copied")
                                    .replace("{}", &item_id.to_string()),
                            );
                        }
                        crate::clipboard::CopyResult::Osc52Only => {
                            st.notify_info(
                                active_t("notify.history_item_copied_osc52")
                                    .replace("{}", &item_id.to_string()),
                            );
                        }
                        crate::clipboard::CopyResult::Failed => {
                            st.notify_error(
                                active_t("notify.history_item_copy_failed")
                                    .replace("{}", &item_id.to_string()),
                            );
                        }
                    }
                } else {
                    st.notify_warning(
                        active_t("notify.history_item_empty")
                            .replace("{}", &item_id.to_string()),
                    );
                }
            }
        }
        HistoryAction::CopyItem => {
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
                            st.notify_success(
                                active_t("notify.history_item_copied")
                                    .replace("{}", &item_id.to_string()),
                            );
                        }
                        crate::clipboard::CopyResult::Osc52Only => {
                            st.notify_info(
                                active_t("notify.history_item_copied_osc52")
                                    .replace("{}", &item_id.to_string()),
                            );
                        }
                        crate::clipboard::CopyResult::Failed => {
                            st.notify_error(active_t("notify.copy_failed"));
                        }
                    }
                } else {
                    st.notify_warning(
                        active_t("notify.history_item_empty")
                            .replace("{}", &item_id.to_string()),
                    );
                }
            }
        }
        HistoryAction::ScrollUp => {
            st.history_view.auto_scroll = false;
            if st.history_view.scroll > 0 {
                st.history_view.scroll = st.history_view.scroll.saturating_sub(1);
                st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            } else {
                request_older_history(&mut st, server_writer, false).await?;
            }
        }
        HistoryAction::PageUp => {
            let page = view_height.saturating_sub(2).max(1);
            st.history_view.auto_scroll = false;
            if st.history_view.scroll > 0 {
                st.history_view.scroll = st.history_view.scroll.saturating_sub(page);
                st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            } else {
                request_older_history(&mut st, server_writer, false).await?;
            }
        }
        HistoryAction::ScrollDown => {
            if st.history_view.scroll < max_scroll {
                st.history_view.scroll = st.history_view.scroll.saturating_add(1);
                st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            }
            if st.history_view.scroll >= max_scroll {
                st.history_view.auto_scroll = true;
            }
        }
        HistoryAction::PageDown => {
            let page = view_height.saturating_sub(2).max(1);
            st.history_view.scroll = (st.history_view.scroll.saturating_add(page)).min(max_scroll);
            st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            if st.history_view.scroll >= max_scroll {
                st.history_view.auto_scroll = true;
            }
        }
        HistoryAction::SelectFirst => {
            st.history_view.auto_scroll = false;
            st.history_view.scroll = 0;
            st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
            request_older_history(&mut st, server_writer, false).await?;
        }
        HistoryAction::SelectLast => {
            st.history_view.auto_scroll = true;
            st.history_view.scroll = max_scroll;
            st.history_view.update_selected_item_from_scroll(view_height, Some(content_width));
        }
    }

    Ok(InputResult::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_map_chords_are_unique() {
        let chords: Vec<KeyChord> = HISTORY_COMMAND_MAP
            .specs()
            .iter()
            .map(|spec| spec.chord)
            .collect();
        assert!(!chords.is_empty());
        for (index, chord) in chords.iter().enumerate() {
            for other in chords.iter().skip(index + 1) {
                assert_ne!(chord, other, "duplicate key chord in HISTORY_COMMAND_MAP");
            }
        }
    }

    /// Dispatches one key into the History handler with a deliberately unsafe
    /// button focus, returning the confirmation flags and the resulting focus.
    async fn dispatch(key: KeyEvent) -> (bool, bool, crate::app::ConfirmDialogButton) {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::History;
        st.confirm_button = crate::app::ConfirmDialogButton::Confirm;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let res = handle_history_key(key, &state, &mut conn)
            .await
            .expect("in-memory dispatch never fails");
        assert_eq!(res, InputResult::Continue);

        let guard = state.lock().await;
        (
            guard.confirm_clear_history,
            guard.confirm_cancel,
            guard.confirm_button,
        )
    }

    #[tokio::test]
    async fn test_shift_c_arms_history_clear_with_safe_cancel() {
        let key = KeyEvent::new(KeyCode::Char('C'), crossterm::event::KeyModifiers::SHIFT);
        let (clear, _cancel, button) = dispatch(key).await;
        assert!(clear, "Shift+C in History must arm the clear confirmation");
        assert_eq!(button, crate::app::ConfirmDialogButton::Cancel);
    }

    #[tokio::test]
    async fn test_bare_c_does_not_arm_history_clear() {
        let key = KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::NONE);
        let (clear, _cancel, _button) = dispatch(key).await;
        assert!(!clear, "a bare c must never arm the destructive clear");
    }

    #[tokio::test]
    async fn test_ctrl_l_no_longer_clears_history() {
        let key = KeyEvent::new(KeyCode::Char('l'), crossterm::event::KeyModifiers::CONTROL);
        let (clear, cancel, _button) = dispatch(key).await;
        assert!(!clear, "C-l must recenter and never clear history");
        assert!(!cancel);
    }
}
