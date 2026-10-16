//! Shared dialog command map.
//!
//! Every modal dialog shares one interaction vocabulary: dismiss with `Esc` or
//! `q`, move focus with `Tab`, activate with `Enter` or `Space`, and use the
//! `y` / `n` accelerators for fast confirmation. Specifying that vocabulary once
//! lets the dialog footer hints be generated from a single source, with the
//! configured `C-` / `M-` labels applied automatically, instead of being
//! hand-laid per dialog.

use crossterm::event::KeyCode;

use super::CommandCategory::{Helpers, Interface};
use super::{CommandMap, CommandSpec, KeyChord};

use self::DialogCommand::*;

/// Universal modal-dialog operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogCommand {
    Dismiss,
    FocusNext,
    FocusPrev,
    Activate,
    ConfirmFast,
    CancelFast,
}

/// Single source of truth for the universal dialog keybindings.
pub static DIALOG_COMMAND_MAP: CommandMap<DialogCommand> = CommandMap::new(&[
    // Interface: dismissal and focus navigation.
    CommandSpec::new(KeyChord::Bare(KeyCode::Esc), Dismiss, Interface, "Dismiss the dialog"),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        Dismiss,
        Interface,
        "Dismiss the dialog",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Tab),
        FocusPrev,
        Interface,
        "Focus the previous control",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        FocusNext,
        Interface,
        "Focus the next control",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Enter),
        Activate,
        Interface,
        "Activate the focused control",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(' ')),
        Activate,
        Interface,
        "Activate the focused control",
    ),
    // Helpers: fast confirmation accelerators.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('y')),
        ConfirmFast,
        Helpers,
        "Confirm immediately",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        CancelFast,
        Helpers,
        "Cancel immediately",
    ),
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InputConfig;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn dialog_map_resolves_universal_navigation() {
        let input = InputConfig::default();

        let dismiss = DIALOG_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &input,
            false,
        );
        assert_eq!(dismiss.map(|spec| spec.action), Some(DialogCommand::Dismiss));

        let next = DIALOG_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &input,
            false,
        );
        assert_eq!(next.map(|spec| spec.action), Some(DialogCommand::FocusNext));

        let prev = DIALOG_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            &input,
            false,
        );
        assert_eq!(prev.map(|spec| spec.action), Some(DialogCommand::FocusPrev));
    }
}
