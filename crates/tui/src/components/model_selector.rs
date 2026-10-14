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

#![allow(dead_code)] // Introced ahead of its dialog integrations; drop once consumed.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Span;
use tauqe_protocol::{ModelEntry, ModelRole};

use crate::app::role_badge;
use crate::config::PrimaryModifier;

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

    /// Resolves a key event against the model-switch chord and the navigation
    /// arrows.
    ///
    /// The chord (`C-M` / `Alt+M`) always switches, even while the instruction
    /// editor owns the keyboard; the arrows switch only when the editor does
    /// not own them, so cursor movement and model browsing never fight over the
    /// same keys (Conventions.md §6, §8). Returns `true` when the selector
    /// consumed the key.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        primary: PrimaryModifier,
        editor_focused: bool,
    ) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        if is_model_switch_chord(&key, primary) {
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

    /// Builds the standardized stepper line as composable spans: the role
    /// badge, the selected entry's display label, the reasoning-effort
    /// indicator, and — when more than one entry is configured — the `[X of Y]`
    /// counter together with the switch hint.
    ///
    /// Returning spans rather than a whole line lets each dialog prepend its
    /// own field label (for example `Reviewer:`) while the selector remains the
    /// single source of truth for the model presentation.
    pub fn render_stepper_spans(&self, primary: PrimaryModifier) -> Vec<Span<'static>> {
        let mut spans = vec![Span::styled(
            format!("{} ", role_badge(self.role)),
            Style::default().fg(Color::Magenta).bold(),
        )];

        match self.selected_entry() {
            Some(entry) => {
                spans.push(Span::styled(
                    entry.display_label(),
                    Style::default().fg(Color::Green).bold(),
                ));
                // `display_label` already folds the effort into the label when
                // the entry has no alias, so the separate indicator only fills
                // the aliased case and never duplicates the effort text.
                if has_alias(entry) {
                    if let Some(effort) = entry.effort {
                        spans.push(Span::styled(
                            format!(" [{}]", effort),
                            Style::default().fg(Color::Yellow).bold(),
                        ));
                    }
                }
            }
            None => spans.push(Span::styled(
                "None".to_string(),
                Style::default().fg(Color::DarkGray),
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
                Style::default().fg(Color::Cyan),
            ));
        }

        spans
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
/// With the Ctrl modifier the chord is `C-M` and the `Alt+M` hardware fallback
/// is advertised alongside it; with the Alt modifier both spellings collapse to
/// `Alt+M`.
fn model_select_hint(primary: PrimaryModifier) -> String {
    match primary {
        PrimaryModifier::Ctrl => "C-M / Alt+M".to_string(),
        PrimaryModifier::Alt => "Alt+M".to_string(),
    }
}

/// Whether the key is the `C-M` / `Alt+M` model-switch chord.
///
/// `Ctrl+Alt` (`AltGr` / compose) is deliberately excluded so that
/// keyboard-layout characters are never mistaken for the chord.
fn is_model_switch_chord(key: &KeyEvent, primary: PrimaryModifier) -> bool {
    if !matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M')) {
        return false;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if ctrl && alt {
        return false;
    }
    primary.matches(key.modifiers) || alt
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
        assert!(!selector.handle_key(chord, PrimaryModifier::Ctrl, true));
        assert!(!selector.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
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
            true,
        ));
        assert_eq!(selector.selected_index(), 1);

        assert!(selector.handle_key(
            key(KeyCode::Char('M'), KeyModifiers::ALT),
            PrimaryModifier::Ctrl,
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
            true,
        ));
        assert_eq!(selector.selected_index(), 0);

        assert!(selector.handle_key(
            key(KeyCode::Char('m'), KeyModifiers::ALT),
            PrimaryModifier::Alt,
            true,
        ));
        assert_eq!(selector.selected_index(), 1);
    }

    #[test]
    fn test_ctrl_alt_compose_is_not_the_chord() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        let compose = key(KeyCode::Char('m'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert!(!selector.handle_key(compose, PrimaryModifier::Ctrl, true));
        assert_eq!(selector.selected_index(), 0);
    }

    #[test]
    fn test_arrows_respect_editor_focus() {
        let mut selector = ModelSelector::new(ModelRole::Reviewer, three_entries(), 0);

        assert!(!selector.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
            true,
        ));
        assert_eq!(selector.selected_index(), 0, "editor keeps the arrows");

        assert!(selector.handle_key(
            key(KeyCode::Down, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
            false,
        ));
        assert_eq!(selector.selected_index(), 1);

        assert!(selector.handle_key(
            key(KeyCode::Up, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
            false,
        ));
        assert_eq!(selector.selected_index(), 0);

        assert!(!selector.handle_key(
            key(KeyCode::Enter, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
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
            "[Rev] Claude Auditor [high]  [1 of 2 • C-M / Alt+M]"
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
            "[Arc] openrouter:c  [3 of 3 • Alt+M]"
        );
    }
}
