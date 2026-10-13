use crossterm::event::KeyEvent;

use crate::app::AppState;
use crate::components::confirm::ConfirmDialog;

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
