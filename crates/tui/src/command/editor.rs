//! Command map for the shared text editor.
//!
//! The editor backs the prompt, the squash message field, the review prompt, the
//! file picker, and the onboarding fields, so its Emacs/Readline bindings are
//! specified once here and surfaced through a dedicated Help tab rather than
//! duplicated into every mode.
//!
//! Descriptions are localization keys (`cmd.editor.*`) resolved through the
//! active locale at render time (Conventions §9).

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
    MoveBufferStart,
    MoveBufferEnd,
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
        "cmd.editor.move_char_backward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('f')),
        MoveCharForward,
        Interface,
        "cmd.editor.move_char_forward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('b')),
        MoveWordBackward,
        Interface,
        "cmd.editor.move_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Left),
        MoveWordBackward,
        Interface,
        "cmd.editor.move_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('f')),
        MoveWordForward,
        Interface,
        "cmd.editor.move_word_forward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Right),
        MoveWordForward,
        Interface,
        "cmd.editor.move_word_forward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('a')),
        MoveBeginningOfLine,
        Interface,
        "cmd.editor.move_beginning_of_line",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('e')),
        MoveEndOfLine,
        Interface,
        "cmd.editor.move_end_of_line",
    ),
    // Interface: buffer boundaries. `M-<` / `M->` are the Emacs spellings, the
    // raw `C-Home` / `C-End` chords the terminal-native equivalents, so the
    // beginning and the end of the buffer stay reachable on every keyboard.
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('<')),
        MoveBufferStart,
        Interface,
        "cmd.editor.move_buffer_start",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Home),
        MoveBufferStart,
        Interface,
        "cmd.editor.move_buffer_start",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('>')),
        MoveBufferEnd,
        Interface,
        "cmd.editor.move_buffer_end",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::End),
        MoveBufferEnd,
        Interface,
        "cmd.editor.move_buffer_end",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('p')),
        MoveLineUp,
        Interface,
        "cmd.editor.move_line_up",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('n')),
        MoveLineDown,
        Interface,
        "cmd.editor.move_line_down",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('d')),
        DeleteForward,
        Interface,
        "cmd.editor.delete_forward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('j')),
        InsertNewline,
        Interface,
        "cmd.editor.insert_newline",
    ),
    // Helpers: kill-ring and clipboard operations.
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('w')),
        KillWordBackward,
        Helpers,
        "cmd.editor.kill_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Backspace),
        KillWordBackward,
        Helpers,
        "cmd.editor.kill_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('d')),
        KillWordForward,
        Helpers,
        "cmd.editor.kill_word_forward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('u')),
        KillToBeginningOfLine,
        Helpers,
        "cmd.editor.kill_to_beginning",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('k')),
        KillLine,
        Helpers,
        "cmd.editor.kill_line",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('y')),
        Yank,
        Helpers,
        "cmd.editor.yank",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Insert),
        Yank,
        Helpers,
        "cmd.editor.yank",
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

        let shift_insert = EDITOR_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT),
            &input,
            true,
        );
        assert_eq!(
            shift_insert.map(|spec| spec.action),
            Some(EditorCommand::Yank)
        );
    }

    #[test]
    fn editor_map_resolves_buffer_boundary_chords() {
        let input = InputConfig::default();

        let meta_less = EDITOR_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('<'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            &input,
            true,
        );
        assert_eq!(
            meta_less.map(|spec| spec.action),
            Some(EditorCommand::MoveBufferStart)
        );

        let ctrl_home = EDITOR_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL),
            &input,
            true,
        );
        assert_eq!(
            ctrl_home.map(|spec| spec.action),
            Some(EditorCommand::MoveBufferStart)
        );

        let meta_greater = EDITOR_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Char('>'), KeyModifiers::ALT),
            &input,
            true,
        );
        assert_eq!(
            meta_greater.map(|spec| spec.action),
            Some(EditorCommand::MoveBufferEnd)
        );

        let ctrl_end = EDITOR_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
            &input,
            true,
        );
        assert_eq!(
            ctrl_end.map(|spec| spec.action),
            Some(EditorCommand::MoveBufferEnd)
        );
    }
}
