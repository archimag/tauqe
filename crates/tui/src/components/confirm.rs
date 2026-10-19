//! Reusable confirmation dialog component.
//!
//! [`ConfirmDialog`] centralizes the interaction contract for every
//! `[ Confirm ] / [ Cancel ]` modal (Conventions.md §7): the focused button,
//! keyboard resolution, and the button geometry shared between rendering and
//! mouse hit-testing. The `[ Cancel ]` button always receives the default focus
//! so that destructive actions require an explicit choice.

use crossterm::event::KeyEvent;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{ConfirmDialogButton, ModalActionsState};
use crate::command::dialog::{DialogCommand, DIALOG_COMMAND_MAP};
use crate::components::modal::{ModalFrame, ModalTone};
use crate::config::InputConfig;
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
    /// The vocabulary is resolved through the shared [`DIALOG_COMMAND_MAP`],
    /// the single source of truth for the modal interaction contract, so no key
    /// is matched directly here (Conventions §3, §5). The arrow keys and
    /// `Tab`/`Shift+Tab` cycle the focused button; `Enter`/`Space` activates it;
    /// `y`/`Y` confirms immediately and `n`/`N`/`Esc`/`q` dismisses safely,
    /// regardless of which button holds focus.
    pub fn handle_key(&mut self, key: KeyEvent) -> ConfirmOutcome {
        // Every dialog chord is physical (`Bare`, `Shift`, or `Ctrl`), so no
        // abstract modifier is consulted during resolution.
        let Some(command) = DIALOG_COMMAND_MAP
            .resolve(key, &InputConfig::default(), false)
            .map(|spec| spec.action)
        else {
            return ConfirmOutcome::Toggled;
        };

        match command {
            DialogCommand::FocusNext
            | DialogCommand::FocusPrev
            | DialogCommand::StepNext
            | DialogCommand::StepPrev => {
                self.toggle();
                ConfirmOutcome::Toggled
            }
            DialogCommand::Activate => {
                let accepted = self.is_confirm();
                self.reset_to_cancel();
                if accepted {
                    ConfirmOutcome::Accepted
                } else {
                    ConfirmOutcome::Dismissed
                }
            }
            DialogCommand::ConfirmFast => {
                self.reset_to_cancel();
                ConfirmOutcome::Accepted
            }
            DialogCommand::CancelFast | DialogCommand::Dismiss => {
                self.reset_to_cancel();
                ConfirmOutcome::Dismissed
            }
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
    /// Modal title, resolved from the runtime locale (Conventions §9, c-1).
    pub title: String,
    /// Semantic border accent of the modal frame.
    pub tone: ModalTone,
    pub body_lines: Vec<Line<'a>>,
    /// Confirm button verb, resolved from the runtime locale.
    pub confirm_label: String,
    /// Cancel button verb, resolved from the runtime locale.
    pub cancel_label: String,
    pub confirm_button: ConfirmDialogButton,
    pub destructive: bool,
    pub width: u16,
}

/// Renders a confirmation modal using the standardized three-tier frame.
pub fn render_modal_confirm(frame: &mut ratatui::Frame, params: ConfirmModalParams) {
    let ConfirmModalParams {
        title,
        tone,
        body_lines,
        confirm_label,
        cancel_label,
        confirm_button,
        destructive,
        width,
    } = params;

    let geom = confirm_modal_geometry(
        frame.area(),
        width,
        body_lines.len(),
        &confirm_label,
        &cancel_label,
    );

    let dialog = ConfirmDialog::from_button(confirm_button);

    let modal = ModalFrame {
        title: &title,
        tone,
        area: geom.area,
        confirm_label: &confirm_label,
        cancel_label: &cancel_label,
        actions_state: dialog.actions(),
        is_actions_focused: true,
        destructive,
        // `None` lets the frame render the shared dialog vocabulary projected
        // through the configured `C-` / `M-` modifiers.
        hint_text: None,
    };

    modal.render(frame, |f, body_area| {
        let body_widget = Paragraph::new(body_lines)
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
        assert_eq!(
            dialog.handle_key(key(KeyCode::Tab)),
            ConfirmOutcome::Toggled
        );
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
