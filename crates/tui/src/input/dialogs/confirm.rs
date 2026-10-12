use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{AppState, ConfirmDialogButton};
use crate::input::is_char_typing;

pub(super) enum ConfirmOutcome {
    Toggled,
    Accepted,
    Dismissed,
}

/// Standard key handling for `[ Confirm ] / [ Cancel ]` modals (§7 Conventions):
/// arrows/Tab cycle buttons, `Enter`/`Space` activates the focused button,
/// `y`/`Y` confirms immediately, `n`/`N`/`Esc`/`q` dismisses safely.
pub(super) fn resolve_confirm(st: &mut AppState, key: KeyEvent) -> ConfirmOutcome {
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
            st.confirm_button = match st.confirm_button {
                ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
                ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
            };
            ConfirmOutcome::Toggled
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let accepted = st.confirm_button == ConfirmDialogButton::Confirm;
            st.confirm_button = ConfirmDialogButton::Cancel;
            if accepted {
                ConfirmOutcome::Accepted
            } else {
                ConfirmOutcome::Dismissed
            }
        }
        KeyCode::Char('y') | KeyCode::Char('Y') if is_char_typing(key.modifiers) => {
            st.confirm_button = ConfirmDialogButton::Cancel;
            ConfirmOutcome::Accepted
        }
        KeyCode::Char('n') | KeyCode::Char('N') if is_char_typing(key.modifiers) => {
            st.confirm_button = ConfirmDialogButton::Cancel;
            ConfirmOutcome::Dismissed
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            st.confirm_button = ConfirmDialogButton::Cancel;
            ConfirmOutcome::Dismissed
        }
        _ => ConfirmOutcome::Toggled,
    }
}
