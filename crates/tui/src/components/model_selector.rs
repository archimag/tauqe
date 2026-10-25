//! Reusable model-selection stepper.
//!
//! Several dialogs let the developer choose among the models configured for a
//! single semantic role: the Review launcher picks a Reviewer, the Plan Refine
//! dialog picks an Architect, and the finding decomposition and point execution
//! forms pick an Architect or a Coder. They all share one interaction: the
//! selected entry, an `[X of Y]` counter, and one keyboard chord that cycles the
//! list. [`ModelSelector`] centralizes that behavior so the dialogs stop
//! re-implementing the index arithmetic, the empty-list guards, and the key
//! resolution independently (Conventions.md §2, §7).
//!
//! The selector stays presentation-agnostic: [`ModelSelector::render_stepper_spans`]
//! returns composable spans so each dialog can prepend its own field label while
//! still sharing one model-presentation rule. That rule is
//! [`crate::model_display::entry_label`] — the single formatter the header, the
//! pickers, and the dialogs all use — so one model can never be named two
//! different ways. The display alias is catalog metadata of the model: it reaches
//! the client through the provider catalog published with the role summary, so
//! the selector never derives a name from the model string (c-1, c-2).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use tauqe_protocol::{ModelEntry, ModelRole};

use crate::config::{MetaModifier, PrimaryModifier};
use crate::model_display::{entry_label, numbered_entry_label};
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
    /// within the list. It is only invoked when the list, not the instruction
    /// editor, holds focus, so intercepting bare keys never violates the
    /// unconditional character insertion rule (Conventions.md §5). Returns
    /// `true` when the key was consumed.
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

    /// Builds the standardized stepper line as composable spans: the selected
    /// entry rendered through the shared interface label formatter, and, when
    /// more than one entry is configured, the `[X of Y]` counter together with
    /// the switch hint. The semantic role is never printed: it is a selection
    /// mechanism, not a property of the model name (c-2).
    ///
    /// Returning spans rather than a whole line lets each dialog prepend its own
    /// field label while the selector remains the single source of truth for the
    /// model presentation.
    pub fn render_stepper_spans(&self, primary: PrimaryModifier) -> Vec<Span<'static>> {
        let engine = current_engine();
        let palette = engine.palette();
        let mut spans = Vec::new();

        match self.selected_entry() {
            Some(entry) => spans.push(Span::styled(
                entry_label(entry),
                engine.fg(palette.status_success).bold(),
            )),
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
    /// Each row is `▶ [N] label`; the selected row is highlighted and the window
    /// follows the selection so it always stays visible. At most
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
    /// with it, so hit-testing stays in lockstep with
    /// [`Self::render_list_lines`] instead of re-deriving the scroll arithmetic
    /// (Conventions.md §6).
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
/// Both the configured primary modifier (`C-`) and the configured meta modifier
/// (`M-`) switch the model, so the chord follows the user's mapping instead of a
/// hardcoded `Alt`. `Ctrl+Alt` (`AltGr` / compose) is deliberately excluded so
/// keyboard-layout characters are never mistaken for the chord.
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

    fn entry(name: &str, effort: Option<ReasoningEffort>) -> ModelEntry {
        ModelEntry::new(ModelRef::openrouter(name), effort)
    }

    fn three_entries() -> Vec<ModelEntry> {
        vec![entry("a", None), entry("b", None), entry("c", None)]
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn text_of(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn text_of_line(line: &Line<'_>) -> String {
        text_of(&line.spans)
    }

    #[test]
    fn new_clamps_out_of_range_selection() {
        let selector = ModelSelector::new(ModelRole::Coder, three_entries(), 9);
        assert_eq!(selector.role(), ModelRole::Coder);
        assert_eq!(selector.len(), 3);
        assert!(!selector.is_empty());
        assert_eq!(selector.selected_index(), 2);
    }

    #[test]
    fn navigation_cycles_forward_and_backward() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        selector.select_next();
        assert_eq!(selector.selected_index(), 1);
        selector.select_next();
        assert_eq!(selector.selected_index(), 2);
        selector.select_next();
        assert_eq!(selector.selected_index(), 0);

        selector.select_prev();
        assert_eq!(selector.selected_index(), 2);
        selector.select_prev();
        assert_eq!(selector.selected_index(), 1);
    }

    #[test]
    fn empty_selector_is_inert_and_never_panics() {
        let mut selector = ModelSelector::new(ModelRole::Architect, Vec::new(), 5);

        assert!(selector.is_empty());
        assert_eq!(selector.len(), 0);
        assert_eq!(selector.selected_index(), 0);
        assert!(selector.selected_entry().is_none());

        selector.select_next();
        selector.select_prev();
        selector.select_index(4);
        assert_eq!(selector.selected_index(), 0);

        let chord = key(KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert!(!selector.handle_key(chord, PrimaryModifier::Ctrl, MetaModifier::Alt, true));
        assert!(!selector.handle_list_key(key(KeyCode::Down, KeyModifiers::NONE), 5));

        assert_eq!(
            text_of(&selector.render_stepper_spans(PrimaryModifier::Ctrl)),
            "None"
        );
    }

    #[test]
    fn chord_switches_even_with_editor_focus() {
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
    fn ctrl_alt_compose_is_not_the_chord() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        let compose = key(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(!selector.handle_key(compose, PrimaryModifier::Ctrl, MetaModifier::Alt, true));
        assert_eq!(selector.selected_index(), 0);
    }

    #[test]
    fn stepper_folds_effort_and_reports_the_counter() {
        let selector = ModelSelector::new(
            ModelRole::Reviewer,
            vec![
                entry("x/a", Some(ReasoningEffort::High)),
                entry("x/b", None),
            ],
            0,
        );

        assert_eq!(
            text_of(&selector.render_stepper_spans(PrimaryModifier::Ctrl)),
            "x/a [high]  [1 of 2 • C-M / M-M]"
        );
        assert_eq!(
            text_of(&selector.render_stepper_spans(PrimaryModifier::Alt)),
            "x/a [high]  [1 of 2 • M-M]"
        );
    }

    #[test]
    fn list_rows_scroll_to_keep_the_selection_visible() {
        let entries: Vec<ModelEntry> = (0..20).map(|i| entry(&format!("m{}", i), None)).collect();
        let mut selector = ModelSelector::new(ModelRole::Reviewer, entries, 0);

        let top = selector.render_list_lines(5);
        assert_eq!(top.len(), 5);
        assert_eq!(text_of_line(&top[0]), " ▶ [1] m0");
        assert_eq!(text_of_line(&top[4]), "   [5] m4");

        selector.select_index(10);
        let mid = selector.render_list_lines(5);
        assert_eq!(text_of_line(&mid[0]), "   [7] m6");
        assert_eq!(text_of_line(&mid[4]), " ▶ [11] m10");
        assert_eq!(selector.visible_start(5), 6);

        selector.select_index(19);
        let bottom = selector.render_list_lines(5);
        assert_eq!(text_of_line(&bottom[4]), " ▶ [20] m19");
        assert_eq!(selector.visible_start(5), 15);
    }

    #[test]
    fn digits_select_in_list_and_out_of_range_is_ignored() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        assert!(selector.handle_list_key(key(KeyCode::Char('3'), KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 2);

        assert!(!selector.handle_list_key(key(KeyCode::Char('9'), KeyModifiers::NONE), 5));
        assert_eq!(selector.selected_index(), 2);
    }
}
