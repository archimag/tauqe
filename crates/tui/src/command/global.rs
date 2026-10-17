//! Global command map: application-wide shortcuts available from any view.
//!
//! Descriptions are localization keys (`cmd.global.*`) resolved through the
//! active locale at render time (Conventions §9), so the Help dialog and the
//! registered chords cannot drift from the keys the application intercepts.

use crossterm::event::KeyCode;

use crate::app::ViewMode;

use super::CommandCategory::{Essential, Helpers, Interface};
use super::{CommandMap, CommandSpec, KeyChord};

use self::GlobalCommand::*;

/// Application-wide commands reachable from every view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalCommand {
    CancelGeneration,
    SquashCommits,
    SwitchView(ViewMode),
    SelectModel,
    SelectTheme,
    SelectLocale,
    ToggleReasoning,
    OpenHelp,
    ReloadConfig,
    Quit,
    KeyboardQuit,
}

/// Single source of truth for the global keybindings.
pub static GLOBAL_COMMAND_MAP: CommandMap<GlobalCommand> = CommandMap::new(&[
    // Essential: the costly work cycle.
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('c')),
        CancelGeneration,
        Essential,
        "cmd.global.cancel_generation",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('s')),
        SquashCommits,
        Essential,
        "cmd.global.squash_commits",
    ),
    // Interface: view switching, navigation, and ancillary pickers.
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('1')),
        SwitchView(ViewMode::Repl),
        Interface,
        "cmd.global.switch_repl",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('2')),
        SwitchView(ViewMode::Plans),
        Interface,
        "cmd.global.switch_plans",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('3')),
        SwitchView(ViewMode::Review),
        Interface,
        "cmd.global.switch_review",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('4')),
        SwitchView(ViewMode::Context),
        Interface,
        "cmd.global.switch_context",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('5')),
        SwitchView(ViewMode::History),
        Interface,
        "cmd.global.switch_history",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('m')),
        SelectModel,
        Interface,
        "cmd.global.select_model",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('t')),
        SelectTheme,
        Interface,
        "cmd.global.select_theme",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('l')),
        SelectLocale,
        Interface,
        "cmd.global.select_locale",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('r')),
        ToggleReasoning,
        Interface,
        "cmd.global.toggle_reasoning",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('h')),
        OpenHelp,
        Interface,
        "cmd.global.open_help",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('?')),
        OpenHelp,
        Interface,
        "cmd.global.open_help",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('?')),
        OpenHelp,
        Interface,
        "cmd.global.open_help",
    ),
    // Helpers: auxiliary and destructive actions.
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('o')),
        ReloadConfig,
        Helpers,
        "cmd.global.reload_config",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('q')),
        Quit,
        Helpers,
        "cmd.global.quit",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('g')),
        KeyboardQuit,
        Helpers,
        "cmd.global.keyboard_quit",
    ),
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InputConfig;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn global_map_resolves_command_chords_from_either_modifier() {
        let input = InputConfig::default();

        let squash = GLOBAL_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &input,
            false,
        );
        assert_eq!(
            squash.map(|spec| spec.action),
            Some(GlobalCommand::SquashCommits)
        );

        let alt_squash = GLOBAL_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT),
            &input,
            false,
        );
        assert_eq!(
            alt_squash.map(|spec| spec.action),
            Some(GlobalCommand::SquashCommits)
        );

        let cancel = GLOBAL_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &input,
            false,
        );
        assert_eq!(
            cancel.map(|spec| spec.action),
            Some(GlobalCommand::CancelGeneration)
        );
    }

    #[test]
    fn global_map_switches_views_by_digit() {
        let input = InputConfig::default();
        let plans = GLOBAL_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL),
            &input,
            false,
        );
        assert_eq!(
            plans.map(|spec| spec.action),
            Some(GlobalCommand::SwitchView(ViewMode::Plans))
        );
    }

    #[test]
    fn global_map_blocks_the_bare_help_key_while_typing() {
        let input = InputConfig::default();

        let navigation = GLOBAL_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            &input,
            false,
        );
        assert_eq!(
            navigation.map(|spec| spec.action),
            Some(GlobalCommand::OpenHelp)
        );

        let typing = GLOBAL_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            &input,
            true,
        );
        assert!(typing.is_none());
    }

    #[test]
    fn global_map_binds_locale_switch_to_the_meta_chord() {
        let input = InputConfig::default();

        let meta_l = GLOBAL_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT),
            &input,
            false,
        );
        assert_eq!(
            meta_l.map(|spec| spec.action),
            Some(GlobalCommand::SelectLocale)
        );

        // The locale picker is a client-only dialog, so the chord stays
        // resolvable from navigation contexts as well as the prompt.
        let typing = GLOBAL_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT),
            &input,
            true,
        );
        assert_eq!(
            typing.map(|spec| spec.action),
            Some(GlobalCommand::SelectLocale)
        );
    }
}
