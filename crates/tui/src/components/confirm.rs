//! Reusable confirmation dialog component.
//!
//! [`ConfirmDialog`] centralizes the interaction contract for every
//! `[ Confirm ] / [ Cancel ]` modal (Conventions.md §7): the focused button,
//! keyboard resolution, and the button geometry shared between rendering and
//! mouse hit-testing. The `[ Cancel ]` button always receives the default focus
//! so that destructive actions require an explicit choice.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{ConfirmDialogButton, ModalActionsState};
use crate::components::editor::is_char_typing;
use crate::components::modal::{ModalFrame, ModalTone};
use crate::input::is_keyboard_quit;
use crate::ui::centered_rect;

/// Outcome of resolving a key press inside a confirmation dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// The focused button changed; the dialog stays open.
    Toggled,
    /// The user confirmed the action.
    Accepted,
    /// The user cancelled or dismissed the dialog safely.
    Dismissed,
}

/// Stateful confirm/cancel control with a safe-cancel default.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfirmDialog {
    actions: ModalActionsState,
}

impl ConfirmDialog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a dialog whose focus starts on the given button.
    pub fn from_button(button: ConfirmDialogButton) -> Self {
        Self {
            actions: button.into(),
        }
    }

    pub fn button(&self) -> ConfirmDialogButton {
        self.actions.focused_button
    }

    pub fn actions(&self) -> ModalActionsState {
        self.actions
    }

    pub fn is_confirm(&self) -> bool {
        self.actions.is_confirm()
    }

    pub fn is_cancel(&self) -> bool {
        self.actions.is_cancel()
    }

    pub fn toggle(&mut self) {
        self.actions.toggle();
    }

    pub fn reset_to_cancel(&mut self) {
        self.actions.reset_to_cancel();
    }

    /// Resolves a key event into a [`ConfirmOutcome`].
    ///
    /// Arrows and `Tab`/`Shift+Tab` cycle the focused button; `Enter`/`Space`
    /// activates it; `y`/`Y` confirms immediately and `n`/`N`/`Esc`/`q`
    /// dismisses safely, regardless of which button holds focus.
    pub fn handle_key(&mut self, key: KeyEvent) -> ConfirmOutcome {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.toggle();
                ConfirmOutcome::Toggled
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let accepted = self.is_confirm();
                self.reset_to_cancel();
                if accepted {
                    ConfirmOutcome::Accepted
                } else {
                    ConfirmOutcome::Dismissed
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if is_char_typing(key.modifiers) => {
                self.reset_to_cancel();
                ConfirmOutcome::Accepted
            }
            KeyCode::Char('n') | KeyCode::Char('N') if is_char_typing(key.modifiers) => {
                self.reset_to_cancel();
                ConfirmOutcome::Dismissed
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.reset_to_cancel();
                ConfirmOutcome::Dismissed
            }
            // C-g (keyboard-quit) dismisses every confirmation safely, exactly
            // like Esc, regardless of which button holds focus.
            KeyCode::Char('g') if is_keyboard_quit(&key) => {
                self.reset_to_cancel();
                ConfirmOutcome::Dismissed
            }
            _ => ConfirmOutcome::Toggled,
        }
    }
}

/// Exact geometry and button bounds of a confirmation modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmModalGeometry {
    pub area: Rect,
    pub confirm_button: Rect,
    pub cancel_button: Rect,
}

/// Computes the geometry and button bounds for a confirmation modal so that
/// rendering and mouse hit-testing agree on the same layout.
pub fn confirm_modal_geometry(
    term: Rect,
    width: u16,
    body_lines_count: usize,
    confirm_label: &str,
    cancel_label: &str,
) -> ConfirmModalGeometry {
    // Grow with the body so long confirmation bodies (queue previews, policy
    // notes) are never clipped: `needed_height` already reserves the borders
    // (2) and the actions bar (3). The height percentage is capped at 90 so the
    // modal keeps a small margin on short terminals while still fitting.
    let needed_height = (body_lines_count as u16).saturating_add(7).max(10);
    let height_percent = (needed_height * 100 / term.height.max(1)).clamp(25, 90);
    let area = centered_rect(width, height_percent, term);

    // Delegate to the shared modal geometry so the confirmation buttons and the
    // reusable modal frame derive their bounds from a single source of truth and
    // cannot drift (Conventions §6, §7).
    let (confirm_button, cancel_button) =
        crate::components::modal::modal_action_buttons(area, confirm_label, cancel_label);

    ConfirmModalGeometry {
        area,
        confirm_button,
        cancel_button,
    }
}

/// Presentation parameters for a confirmation modal.
pub struct ConfirmModalParams<'a> {
    pub title: &'a str,
    /// Semantic border accent of the modal frame.
    pub tone: ModalTone,
    pub body_lines: Vec<Line<'a>>,
    pub confirm_label: &'a str,
    pub cancel_label: &'a str,
    pub confirm_button: ConfirmDialogButton,
    pub destructive: bool,
    pub width: u16,
}

/// Renders a confirmation modal using the standardized three-tier frame.
pub fn render_modal_confirm(frame: &mut ratatui::Frame, params: ConfirmModalParams) {
    let geom = confirm_modal_geometry(
        frame.area(),
        params.width,
        params.body_lines.len(),
        params.confirm_label,
        params.cancel_label,
    );

    let dialog = ConfirmDialog::from_button(params.confirm_button);

    let modal = ModalFrame {
        title: params.title,
        tone: params.tone,
        area: geom.area,
        confirm_label: params.confirm_label,
        cancel_label: params.cancel_label,
        actions_state: dialog.actions(),
        is_actions_focused: true,
        destructive: params.destructive,
        // `None` lets the frame render the shared dialog vocabulary projected
        // through the configured `C-` / `M-` modifiers.
        hint_text: None,
    };

    modal.render(frame, |f, body_area| {
        let body_widget = Paragraph::new(params.body_lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false });
        f.render_widget(body_widget, body_area);
    });
}

#[cfg(test)]
mod tests {
    use super::{ConfirmDialog, ConfirmOutcome};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn defaults_to_safe_cancel() {
        let dialog = ConfirmDialog::new();
        assert!(dialog.is_cancel());
        assert!(!dialog.is_confirm());
    }

    #[test]
    fn enter_on_cancel_dismisses_without_confirming() {
        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            ConfirmOutcome::Dismissed
        );
        assert!(dialog.is_cancel());
    }

    #[test]
    fn tab_toggles_then_enter_confirms() {
        let mut dialog = ConfirmDialog::new();
        assert_eq!(dialog.handle_key(key(KeyCode::Tab)), ConfirmOutcome::Toggled);
        assert!(dialog.is_confirm());
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            ConfirmOutcome::Accepted
        );
        assert!(dialog.is_cancel());
    }

    #[test]
    fn accelerators_confirm_and_dismiss() {
        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('y'))),
            ConfirmOutcome::Accepted
        );

        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('n'))),
            ConfirmOutcome::Dismissed
        );

        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Esc)),
            ConfirmOutcome::Dismissed
        );
    }

    #[test]
    fn ctrl_g_dismisses_safely() {
        let mut dialog = ConfirmDialog::new();
        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(dialog.handle_key(ctrl_g), ConfirmOutcome::Dismissed);
        assert!(dialog.is_cancel());
    }
}
