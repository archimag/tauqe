//! Typed single source of truth for keyboard commands.
//!
//! Every keyboard binding is described exactly once as a [`CommandSpec`]: an
//! abstract [`KeyChord`], the [`CommandCategory`] that ranks its cognitive
//! weight, a stable action, and a help description. The input dispatcher and the
//! help generator both read these same structures, so the keys the developer
//! sees in help can never drift from the keys the application actually
//! intercepts.
//!
//! The module is the foundation of the command-map refactor. The dispatchers and
//! help surfaces are migrated onto it in later steps, so part of its public
//! surface is intentionally not yet consumed by production code.
#![allow(dead_code)]

pub mod dialog;
pub mod editor;
pub mod global;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{InputConfig, MetaModifier, PrimaryModifier};

/// Abstract keyboard chord expressed independently of the physical modifiers.
///
/// `Primary` and `Meta` are resolved dynamically through [`InputConfig`], so one
/// specification describes `Ctrl+S` or `Alt+S` depending on the developer's
/// `[input] primary_modifier`. `Shift`, `Bare`, `Ctrl`, and `Alt` are fixed
/// chords used where an abstract modifier would be wrong: reverse motion and
/// destructive actions (`Shift`), structural navigation (`Bare`), and raw chords
/// whose physical key must never be remapped (`Ctrl`, `Alt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyChord {
    /// Primary command modifier (`C-`).
    Primary(KeyCode),
    /// Meta command modifier (`M-`).
    Meta(KeyCode),
    /// Either command modifier; the chord is reachable on both.
    PrimaryOrMeta(KeyCode),
    /// Explicit `Shift` combination.
    Shift(KeyCode),
    /// Bare key without command modifiers; active only in structural navigation.
    Bare(KeyCode),
    /// Raw `Ctrl` combination, independent of the primary modifier.
    Ctrl(KeyCode),
    /// Raw `Alt` combination, independent of the meta modifier.
    Alt(KeyCode),
}

impl KeyChord {
    /// Reports whether the chord matches the key event.
    ///
    /// `Bare` chords are the structural-navigation bindings (Magit / Org-mode).
    /// They are blocked whenever a text editor is active, so a bare key is never
    /// stolen from the buffer and the unconditional character insertion
    /// invariant holds. `Primary` and `Meta` resolve through `input`, while
    /// `Ctrl` and `Alt` always match their physical modifier.
    pub fn matches(&self, key: KeyEvent, input: &InputConfig, text_input_active: bool) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match *self {
            KeyChord::Bare(expected) => {
                !text_input_active && !ctrl && !alt && bare_code(expected, key)
            }
            KeyChord::Primary(expected) => {
                input.is_primary(key.modifiers) && lenient_code(expected, key.code)
            }
            KeyChord::Meta(expected) => {
                input.is_meta(key.modifiers) && lenient_code(expected, key.code)
            }
            KeyChord::PrimaryOrMeta(expected) => {
                (input.is_primary(key.modifiers) || input.is_meta(key.modifiers))
                    && lenient_code(expected, key.code)
            }
            KeyChord::Shift(expected) => {
                key.modifiers.contains(KeyModifiers::SHIFT)
                    && !ctrl
                    && !alt
                    && lenient_code(expected, key.code)
            }
            KeyChord::Ctrl(expected) => ctrl && !alt && lenient_code(expected, key.code),
            KeyChord::Alt(expected) => alt && !ctrl && lenient_code(expected, key.code),
        }
    }

    /// Formats the chord for display using the configured modifier labels.
    ///
    /// The abstract `C-` / `M-` prefixes are projected through the developer's
    /// actual modifiers, and `PrimaryOrMeta` shows both, so help text always
    /// names the real keys instead of the abstract notation.
    pub fn display(&self, primary_mod: PrimaryModifier, meta_mod: MetaModifier) -> String {
        match *self {
            KeyChord::Primary(code) => {
                format!("{}{}", primary_mod.prefix_str(), code_label(code, true))
            }
            KeyChord::Meta(code) => format!("{}{}", meta_mod.prefix_str(), code_label(code, true)),
            KeyChord::PrimaryOrMeta(code) => format!(
                "{}{} / {}{}",
                primary_mod.prefix_str(),
                code_label(code, true),
                meta_mod.prefix_str(),
                code_label(code, true),
            ),
            KeyChord::Shift(code) => format!("Shift+{}", code_label(code, true)),
            KeyChord::Bare(code) => code_label(code, false),
            KeyChord::Ctrl(code) => format!("Ctrl+{}", code_label(code, true)),
            KeyChord::Alt(code) => format!("Alt+{}", code_label(code, true)),
        }
    }
}

/// Matches a bare structural-navigation key.
///
/// A character code already encodes its Shift state, so the exact character is
/// the whole test (`c` and `C` are distinct bindings). Navigation keys carry
/// Shift meaningfully, so a bare navigation key never matches while Shift is
/// held, leaving `Shift` chords unambiguous.
fn bare_code(expected: KeyCode, key: KeyEvent) -> bool {
    match expected {
        KeyCode::Char(_) => expected == key.code,
        _ => !key.modifiers.contains(KeyModifiers::SHIFT) && expected == key.code,
    }
}

/// Case-insensitive key-code comparison used by command chords, because
/// terminals report the letter case of a chord inconsistently with Shift.
fn lenient_code(expected: KeyCode, actual: KeyCode) -> bool {
    match (expected, actual) {
        (KeyCode::Char(expected), KeyCode::Char(actual)) => expected.eq_ignore_ascii_case(&actual),
        (expected, actual) => expected == actual,
    }
}

/// Renders a key code as a compact human label, optionally uppercasing letters.
fn code_label(code: KeyCode, upper: bool) -> String {
    match code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) if upper => c.to_ascii_uppercase().to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "BackTab".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Del".to_string(),
        KeyCode::Insert => "Ins".to_string(),
        KeyCode::Left => "←".to_string(),
        KeyCode::Right => "→".to_string(),
        KeyCode::Up => "↑".to_string(),
        KeyCode::Down => "↓".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PgUp".to_string(),
        KeyCode::PageDown => "PgDn".to_string(),
        KeyCode::F(n) => format!("F{}", n),
        other => format!("{:?}", other),
    }
}

/// Cognitive ranking of a command, from the primary work cycle to rare helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandCategory {
    /// Expensive operations that spend tokens or touch Git.
    Essential,
    /// Presentation, navigation, and view state.
    Interface,
    /// Auxiliary helpers that do not mutate interface state.
    Helpers,
}

impl CommandCategory {
    /// Canonical display order: Essential, Interface, Helpers.
    pub const ALL: [CommandCategory; 3] = [Self::Essential, Self::Interface, Self::Helpers];

    /// Human-readable section title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Essential => "Essential",
            Self::Interface => "Interface",
            Self::Helpers => "Helpers",
        }
    }
}

/// One keyboard command: the abstract chord, its action, its cognitive category,
/// and its help description.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec<A> {
    pub chord: KeyChord,
    pub action: A,
    pub category: CommandCategory,
    pub description: &'static str,
}

impl<A> CommandSpec<A> {
    /// Builds a command specification. `const` so registries are declared as
    /// `static` values.
    pub const fn new(
        chord: KeyChord,
        action: A,
        category: CommandCategory,
        description: &'static str,
    ) -> Self {
        Self {
            chord,
            action,
            category,
            description,
        }
    }
}

/// Ordered, static registry of command specifications for one scope (global,
/// editor, or a modal dialog).
#[derive(Debug, Clone, Copy)]
pub struct CommandMap<A: 'static> {
    specs: &'static [CommandSpec<A>],
}

impl<A: 'static> CommandMap<A> {
    /// Wraps a static slice of command specifications.
    pub const fn new(specs: &'static [CommandSpec<A>]) -> Self {
        Self { specs }
    }

    /// All specifications in declaration order.
    pub fn specs(&self) -> &'static [CommandSpec<A>] {
        self.specs
    }

    /// Resolves the first command whose chord matches the key event.
    pub fn resolve(
        &self,
        key: KeyEvent,
        input: &InputConfig,
        text_input_active: bool,
    ) -> Option<&'static CommandSpec<A>> {
        self.specs
            .iter()
            .find(|spec| spec.chord.matches(key, input, text_input_active))
    }

    /// Iterates the commands of one cognitive category, preserving order.
    pub fn by_category(
        &self,
        category: CommandCategory,
    ) -> impl Iterator<Item = &'static CommandSpec<A>> {
        let specs: &'static [CommandSpec<A>] = self.specs;
        specs.iter().filter(move |spec| spec.category == category)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl_primary() -> InputConfig {
        InputConfig {
            primary_modifier: PrimaryModifier::Ctrl,
            meta_modifier: MetaModifier::Alt,
            ..InputConfig::default()
        }
    }

    fn alt_primary() -> InputConfig {
        InputConfig {
            primary_modifier: PrimaryModifier::Alt,
            meta_modifier: MetaModifier::Ctrl,
            ..InputConfig::default()
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn bare_chord_requires_no_command_modifier() {
        let input = ctrl_primary();
        let chord = KeyChord::Bare(KeyCode::Char('n'));

        assert!(chord.matches(key(KeyCode::Char('n'), KeyModifiers::NONE), &input, false));
        assert!(!chord.matches(key(KeyCode::Char('n'), KeyModifiers::CONTROL), &input, false));
        assert!(!chord.matches(key(KeyCode::Char('n'), KeyModifiers::ALT), &input, false));
        assert!(!chord.matches(key(KeyCode::Char('p'), KeyModifiers::NONE), &input, false));
    }

    #[test]
    fn bare_chord_is_blocked_while_text_input_is_active() {
        let input = ctrl_primary();
        let chord = KeyChord::Bare(KeyCode::Char('s'));

        assert!(chord.matches(key(KeyCode::Char('s'), KeyModifiers::NONE), &input, false));
        assert!(!chord.matches(key(KeyCode::Char('s'), KeyModifiers::NONE), &input, true));
    }

    #[test]
    fn bare_character_chord_is_case_sensitive() {
        let input = ctrl_primary();
        let lower = KeyChord::Bare(KeyCode::Char('c'));

        assert!(lower.matches(key(KeyCode::Char('c'), KeyModifiers::NONE), &input, false));
        assert!(!lower.matches(key(KeyCode::Char('C'), KeyModifiers::SHIFT), &input, false));
    }

    #[test]
    fn bare_navigation_chord_never_matches_with_shift() {
        let input = ctrl_primary();
        let tab = KeyChord::Bare(KeyCode::Tab);

        assert!(tab.matches(key(KeyCode::Tab, KeyModifiers::NONE), &input, false));
        assert!(!tab.matches(key(KeyCode::Tab, KeyModifiers::SHIFT), &input, false));
    }

    #[test]
    fn primary_and_meta_follow_the_configured_modifiers() {
        let ctrl = ctrl_primary();
        let primary = KeyChord::Primary(KeyCode::Char('o'));
        assert!(primary.matches(key(KeyCode::Char('o'), KeyModifiers::CONTROL), &ctrl, true));
        assert!(!primary.matches(key(KeyCode::Char('o'), KeyModifiers::ALT), &ctrl, true));

        let alt = alt_primary();
        assert!(primary.matches(key(KeyCode::Char('o'), KeyModifiers::ALT), &alt, true));
        assert!(!primary.matches(key(KeyCode::Char('o'), KeyModifiers::CONTROL), &alt, true));
    }

    #[test]
    fn meta_chord_follows_the_configured_meta_modifier() {
        let ctrl = ctrl_primary();
        let meta = KeyChord::Meta(KeyCode::Char('t'));

        assert!(meta.matches(key(KeyCode::Char('t'), KeyModifiers::ALT), &ctrl, true));
        assert!(!meta.matches(key(KeyCode::Char('t'), KeyModifiers::CONTROL), &ctrl, true));
    }

    #[test]
    fn primary_or_meta_matches_either_command_modifier() {
        let input = ctrl_primary();
        let chord = KeyChord::PrimaryOrMeta(KeyCode::Char('s'));

        assert!(chord.matches(key(KeyCode::Char('s'), KeyModifiers::CONTROL), &input, true));
        assert!(chord.matches(key(KeyCode::Char('s'), KeyModifiers::ALT), &input, true));
        assert!(!chord.matches(key(KeyCode::Char('s'), KeyModifiers::NONE), &input, true));
    }

    #[test]
    fn shift_chord_requires_shift_without_command_modifiers() {
        let input = ctrl_primary();
        let chord = KeyChord::Shift(KeyCode::Tab);

        assert!(chord.matches(key(KeyCode::Tab, KeyModifiers::SHIFT), &input, false));
        assert!(!chord.matches(key(KeyCode::Tab, KeyModifiers::NONE), &input, false));
        assert!(!chord.matches(
            key(KeyCode::Tab, KeyModifiers::SHIFT | KeyModifiers::CONTROL),
            &input,
            false
        ));
    }

    #[test]
    fn raw_ctrl_and_alt_chords_ignore_the_abstract_modifiers() {
        let alt = alt_primary();

        let raw_ctrl = KeyChord::Ctrl(KeyCode::Char('c'));
        assert!(raw_ctrl.matches(key(KeyCode::Char('c'), KeyModifiers::CONTROL), &alt, true));
        assert!(!raw_ctrl.matches(key(KeyCode::Char('c'), KeyModifiers::ALT), &alt, true));

        let raw_alt = KeyChord::Alt(KeyCode::Enter);
        assert!(raw_alt.matches(key(KeyCode::Enter, KeyModifiers::ALT), &alt, true));
        assert!(!raw_alt.matches(key(KeyCode::Enter, KeyModifiers::CONTROL), &alt, true));
    }

    #[test]
    fn altgr_compose_chord_is_never_a_command() {
        let input = ctrl_primary();
        let compose = KeyModifiers::CONTROL | KeyModifiers::ALT;

        assert!(!KeyChord::Primary(KeyCode::Char('2'))
            .matches(key(KeyCode::Char('2'), compose), &input, true));
        assert!(!KeyChord::Meta(KeyCode::Char('2'))
            .matches(key(KeyCode::Char('2'), compose), &input, true));
        assert!(!KeyChord::Ctrl(KeyCode::Char('2'))
            .matches(key(KeyCode::Char('2'), compose), &input, true));
        assert!(!KeyChord::Alt(KeyCode::Char('2'))
            .matches(key(KeyCode::Char('2'), compose), &input, true));
    }

    #[test]
    fn display_projects_abstract_modifiers_through_the_configuration() {
        let ctrl = ctrl_primary();
        let (p, m) = (ctrl.primary_modifier, ctrl.meta_modifier);

        assert_eq!(KeyChord::Primary(KeyCode::Char('s')).display(p, m), "Ctrl+S");
        assert_eq!(KeyChord::Meta(KeyCode::Char('s')).display(p, m), "Alt+S");
        assert_eq!(
            KeyChord::PrimaryOrMeta(KeyCode::Char('s')).display(p, m),
            "Ctrl+S / Alt+S"
        );
        assert_eq!(KeyChord::Bare(KeyCode::Char('n')).display(p, m), "n");
        assert_eq!(KeyChord::Bare(KeyCode::Char(' ')).display(p, m), "Space");
        assert_eq!(KeyChord::Shift(KeyCode::Tab).display(p, m), "Shift+Tab");
        assert_eq!(KeyChord::Ctrl(KeyCode::Char('c')).display(p, m), "Ctrl+C");
        assert_eq!(KeyChord::Alt(KeyCode::Enter).display(p, m), "Alt+Enter");

        let alt = alt_primary();
        let (ap, am) = (alt.primary_modifier, alt.meta_modifier);
        assert_eq!(KeyChord::Primary(KeyCode::Char('s')).display(ap, am), "Alt+S");
        assert_eq!(KeyChord::Meta(KeyCode::Char('s')).display(ap, am), "Ctrl+S");
    }

    #[test]
    fn command_category_orders_essential_first() {
        assert_eq!(
            CommandCategory::ALL,
            [
                CommandCategory::Essential,
                CommandCategory::Interface,
                CommandCategory::Helpers,
            ]
        );
        assert_eq!(CommandCategory::Essential.title(), "Essential");
        assert!(CommandCategory::Essential < CommandCategory::Helpers);
    }

    #[test]
    fn command_map_resolves_first_match_and_groups_by_category() {
        static SPECS: &[CommandSpec<u8>] = &[
            CommandSpec::new(
                KeyChord::Bare(KeyCode::Char('x')),
                1,
                CommandCategory::Essential,
                "first",
            ),
            CommandSpec::new(
                KeyChord::Bare(KeyCode::Char('x')),
                2,
                CommandCategory::Interface,
                "second",
            ),
            CommandSpec::new(
                KeyChord::Primary(KeyCode::Char('y')),
                3,
                CommandCategory::Helpers,
                "third",
            ),
        ];
        let map = CommandMap::new(SPECS);
        let input = ctrl_primary();

        let resolved = map.resolve(key(KeyCode::Char('x'), KeyModifiers::NONE), &input, false);
        assert_eq!(resolved.map(|spec| spec.action), Some(1));

        // Blocked while text input is active, so the bare command no longer resolves.
        assert!(map
            .resolve(key(KeyCode::Char('x'), KeyModifiers::NONE), &input, true)
            .is_none());

        let essential: Vec<u8> = map
            .by_category(CommandCategory::Essential)
            .map(|spec| spec.action)
            .collect();
        assert_eq!(essential, vec![1]);

        let helpers: Vec<u8> = map
            .by_category(CommandCategory::Helpers)
            .map(|spec| spec.action)
            .collect();
        assert_eq!(helpers, vec![3]);
    }
}
