//! Shared keyboard contract of the plan-synthesis dialog family.
//!
//! The plan creation form and the plan refinement form stack the same four
//! keyboard targets: the Planner ladder that drafts the plan, the Critic ladder
//! that audits it independently, the instructions editor, and the action bar.
//! Specifying that contract once keeps both surfaces on the same focus walk,
//! the same submit accelerator, and the same safe-cancellation vocabulary
//! (Conventions §3, §5, §6; c-typed-plan-creation-dialog,
//! c-typed-plan-mutation-dialog).

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ConfirmDialogButton};
use crate::command::dialog::{DialogCommand, MODEL_LIST_DIALOG_COMMAND_MAP};
use crate::components::model_selector::ModelSelector;
use crate::config::InputConfig;
use crate::editor::InputEditor;
use crate::views::plans::{PlanCreateDialogState, PlanFormFocus, PlanRefineDialogState};

/// Dialog state the shared plan-synthesis keyboard contract drives.
pub(super) trait PlanFormDialog {
    /// The Architect ladder that drafts the plan.
    fn planner(&mut self) -> &mut ModelSelector;
    /// The Architect ladder that audits the draft independently.
    fn critic(&mut self) -> &mut ModelSelector;
    /// Flips the independent critique phase.
    fn toggle_skip_critique(&mut self);
    /// The directives or refinement instructions editor.
    fn instructions_editor(&mut self) -> &mut InputEditor;
    /// Section that currently owns the keyboard.
    fn focus(&self) -> PlanFormFocus;
    /// Focuses one section.
    fn set_focus(&mut self, focus: PlanFormFocus);
    /// The focused action button.
    fn confirm_button(&self) -> ConfirmDialogButton;
    /// Focuses one action button.
    fn set_confirm_button(&mut self, button: ConfirmDialogButton);
}

/// What the caller must do after the shared core resolved one key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanFormKeyOutcome {
    /// The dialog consumed the key into its own state.
    Consumed,
    /// The developer asked to run the two-phase synthesis.
    Submit,
    /// The developer dismissed the dialog without a request.
    Dismiss,
    /// The instructions editor's yank chord fired; the caller runs
    /// [`hoisted_plan_form_yank`].
    Yank,
}

/// Resolves one key press against the shared plan-synthesis contract.
pub(super) fn handle_plan_form_key<D: PlanFormDialog>(
    dialog: &mut D,
    key: KeyEvent,
    input: &InputConfig,
    planner_rows: usize,
    critic_rows: usize,
) -> PlanFormKeyOutcome {
    let primary = input.primary_modifier;
    let meta = input.meta_modifier;
    let focus = dialog.focus();

    // The C-M / M-M chord always switches the model of the focused ladder: the
    // Planner while another section owns the keyboard, the Critic while its own
    // section does (Conventions §5.2).
    let switched = if focus.is_critic_list() {
        dialog.critic().handle_key(key, primary, meta, false)
    } else {
        dialog
            .planner()
            .handle_key(key, primary, meta, focus.is_instructions_editor())
    };
    if switched {
        return PlanFormKeyOutcome::Consumed;
    }

    // Bare digits, arrows, and paging act on the ladder that owns the keyboard.
    let list_handled = if focus.is_planner_list() {
        dialog.planner().handle_list_key(key, planner_rows)
    } else if focus.is_critic_list() {
        dialog.critic().handle_list_key(key, critic_rows)
    } else {
        false
    };
    if list_handled {
        return PlanFormKeyOutcome::Consumed;
    }

    let is_ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::ALT);
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Alt+digit picks a Planner by position without leaving the editor; the
    // compose chord is excluded, because it belongs to character composition.
    if focus.is_instructions_editor()
        && !is_ctrl_alt
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT)
    {
        if let KeyCode::Char(c @ '1'..='9') = key.code {
            let index = c as usize - '1' as usize;
            if index < dialog.planner().len() {
                dialog.planner().select_index(index);
                return PlanFormKeyOutcome::Consumed;
            }
        }
    }

    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));

    match MODEL_LIST_DIALOG_COMMAND_MAP
        .resolve(key, input, false)
        .map(|spec| spec.action)
    {
        Some(DialogCommand::SubmitAccelerator) => PlanFormKeyOutcome::Submit,
        Some(DialogCommand::Dismiss) if focus.is_actions() || key.code == KeyCode::Esc => {
            PlanFormKeyOutcome::Dismiss
        }
        Some(DialogCommand::ConfirmFast) if focus.is_actions() => PlanFormKeyOutcome::Submit,
        Some(DialogCommand::CancelFast) if focus.is_actions() => PlanFormKeyOutcome::Dismiss,
        // The critic section owns the switch that disables the independent
        // audit phase, so activation inside it toggles the phase instead of
        // leaving the section.
        Some(DialogCommand::Activate) if focus.is_critic_list() => {
            dialog.toggle_skip_critique();
            PlanFormKeyOutcome::Consumed
        }
        Some(DialogCommand::FocusNext) => {
            focus_next(dialog);
            PlanFormKeyOutcome::Consumed
        }
        Some(DialogCommand::FocusPrev) => {
            focus_prev(dialog);
            PlanFormKeyOutcome::Consumed
        }
        // The vertical walk moves the keyboard between the two ladders and the
        // action bar. The multiline editor keeps the chords for cursor line
        // movement, so a press inside it falls through to the editor
        // delegation below (Conventions §6.2).
        Some(DialogCommand::FieldNext) if !focus.is_instructions_editor() => {
            focus_next(dialog);
            PlanFormKeyOutcome::Consumed
        }
        Some(DialogCommand::FieldPrev) if !focus.is_instructions_editor() => {
            focus_prev(dialog);
            PlanFormKeyOutcome::Consumed
        }
        Some(DialogCommand::StepNext) | Some(DialogCommand::StepPrev) if focus.is_actions() => {
            dialog.set_confirm_button(toggle(dialog.confirm_button()));
            PlanFormKeyOutcome::Consumed
        }
        // Execution spends tokens, so it starts only from the focused button:
        // the safe `Cancel` default dismisses the dialog instead.
        Some(DialogCommand::Activate) if !has_shift && focus.is_actions() => {
            if dialog.confirm_button() == ConfirmDialogButton::Confirm {
                PlanFormKeyOutcome::Submit
            } else {
                PlanFormKeyOutcome::Dismiss
            }
        }
        _ => {
            // The yank chord is reported instead of performed, because the
            // clipboard read is a native round trip the caller hoists out of
            // the locked state; every other key is delegated to the editor.
            if focus.is_instructions_editor() {
                if dialog.instructions_editor().takes_yank_chord(key, is_cmd) {
                    return PlanFormKeyOutcome::Yank;
                }
                crate::input::handle_editor_key(dialog.instructions_editor(), key, is_cmd, true);
            }
            PlanFormKeyOutcome::Consumed
        }
    }
}

/// Advances the focus, steering the confirm-button ping-pong of the action bar.
fn focus_next<D: PlanFormDialog>(dialog: &mut D) {
    if dialog.focus().is_actions() && dialog.confirm_button() == ConfirmDialogButton::Cancel {
        dialog.set_confirm_button(ConfirmDialogButton::Confirm);
        return;
    }
    if dialog.focus().is_actions() {
        dialog.set_confirm_button(ConfirmDialogButton::Cancel);
    }
    let planner_available = dialog.planner().len() > 1;
    let mut focus = dialog.focus();
    focus.cycle_next(planner_available);
    dialog.set_focus(focus);
}

/// Moves the focus backward, steering the confirm-button ping-pong of the bar.
fn focus_prev<D: PlanFormDialog>(dialog: &mut D) {
    if dialog.focus().is_actions() && dialog.confirm_button() == ConfirmDialogButton::Confirm {
        dialog.set_confirm_button(ConfirmDialogButton::Cancel);
        return;
    }
    let planner_available = dialog.planner().len() > 1;
    let mut focus = dialog.focus();
    focus.cycle_prev(planner_available);
    dialog.set_focus(focus);
    if dialog.focus().is_actions() {
        dialog.set_confirm_button(ConfirmDialogButton::Confirm);
    }
}

/// Returns the other button of a dialog's two-action bar.
fn toggle(button: ConfirmDialogButton) -> ConfirmDialogButton {
    match button {
        ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
        ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
    }
}

impl PlanFormDialog for PlanCreateDialogState {
    fn planner(&mut self) -> &mut ModelSelector {
        &mut self.planner
    }

    fn critic(&mut self) -> &mut ModelSelector {
        &mut self.critic
    }

    fn toggle_skip_critique(&mut self) {
        self.skip_critique = !self.skip_critique;
    }

    fn instructions_editor(&mut self) -> &mut InputEditor {
        &mut self.directives_editor
    }

    fn focus(&self) -> PlanFormFocus {
        self.focus
    }

    fn set_focus(&mut self, focus: PlanFormFocus) {
        self.focus = focus;
    }

    fn confirm_button(&self) -> ConfirmDialogButton {
        self.confirm_button
    }

    fn set_confirm_button(&mut self, button: ConfirmDialogButton) {
        self.confirm_button = button;
    }
}

impl PlanFormDialog for PlanRefineDialogState {
    fn planner(&mut self) -> &mut ModelSelector {
        &mut self.planner
    }

    fn critic(&mut self) -> &mut ModelSelector {
        &mut self.critic
    }

    fn toggle_skip_critique(&mut self) {
        self.skip_critique = !self.skip_critique;
    }

    fn instructions_editor(&mut self) -> &mut InputEditor {
        &mut self.instructions_editor
    }

    fn focus(&self) -> PlanFormFocus {
        self.focus
    }

    fn set_focus(&mut self, focus: PlanFormFocus) {
        self.focus = focus;
    }

    fn confirm_button(&self) -> ConfirmDialogButton {
        self.confirm_button
    }

    fn set_confirm_button(&mut self, button: ConfirmDialogButton) {
        self.confirm_button = button;
    }
}

/// Parks the dialog, reads the system clipboard with the state guard released,
/// and applies the answer to the instructions editor.
///
/// A yank reads the system clipboard, which is a native round trip bounded at
/// `NATIVE_CLIPBOARD_TIMEOUT`. The dialog handlers run while the application
/// state guard is held, so the read is hoisted: the dialog returns to the
/// state, the guard is dropped, the clipboard is awaited, and the dialog is
/// re-taken to receive the text (c-1). `park` and `take` name the state slot
/// the dialog lives in, so every dialog of the family shares the one read.
pub(super) async fn hoisted_plan_form_yank<D, P, T>(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    dialog: D,
    park: P,
    take: T,
) where
    D: PlanFormDialog,
    P: Fn(&mut AppState, D),
    T: FnOnce(&mut AppState) -> Option<D>,
{
    park(&mut st, dialog);
    drop(st);

    let clipboard = crate::clipboard::get_from_clipboard_async().await;

    let mut st = state.lock().await;
    if let Some(mut dialog) = take(&mut st) {
        dialog.instructions_editor().yank_with(clipboard);
        park(&mut st, dialog);
    }
}
