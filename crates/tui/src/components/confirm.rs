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
use ratatui::widgets::Paragraph;

use crate::app::{ConfirmDialogButton, ModalActionsState};
use crate::command::dialog::{DialogCommand, DIALOG_COMMAND_MAP};
use crate::components::modal::{modal_action_buttons, modal_geometry, ModalFrame, ModalTone};
use crate::config::InputConfig;
use crate::ui::{centered_rect, wrap_line, wrap_lines};

/// Outcome of resolving a key press inside a confirmation dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// The focused button changed; the dialog stays open.
    Toggled,
    /// The press resolved to no command this confirmation understands, so the
    /// dialog stays open and its focus is untouched.
    ///
    /// The outcome is distinct from [`ConfirmOutcome::Toggled`], which always
    /// reports a real focus transition, so a caller can tell an unbound key
    /// from a button move. The submit accelerator of the model-list dialog
    /// family is never produced by a binary confirmation either, so that chord
    /// is reported here as well.
    Ignored,
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
    /// regardless of which button holds focus. A press that resolves to no
    /// command at all is reported as [`ConfirmOutcome::Ignored`], so a caller
    /// can always tell an actual focus transition from an unbound key.
    ///
    /// `input` is the workstation's configured keyboard mapping, exactly as the
    /// view command maps receive it: the shared chord matcher projects every
    /// character through the configured layout before it compares, so a card
    /// stays reachable from an alternative layout such as `ru_jcuken` or a
    /// custom `langmap` (Conventions §5.1). Resolving with a default
    /// configuration would silently drop those presses.
    pub fn handle_key(&mut self, key: KeyEvent, input: &InputConfig) -> ConfirmOutcome {
        // Every dialog chord is physical (`Bare`, `Shift`, or `Ctrl`), so no
        // abstract modifier is consulted during resolution; the layout
        // projection is applied by the shared chord matcher.
        let Some(command) = DIALOG_COMMAND_MAP
            .resolve(key, input, false)
            .map(|spec| spec.action)
        else {
            return ConfirmOutcome::Ignored;
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
            // The model-list submit accelerator is declared only by
            // `MODEL_LIST_DIALOG_COMMAND_MAP`, so a binary confirmation never
            // resolves it. Reporting it as `Ignored` keeps the dialog open and
            // its focus untouched, exactly like a press the map does not bind
            // (Conventions §4).
            // The vertical field walk belongs to dialogs that stack several
            // sections; a binary confirmation owns one action row, so the chord
            // reaches nothing and the dialog keeps its focus (Conventions §6.2).
            DialogCommand::FieldNext | DialogCommand::FieldPrev => ConfirmOutcome::Ignored,
            DialogCommand::SubmitAccelerator => ConfirmOutcome::Ignored,
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

/// Nominal card width that the percentage split implies before it rounds.
///
/// `width_percent` is a percentage of the terminal, exactly as [`centered_rect`]
/// reads it, so the first budgeting pass needs the column count it stands for.
fn nominal_card_width(term: Rect, width_percent: u16) -> u16 {
    let columns = u32::from(term.width) * u32::from(width_percent.min(100)) / 100;
    u16::try_from(columns).unwrap_or(u16::MAX).max(4)
}

/// Width of the band the shared modal frame hands the body of a `card_width`
/// card: the two border columns and the one-column horizontal padding on each
/// side carry no content (Conventions §6.8).
fn body_inner_width(card_width: u16) -> usize {
    usize::from(card_width.saturating_sub(4)).max(1)
}

/// Visual rows the body occupies once wrapped at `inner_width`.
///
/// A logical line wider than the band becomes several visual rows, so counting
/// logical lines under-budgets the card by exactly the rows the rendering wrap
/// adds.
fn wrapped_body_rows(body_lines: &[Line<'_>], inner_width: usize) -> usize {
    let inner_width = inner_width.max(1);
    body_lines
        .iter()
        .map(|line| {
            if line.width() <= inner_width {
                1
            } else {
                wrap_line(line.clone(), inner_width).len()
            }
        })
        .sum()
}

/// Card area for a body occupying `rows` visual rows.
///
/// `needed_height` already reserves the borders (2), the title inset, the
/// pre-action spacer, the action bar, and the docked hint strip. The height
/// percentage is capped at 90 so the modal keeps a small margin on short
/// terminals while still fitting.
///
/// The body length is model-controllable (an Architect refusal reason, a queue
/// preview), so the whole computation runs in `usize` with saturating arithmetic
/// and only then narrows back into `u16` (Conventions §4).
fn confirm_card_area(term: Rect, width_percent: u16, rows: usize) -> Rect {
    let needed_height = rows.saturating_add(7).max(10);
    let term_height = usize::from(term.height).max(1);
    let ratio = needed_height.saturating_mul(100) / term_height;
    let height_percent = u16::try_from(ratio).unwrap_or(u16::MAX).clamp(25, 90);
    centered_rect(width_percent, height_percent, term)
}

/// Computes the geometry and button bounds for a confirmation modal so that
/// rendering and mouse hit-testing agree on the same layout.
///
/// The height is budgeted from the *wrapped* body, and the renderer draws exactly
/// the lines counted here at exactly the band width counted here, so the sizing
/// model and the rendering model are one artifact instead of two independent
/// approximations (Conventions §1.3).
pub fn confirm_modal_geometry(
    term: Rect,
    width: u16,
    body_lines: &[Line<'_>],
    confirm_label: &str,
    cancel_label: &str,
) -> ConfirmModalGeometry {
    // Pass 1 budgets the card from the nominal card width the requested
    // percentage stands for.
    let nominal_inner = body_inner_width(nominal_card_width(term, width));
    let area = confirm_card_area(term, width, wrapped_body_rows(body_lines, nominal_inner));

    // Pass 2 re-budgets at the band the shared frame actually hands the body:
    // the percentage split can round the drawn card away from the nominal one.
    // Only the height can change here — the card width is a pure function of the
    // terminal width and the width percent — so one re-budget converges.
    let body_width = modal_geometry(area).body.width;
    let area = confirm_card_area(
        term,
        width,
        wrapped_body_rows(body_lines, usize::from(body_width)),
    );

    // Delegate to the shared modal geometry so the confirmation buttons and the
    // reusable modal frame derive their bounds from a single source of truth and
    // cannot drift (Conventions §6, §7).
    let (confirm_button, cancel_button) = modal_action_buttons(area, confirm_label, cancel_label);

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

    // The card is sized from the wrapped body and then drawn with the very same
    // lines at the very same band width, so `Paragraph` can never add a row the
    // budget did not reserve (Conventions §1.3, §6.7).
    let geom = confirm_modal_geometry(
        frame.area(),
        width,
        &body_lines,
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
        // The budget counted the body at exactly this band width, so drawing the
        // pre-wrapped lines needs no second wrap and the drawn rows are the
        // counted rows.
        let wrapped = wrap_lines(body_lines, usize::from(body_area.width));
        let body_widget = Paragraph::new(wrapped).alignment(Alignment::Center);
        f.render_widget(body_widget, body_area);
    });
}

#[cfg(test)]
mod tests {
    use super::{confirm_modal_geometry, ConfirmDialog, ConfirmOutcome};
    use crate::config::{InputConfig, LayoutPreset};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Canonical workstation input: `Ctrl` primary, `Alt` meta, no layout, so a
    /// physical-key assertion never depends on a developer's configuration.
    fn canonical() -> InputConfig {
        InputConfig::default()
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
            dialog.handle_key(key(KeyCode::Enter), &canonical()),
            ConfirmOutcome::Dismissed
        );
        assert!(dialog.is_cancel());
    }

    #[test]
    fn tab_toggles_then_enter_confirms() {
        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Tab), &canonical()),
            ConfirmOutcome::Toggled
        );
        assert!(dialog.is_confirm());
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter), &canonical()),
            ConfirmOutcome::Accepted
        );
        assert!(dialog.is_cancel());
    }

    /// A press the dialog understands as nothing at all is reported as
    /// `Ignored` rather than as a focus move, and it never moves the focus, so
    /// a caller can always tell an unbound key from a real transition.
    #[test]
    fn unbound_key_is_reported_as_ignored_without_moving_the_focus() {
        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('z')), &canonical()),
            ConfirmOutcome::Ignored
        );
        assert!(dialog.is_cancel(), "the safe default keeps the focus");

        // The submit accelerator belongs to the model-list dialog family, so a
        // binary confirmation never produces it and reports the chord as
        // ignored as well.
        let mut dialog = ConfirmDialog::new();
        let submit = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(
            dialog.handle_key(submit, &canonical()),
            ConfirmOutcome::Ignored
        );
        assert!(dialog.is_cancel());
    }

    #[test]
    fn accelerators_confirm_and_dismiss() {
        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('y')), &canonical()),
            ConfirmOutcome::Accepted
        );

        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('n')), &canonical()),
            ConfirmOutcome::Dismissed
        );

        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Esc), &canonical()),
            ConfirmOutcome::Dismissed
        );
    }

    /// A confirmation card resolves the same layout projection as every view
    /// command map: `н` sits on `y` and `т` on `n`, so both fast accelerators
    /// stay reachable from the JCUKEN layout (Conventions §5.1).
    #[test]
    fn ru_jcuken_layout_reaches_the_dialog_vocabulary() {
        let input = InputConfig {
            layout: LayoutPreset::RuJcuken,
            ..canonical()
        };

        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('н')), &input),
            ConfirmOutcome::Accepted
        );

        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('т')), &input),
            ConfirmOutcome::Dismissed
        );
    }

    /// A custom `langmap` widens the vocabulary through the very projection the
    /// command maps use, so the card never carries a second translation table.
    #[test]
    fn custom_langmap_reaches_the_dialog_vocabulary() {
        let input = InputConfig {
            langmap: Some("ф:y".to_string()),
            ..canonical()
        };

        let mut dialog = ConfirmDialog::new();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('ф')), &input),
            ConfirmOutcome::Accepted
        );
    }

    /// A body far longer than `u16::MAX / 100` lines is untrusted, model-sized
    /// input: the geometry must stay inside the terminal and the arithmetic must
    /// never overflow, truncate, or panic in a debug build (Conventions §4).
    #[test]
    fn large_body_geometry_stays_within_terminal() {
        let term = Rect::new(0, 0, 100, 40);
        for lines in [648usize, 656, 700, 10_000, 70_000] {
            let body: Vec<Line> = vec![Line::raw("body"); lines];
            let geom = confirm_modal_geometry(term, 70, &body, " [ Confirm ] ", " [ Cancel ] ");
            assert!(geom.area.height <= term.height, "card exceeds the terminal");
            assert!(
                geom.area.bottom() <= term.bottom(),
                "card bottom is clipped"
            );
            assert!(
                geom.area.height > term.height / 4,
                "a long body must not collapse"
            );
            assert!(geom.confirm_button.right() <= geom.area.right());
            assert!(geom.cancel_button.right() <= geom.area.right());
        }
    }

    /// The card must grow with the *wrapped* visual count, not the logical line
    /// count: the renderer draws the wrapped rows, so a logical-only budget
    /// clips exactly the rows wrapping adds (finding #42, Conventions §6.7).
    #[test]
    fn wrapped_body_grows_the_card_instead_of_clipping_it() {
        let term = Rect::new(0, 0, 80, 60);
        let logical: Vec<Line> = vec![Line::raw("short line"); 6];
        let wide: Vec<Line> = (0..6).map(|_| Line::raw("x".repeat(200))).collect();

        let compact = confirm_modal_geometry(term, 58, &logical, "Confirm", "Cancel");
        let tall = confirm_modal_geometry(term, 58, &wide, "Confirm", "Cancel");

        assert!(
            tall.area.height > compact.area.height,
            "a wrapped body must claim more rows than a logical one: {} vs {}",
            tall.area.height,
            compact.area.height
        );
        assert!(
            tall.area.bottom() <= term.bottom(),
            "card bottom is clipped"
        );
    }

    #[test]
    fn ctrl_g_dismisses_safely() {
        let mut dialog = ConfirmDialog::new();
        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(
            dialog.handle_key(ctrl_g, &canonical()),
            ConfirmOutcome::Dismissed
        );
        assert!(dialog.is_cancel());
    }
}
