//! Typed single source of truth for keyboard commands.
//!
//! Every keyboard binding is described exactly once as a [`CommandSpec`]: an
//! abstract [`KeyChord`], the [`CommandCategory`] that ranks its cognitive
//! weight, a stable action, and a help description. The input dispatcher and the
//! help generator both read these same structures, so the keys the developer
//! sees in help can never drift from the keys the application actually
//! intercepts.

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
                !ctrl
                    && !alt
                    && (key.modifiers.contains(KeyModifiers::SHIFT)
                        || matches!(key.code, KeyCode::BackTab))
                    && shift_code(expected, key.code)
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
/// A bare character is selected by its exact case, but a terminal reports the
/// Shift state of a letter inconsistently: a lowercase character pressed with
/// `Shift` arrives either as the uppercase character alone or as the lowercase
/// character carrying the `SHIFT` modifier. Both spellings must reach the same
/// binding and never bleed into the sibling case, so the two letter cases are
/// matched deliberately:
///
/// - a lowercase expectation (`c`) matches only the lowercase character without
///   `Shift`, so the destructive `Shift+C` chord can never be captured by the
///   copy binding;
/// - an uppercase expectation (`C`) matches the uppercase character with or
///   without `Shift`, and the lowercase character carrying `Shift`, so the
///   destructive chord stays reachable on every terminal.
///
/// Symbols, digits, and dedicated navigation keys keep the exact-code test, and
/// a navigation key never matches while `Shift` is held, leaving `Shift` chords
/// unambiguous.
fn bare_code(expected: KeyCode, key: KeyEvent) -> bool {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match expected {
        KeyCode::Char(expected_char) if expected_char.is_ascii_lowercase() => {
            !shift && matches!(key.code, KeyCode::Char(actual) if actual == expected_char)
        }
        KeyCode::Char(expected_char) if expected_char.is_ascii_uppercase() => match key.code {
            KeyCode::Char(actual) => {
                actual.eq_ignore_ascii_case(&expected_char)
                    && (shift || actual.is_ascii_uppercase())
            }
            _ => false,
        },
        KeyCode::Char(_) => expected == key.code,
        _ => !shift && expected == key.code,
    }
}

/// Compares key codes for an explicit `Shift` chord.
///
/// `Shift+Tab` reaches the application in two spellings: the dedicated
/// `BackTab` code of enhanced keyboards and the plain `Tab` code carrying the
/// `SHIFT` modifier. Both name the same intent, so the chord matches either one
/// and reverse motion is never lost on a modern terminal.
fn shift_code(expected: KeyCode, actual: KeyCode) -> bool {
    match (expected, actual) {
        (KeyCode::Tab, KeyCode::BackTab) | (KeyCode::BackTab, KeyCode::Tab) => true,
        (expected, actual) => lenient_code(expected, actual),
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
        // `BackTab` is `Shift+Tab` reported as a dedicated code, so it is
        // labeled `Tab`: a `Shift` chord then renders as the canonical
        // `Shift+Tab` and deduplicates against the enhanced-keyboard spelling.
        KeyCode::BackTab => "Tab".to_string(),
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
}

/// One keyboard command: the abstract chord, its action, its cognitive category,
/// and the localization key of its help description.
///
/// The description is not stored as a literal English string (Conventions §9):
/// the spec carries an i18n key, and [`CommandSpec::description`] resolves it
/// through the active locale when Help renders the command. The command registry
/// stays a single source of truth for both input dispatch and Help rendering.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec<A> {
    pub chord: KeyChord,
    pub action: A,
    pub category: CommandCategory,
    pub description_key: &'static str,
}

impl<A> CommandSpec<A> {
    /// Builds a command specification from its localization key. `const` so
    /// registries are declared as `static` values.
    pub const fn new(
        chord: KeyChord,
        action: A,
        category: CommandCategory,
        description_key: &'static str,
    ) -> Self {
        Self {
            chord,
            action,
            category,
            description_key,
        }
    }

    /// Resolves the command description through the active locale.
    ///
    /// The stored key is looked up in the canonical catalog (Conventions §9). A
    /// key that is not registered falls back to its literal text, so a registry
    /// not yet migrated to the catalog keeps rendering its description instead
    /// of disappearing from Help.
    pub fn description(&self) -> String {
        let localized = crate::i18n::active_t(self.description_key);
        if localized.is_empty() {
            self.description_key.to_string()
        } else {
            localized
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
    use super::dialog::DIALOG_COMMAND_MAP;
    use super::editor::EDITOR_COMMAND_MAP;
    use super::global::GLOBAL_COMMAND_MAP;
    use super::*;

    use crate::views::context::input::{ContextAction, CONTEXT_COMMAND_MAP};
    use crate::views::history::input::HISTORY_COMMAND_MAP;
    use crate::views::plans::input::{PlansAction, PLANS_COMMAND_MAP};
    use crate::views::repl::input::REPL_COMMAND_MAP;
    use crate::views::review::input::{ReviewAction, REVIEW_COMMAND_MAP};

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
        assert!(!chord.matches(
            key(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &input,
            false
        ));
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
    fn shift_chord_accepts_the_backtab_spelling_of_shift_tab() {
        let input = ctrl_primary();

        // An enhanced keyboard reports `Shift+Tab` as the dedicated `BackTab`
        // code, and some terminals omit the `SHIFT` modifier with it. Both
        // spellings must resolve to the same action.
        let chord = KeyChord::Shift(KeyCode::Tab);
        assert!(chord.matches(key(KeyCode::BackTab, KeyModifiers::SHIFT), &input, false));
        assert!(chord.matches(key(KeyCode::BackTab, KeyModifiers::NONE), &input, false));
        assert!(!chord.matches(key(KeyCode::Tab, KeyModifiers::NONE), &input, false));

        let backtab_chord = KeyChord::Shift(KeyCode::BackTab);
        assert!(backtab_chord.matches(key(KeyCode::Tab, KeyModifiers::SHIFT), &input, false));
        assert!(backtab_chord.matches(key(KeyCode::BackTab, KeyModifiers::NONE), &input, false));
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

        assert!(!KeyChord::Primary(KeyCode::Char('2')).matches(
            key(KeyCode::Char('2'), compose),
            &input,
            true
        ));
        assert!(!KeyChord::Meta(KeyCode::Char('2')).matches(
            key(KeyCode::Char('2'), compose),
            &input,
            true
        ));
        assert!(!KeyChord::Ctrl(KeyCode::Char('2')).matches(
            key(KeyCode::Char('2'), compose),
            &input,
            true
        ));
        assert!(!KeyChord::Alt(KeyCode::Char('2')).matches(
            key(KeyCode::Char('2'), compose),
            &input,
            true
        ));
    }

    #[test]
    fn display_projects_abstract_modifiers_through_the_configuration() {
        let ctrl = ctrl_primary();
        let (p, m) = (ctrl.primary_modifier, ctrl.meta_modifier);

        assert_eq!(
            KeyChord::Primary(KeyCode::Char('s')).display(p, m),
            "Ctrl+S"
        );
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
        assert_eq!(
            KeyChord::Primary(KeyCode::Char('s')).display(ap, am),
            "Alt+S"
        );
        assert_eq!(KeyChord::Meta(KeyCode::Char('s')).display(ap, am), "Ctrl+S");
    }

    #[test]
    fn boundary_chords_match_meta_angles_and_ctrl_home_end() {
        let input = ctrl_primary();

        // `M-<` arrives as the `<` character with Alt, optionally with Shift
        // when the developer is forced to press `Shift+,` on a US layout.
        let meta_less = KeyChord::Meta(KeyCode::Char('<'));
        assert!(meta_less.matches(
            key(KeyCode::Char('<'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            &input,
            false
        ));
        assert!(meta_less.matches(key(KeyCode::Char('<'), KeyModifiers::ALT), &input, false));
        assert!(!meta_less.matches(key(KeyCode::Char('<'), KeyModifiers::NONE), &input, false));

        let meta_greater = KeyChord::Meta(KeyCode::Char('>'));
        assert!(meta_greater.matches(key(KeyCode::Char('>'), KeyModifiers::ALT), &input, false));
        assert!(!meta_greater.matches(key(KeyCode::Char('<'), KeyModifiers::ALT), &input, false));

        // The dedicated buffer-end keys are raw Ctrl chords, distinct from the
        // bare `Home` / `End` structural bindings.
        let ctrl_home = KeyChord::Ctrl(KeyCode::Home);
        assert!(ctrl_home.matches(key(KeyCode::Home, KeyModifiers::CONTROL), &input, false));
        assert!(!ctrl_home.matches(key(KeyCode::Home, KeyModifiers::NONE), &input, false));
        assert!(!ctrl_home.matches(key(KeyCode::End, KeyModifiers::CONTROL), &input, false));

        let ctrl_end = KeyChord::Ctrl(KeyCode::End);
        assert!(ctrl_end.matches(key(KeyCode::End, KeyModifiers::CONTROL), &input, false));
        assert!(!ctrl_end.matches(key(KeyCode::End, KeyModifiers::NONE), &input, false));

        // Display keeps the angle glyphs intact so Help names the real keys.
        let (p, m) = (input.primary_modifier, input.meta_modifier);
        assert_eq!(meta_less.display(p, m), "Alt+<");
        assert_eq!(meta_greater.display(p, m), "Alt+>");
        assert_eq!(ctrl_home.display(p, m), "Ctrl+Home");
        assert_eq!(ctrl_end.display(p, m), "Ctrl+End");
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

    // --- Integration tests: single source of truth and anti-drift (c-1, c-2, c-3) ---

    /// Asserts every chord in a map is declared exactly once.
    ///
    /// Two specs sharing an abstract chord would make resolution order-dependent
    /// and would let the generated Help advertise a key that dispatch never
    /// reaches, so uniqueness is the invariant that keeps a map a true single
    /// source of truth (c-1).
    fn assert_unique_chords<A: 'static>(map: &CommandMap<A>, label: &str) {
        let specs = map.specs();
        assert!(
            !specs.is_empty(),
            "{label}: a command map must not be empty"
        );
        for (index, spec) in specs.iter().enumerate() {
            for other in specs.iter().skip(index + 1) {
                assert_ne!(
                    spec.chord, other.chord,
                    "{label}: duplicate key chord {:?}",
                    spec.chord
                );
            }
        }
    }

    /// Asserts every command carries a description that resolves to text.
    ///
    /// A spec with a blank description would silently disappear from the
    /// generated Help dialog, which is exactly the drift this suite forbids
    /// (c-1, c-3).
    fn assert_every_command_is_described<A: 'static>(map: &CommandMap<A>, label: &str) {
        for spec in map.specs() {
            assert!(
                !spec.description_key.is_empty(),
                "{label}: every command must carry a description key"
            );
            assert!(
                !spec.description().is_empty(),
                "{label}: description '{}' must resolve to non-empty text",
                spec.description_key
            );
        }
    }

    fn physical_primary_modifiers(modifier: PrimaryModifier) -> KeyModifiers {
        match modifier {
            PrimaryModifier::Ctrl => KeyModifiers::CONTROL,
            PrimaryModifier::Alt => KeyModifiers::ALT,
        }
    }

    fn physical_meta_modifiers(modifier: MetaModifier) -> KeyModifiers {
        match modifier {
            MetaModifier::Alt => KeyModifiers::ALT,
            MetaModifier::Ctrl => KeyModifiers::CONTROL,
        }
    }

    /// Asserts every abstract `Primary` / `Meta` / `PrimaryOrMeta` chord resolves
    /// to its own action under one modifier configuration.
    ///
    /// This is the anti-drift guarantee behind the symmetric modifier
    /// abstraction (c-2): the physical key tracks the developer's configured
    /// `primary_modifier` / `meta_modifier`, while the resolved action stays
    /// anchored to the abstract chord, so a remapped workstation never silently
    /// rebinds a command.
    fn assert_abstract_modifiers_for_config<A>(
        map: &CommandMap<A>,
        input: &InputConfig,
        label: &str,
    ) where
        A: Copy + PartialEq + std::fmt::Debug + 'static,
    {
        for spec in map.specs() {
            let (code, modifiers) = match spec.chord {
                KeyChord::Primary(code) => {
                    (code, physical_primary_modifiers(input.primary_modifier))
                }
                KeyChord::Meta(code) => (code, physical_meta_modifiers(input.meta_modifier)),
                KeyChord::PrimaryOrMeta(code) => {
                    (code, physical_primary_modifiers(input.primary_modifier))
                }
                _ => continue,
            };
            let resolved_action = map
                .resolve(KeyEvent::new(code, modifiers), input, false)
                .map(|resolved| resolved.action);
            assert_eq!(
                resolved_action,
                Some(spec.action),
                "{label}: chord {:?} must resolve to its own action under every modifier configuration",
                spec.chord
            );
        }
    }

    fn assert_map_follows_both_configurations<A>(map: &CommandMap<A>, label: &str)
    where
        A: Copy + PartialEq + std::fmt::Debug + 'static,
    {
        assert_abstract_modifiers_for_config(map, &ctrl_primary(), label);
        assert_abstract_modifiers_for_config(map, &alt_primary(), label);
    }

    #[test]
    fn every_command_map_declares_unique_key_chords() {
        assert_unique_chords(&GLOBAL_COMMAND_MAP, "GLOBAL_COMMAND_MAP");
        assert_unique_chords(&EDITOR_COMMAND_MAP, "EDITOR_COMMAND_MAP");
        assert_unique_chords(&DIALOG_COMMAND_MAP, "DIALOG_COMMAND_MAP");
        assert_unique_chords(&CONTEXT_COMMAND_MAP, "CONTEXT_COMMAND_MAP");
        assert_unique_chords(&HISTORY_COMMAND_MAP, "HISTORY_COMMAND_MAP");
        assert_unique_chords(&PLANS_COMMAND_MAP, "PLANS_COMMAND_MAP");
        assert_unique_chords(&REPL_COMMAND_MAP, "REPL_COMMAND_MAP");
        assert_unique_chords(&REVIEW_COMMAND_MAP, "REVIEW_COMMAND_MAP");
    }

    #[test]
    fn every_registered_command_is_described() {
        assert_every_command_is_described(&GLOBAL_COMMAND_MAP, "GLOBAL_COMMAND_MAP");
        assert_every_command_is_described(&EDITOR_COMMAND_MAP, "EDITOR_COMMAND_MAP");
        assert_every_command_is_described(&DIALOG_COMMAND_MAP, "DIALOG_COMMAND_MAP");
        assert_every_command_is_described(&CONTEXT_COMMAND_MAP, "CONTEXT_COMMAND_MAP");
        assert_every_command_is_described(&HISTORY_COMMAND_MAP, "HISTORY_COMMAND_MAP");
        assert_every_command_is_described(&PLANS_COMMAND_MAP, "PLANS_COMMAND_MAP");
        assert_every_command_is_described(&REPL_COMMAND_MAP, "REPL_COMMAND_MAP");
        assert_every_command_is_described(&REVIEW_COMMAND_MAP, "REVIEW_COMMAND_MAP");
    }

    /// Asserts every command description key is registered in the canonical
    /// catalog.
    ///
    /// The command maps now carry localization keys instead of raw English
    /// literals, so a key that is missing from the catalog would render an
    /// empty Help description at runtime. Iterating all eight maps against the
    /// catalog turns that silent drift into a deterministic test failure
    /// (Conventions §3, §9).
    #[test]
    fn every_command_description_key_exists_in_canonical_catalog() {
        use crate::i18n::catalog::canonical_catalog;
        use std::collections::HashSet;

        let catalog: HashSet<String> = canonical_catalog()
            .into_iter()
            .map(|item| item.key)
            .collect();

        fn assert_keys<A: 'static>(map: &CommandMap<A>, catalog: &HashSet<String>, label: &str) {
            for spec in map.specs() {
                assert!(
                    catalog.contains(spec.description_key),
                    "{label}: description key '{}' is missing from the canonical catalog",
                    spec.description_key
                );
            }
        }

        assert_keys(&GLOBAL_COMMAND_MAP, &catalog, "GLOBAL_COMMAND_MAP");
        assert_keys(&EDITOR_COMMAND_MAP, &catalog, "EDITOR_COMMAND_MAP");
        assert_keys(&DIALOG_COMMAND_MAP, &catalog, "DIALOG_COMMAND_MAP");
        assert_keys(&CONTEXT_COMMAND_MAP, &catalog, "CONTEXT_COMMAND_MAP");
        assert_keys(&HISTORY_COMMAND_MAP, &catalog, "HISTORY_COMMAND_MAP");
        assert_keys(&PLANS_COMMAND_MAP, &catalog, "PLANS_COMMAND_MAP");
        assert_keys(&REPL_COMMAND_MAP, &catalog, "REPL_COMMAND_MAP");
        assert_keys(&REVIEW_COMMAND_MAP, &catalog, "REVIEW_COMMAND_MAP");
    }

    #[test]
    fn abstract_modifiers_follow_the_configured_primary_and_meta() {
        assert_map_follows_both_configurations(&GLOBAL_COMMAND_MAP, "GLOBAL_COMMAND_MAP");
        assert_map_follows_both_configurations(&EDITOR_COMMAND_MAP, "EDITOR_COMMAND_MAP");
        assert_map_follows_both_configurations(&CONTEXT_COMMAND_MAP, "CONTEXT_COMMAND_MAP");
        assert_map_follows_both_configurations(&HISTORY_COMMAND_MAP, "HISTORY_COMMAND_MAP");
        assert_map_follows_both_configurations(&PLANS_COMMAND_MAP, "PLANS_COMMAND_MAP");
        assert_map_follows_both_configurations(&REPL_COMMAND_MAP, "REPL_COMMAND_MAP");
        assert_map_follows_both_configurations(&REVIEW_COMMAND_MAP, "REVIEW_COMMAND_MAP");
    }

    /// Resolves a key event against a command map with text input inactive.
    fn resolve_action<A: Copy + 'static>(
        map: &CommandMap<A>,
        key: KeyEvent,
        input: &InputConfig,
    ) -> Option<A> {
        map.resolve(key, input, false).map(|spec| spec.action)
    }

    /// Asserts `Home`, `M-<`, and `C-Home` all select the first row in a view.
    fn assert_first_row_chords_agree<A>(map: &CommandMap<A>, label: &str)
    where
        A: Copy + PartialEq + std::fmt::Debug + 'static,
    {
        let input = ctrl_primary();
        let home = resolve_action(map, key(KeyCode::Home, KeyModifiers::NONE), &input);
        let meta_less = resolve_action(map, key(KeyCode::Char('<'), KeyModifiers::ALT), &input);
        let ctrl_home = resolve_action(map, key(KeyCode::Home, KeyModifiers::CONTROL), &input);
        assert!(home.is_some(), "{label}: Home must select the first row");
        assert_eq!(home, meta_less, "{label}: M-< must match Home");
        assert_eq!(home, ctrl_home, "{label}: C-Home must match Home");
    }

    /// Asserts `End`, `M->`, and `C-End` all select the last row in a view.
    fn assert_last_row_chords_agree<A>(map: &CommandMap<A>, label: &str)
    where
        A: Copy + PartialEq + std::fmt::Debug + 'static,
    {
        let input = ctrl_primary();
        let end = resolve_action(map, key(KeyCode::End, KeyModifiers::NONE), &input);
        let meta_greater = resolve_action(map, key(KeyCode::Char('>'), KeyModifiers::ALT), &input);
        let ctrl_end = resolve_action(map, key(KeyCode::End, KeyModifiers::CONTROL), &input);
        assert!(end.is_some(), "{label}: End must select the last row");
        assert_eq!(end, meta_greater, "{label}: M-> must match End");
        assert_eq!(end, ctrl_end, "{label}: C-End must match End");
    }

    #[test]
    fn boundary_navigation_chords_are_unified_across_views() {
        assert_first_row_chords_agree(&CONTEXT_COMMAND_MAP, "CONTEXT_COMMAND_MAP");
        assert_last_row_chords_agree(&CONTEXT_COMMAND_MAP, "CONTEXT_COMMAND_MAP");
        assert_first_row_chords_agree(&PLANS_COMMAND_MAP, "PLANS_COMMAND_MAP");
        assert_last_row_chords_agree(&PLANS_COMMAND_MAP, "PLANS_COMMAND_MAP");
        assert_first_row_chords_agree(&REVIEW_COMMAND_MAP, "REVIEW_COMMAND_MAP");
        assert_last_row_chords_agree(&REVIEW_COMMAND_MAP, "REVIEW_COMMAND_MAP");
    }

    #[test]
    fn x_reserved_for_execution_never_destructive() {
        let input = ctrl_primary();

        assert_eq!(
            resolve_action(
                &PLANS_COMMAND_MAP,
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                &input
            ),
            Some(PlansAction::Execute),
            "x must execute the selected plan step"
        );
        assert_eq!(
            resolve_action(
                &PLANS_COMMAND_MAP,
                key(KeyCode::Char('X'), KeyModifiers::NONE),
                &input
            ),
            Some(PlansAction::ExecuteAll),
            "X must execute the plan batch"
        );

        assert_eq!(
            resolve_action(
                &REVIEW_COMMAND_MAP,
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                &input
            ),
            Some(ReviewAction::Execute),
            "x must execute the selected finding"
        );
        assert_eq!(
            resolve_action(
                &REVIEW_COMMAND_MAP,
                key(KeyCode::Char('X'), KeyModifiers::NONE),
                &input
            ),
            Some(ReviewAction::ExecuteAll),
            "X must execute the review batch"
        );

        assert_eq!(
            resolve_action(
                &CONTEXT_COMMAND_MAP,
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                &input
            ),
            None,
            "x must never be bound in the Context view"
        );
    }

    #[test]
    fn r_refines_selection_and_shift_r_refines_everything() {
        let input = ctrl_primary();

        assert_eq!(
            resolve_action(
                &PLANS_COMMAND_MAP,
                key(KeyCode::Char('r'), KeyModifiers::NONE),
                &input
            ),
            Some(PlansAction::Replan),
            "lowercase r must refine the selected plan horizon"
        );
        assert_eq!(
            resolve_action(
                &PLANS_COMMAND_MAP,
                key(KeyCode::Char('R'), KeyModifiers::NONE),
                &input
            ),
            Some(PlansAction::ReplanAll),
            "uppercase R must replan every blocked step"
        );

        assert_eq!(
            resolve_action(
                &REVIEW_COMMAND_MAP,
                key(KeyCode::Char('r'), KeyModifiers::NONE),
                &input
            ),
            Some(ReviewAction::Refine),
            "lowercase r must decompose the selected finding"
        );
        assert_eq!(
            resolve_action(
                &REVIEW_COMMAND_MAP,
                key(KeyCode::Char('R'), KeyModifiers::NONE),
                &input
            ),
            Some(ReviewAction::RefineAll),
            "uppercase R must decompose every awaiting finding"
        );
    }

    #[test]
    fn shift_n_starts_a_new_review_session() {
        let input = ctrl_primary();
        assert_eq!(
            resolve_action(
                &REVIEW_COMMAND_MAP,
                key(KeyCode::Char('N'), KeyModifiers::NONE),
                &input
            ),
            Some(ReviewAction::RunReview),
            "uppercase N must start a new review session"
        );
    }

    #[test]
    fn lowercase_g_refreshes_in_every_navigation_view() {
        let input = ctrl_primary();
        let refresh = key(KeyCode::Char('g'), KeyModifiers::NONE);
        assert_eq!(
            resolve_action(&PLANS_COMMAND_MAP, refresh, &input),
            Some(PlansAction::Refresh),
            "Plans: g must refresh"
        );
        assert_eq!(
            resolve_action(&REVIEW_COMMAND_MAP, refresh, &input),
            Some(ReviewAction::Refresh),
            "Review: g must refresh"
        );
        assert_eq!(
            resolve_action(&CONTEXT_COMMAND_MAP, refresh, &input),
            Some(ContextAction::Refresh),
            "Context: g must refresh"
        );
    }

    // --- Exhaustive key-event collision detector (c-2) ---

    /// Builds the exhaustive matrix of physical key events a terminal can
    /// report: every printable ASCII character and every dedicated navigation
    /// key, each crossed with the `None`, `Shift`, `Ctrl`, and `Alt` modifier
    /// states.
    ///
    /// The matrix deliberately includes both spellings a terminal uses for a
    /// shifted letter — the uppercase character alone and the lowercase
    /// character carrying `SHIFT` — so the discriminator between a bare `c` and
    /// the destructive `Shift+C` is exercised rather than assumed (c-1, c-2).
    fn key_event_matrix() -> Vec<(KeyCode, KeyModifiers)> {
        let modifiers = [
            KeyModifiers::NONE,
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
        ];
        let mut events = Vec::new();
        for byte in 0x20u8..=0x7e {
            let character = byte as char;
            for modifier in modifiers {
                events.push((KeyCode::Char(character), modifier));
            }
        }
        let navigation = [
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Esc,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ];
        for code in navigation {
            for modifier in modifiers {
                events.push((code, modifier));
            }
        }
        events
    }

    /// Asserts that no single key event is claimed by two commands with
    /// different actions.
    ///
    /// Comparing chords for equality is insufficient: distinct [`KeyChord`]
    /// variants (`Bare`, `Shift`, `Primary`, `Meta`, `Ctrl`, `Alt`) can match the
    /// same physical [`KeyEvent`]. The detector resolves the full event matrix
    /// against every specification and fails when two specs with different
    /// actions both match, which is exactly the ambiguity that makes a command
    /// map order-dependent and lets one key silently shadow another (c-2).
    fn assert_no_key_event_collisions<A>(map: &CommandMap<A>, input: &InputConfig, label: &str)
    where
        A: Copy + PartialEq + std::fmt::Debug + 'static,
    {
        for (code, modifiers) in key_event_matrix() {
            let mut claimed: Option<&'static CommandSpec<A>> = None;
            for spec in map.specs() {
                if !spec
                    .chord
                    .matches(KeyEvent::new(code, modifiers), input, false)
                {
                    continue;
                }
                match claimed {
                    None => claimed = Some(spec),
                    Some(existing) if existing.action == spec.action => {}
                    Some(existing) => panic!(
                        "{label}: key event {code:?} with modifiers {modifiers:?} is \
                         ambiguous: claimed by {:?} (chord {:?}) and {:?} (chord {:?})",
                        existing.action, existing.chord, spec.action, spec.chord,
                    ),
                }
            }
        }
    }

    /// Proves every registered command map is deterministic over the physical
    /// keyboard, so the order of declaration can never change which action a key
    /// resolves to (c-2).
    ///
    /// The maps are authored against the canonical modifier assignment (`C-` on
    /// `Ctrl`, `M-` on `Alt`); resolving the event matrix under that assignment
    /// turns `Primary` into `Ctrl` and `Meta` into `Alt`, so the abstract chords
    /// are covered by the physical `Ctrl` and `Alt` modifier states of the
    /// matrix without overloading the raw navigation chords.
    #[test]
    fn no_key_event_collisions_across_all_command_maps() {
        let input = ctrl_primary();

        assert_no_key_event_collisions(&GLOBAL_COMMAND_MAP, &input, "GLOBAL_COMMAND_MAP");
        assert_no_key_event_collisions(&EDITOR_COMMAND_MAP, &input, "EDITOR_COMMAND_MAP");
        assert_no_key_event_collisions(&DIALOG_COMMAND_MAP, &input, "DIALOG_COMMAND_MAP");
        assert_no_key_event_collisions(&CONTEXT_COMMAND_MAP, &input, "CONTEXT_COMMAND_MAP");
        assert_no_key_event_collisions(&HISTORY_COMMAND_MAP, &input, "HISTORY_COMMAND_MAP");
        assert_no_key_event_collisions(&PLANS_COMMAND_MAP, &input, "PLANS_COMMAND_MAP");
        assert_no_key_event_collisions(&REPL_COMMAND_MAP, &input, "REPL_COMMAND_MAP");
        assert_no_key_event_collisions(&REVIEW_COMMAND_MAP, &input, "REVIEW_COMMAND_MAP");
    }
}
