//! Shared keyboard contract of the model-list dialog family.
//!
//! The execution, refinement, and post-analysis dialogs embed a
//! [`ModelSelector`] above an instructions editor and an action bar, and each of
//! them used to hand-roll the same keyboard contract. The copies drifted in
//! exactly the cross-cutting rules a per-surface copy cannot keep uniform: the
//! bare-press guard of the letter accelerators, which let `Ctrl+Y` confirm a
//! dialog and launch a token-spending turn, and the `Shift+Tab` spellings the
//! shared chord machinery deliberately tolerates, which cycled the focus forward
//! instead of backward. This module is the single home of that contract
//! (Conventions §3, §5, §7).
//!
//! [`handle_dialog_key`] resolves one key end to end against the shared
//! [`MODEL_LIST_DIALOG_COMMAND_MAP`], and [`hoisted_editor_yank`] owns the one
//! system-clipboard read the instructions editor needs, so that read leaves the
//! locked state in exactly one place (c-1).

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{
    AppState, ConfirmDialogButton, ConfirmExecuteReviewState, ConfirmExecuteScopeState,
    ConfirmReviewAnalyzeState,
};
use crate::command::dialog::{DialogCommand, MODEL_LIST_DIALOG_COMMAND_MAP};
use crate::components::model_selector::ModelSelector;
use crate::config::InputConfig;
use crate::editor::InputEditor;

/// Dialog state the shared model-list keyboard contract drives.
///
/// Every dialog of the family keeps its own target, title, and launch, but they
/// all expose the same interactive pieces: the embedded model list, the shared
/// confirmation button, the focus predicates over the list, the instructions
/// editor, and the action bar, the focus walk across those sections, and the
/// optional editor that owns the text keys. Exposing them behind one trait is
/// what lets [`handle_dialog_key`] resolve a key once instead of four times.
pub(super) trait ModelListDialog {
    /// The embedded model list that the switch chord, digits, and arrows drive.
    fn selector(&mut self) -> &mut ModelSelector;

    /// The focused button of the dialog's action bar.
    fn confirm_button(&self) -> ConfirmDialogButton;

    /// Focuses one of the dialog's action buttons.
    fn set_confirm_button(&mut self, button: ConfirmDialogButton);

    /// Whether the embedded model list owns the keyboard.
    fn is_model_list_focused(&self) -> bool;

    /// Whether the instructions editor owns the keyboard.
    fn is_editor_focused(&self) -> bool;

    /// Whether the action bar owns the keyboard.
    fn is_actions_focused(&self) -> bool;

    /// Advances the focus to the next section in visual order.
    fn cycle_focus_next(&mut self);

    /// Moves the focus to the previous section in visual order.
    fn cycle_focus_prev(&mut self);

    /// The instructions editor, when the dialog carries one.
    fn instructions_editor(&mut self) -> Option<&mut InputEditor>;
}

/// What the caller must do after the shared core resolved one key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModelListKeyOutcome {
    /// The dialog consumed the key into its own state.
    Consumed,
    /// The developer asked to run the dialog's operation.
    Submit,
    /// The developer dismissed the dialog without a request.
    Dismiss,
    /// The instructions editor's yank chord fired; the caller runs
    /// [`hoisted_editor_yank`].
    Yank,
}

/// Resolves one key press against the shared model-list dialog contract.
///
/// The order is the one every dialog of the family established: the model-switch
/// chord, then the list's own navigation while it owns the keyboard, then the
/// editor's `Alt`-digit quick-select, then the shared dialog vocabulary, and
/// finally the editor delegation. Resolving the vocabulary through
/// [`MODEL_LIST_DIALOG_COMMAND_MAP`] is what keeps the letter accelerators on
/// bare presses and makes both `Shift+Tab` spellings move the focus backward, so
/// no dialog can drift from the crate's input discipline again
/// (Conventions §3, §5).
pub(super) fn handle_dialog_key<D: ModelListDialog>(
    dialog: &mut D,
    key: KeyEvent,
    input: &InputConfig,
    visible_rows: usize,
) -> ModelListKeyOutcome {
    let primary = input.primary_modifier;
    let meta = input.meta_modifier;

    // The C-M / M-M chord always switches the model; the arrows browse the list
    // only while the list owns the keyboard.
    let list_focused = dialog.is_model_list_focused();
    if dialog
        .selector()
        .handle_key(key, primary, meta, !list_focused)
    {
        return ModelListKeyOutcome::Consumed;
    }

    // Bare digits, arrows, and paging act on the model list only while it owns
    // the keyboard; elsewhere the editor keeps those keys.
    if list_focused && dialog.selector().handle_list_key(key, visible_rows) {
        return ModelListKeyOutcome::Consumed;
    }

    let is_ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::ALT);
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Alt+digit picks a model by position without leaving the editor. The
    // compose chord is excluded, because it belongs to character composition.
    if dialog.is_editor_focused()
        && !is_ctrl_alt
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT)
    {
        if let KeyCode::Char(c @ '1'..='9') = key.code {
            let index = c as usize - '1' as usize;
            if index < dialog.selector().len() {
                dialog.selector().select_index(index);
                return ModelListKeyOutcome::Consumed;
            }
        }
    }

    // The default arm owns the editor, so the editor's command flag is derived
    // once: the configured primary modifier together with the raw `Ctrl` chord,
    // with the AltGr compose chord excluded.
    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));

    match MODEL_LIST_DIALOG_COMMAND_MAP
        .resolve(key, input, false)
        .map(|spec| spec.action)
    {
        // Submission accelerates from any section, so a dialog carrying an
        // embedded list confirms without walking the focus to the action bar.
        Some(DialogCommand::SubmitAccelerator) => ModelListKeyOutcome::Submit,
        // `Esc` dismisses from every section; `q` is a dismissal only from the
        // action bar, so it stays a literal character inside the editors.
        Some(DialogCommand::Dismiss) if dismisses(dialog, key) => ModelListKeyOutcome::Dismiss,
        // Fast accelerators mirror the shared dialog vocabulary. They are
        // limited to the action bar so a bare letter stays literal text inside
        // the instructions editor (Conventions §5).
        Some(DialogCommand::ConfirmFast) if dialog.is_actions_focused() => {
            ModelListKeyOutcome::Submit
        }
        Some(DialogCommand::CancelFast) if dialog.is_actions_focused() => {
            ModelListKeyOutcome::Dismiss
        }
        Some(DialogCommand::FocusNext) => {
            focus_next(dialog);
            ModelListKeyOutcome::Consumed
        }
        Some(DialogCommand::FocusPrev) => {
            focus_prev(dialog);
            ModelListKeyOutcome::Consumed
        }
        // The vertical walk moves the keyboard between the dialog sections. A
        // multiline editor keeps the chords for cursor line movement, so a
        // press inside it falls through to the editor delegation below
        // (Conventions §6.2).
        Some(DialogCommand::FieldNext) if !dialog.is_editor_focused() => {
            dialog.cycle_focus_next();
            ModelListKeyOutcome::Consumed
        }
        Some(DialogCommand::FieldPrev) if !dialog.is_editor_focused() => {
            dialog.cycle_focus_prev();
            ModelListKeyOutcome::Consumed
        }
        // Stepping moves between the action buttons without leaving the bar.
        Some(DialogCommand::StepNext) | Some(DialogCommand::StepPrev)
            if dialog.is_actions_focused() =>
        {
            dialog.set_confirm_button(toggle(dialog.confirm_button()));
            ModelListKeyOutcome::Consumed
        }
        // Execution spends tokens (or discards typed directives), so it starts
        // only from the focused button: the safe `Cancel` default dismisses the
        // dialog instead.
        Some(DialogCommand::Activate) if !has_shift && dialog.is_actions_focused() => {
            if dialog.confirm_button() == ConfirmDialogButton::Confirm {
                ModelListKeyOutcome::Submit
            } else {
                ModelListKeyOutcome::Dismiss
            }
        }
        _ => {
            // The yank chord is reported instead of performed, because the
            // clipboard read is a native round trip the caller hoists out of the
            // locked state; every other key is delegated to the shared editor.
            if dialog.is_editor_focused() {
                if let Some(editor) = dialog.instructions_editor() {
                    if editor.takes_yank_chord(key, is_cmd) {
                        return ModelListKeyOutcome::Yank;
                    }
                    crate::input::handle_editor_key(editor, key, is_cmd, true);
                }
            }
            ModelListKeyOutcome::Consumed
        }
    }
}

/// `Esc` dismisses from every section; `q` is a dismissal only from the action
/// bar, so it stays a literal character inside the editors.
fn dismisses<D: ModelListDialog>(dialog: &D, key: KeyEvent) -> bool {
    key.code == KeyCode::Esc || dialog.is_actions_focused()
}

/// Advances the focus, steering the confirm-button ping-pong of the action bar.
fn focus_next<D: ModelListDialog>(dialog: &mut D) {
    if dialog.is_actions_focused() && dialog.confirm_button() == ConfirmDialogButton::Cancel {
        dialog.set_confirm_button(ConfirmDialogButton::Confirm);
        return;
    }
    if dialog.is_actions_focused() {
        dialog.set_confirm_button(ConfirmDialogButton::Cancel);
    }
    dialog.cycle_focus_next();
}

/// Moves the focus backward, steering the confirm-button ping-pong of the bar.
fn focus_prev<D: ModelListDialog>(dialog: &mut D) {
    if dialog.is_actions_focused() && dialog.confirm_button() == ConfirmDialogButton::Confirm {
        dialog.set_confirm_button(ConfirmDialogButton::Cancel);
        return;
    }
    dialog.cycle_focus_prev();
    if dialog.is_actions_focused() {
        dialog.set_confirm_button(ConfirmDialogButton::Confirm);
    }
}

/// Returns the other button of the dialog's two-action bar.
fn toggle(button: ConfirmDialogButton) -> ConfirmDialogButton {
    match button {
        ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
        ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
    }
}

impl ModelListDialog for ConfirmExecuteReviewState {
    fn selector(&mut self) -> &mut ModelSelector {
        &mut self.selector
    }

    fn confirm_button(&self) -> ConfirmDialogButton {
        self.confirm_button
    }

    fn set_confirm_button(&mut self, button: ConfirmDialogButton) {
        self.confirm_button = button;
    }

    fn is_model_list_focused(&self) -> bool {
        self.focus.is_model_list()
    }

    fn is_editor_focused(&self) -> bool {
        self.focus.is_instructions_editor()
    }

    fn is_actions_focused(&self) -> bool {
        self.focus.is_actions()
    }

    fn cycle_focus_next(&mut self) {
        self.focus.cycle_next(self.selector.len());
    }

    fn cycle_focus_prev(&mut self) {
        self.focus.cycle_prev(self.selector.len());
    }

    fn instructions_editor(&mut self) -> Option<&mut InputEditor> {
        Some(&mut self.instructions_editor)
    }
}

impl ModelListDialog for ConfirmExecuteScopeState {
    fn selector(&mut self) -> &mut ModelSelector {
        &mut self.selector
    }

    fn confirm_button(&self) -> ConfirmDialogButton {
        self.confirm_button
    }

    fn set_confirm_button(&mut self, button: ConfirmDialogButton) {
        self.confirm_button = button;
    }

    fn is_model_list_focused(&self) -> bool {
        self.focus.is_model_list()
    }

    /// The plan states every step's instructions itself, so the scope dialog
    /// carries no directives editor and the text keys have no destination.
    fn is_editor_focused(&self) -> bool {
        false
    }

    fn is_actions_focused(&self) -> bool {
        self.focus.is_actions()
    }

    fn cycle_focus_next(&mut self) {
        self.focus.cycle_next(self.selector.len());
    }

    fn cycle_focus_prev(&mut self) {
        self.focus.cycle_prev(self.selector.len());
    }

    fn instructions_editor(&mut self) -> Option<&mut InputEditor> {
        None
    }
}

impl ModelListDialog for ConfirmReviewAnalyzeState {
    fn selector(&mut self) -> &mut ModelSelector {
        &mut self.selector
    }

    fn confirm_button(&self) -> ConfirmDialogButton {
        self.confirm_button
    }

    fn set_confirm_button(&mut self, button: ConfirmDialogButton) {
        self.confirm_button = button;
    }

    fn is_model_list_focused(&self) -> bool {
        self.focus.is_model_list()
    }

    /// The classification dialog carries only the Architect ladder and the
    /// action bar, so no section receives text keys.
    fn is_editor_focused(&self) -> bool {
        false
    }

    fn is_actions_focused(&self) -> bool {
        self.focus.is_actions()
    }

    fn cycle_focus_next(&mut self) {
        self.focus.cycle_next(self.selector.len());
    }

    fn cycle_focus_prev(&mut self) {
        self.focus.cycle_prev(self.selector.len());
    }

    fn instructions_editor(&mut self) -> Option<&mut InputEditor> {
        None
    }
}

/// Parks the dialog, reads the system clipboard with the state guard released,
/// and applies the answer to the instructions editor.
///
/// A yank reads the system clipboard, which is a native round trip bounded at
/// `NATIVE_CLIPBOARD_TIMEOUT`. The dialog handlers run while the application
/// state guard is held, so the read is hoisted: the dialog returns to the state,
/// the guard is dropped, the clipboard is awaited, and the dialog is re-taken to
/// receive the text (c-1). `park` and `take` name the state slot the dialog
/// lives in, so every dialog of the family shares the one read.
pub(super) async fn hoisted_editor_yank<D, P, T>(
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    dialog: D,
    park: P,
    take: T,
) where
    D: ModelListDialog,
    P: Fn(&mut AppState, D),
    T: FnOnce(&mut AppState) -> Option<D>,
{
    park(&mut st, dialog);
    drop(st);

    let clipboard = crate::clipboard::get_from_clipboard_async().await;

    let mut st = state.lock().await;
    if let Some(mut dialog) = take(&mut st) {
        if let Some(editor) = dialog.instructions_editor() {
            editor.yank_with(clipboard);
        }
        park(&mut st, dialog);
    }
}
