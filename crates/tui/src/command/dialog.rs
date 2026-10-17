//! Shared dialog command map.
//!
//! Every modal dialog shares one interaction vocabulary: dismiss with `Esc`,
//! `q`, or the `C-g` keyboard-quit; move focus with `Tab` / `Shift+Tab` or the
//! `←` / `→` button steppers; activate with `Enter` or `Space`; and use the
//! `y` / `Y` and `n` / `N` accelerators for fast confirmation. Specifying that
//! vocabulary once lets the dialog footer hints be generated from a single
//! source, with the configured `C-` / `M-` labels applied automatically,
//! instead of being hand-laid per dialog.
//!
//! Descriptions are localization keys (`cmd.dialog.*`) resolved through the
//! active locale at render time (Conventions §9).

use crossterm::event::KeyCode;

use super::CommandCategory::{Helpers, Interface};
use super::{CommandMap, CommandSpec, KeyChord};

use self::DialogCommand::*;

/// Universal modal-dialog operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogCommand {
    /// Cancel and close the dialog.
    Dismiss,
    /// Cycle focus forward through the dialog's controls.
    FocusNext,
    /// Cycle focus backward through the dialog's controls.
    FocusPrev,
    /// Step focus forward between adjacent action buttons.
    StepNext,
    /// Step focus backward between adjacent action buttons.
    StepPrev,
    /// Activate the focused control.
    Activate,
    /// Confirm the dialog immediately, regardless of focus.
    ConfirmFast,
    /// Cancel the dialog immediately, regardless of focus.
    CancelFast,
}

/// Single source of truth for the universal dialog keybindings.
///
/// The vocabulary is complete so that every confirmation, cancellation, and
/// button-navigation key is described exactly once here and resolved through
/// [`DIALOG_COMMAND_MAP`], never re-implemented as ad-hoc key comparisons in a
/// dialog handler (Conventions §3, §5).
pub static DIALOG_COMMAND_MAP: CommandMap<DialogCommand> = CommandMap::new(&[
    // Interface: dismissal. `Esc` is the canonical chord, `q` is the Magit
    // alias, and `C-g` is the Emacs keyboard-quit that dependent surfaces
    // already normalize to `Esc` before dispatch.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        Dismiss,
        Interface,
        "cmd.dialog.dismiss",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        Dismiss,
        Interface,
        "cmd.dialog.dismiss",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('g')),
        Dismiss,
        Interface,
        "cmd.dialog.dismiss",
    ),
    // Interface: vertical focus cycling. Terminals report `Shift+Tab` either as
    // `Tab` with `Shift` (enhanced keyboard protocols) or as a dedicated
    // `BackTab` code, so both spellings are bound to the same command.
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Tab),
        FocusPrev,
        Interface,
        "cmd.dialog.focus_prev",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::BackTab),
        FocusPrev,
        Interface,
        "cmd.dialog.focus_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        FocusNext,
        Interface,
        "cmd.dialog.focus_next",
    ),
    // Interface: horizontal button stepping inside an action bar. The chords
    // reuse the focus descriptions because a step is a one-control focus move.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Left),
        StepPrev,
        Interface,
        "cmd.dialog.focus_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Right),
        StepNext,
        Interface,
        "cmd.dialog.focus_next",
    ),
    // Interface: activation of the focused control.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Enter),
        Activate,
        Interface,
        "cmd.dialog.activate",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(' ')),
        Activate,
        Interface,
        "cmd.dialog.activate",
    ),
    // Helpers: fast confirmation accelerators. Both letter cases are bound so
    // the accelerator fires whether or not `Shift` is held (Conventions §6).
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('y')),
        ConfirmFast,
        Helpers,
        "cmd.dialog.confirm_fast",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('Y')),
        ConfirmFast,
        Helpers,
        "cmd.dialog.confirm_fast",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        CancelFast,
        Helpers,
        "cmd.dialog.cancel_fast",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('N')),
        CancelFast,
        Helpers,
        "cmd.dialog.cancel_fast",
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
