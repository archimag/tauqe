//! Command map for the shared text editor.
//!
//! The editor backs the prompt, the squash message field, the review prompt, the
//! file picker, and the onboarding fields, so its Emacs/Readline bindings are
//! specified once here and surfaced through a dedicated Help tab rather than
//! duplicated into every mode.

use crossterm::event::KeyCode;

use super::CommandCategory::{Helpers, Interface};
use super::{CommandMap, CommandSpec, KeyChord};

use self::EditorCommand::*;

/// Emacs/Readline operations performed by the text editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorCommand {
    MoveCharBackward,
    MoveCharForward,
    MoveWordBackward,
    MoveWordForward,
    MoveBeginningOfLine,
    MoveEndOfLine,
    MoveLineUp,
    MoveLineDown,
    DeleteForward,
    InsertNewline,
    KillWordBackward,
    KillWordForward,
    KillToBeginningOfLine,
    KillLine,
    Yank,
}

/// Single source of truth for the text editor keybindings.
pub static EDITOR_COMMAND_MAP: CommandMap<EditorCommand> = CommandMap::new(&[
    // Interface: cursor movement and line editing.
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('b')),
        MoveCharBackward,
        Interface,
        "Move one character backward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('f')),
        MoveCharForward,
        Interface,
        "Move one character forward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('b')),
        MoveWordBackward,
        Interface,
        "Move one word backward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Left),
        MoveWordBackward,
        Interface,
        "Move one word backward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('f')),
        MoveWordForward,
        Interface,
        "Move one word forward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Right),
        MoveWordForward,
        Interface,
        "Move one word forward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('a')),
        MoveBeginningOfLine,
        Interface,
        "Move to the beginning of the line",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('e')),
        MoveEndOfLine,
        Interface,
        "Move to the end of the line",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('p')),
        MoveLineUp,
        Interface,
        "Move the cursor one line up",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('n')),
        MoveLineDown,
        Interface,
        "Move the cursor one line down",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('d')),
        DeleteForward,
        Interface,
        "Delete the character forward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('j')),
        InsertNewline,
        Interface,
        "Insert a newline",
    ),
    // Helpers: kill-ring and clipboard operations.
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('w')),
        KillWordBackward,
        Helpers,
        "Kill the word before the cursor",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Backspace),
        KillWordBackward,
        Helpers,
        "Kill the word before the cursor",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('d')),
        KillWordForward,
        Helpers,
        "Kill the word after the cursor",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('u')),
        KillToBeginningOfLine,
        Helpers,
        "Kill to the beginning of the line",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('k')),
        KillLine,
        Helpers,
        "Kill to the end of the line",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('y')),
        Yank,
        Helpers,
        "Yank from the kill ring or the clipboard",
    ),
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InputConfig;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn editor_map_resolves_readline_chords_while_typing() {
        let input = InputConfig::default();

        let kill = EDITOR_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &input,
            true,
        );
        assert_eq!(
            kill.map(|spec| spec.action),
            Some(EditorCommand::KillWordBackward)
        );

        let char_back = EDITOR_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &input,
            true,
        );
        assert_eq!(
            char_back.map(|spec| spec.action),
            Some(EditorCommand::MoveCharBackward)
        );

        let word_back = EDITOR_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            &input,
            true,
        );
        assert_eq!(
            word_back.map(|spec| spec.action),
            Some(EditorCommand::MoveWordBackward)
        );
    }
}
