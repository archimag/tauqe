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

use crate::command::{is_bare_press, translate_code};
use crate::config::InputConfig;
/// First row of the window of `visible_rows` rows that keeps row `selected`
/// visible.
///
/// This is the single definition of the keep-selection-visible arithmetic. The
/// window moves just enough to cover the selection and then clamps to the last
/// full window, so a row that is already visible never moves the window and the
/// window never scrolls past the end of the list. A surface that keeps an offset
/// between frames passes the one it holds; a surface that projects the window
/// from scratch on every frame passes `0`, because it remembers no window of its
/// own. One definition therefore serves every list in the interface, and a
/// correction reaches all of them at once instead of drifting between copies.
pub fn keep_selection_visible(
    scroll_offset: usize,
    selected: usize,
    item_count: usize,
    visible_rows: usize,
) -> usize {
    if item_count == 0 || visible_rows == 0 {
        return 0;
    }
    let height = visible_rows.min(item_count);
    let mut start = scroll_offset;
    if selected < start {
        start = selected;
    } else if selected >= start.saturating_add(height) {
        start = selected + 1 - height;
    }
    start.min(item_count.saturating_sub(height))
}
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
    ///
    /// The offset arithmetic is the shared [`keep_selection_visible`], so every
    /// list that keeps an offset between frames moves its window by the same
    /// rule.
    pub fn ensure_visible(&mut self, visible_height: usize) {
        self.scroll_offset = keep_selection_visible(
            self.scroll_offset,
            self.selected_index,
            self.item_count,
            visible_height,
        );
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
    /// through the projection unchanged. The letters are bare presses only, so a
    /// command chord or the AltGr compose chord moves nothing (Conventions §3,
    /// §5.1).
    pub fn handle_nav_key(
        &mut self,
        key: KeyEvent,
        input: &InputConfig,
        visible_height: usize,
    ) -> bool {
        let code = translate_code(key.code, input);
        let bare = is_bare_press(key.modifiers);
        match code {
            KeyCode::Up => {
                self.select_prev(visible_height);
                true
            }
            KeyCode::Down => {
                self.select_next(visible_height);
                true
            }
            // The letter chords are bare presses, so a press carrying a command
            // modifier or the AltGr compose chord moves nothing and falls
            // through to the caller (Conventions §3, §5.1).
            KeyCode::Char('k') | KeyCode::Char('p') if bare => {
                self.select_prev(visible_height);
                true
            }
            KeyCode::Char('j') | KeyCode::Char('n') if bare => {
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

    /// The letter chords are bare presses, so a press carrying a command
    /// modifier or the AltGr compose chord moves nothing, while the dedicated
    /// navigation keys stay unconditional (Conventions §3, §5.1).
    #[test]
    fn test_handle_nav_key_refuses_command_modified_letters() {
        let input = InputConfig::default();
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ] {
            let mut selector = ListSelector::new(20, 5);
            for letter in ['j', 'k', 'n', 'p'] {
                let press = KeyEvent::new(KeyCode::Char(letter), modifiers);
                assert!(
                    !selector.handle_nav_key(press, &input, 10),
                    "{letter:?} with {modifiers:?} must move nothing"
                );
            }
            assert_eq!(selector.selected_index(), 5);
            assert_eq!(selector.scroll_offset(), 0);

            assert!(selector.handle_nav_key(KeyEvent::new(KeyCode::Down, modifiers), &input, 10));
            assert_eq!(selector.selected_index(), 6);
        }
    }
    /// The shared window arithmetic covers both shapes of the interface: a list
    /// that keeps its offset between frames and a surface that projects the
    /// window from scratch. Both must agree on the window the same selection
    /// produces, which is what keeps the call sites from drifting apart.
    #[test]
    fn test_keep_selection_visible_serves_stateful_and_stateless_windows() {
        // A selection inside the window neither moves nor shrinks it.
        assert_eq!(keep_selection_visible(0, 0, 20, 5), 0);
        assert_eq!(keep_selection_visible(0, 4, 20, 5), 0);
        assert_eq!(keep_selection_visible(10, 12, 20, 5), 10);

        // Below the window the offset follows the selection one row at a time.
        assert_eq!(keep_selection_visible(0, 5, 20, 5), 1);
        assert_eq!(keep_selection_visible(0, 19, 20, 5), 15);

        // Above the window the offset rises to the selection itself.
        assert_eq!(keep_selection_visible(10, 3, 20, 5), 3);

        // A list shorter than its window has exactly one position.
        assert_eq!(keep_selection_visible(3, 0, 2, 5), 0);
        assert_eq!(keep_selection_visible(0, 1, 2, 5), 0);

        // Degenerate inputs stay stable instead of panicking.
        assert_eq!(keep_selection_visible(0, 0, 0, 5), 0);
        assert_eq!(keep_selection_visible(0, 0, 5, 0), 0);
        assert_eq!(keep_selection_visible(7, 9, 0, 0), 0);
    }
}
