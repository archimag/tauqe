//! Generic scrollable list selector.
//!
//! The model picker and the plan/review status dialogs all present a vertical
//! list navigated with the same keys and the same viewport arithmetic.
//! [`ListSelector`] centralizes that behavior: it owns the selected index, the
//! scroll offset, and the "keep the selection visible" math, while callers keep
//! their own item vector and supply the visible height. Sharing one component
//! removes the duplicated index math that previously drifted between the render
//! and the mouse hit-testing code paths (Conventions.md §7).

use crossterm::event::{KeyCode, KeyEvent};

use crate::command::translate_code;
use crate::config::InputConfig;

/// State for a keyboard-navigable, scrollable list.
///
/// The component is content-agnostic: it stores only the selection cursor, the
/// scroll offset, and the row count. Callers own the actual items and pass the
/// visible height on each navigation call, so the same logic serves the model
/// picker and the status dialogs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListSelector {
    selected_index: usize,
    scroll_offset: usize,
    item_count: usize,
}

impl ListSelector {
    /// Creates a selector over `item_count` rows, clamping `selected_index` into
    /// range.
    pub fn new(item_count: usize, selected_index: usize) -> Self {
        let mut selector = Self {
            selected_index,
            scroll_offset: 0,
            item_count,
        };
        selector.clamp_selection();
        selector
    }

    /// Number of rows in the list.
    pub fn item_count(&self) -> usize {
        self.item_count
    }

    /// Index of the currently selected row.
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// First visible row, given the last navigation call's viewport height.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Updates the row count, clamping the selection when the list shrinks.
    pub fn set_item_count(&mut self, item_count: usize) {
        self.item_count = item_count;
        self.clamp_selection();
    }

    /// Moves the selection up by one row.
    pub fn select_prev(&mut self, visible_height: usize) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
            self.ensure_visible(visible_height);
        }
    }

    /// Moves the selection down by one row.
    pub fn select_next(&mut self, visible_height: usize) {
        if self.selected_index + 1 < self.item_count {
            self.selected_index += 1;
            self.ensure_visible(visible_height);
        }
    }

    /// Moves the selection directly to `index`, clamping it into range.
    pub fn select_index(&mut self, index: usize, visible_height: usize) {
        if self.item_count == 0 {
            return;
        }
        self.selected_index = index.min(self.item_count - 1);
        self.ensure_visible(visible_height);
    }

    /// Moves the selection to the first row.
    pub fn select_first(&mut self, visible_height: usize) {
        self.selected_index = 0;
        self.ensure_visible(visible_height);
    }

    /// Moves the selection to the last row.
    pub fn select_last(&mut self, visible_height: usize) {
        if self.item_count > 0 {
            self.selected_index = self.item_count - 1;
            self.ensure_visible(visible_height);
        }
    }

    /// Moves the selection up by one page.
    pub fn page_up(&mut self, visible_height: usize) {
        self.selected_index = self.selected_index.saturating_sub(visible_height.max(1));
        self.ensure_visible(visible_height);
    }

    /// Moves the selection down by one page.
    pub fn page_down(&mut self, visible_height: usize) {
        let last = self.item_count.saturating_sub(1);
        self.selected_index = self
            .selected_index
            .saturating_add(visible_height.max(1))
            .min(last);
        self.ensure_visible(visible_height);
    }

    /// Scrolls the window so the selected row stays visible.
    pub fn ensure_visible(&mut self, visible_height: usize) {
        if visible_height == 0 || self.item_count == 0 {
            self.scroll_offset = 0;
            return;
        }
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset.saturating_add(visible_height) {
            self.scroll_offset = self.selected_index + 1 - visible_height;
        }
        let max_scroll = self.item_count.saturating_sub(visible_height);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
    }

    /// Applies a standard navigation key (`↑`/`↓`, `n`/`p`, `j`/`k`,
    /// `PgUp`/`PgDn`, `Home`/`End`). Returns `true` when the key moved the
    /// selection so the caller can redraw; returns `false` for any other key so
    /// the caller can handle activation or dismissal.
    ///
    /// `input` is the workstation's configured keyboard mapping. The letter
    /// chords are literal here rather than command specifications, so the press
    /// is projected through the configured layout before it is matched: `о`,
    /// `л`, `т`, and `з` walk the list from a Cyrillic layout exactly as `j`,
    /// `k`, `n`, and `p` do (Conventions §5.1). Dedicated navigation keys pass
    /// through the projection unchanged.
    pub fn handle_nav_key(
        &mut self,
        key: KeyEvent,
        input: &InputConfig,
        visible_height: usize,
    ) -> bool {
        let code = translate_code(key.code, input);
        match code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('p') => {
                self.select_prev(visible_height);
                true
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('n') => {
                self.select_next(visible_height);
                true
            }
            KeyCode::PageUp => {
                self.page_up(visible_height);
                true
            }
            KeyCode::PageDown => {
                self.page_down(visible_height);
                true
            }
            KeyCode::Home => {
                self.select_first(visible_height);
                true
            }
            KeyCode::End => {
                self.select_last(visible_height);
                true
            }
            _ => false,
        }
    }

    fn clamp_selection(&mut self) {
        if self.item_count == 0 {
            self.selected_index = 0;
            self.scroll_offset = 0;
        } else if self.selected_index >= self.item_count {
            self.selected_index = self.item_count - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn test_navigation_scrolls_and_clamps() {
        let mut selector = ListSelector::new(20, 0);
        assert_eq!(selector.selected_index(), 0);
        assert_eq!(selector.scroll_offset(), 0);

        for _ in 0..4 {
            selector.select_next(5);
        }
        assert_eq!(selector.selected_index(), 4);
        assert_eq!(selector.scroll_offset(), 0);

        selector.select_next(5);
        assert_eq!(selector.selected_index(), 5);
        assert_eq!(selector.scroll_offset(), 1);

        selector.select_last(5);
        assert_eq!(selector.selected_index(), 19);
        assert_eq!(selector.scroll_offset(), 15);

        selector.select_first(5);
        assert_eq!(selector.selected_index(), 0);
        assert_eq!(selector.scroll_offset(), 0);
    }

    #[test]
    fn test_handle_nav_key_pages_and_ignores_others() {
        let mut selector = ListSelector::new(20, 0);
        let input = InputConfig::default();
        assert!(selector.handle_nav_key(key(KeyCode::Down), &input, 5));
        assert_eq!(selector.selected_index(), 1);
        assert!(selector.handle_nav_key(key(KeyCode::Char('n')), &input, 5));
        assert_eq!(selector.selected_index(), 2);
        assert!(selector.handle_nav_key(key(KeyCode::PageDown), &input, 5));
        assert_eq!(selector.selected_index(), 7);
        assert!(selector.handle_nav_key(key(KeyCode::Char('p')), &input, 5));
        assert_eq!(selector.selected_index(), 6);
        assert!(!selector.handle_nav_key(key(KeyCode::Enter), &input, 5));
    }

    /// The letter chords follow the configured layout, so every picker that
    /// reuses this arithmetic stays navigable from the JCUKEN layout
    /// (Conventions §5.1).
    #[test]
    fn test_handle_nav_key_follows_the_configured_layout() {
        use crate::config::LayoutPreset;

        let input = InputConfig {
            layout: LayoutPreset::RuJcuken,
            ..InputConfig::default()
        };
        let mut selector = ListSelector::new(20, 0);

        // `т` sits on `n`, `о` on `j`, `з` on `p`, and `л` on `k`.
        for cyrillic in ['т', 'т', 'о'] {
            assert!(selector.handle_nav_key(key(KeyCode::Char(cyrillic)), &input, 10));
        }
        assert_eq!(selector.selected_index(), 3);
        for cyrillic in ['з', 'л'] {
            assert!(selector.handle_nav_key(key(KeyCode::Char(cyrillic)), &input, 10));
        }
        assert_eq!(selector.selected_index(), 1);
    }

    #[test]
    fn test_empty_list_is_stable() {
        let mut selector = ListSelector::new(0, 0);
        selector.select_next(5);
        selector.select_prev(5);
        selector.page_down(5);
        assert_eq!(selector.selected_index(), 0);
        assert_eq!(selector.scroll_offset(), 0);
    }

    #[test]
    fn test_shrink_clamps_selection() {
        let mut selector = ListSelector::new(10, 9);
        selector.set_item_count(3);
        assert_eq!(selector.selected_index(), 2);
        assert_eq!(selector.item_count(), 3);
    }
}
