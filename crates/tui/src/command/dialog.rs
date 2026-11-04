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

use super::CommandCategory::{Essential, Helpers, Interface};
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
    /// Submit the dialog through its explicit accelerator chord.
    ///
    /// The model-list dialog family declares it as the configured primary
    /// modifier together with `Enter` (`C-Enter`), so a dialog carrying an
    /// embedded list can submit from any section while every bare key stays
    /// inside the focused editor.
    SubmitAccelerator,
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

/// Single source of truth for the keyboard contract of a dialog that embeds a
/// model list.
///
/// The execution, refinement, and post-analysis dialogs share one vocabulary:
/// the shared dialog chords plus the submit accelerator their forms launch
/// from. Declaring it once stops the handlers from drifting apart, because the
/// `Shift+Tab` spelling and the bare-press guard of the letter accelerators
/// are exactly the rules the hand-written copies already diverged on
/// (Conventions §3, §5).
///
/// Every letter accelerator is a [`KeyChord::Bare`] chord, so neither a command
/// modifier nor the AltGr compose chord can reach it: `Ctrl+Y` and `Ctrl+Q`
/// stay the developer's yank and quit chords instead of launching a
/// token-spending turn or discarding typed instructions.
// Declared here as the executable contract of the model-list dialog family;
// the anti-drift suites in `crate::command` exercise it until the handlers
// resolve it directly.
#[allow(dead_code)]
pub static MODEL_LIST_DIALOG_COMMAND_MAP: CommandMap<DialogCommand> = CommandMap::new(&[
    // Interface: dismissal. `Esc` is the canonical chord, `q` is the Magit
    // alias, and `C-g` is the Emacs keyboard-quit.
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
    // Interface: focus cycling. Both terminal spellings of `Shift+Tab` are
    // declared so the shifted press always moves the focus backward, whether
    // the terminal reports the dedicated `BackTab` code or `Tab` carrying
    // `SHIFT`.
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
    // Interface: horizontal button stepping. The chords reuse the focus
    // descriptions because a step is a one-control focus move.
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
    // Helpers: the letter accelerators are bare presses, so a command chord or
    // the AltGr compose chord can never confirm a dialog that carries unsent
    // instructions. Both letter cases are bound so the accelerator fires
    // whether or not `Shift` is held (Conventions §6).
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
    // Essential: submission through the explicit accelerator. The configured
    // primary modifier together with `Enter` is the chord the handlers derive,
    // and the raw `Ctrl` spelling keeps it reachable on a workstation whose
    // primary modifier is `Alt`. The compose chord is excluded by the chord
    // machinery itself, so `AltGr+Enter` submits nothing.
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Enter),
        SubmitAccelerator,
        Essential,
        "cmd.dialog.submit_accelerator",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Enter),
        SubmitAccelerator,
        Essential,
        "cmd.dialog.submit_accelerator",
    ),
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InputConfig, MetaModifier, PrimaryModifier};
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn dialog_map_resolves_universal_navigation() {
        let input = InputConfig::default();

        let dismiss = DIALOG_COMMAND_MAP.resolve(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &input,
            false,
        );
        assert_eq!(
            dismiss.map(|spec| spec.action),
            Some(DialogCommand::Dismiss)
        );

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

    /// Workstation input with `Alt` as the primary and `Ctrl` as the meta
    /// modifier, so a chord assertion proves the accelerator follows the
    /// configuration instead of a hardcoded physical key.
    fn alt_primary() -> InputConfig {
        InputConfig {
            primary_modifier: PrimaryModifier::Alt,
            meta_modifier: MetaModifier::Ctrl,
            ..InputConfig::default()
        }
    }

    /// Physical `Enter` press of the configured primary modifier.
    fn primary_enter(input: &InputConfig) -> KeyEvent {
        match input.primary_modifier {
            PrimaryModifier::Ctrl => KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            PrimaryModifier::Alt => KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        }
    }

    fn resolve(input: &InputConfig, key: KeyEvent) -> Option<DialogCommand> {
        MODEL_LIST_DIALOG_COMMAND_MAP
            .resolve(key, input, false)
            .map(|spec| spec.action)
    }

    /// `C-Enter` submits from any section of a model-list dialog, and the raw
    /// `Ctrl` spelling keeps the accelerator reachable on a workstation whose
    /// primary modifier is `Alt`.
    #[test]
    fn model_list_map_submits_on_the_primary_and_raw_ctrl_enter() {
        for (input, label) in [(InputConfig::default(), "ctrl"), (alt_primary(), "alt")] {
            let expected = Some(DialogCommand::SubmitAccelerator);
            assert_eq!(
                resolve(&input, primary_enter(&input)),
                expected,
                "{label}: the configured primary modifier plus Enter must submit"
            );
            assert_eq!(
                resolve(&input, KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
                expected,
                "{label}: the raw Ctrl+Enter spelling must submit as well"
            );
        }
    }

    /// The letter accelerators are bare presses: the yank, kill, and quit
    /// chords of the shared editor vocabulary can never confirm or dismiss a
    /// dialog that still holds unsent instructions.
    #[test]
    fn model_list_map_ignores_letter_accelerators_under_command_modifiers() {
        let input = InputConfig::default();
        for code in [KeyCode::Char('y'), KeyCode::Char('n'), KeyCode::Char('q')] {
            for modifiers in [
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ] {
                assert_eq!(
                    resolve(&input, KeyEvent::new(code, modifiers)),
                    None,
                    "{code:?} with {modifiers:?} must reach no command"
                );
            }
        }

        assert_eq!(
            resolve(
                &input,
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
            ),
            Some(DialogCommand::ConfirmFast)
        );
        assert_eq!(
            resolve(
                &input,
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)
            ),
            Some(DialogCommand::CancelFast)
        );
        assert_eq!(
            resolve(
                &input,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            ),
            Some(DialogCommand::Dismiss)
        );
    }

    /// Both terminal spellings of `Shift+Tab` cycle focus backward, so a
    /// shifted press can never move the focus forward.
    #[test]
    fn model_list_map_cycles_backward_on_both_shift_tab_spellings() {
        let input = InputConfig::default();
        for key in [
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
        ] {
            assert_eq!(
                resolve(&input, key),
                Some(DialogCommand::FocusPrev),
                "{key:?} must cycle the focus backward"
            );
        }
        assert_eq!(
            resolve(&input, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some(DialogCommand::FocusNext)
        );
    }
}
