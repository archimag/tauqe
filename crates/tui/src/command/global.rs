//! Global command map: application-wide shortcuts available from any view.

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
        "Cancel the active generation",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('s')),
        SquashCommits,
        Essential,
        "Open the squash AI commits dialog",
    ),
    // Interface: view switching, navigation, and ancillary pickers.
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('1')),
        SwitchView(ViewMode::Develop),
        Interface,
        "Switch to the REPL view",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('2')),
        SwitchView(ViewMode::Plans),
        Interface,
        "Switch to the Plans view",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('3')),
        SwitchView(ViewMode::Review),
        Interface,
        "Switch to the Review view",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('4')),
        SwitchView(ViewMode::Context),
        Interface,
        "Switch to the Context view",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('5')),
        SwitchView(ViewMode::History),
        Interface,
        "Switch to the History view",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('m')),
        SelectModel,
        Interface,
        "Select the active model",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('t')),
        SelectTheme,
        Interface,
        "Select the color theme",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('r')),
        ToggleReasoning,
        Interface,
        "Fold or unfold the reasoning stream",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('h')),
        OpenHelp,
        Interface,
        "Open the Help dialog",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('?')),
        OpenHelp,
        Interface,
        "Open the Help dialog",
    ),
    // Helpers: auxiliary and destructive actions.
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('o')),
        ReloadConfig,
        Helpers,
        "Reload configuration from disk",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('q')),
        Quit,
        Helpers,
        "Quit the application",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('g')),
        KeyboardQuit,
        Helpers,
        "Abort the active operation",
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
}
