use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{AppState, BatchCancelButton};
use crate::components::confirm::ConfirmDialog;
use crate::components::editor::is_char_typing;
use crate::input::is_keyboard_quit;

pub(super) use crate::components::confirm::ConfirmOutcome;

/// Standard key handling for `[ Confirm ] / [ Cancel ]` modals (Conventions.md §7),
/// delegated to the reusable [`ConfirmDialog`] component so every confirmation
/// modal shares one interaction contract.
pub(super) fn resolve_confirm(st: &mut AppState, key: KeyEvent) -> ConfirmOutcome {
    let mut dialog = ConfirmDialog::from_button(st.confirm_button);
    let outcome = dialog.handle_key(key);
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
pub(super) fn resolve_batch_cancel(st: &mut AppState, key: KeyEvent) -> BatchCancelOutcome {
    match key.code {
        KeyCode::Left | KeyCode::Char('p') | KeyCode::BackTab => {
            st.batch_cancel_button.cycle_prev();
            BatchCancelOutcome::Toggled
        }
        KeyCode::Right | KeyCode::Char('n') | KeyCode::Tab => {
            st.batch_cancel_button.cycle_next();
            BatchCancelOutcome::Toggled
        }
        KeyCode::Char('s') | KeyCode::Char('S') if is_char_typing(key.modifiers) => {
            BatchCancelOutcome::Chosen(BatchCancelButton::SoftStop)
        }
        KeyCode::Char('h') | KeyCode::Char('H') if is_char_typing(key.modifiers) => {
            BatchCancelOutcome::Chosen(BatchCancelButton::HardStop)
        }
        KeyCode::Char('c') | KeyCode::Char('C') if is_char_typing(key.modifiers) => {
            BatchCancelOutcome::Chosen(BatchCancelButton::Continue)
        }
        KeyCode::Esc | KeyCode::Char('q') => BatchCancelOutcome::Dismissed,
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
}
