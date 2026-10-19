//! Reusable model-selection stepper.
//!
//! Several dialogs let the developer choose among the models configured for a
//! single semantic role — the Review launcher picks a Reviewer, the Plan Refine
//! dialog picks an Architect, and future surfaces need the same control. They
//! all share one interaction: a role badge, the selected entry, a
//! reasoning-effort indicator, an `[X of Y]` counter, and one keyboard chord
//! that cycles the list. [`ModelSelector`] centralizes that behavior so the
//! dialogs stop re-implementing the index arithmetic, the empty-list guards,
//! and the key resolution independently (Conventions.md §2, §7).
//!
//! The selector stays presentation-agnostic: [`ModelSelector::render_stepper_spans`]
//! returns composable spans so each dialog can prepend its own field label while
//! still sharing one model-presentation rule.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use tauqe_protocol::{ModelEntry, ModelRole};

use crate::app::{numbered_entry_label, role_badge};
use crate::config::{MetaModifier, PrimaryModifier};
use crate::theme::engine::current_engine;

/// Stateful selector over the models configured for one semantic role.
///
/// Holds the target role, the available entries, and the current index, and
/// guarantees that navigation is always cyclic and that an empty list can never
/// panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelector {
    role: ModelRole,
    entries: Vec<ModelEntry>,
    selected_index: usize,
}

impl ModelSelector {
    /// Creates a selector over `entries`, clamping `selected_index` into range.
    pub fn new(role: ModelRole, entries: Vec<ModelEntry>, selected_index: usize) -> Self {
        let mut selector = Self {
            role,
            entries,
            selected_index,
        };
        selector.clamp();
        selector
    }

    /// Semantic role whose configured models this selector presents.
    pub fn role(&self) -> ModelRole {
        self.role
    }

    /// Number of selectable entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the selector holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Index of the currently selected entry.
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Currently selected entry, if any.
    pub fn selected_entry(&self) -> Option<&ModelEntry> {
        self.entries.get(self.selected_index)
    }

    /// Advances to the next entry, wrapping around at the end.
    pub fn select_next(&mut self) {
        if !self.entries.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.entries.len();
        }
    }

    /// Moves to the previous entry, wrapping around at the start.
    pub fn select_prev(&mut self) {
        if !self.entries.is_empty() {
            self.selected_index = self
                .selected_index
                .checked_sub(1)
                .unwrap_or(self.entries.len() - 1);
        }
    }

    /// Selects the entry at `index`, clamping into range.
    ///
    /// Numeric quick-select resolves a digit to an index and only calls this
    /// when the index is in range, so the clamp is a defensive guard rather
    /// than a user-visible behavior.
    pub fn select_index(&mut self, index: usize) {
        self.selected_index = index;
        self.clamp();
    }

    /// Resolves a key event against the model-switch chord and the navigation
    /// arrows.
    ///
    /// The chord (`C-M` / `M-M`) always switches, even while the instruction
    /// editor owns the keyboard; the arrows switch only when the editor does
    /// not own them, so cursor movement and model browsing never fight over the
    /// same keys (Conventions.md §6, §8). Returns `true` when the selector
    /// consumed the key.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        primary: PrimaryModifier,
        meta: MetaModifier,
        editor_focused: bool,
    ) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        if is_model_switch_chord(&key, primary, meta) {
            self.select_next();
            return true;
        }

        if !editor_focused {
            match key.code {
                KeyCode::Up => {
                    self.select_prev();
                    return true;
                }
                KeyCode::Down => {
                    self.select_next();
                    return true;
                }
                _ => {}
            }
        }

        false
    }

    /// Handles a key while the embedded model list owns focus.
    ///
    /// This is the non-input navigation contract: bare digits `1..9` select by
    /// position, arrows step cyclically, and `PgUp`/`PgDn`/`Home`/`End` jump
    /// within the list. It is only invoked when the list — not the instruction
    /// editor — holds focus, so intercepting bare keys never violates the
    /// unconditional character insertion rule (Conventions §6). Returns `true`
    /// when the key was consumed.
    pub fn handle_list_key(&mut self, key: KeyEvent, visible_height: usize) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        match key.code {
            KeyCode::Char(c @ '1'..='9') => {
                let index = c as usize - '1' as usize;
                if index < self.entries.len() {
                    self.select_index(index);
                    true
                } else {
                    false
                }
            }
            KeyCode::Up => {
                self.select_prev();
                true
            }
            KeyCode::Down => {
                self.select_next();
                true
            }
            KeyCode::PageUp => {
                self.selected_index = self.selected_index.saturating_sub(visible_height.max(1));
                true
            }
            KeyCode::PageDown => {
                self.selected_index = self
                    .selected_index
                    .saturating_add(visible_height.max(1))
                    .min(self.entries.len() - 1);
                true
            }
            KeyCode::Home => {
                self.select_index(0);
                true
            }
            KeyCode::End => {
                self.select_index(self.entries.len() - 1);
                true
            }
            _ => false,
        }
    }

    /// Builds the standardized stepper line as composable spans: the role
    /// badge, the selected entry's display label, the reasoning-effort
    /// indicator, and — when more than one entry is configured — the `[X of Y]`
    /// counter together with the switch hint.
    ///
    /// Returning spans rather than a whole line lets each dialog prepend its
    /// own field label (for example `Reviewer:`) while the selector remains the
    /// single source of truth for the model presentation.
    pub fn render_stepper_spans(&self, primary: PrimaryModifier) -> Vec<Span<'static>> {
        let engine = current_engine();
        let palette = engine.palette();
        let mut spans = vec![Span::styled(
            format!("{} ", role_badge(self.role)),
            engine.fg(palette.button_active_bg).bold(),
        )];

        match self.selected_entry() {
            Some(entry) => {
                spans.push(Span::styled(
                    entry.display_label(),
                    engine.fg(palette.status_success).bold(),
                ));
                // `display_label` already folds the effort into the label when
                // the entry has no alias, so the separate indicator only fills
                // the aliased case and never duplicates the effort text.
                if has_alias(entry) {
                    if let Some(effort) = entry.effort {
                        spans.push(Span::styled(
                            format!(" [{}]", effort),
                            engine.fg(palette.status_warning).bold(),
                        ));
                    }
                }
            }
            None => spans.push(Span::styled(
                "None".to_string(),
                engine.fg(palette.text_muted),
            )),
        }

        if self.entries.len() > 1 {
            spans.push(Span::styled(
                format!(
                    "  [{} of {} • {}]",
                    self.selected_index + 1,
                    self.entries.len(),
                    model_select_hint(primary)
                ),
                engine.fg(palette.status_info),
            ));
        }

        spans
    }

    /// Renders the embedded model list as bounded, scrollable rows.
    ///
    /// Each row is `▶ [N] <label>`; the selected row is highlighted and the
    /// window follows the selection so it always stays visible. At most
    /// `visible_height` rows are produced, so the caller can paste the result
    /// into a dialog body of any size without further clipping.
    pub fn render_list_lines(&self, visible_height: usize) -> Vec<Line<'static>> {
        let engine = current_engine();
        let palette = engine.palette();
        let (start, end) = self.visible_range(visible_height);
        let mut lines = Vec::with_capacity(end.saturating_sub(start));

        for (index, entry) in self.entries.iter().enumerate().take(end).skip(start) {
            let is_selected = index == self.selected_index;
            let marker = if is_selected { " ▶ " } else { "   " };
            let marker_style = if is_selected {
                engine.fg(palette.status_success).bold()
            } else {
                engine.fg(palette.text_muted)
            };
            let row_style = if is_selected {
                engine
                    .bg(palette.overlay)
                    .fg(engine.color(palette.text_primary))
                    .bold()
            } else {
                engine.fg(palette.text_secondary)
            };

            lines.push(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(numbered_entry_label(index, entry), row_style),
            ]));
        }

        lines
    }

    /// Index of the first row drawn in a window of `visible_height` rows.
    ///
    /// Dialog mouse handlers translate a clicked row back into an entry index
    /// with it, so hit-testing stays in lockstep with [`Self::render_list_lines`]
    /// instead of re-deriving the scroll arithmetic (Conventions §2, §6).
    pub fn visible_start(&self, visible_height: usize) -> usize {
        self.visible_range(visible_height).0
    }

    /// Resolves the `[start, end)` window of rows to draw so the selected entry
    /// stays visible within `visible_height`, clamped to the list bounds.
    fn visible_range(&self, visible_height: usize) -> (usize, usize) {
        let len = self.entries.len();
        if len == 0 || visible_height == 0 {
            return (0, 0);
        }

        let height = visible_height.min(len);
        let max_start = len - height;
        let mut start = 0;
        if self.selected_index >= height {
            start = self.selected_index + 1 - height;
        }
        start = start.min(max_start);
        (start, start + height)
    }

    fn clamp(&mut self) {
        if self.entries.is_empty() {
            self.selected_index = 0;
        } else if self.selected_index >= self.entries.len() {
            self.selected_index = self.entries.len() - 1;
        }
    }
}

/// Whether the entry carries a non-blank alias.
fn has_alias(entry: &ModelEntry) -> bool {
    entry
        .alias
        .as_deref()
        .is_some_and(|alias| !alias.trim().is_empty())
}

/// Renders the model-switch hint for the active primary modifier.
///
/// The chord is advertised in the abstract `C-` / `M-` vocabulary: with the
/// Ctrl primary the `C-M` chord and the `M-M` meta fallback are shown together;
/// with the Alt primary both collapse to `M-M`.
pub fn model_select_hint(primary: PrimaryModifier) -> String {
    match primary {
        PrimaryModifier::Ctrl => "C-M / M-M".to_string(),
        PrimaryModifier::Alt => "M-M".to_string(),
    }
}

/// Whether the key is the model-switch chord (`C-M` / `M-M`).
///
/// Both the configured primary modifier (`C-`) and the configured meta
/// modifier (`M-`) switch the model, so the chord follows the user's mapping
/// instead of a hardcoded `Alt`. `Ctrl+Alt` (`AltGr` / compose) is deliberately
/// excluded so that keyboard-layout characters are never mistaken for the
/// chord.
fn is_model_switch_chord(key: &KeyEvent, primary: PrimaryModifier, meta: MetaModifier) -> bool {
    if !matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M')) {
        return false;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if ctrl && alt {
        return false;
    }
    primary.matches(key.modifiers) || meta.matches(key.modifiers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ModelRef, ReasoningEffort};

    fn entry(name: &str, effort: Option<ReasoningEffort>, alias: Option<&str>) -> ModelEntry {
        ModelEntry::new(
            ModelRef::openrouter(name),
            effort,
            alias.map(|value| value.to_string()),
        )
    }

    fn three_entries() -> Vec<ModelEntry> {
        vec![
            entry("a", None, None),
            entry("b", None, None),
            entry("c", None, None),
        ]
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn text_of(spans: &[Span<'_>]) -> String {
        let mut text = String::new();
        for span in spans {
            text.push_str(span.content.as_ref());
        }
        text
    }

    fn text_of_line(line: &Line<'_>) -> String {
        text_of(&line.spans)
    }

    #[test]
    fn test_new_clamps_out_of_range_selection() {
        let selector = ModelSelector::new(ModelRole::Coder, three_entries(), 9);
        assert_eq!(selector.role(), ModelRole::Coder);
        assert_eq!(selector.len(), 3);
        assert!(!selector.is_empty());
        assert_eq!(selector.selected_index(), 2);
    }

    #[test]
    fn test_navigation_cycles_forward_and_backward() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        selector.select_next();
        assert_eq!(selector.selected_index(), 1);
        selector.select_next();
        assert_eq!(selector.selected_index(), 2);
        selector.select_next();
        assert_eq!(selector.selected_index(), 0, "next wraps forward");

        selector.select_prev();
        assert_eq!(selector.selected_index(), 2, "prev wraps backward");
        selector.select_prev();
        assert_eq!(selector.selected_index(), 1);
    }

    #[test]
    fn test_empty_selector_is_inert_and_never_panics() {
        let mut selector = ModelSelector::new(ModelRole::Architect, Vec::new(), 5);

        assert!(selector.is_empty());
        assert_eq!(selector.len(), 0);
        assert_eq!(selector.selected_index(), 0);
        assert!(selector.selected_entry().is_none());

        selector.select_next();
        selector.select_prev();
        assert_eq!(selector.selected_index(), 0);

        let chord = key(KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert!(!selector.handle_key(chord, PrimaryModifier::Ctrl, MetaModifier::Alt, true));
        assert!(!selector.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
            false,
        ));

        assert_eq!(
            text_of(&selector.render_stepper_spans(PrimaryModifier::Ctrl)),
            "[Arc] None"
        );
    }

    #[test]
    fn test_chord_switches_even_with_editor_focus() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        assert!(selector.handle_key(
            key(KeyCode::Char('m'), KeyModifiers::CONTROL),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
            true,
        ));
        assert_eq!(selector.selected_index(), 1);

        assert!(selector.handle_key(
            key(KeyCode::Char('M'), KeyModifiers::ALT),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
            true,
        ));
        assert_eq!(selector.selected_index(), 2);
    }

    #[test]
    fn test_alt_primary_only_accepts_the_alt_chord() {
        let mut selector = ModelSelector::new(ModelRole::Architect, three_entries(), 0);

        assert!(!selector.handle_key(
            key(KeyCode::Char('m'), KeyModifiers::CONTROL),
            PrimaryModifier::Alt,
            MetaModifier::Alt,
            true,
        ));
        assert_eq!(selector.selected_index(), 0);

        assert!(selector.handle_key(
            key(KeyCode::Char('m'), KeyModifiers::ALT),
            PrimaryModifier::Alt,
            MetaModifier::Alt,
            true,
        ));
        assert_eq!(selector.selected_index(), 1);
    }

    #[test]
    fn test_ctrl_alt_compose_is_not_the_chord() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        let compose = key(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(!selector.handle_key(compose, PrimaryModifier::Ctrl, MetaModifier::Alt, true));
        assert_eq!(selector.selected_index(), 0);
    }

    #[test]
    fn test_arrows_respect_editor_focus() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        assert!(!selector.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
            true,
        ));
        assert_eq!(selector.selected_index(), 0, "editor keeps the arrows");

        assert!(selector.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
            false,
        ));
        assert_eq!(selector.selected_index(), 1);

        assert!(selector.handle_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
            false,
        ));
        assert_eq!(selector.selected_index(), 0);

        assert!(!selector.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
            false,
        ));
    }

    #[test]
    fn test_stepper_reports_alias_effort_and_counter() {
        let selector = ModelSelector::new(
            ModelRole::Reviewer,
            vec![
                entry("x/a", Some(ReasoningEffort::High), Some("Claude Auditor")),
                entry("x/b", None, None),
            ],
            0,
        );

        assert_eq!(
            text_of(&selector.render_stepper_spans(PrimaryModifier::Ctrl)),
            "[Rev] Claude Auditor [high]  [1 of 2 • C-M / M-M]"
        );
    }

    #[test]
    fn test_stepper_without_alias_folds_effort_into_label() {
        let selector = ModelSelector::new(
            ModelRole::Primary,
            vec![entry("x/a", Some(ReasoningEffort::High), None)],
            0,
        );

        assert_eq!(
            text_of(&selector.render_stepper_spans(PrimaryModifier::Alt)),
            "[Pri] openrouter:x/a [high]"
        );
    }

    #[test]
    fn test_stepper_alt_primary_renders_alt_hint() {
        let selector = ModelSelector::new(ModelRole::Architect, three_entries(), 2);

        assert_eq!(
            text_of(&selector.render_stepper_spans(PrimaryModifier::Alt)),
            "[Arc] openrouter:c  [3 of 3 • M-M]"
        );
    }

    #[test]
    fn test_select_index_clamps_into_range() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        selector.select_index(2);
        assert_eq!(selector.selected_index(), 2);

        selector.select_index(9);
        assert_eq!(
            selector.selected_index(),
            2,
            "out of range clamps to the last entry"
        );

        let mut empty = ModelSelector::new(ModelRole::Coder, Vec::new(), 0);
        empty.select_index(4);
        assert_eq!(empty.selected_index(), 0);
    }

    #[test]
    fn test_render_list_lines_numbers_and_marks_selection() {
        let selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 1);
        let lines = selector.render_list_lines(5);

        assert_eq!(lines.len(), 3);
        assert_eq!(text_of_line(&lines[0]), "   [1] openrouter:a");
        assert_eq!(text_of_line(&lines[1]), " ▶ [2] openrouter:b");
        assert_eq!(text_of_line(&lines[2]), "   [3] openrouter:c");
    }

    #[test]
    fn test_render_list_lines_scrolls_to_keep_selection_visible() {
        let entries: Vec<ModelEntry> = (0..20)
            .map(|i| entry(&format!("m{}", i), None, None))
            .collect();
        let mut selector = ModelSelector::new(ModelRole::Reviewer, entries, 0);

        let top = selector.render_list_lines(5);
        assert_eq!(top.len(), 5);
        assert_eq!(text_of_line(&top[0]), " ▶ [1] openrouter:m0");
        assert_eq!(text_of_line(&top[4]), "   [5] openrouter:m4");

        selector.select_index(10);
        let mid = selector.render_list_lines(5);
        assert_eq!(text_of_line(&mid[0]), "   [7] openrouter:m6");
        assert_eq!(text_of_line(&mid[4]), " ▶ [11] openrouter:m10");

        selector.select_index(19);
        let bottom = selector.render_list_lines(5);
        assert_eq!(text_of_line(&bottom[0]), "   [16] openrouter:m15");
        assert_eq!(text_of_line(&bottom[4]), " ▶ [20] openrouter:m19");
    }

    #[test]
    fn test_render_list_lines_empty_and_unbounded() {
        let empty = ModelSelector::new(ModelRole::Reviewer, Vec::new(), 0);
        assert!(empty.render_list_lines(5).is_empty());

        let selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);
        assert_eq!(selector.render_list_lines(10).len(), 3);
        assert_eq!(selector.render_list_lines(0).len(), 0);
    }

    #[test]
    fn test_visible_start_matches_rendered_window() {
        let entries: Vec<ModelEntry> = (0..20)
            .map(|i| entry(&format!("m{}", i), None, None))
            .collect();
        let mut selector = ModelSelector::new(ModelRole::Reviewer, entries, 0);

        assert_eq!(selector.visible_start(5), 0);

        selector.select_index(10);
        assert_eq!(selector.visible_start(5), 6);

        selector.select_index(19);
        assert_eq!(selector.visible_start(5), 15);
    }

    #[test]
    fn test_handle_list_key_digits_select_in_range() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        assert!(selector.handle_list_key(key(KeyCode::Char('3'), KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 2);

        // An out-of-range digit is not consumed so the caller can handle it.
        assert!(!selector.handle_list_key(key(KeyCode::Char('9'), KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 2);

        assert!(!selector.handle_list_key(key(KeyCode::Char('z'), KeyModifiers::NONE), 5));
        assert!(!selector.handle_list_key(key(KeyCode::Enter, KeyModifiers::NONE), 5));
    }

    #[test]
    fn test_handle_list_key_arrows_page_and_bounds() {
        let entries: Vec<ModelEntry> = (0..12)
            .map(|i| entry(&format!("m{}", i), None, None))
            .collect();
        let mut selector = ModelSelector::new(ModelRole::Reviewer, entries, 0);

        assert!(selector.handle_list_key(key(KeyCode::Down, KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 1);
        assert!(selector.handle_list_key(key(KeyCode::Up, KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 0);

        assert!(selector.handle_list_key(key(KeyCode::PageDown, KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 5);
        assert!(selector.handle_list_key(key(KeyCode::End, KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 11);
        assert!(selector.handle_list_key(key(KeyCode::PageDown, KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 11, "paging clamps at the end");

        assert!(selector.handle_list_key(key(KeyCode::PageUp, KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 6);
        assert!(selector.handle_list_key(key(KeyCode::Home, KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 0);
    }

    #[test]
    fn test_handle_list_key_empty_selector_is_inert() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, Vec::new(), 0);

        assert!(!selector.handle_list_key(key(KeyCode::Char('1'), KeyModifiers::NONE), 5));
        assert!(!selector.handle_list_key(key(KeyCode::Down, KeyModifiers::NONE), 5));
        assert!(!selector.handle_list_key(key(KeyCode::Home, KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 0);
    }

    #[test]
    fn test_numbered_row_label_includes_effort_for_aliased_entries() {
        let selector = ModelSelector::new(
            ModelRole::Reviewer,
            vec![entry(
                "x/a",
                Some(ReasoningEffort::High),
                Some("Claude Auditor"),
            )],
            0,
        );
        let rows = selector.render_list_lines(5);

        assert_eq!(text_of_line(&rows[0]), " ▶ [1] Claude Auditor [high]");
    }
}
