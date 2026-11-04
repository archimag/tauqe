use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{AppState, BatchCancelButton};
use crate::command::{is_bare_press, translate_code};
use crate::components::confirm::ConfirmDialog;
use crate::input::is_keyboard_quit;

pub(super) use crate::components::confirm::ConfirmOutcome;

/// Standard key handling for `[ Confirm ] / [ Cancel ]` modals (Conventions.md §7),
/// delegated to the reusable [`ConfirmDialog`] component so every confirmation
/// modal shares one interaction contract.
///
/// The workstation's configured input travels with the key, so a card resolves
/// the very layout projection the view command maps resolve: a chord authored
/// as a Latin key stays reachable from an alternative layout (Conventions §5.1).
pub(super) fn resolve_confirm(st: &mut AppState, key: KeyEvent) -> ConfirmOutcome {
    let mut dialog = ConfirmDialog::from_button(st.confirm_button);
    let outcome = dialog.handle_key(key, &st.tui_config.input);
    st.confirm_button = dialog.button();
    outcome
}

/// Outcome of resolving a key inside the batch-cancellation dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchCancelOutcome {
    /// Focus moved between action buttons; the dialog stays open.
    Toggled,
    /// The dialog was dismissed without choosing an action.
    Dismissed,
    /// The developer chose the given action.
    Chosen(BatchCancelButton),
}

/// Resolves a key press inside the three-way batch-cancellation dialog.
///
/// `Tab`, the arrows, and `n`/`p` move focus between `Continue`, `Soft Stop`,
/// and `Hard Stop`; `Enter`/`Space` commits the focused button; `s`, `h`, and
/// `c` are fast accelerators; `Esc` and `q` dismiss the dialog and keep the
/// batch running, which is the safe default (Conventions.md §7).
///
/// The accelerators and the focus walk are literal letters rather than command
/// specifications, so the press is projected through the configured keyboard
/// layout before it is matched: `ы`, `р`, and `с` reach `s`, `h`, and `c` on the
/// JCUKEN layout, and a custom `langmap` widens the vocabulary the same way
/// (Conventions §5.1). The projection result is bound before the match so the
/// shared borrow of the state ends before an arm mutates the focused button.
/// Every letter is a bare press, so a command chord or the AltGr compose
/// chord neither chooses a stop action nor dismisses the card (Conventions
/// §3, §5.1).
pub(super) fn resolve_batch_cancel(st: &mut AppState, key: KeyEvent) -> BatchCancelOutcome {
    let code = translate_code(key.code, &st.tui_config.input);
    let bare = is_bare_press(key.modifiers);
    match code {
        KeyCode::Left | KeyCode::BackTab => {
            st.batch_cancel_button.cycle_prev();
            BatchCancelOutcome::Toggled
        }
        KeyCode::Right | KeyCode::Tab => {
            st.batch_cancel_button.cycle_next();
            BatchCancelOutcome::Toggled
        }
        // The focus walk and the fast accelerators are bare presses, so a
        // command chord or the AltGr compose chord reaches none of them
        // (Conventions §3, §5.1).
        KeyCode::Char('p') if bare => {
            st.batch_cancel_button.cycle_prev();
            BatchCancelOutcome::Toggled
        }
        KeyCode::Char('n') if bare => {
            st.batch_cancel_button.cycle_next();
            BatchCancelOutcome::Toggled
        }
        KeyCode::Char('s') | KeyCode::Char('S') if bare => {
            BatchCancelOutcome::Chosen(BatchCancelButton::SoftStop)
        }
        KeyCode::Char('h') | KeyCode::Char('H') if bare => {
            BatchCancelOutcome::Chosen(BatchCancelButton::HardStop)
        }
        KeyCode::Char('c') | KeyCode::Char('C') if bare => {
            BatchCancelOutcome::Chosen(BatchCancelButton::Continue)
        }
        KeyCode::Esc => BatchCancelOutcome::Dismissed,
        KeyCode::Char('q') if bare => BatchCancelOutcome::Dismissed,
        KeyCode::Char('g') if is_keyboard_quit(&key) => BatchCancelOutcome::Dismissed,
        KeyCode::Enter | KeyCode::Char(' ') => BatchCancelOutcome::Chosen(st.batch_cancel_button),
        _ => BatchCancelOutcome::Toggled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn accelerators_choose_each_batch_action() {
        let mut st = crate::app::test_state();
        assert_eq!(
            resolve_batch_cancel(&mut st, key(KeyCode::Char('s'))),
            BatchCancelOutcome::Chosen(BatchCancelButton::SoftStop)
        );
        assert_eq!(
            resolve_batch_cancel(&mut st, key(KeyCode::Char('h'))),
            BatchCancelOutcome::Chosen(BatchCancelButton::HardStop)
        );
        assert_eq!(
            resolve_batch_cancel(&mut st, key(KeyCode::Char('c'))),
            BatchCancelOutcome::Chosen(BatchCancelButton::Continue)
        );
    }

    #[test]
    fn escape_dismisses_and_enter_commits_focus() {
        let mut st = crate::app::test_state();
        assert_eq!(
            resolve_batch_cancel(&mut st, key(KeyCode::Esc)),
            BatchCancelOutcome::Dismissed
        );

        let mut st = crate::app::test_state();
        assert_eq!(
            resolve_batch_cancel(&mut st, key(KeyCode::Right)),
            BatchCancelOutcome::Toggled
        );
        assert_eq!(st.batch_cancel_button, BatchCancelButton::SoftStop);
        assert_eq!(
            resolve_batch_cancel(&mut st, key(KeyCode::Enter)),
            BatchCancelOutcome::Chosen(BatchCancelButton::SoftStop)
        );
    }

    /// The business-letter accelerators follow the configured layout in the
    /// three-way card too, so a Cyrillic press reaches the action its Latin
    /// chord names (Conventions §5.1).
    #[test]
    fn ru_jcuken_layout_reaches_the_batch_accelerators() {
        use crate::config::LayoutPreset;

        for (cyrillic, expected) in [
            ('ы', BatchCancelOutcome::Chosen(BatchCancelButton::SoftStop)),
            ('р', BatchCancelOutcome::Chosen(BatchCancelButton::HardStop)),
            ('с', BatchCancelOutcome::Chosen(BatchCancelButton::Continue)),
        ] {
            let mut st = crate::app::test_state();
            st.tui_config.input.layout = LayoutPreset::RuJcuken;
            assert_eq!(
                resolve_batch_cancel(&mut st, key(KeyCode::Char(cyrillic))),
                expected,
                "{cyrillic:?} sits on the Latin accelerator"
            );
        }
    }

    /// The focus walk uses the same projection: `т` sits on `n` and `з` on `p`,
    /// so the three buttons stay reachable from the Cyrillic layout.
    #[test]
    fn ru_jcuken_layout_reaches_the_batch_focus_walk() {
        use crate::config::LayoutPreset;

        let mut st = crate::app::test_state();
        st.tui_config.input.layout = LayoutPreset::RuJcuken;

        assert_eq!(
            resolve_batch_cancel(&mut st, key(KeyCode::Char('т'))),
            BatchCancelOutcome::Toggled
        );
        assert_eq!(st.batch_cancel_button, BatchCancelButton::SoftStop);

        assert_eq!(
            resolve_batch_cancel(&mut st, key(KeyCode::Char('з'))),
            BatchCancelOutcome::Toggled
        );
        assert_eq!(st.batch_cancel_button, BatchCancelButton::Continue);
    }

    fn key_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// Command-modified and composed letters reach no accelerator and move no
    /// focus: `Ctrl+S`/`Ctrl+H` never choose a stop action, `Ctrl+N`/`Ctrl+P`
    /// never walk the buttons, and `Ctrl+Q` never dismisses the card, while
    /// `Esc` stays unconditional (Conventions §3, §5.1).
    #[test]
    fn command_modified_letters_are_refused_by_every_guarded_arm() {
        let letters = ['s', 'S', 'h', 'H', 'c', 'C', 'n', 'p', 'q'];
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ] {
            for letter in letters {
                let mut st = crate::app::test_state();
                assert_eq!(st.batch_cancel_button, BatchCancelButton::Continue);
                let outcome =
                    resolve_batch_cancel(&mut st, key_with(KeyCode::Char(letter), modifiers));
                assert_eq!(
                    outcome,
                    BatchCancelOutcome::Toggled,
                    "{letter:?} with {modifiers:?} must reach no action"
                );
                assert_eq!(
                    st.batch_cancel_button,
                    BatchCancelButton::Continue,
                    "{letter:?} with {modifiers:?} must not move the focus"
                );
            }
        }

        // `Esc` keeps its unconditional dismissal.
        let mut st = crate::app::test_state();
        let escaped = resolve_batch_cancel(&mut st, key_with(KeyCode::Esc, KeyModifiers::CONTROL));
        assert_eq!(escaped, BatchCancelOutcome::Dismissed);
    }
}
