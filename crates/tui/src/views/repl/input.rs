use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::{methods, ModelAskParams};
use tokio::sync::Mutex;

use crate::app::{AppState, DiffInspectorState};
use crate::command::CommandCategory::{Essential, Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::config::InputConfig;
use crate::i18n::engine::active_t;
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};
use crate::views::repl::{ReplFocus, ReplView, TurnStatus};

/// Time window within which a second `Esc` confirms clearing a non-empty prompt.
const ESC_CLEAR_WINDOW: Duration = Duration::from_secs(2);

pub static REPL_COMMAND_MAP: CommandMap<ReplAction> = CommandMap::new(&[
    // Essential: the costly work cycle.
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Enter),
        ReplAction::SendPrompt,
        Essential,
        "cmd.repl.send",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Enter),
        ReplAction::SendPrompt,
        Essential,
        "cmd.repl.send",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Enter),
        ReplAction::SendPrompt,
        Essential,
        "cmd.repl.send",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('s')),
        ReplAction::OpenSquashDialog,
        Essential,
        "cmd.repl.squash",
    ),
    // Interface: focus, folding, file navigation, scrolling, and editor navigation.
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Tab),
        ReplAction::ToggleFocus,
        Interface,
        "cmd.repl.toggle_focus",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::BackTab),
        ReplAction::ToggleFocus,
        Interface,
        "cmd.repl.toggle_focus",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::BackTab),
        ReplAction::ToggleFocus,
        Interface,
        "cmd.repl.toggle_focus",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char(' ')),
        ReplAction::ToggleFocus,
        Interface,
        "cmd.repl.toggle_focus",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char(' ')),
        ReplAction::ToggleFocus,
        Interface,
        "cmd.repl.toggle_focus",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char(' ')),
        ReplAction::ToggleSelectedFileFold,
        Interface,
        "cmd.repl.toggle_file_fold",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('t')),
        ReplAction::ToggleSelectedFileFold,
        Interface,
        "cmd.repl.toggle_file_fold",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('t')),
        ReplAction::ToggleSelectedFileFold,
        Interface,
        "cmd.repl.toggle_file_fold",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('t')),
        ReplAction::ToggleSelectedFileFold,
        Interface,
        "cmd.repl.toggle_file_fold",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('[')),
        ReplAction::PrevModifiedFile,
        Interface,
        "cmd.repl.prev_file",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char(']')),
        ReplAction::NextModifiedFile,
        Interface,
        "cmd.repl.next_file",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('[')),
        ReplAction::PrevModifiedFile,
        Interface,
        "cmd.repl.prev_file",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char(']')),
        ReplAction::NextModifiedFile,
        Interface,
        "cmd.repl.next_file",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('l')),
        ReplAction::RecenterSelectedFile,
        Interface,
        "cmd.repl.recenter",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('l')),
        ReplAction::RecenterSelectedFile,
        Interface,
        "cmd.repl.recenter",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Up),
        ReplAction::ScrollResponseUp,
        Interface,
        "cmd.repl.scroll_up",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Down),
        ReplAction::ScrollResponseDown,
        Interface,
        "cmd.repl.scroll_down",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Up),
        ReplAction::ScrollResponseUp,
        Interface,
        "cmd.repl.scroll_up",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Down),
        ReplAction::ScrollResponseDown,
        Interface,
        "cmd.repl.scroll_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageUp),
        ReplAction::ScrollResponsePageUp,
        Interface,
        "cmd.repl.page_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageDown),
        ReplAction::ScrollResponsePageDown,
        Interface,
        "cmd.repl.page_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        ReplAction::LineUpOrHistoryPrev,
        Interface,
        "cmd.repl.line_up_or_history_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        ReplAction::LineDownOrHistoryNext,
        Interface,
        "cmd.repl.line_down_or_history_next",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('p')),
        ReplAction::MoveLineUp,
        Interface,
        "cmd.repl.move_line_up",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('n')),
        ReplAction::MoveLineDown,
        Interface,
        "cmd.repl.move_line_down",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('p')),
        ReplAction::HistoryPrev,
        Interface,
        "cmd.repl.history_prev",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('n')),
        ReplAction::HistoryNext,
        Interface,
        "cmd.repl.history_next",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('a')),
        ReplAction::MoveBeginningOfLine,
        Interface,
        "cmd.repl.move_beginning_of_line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('a')),
        ReplAction::MoveBeginningOfLine,
        Interface,
        "cmd.repl.move_beginning_of_line",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('e')),
        ReplAction::MoveEndOfLine,
        Interface,
        "cmd.repl.move_end_of_line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('e')),
        ReplAction::MoveEndOfLine,
        Interface,
        "cmd.repl.move_end_of_line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('b')),
        ReplAction::MoveCharBackward,
        Interface,
        "cmd.repl.move_char_backward",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('f')),
        ReplAction::MoveCharForward,
        Interface,
        "cmd.repl.move_char_forward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('b')),
        ReplAction::MoveWordBackward,
        Interface,
        "cmd.repl.move_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('f')),
        ReplAction::MoveWordForward,
        Interface,
        "cmd.repl.move_word_forward",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Left),
        ReplAction::MoveWordBackward,
        Interface,
        "cmd.repl.move_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Right),
        ReplAction::MoveWordForward,
        Interface,
        "cmd.repl.move_word_forward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Left),
        ReplAction::MoveWordBackward,
        Interface,
        "cmd.repl.move_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Right),
        ReplAction::MoveWordForward,
        Interface,
        "cmd.repl.move_word_forward",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Left),
        ReplAction::MoveCharBackward,
        Interface,
        "cmd.repl.move_char_backward",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Right),
        ReplAction::MoveCharForward,
        Interface,
        "cmd.repl.move_char_forward",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Home),
        ReplAction::MoveBeginningOfLine,
        Interface,
        "cmd.repl.move_beginning_of_line",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::End),
        ReplAction::MoveEndOfLine,
        Interface,
        "cmd.repl.move_end_of_line",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('<')),
        ReplAction::MoveBufferStart,
        Interface,
        "cmd.repl.move_buffer_start",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Home),
        ReplAction::MoveBufferStart,
        Interface,
        "cmd.repl.move_buffer_start",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('>')),
        ReplAction::MoveBufferEnd,
        Interface,
        "cmd.repl.move_buffer_end",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::End),
        ReplAction::MoveBufferEnd,
        Interface,
        "cmd.repl.move_buffer_end",
    ),
    // Helpers: editing and clipboard operations.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        ReplAction::InsertSpaces,
        Helpers,
        "cmd.repl.insert_spaces",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Enter),
        ReplAction::InsertNewline,
        Helpers,
        "cmd.repl.insert_newline",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Enter),
        ReplAction::InsertNewline,
        Helpers,
        "cmd.repl.insert_newline",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('j')),
        ReplAction::InsertNewline,
        Helpers,
        "cmd.repl.insert_newline",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Backspace),
        ReplAction::DeleteBackward,
        Helpers,
        "cmd.repl.delete_backward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Backspace),
        ReplAction::KillWordBackward,
        Helpers,
        "cmd.repl.kill_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('d')),
        ReplAction::DeleteForward,
        Helpers,
        "cmd.repl.delete_forward",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Delete),
        ReplAction::DeleteForward,
        Helpers,
        "cmd.repl.delete_forward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('k')),
        ReplAction::KillLine,
        Helpers,
        "cmd.repl.kill_line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('k')),
        ReplAction::KillLine,
        Helpers,
        "cmd.repl.kill_line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('u')),
        ReplAction::KillToBeginningOfLine,
        Helpers,
        "cmd.repl.kill_to_beginning",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('w')),
        ReplAction::KillWordBackward,
        Helpers,
        "cmd.repl.kill_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('w')),
        ReplAction::KillWordBackward,
        Helpers,
        "cmd.repl.kill_word_backward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('d')),
        ReplAction::KillWordForward,
        Helpers,
        "cmd.repl.kill_word_forward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('y')),
        ReplAction::Yank,
        Helpers,
        "cmd.repl.yank",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('y')),
        ReplAction::Yank,
        Helpers,
        "cmd.repl.yank",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Insert),
        ReplAction::Yank,
        Helpers,
        "cmd.repl.yank",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('y')),
        ReplAction::CopyModelResponse,
        Helpers,
        "cmd.repl.copy_response",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('c')),
        ReplAction::CopyModelResponse,
        Helpers,
        "cmd.repl.copy_response",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('g')),
        ReplAction::ClearPromptSaving,
        Helpers,
        "cmd.repl.clear_prompt",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        ReplAction::ClearPromptSaving,
        Helpers,
        "cmd.repl.clear_prompt",
    ),
]);

/// Viewport-only chords of the Develop tab.
///
/// The bare letter `v` opens the diff inspector while the response area owns the
/// keyboard. It lives in its own map because the prompt editor must insert every
/// printable character unconditionally: a bare letter can never be a chord of
/// the text-input scope (Conventions §5.1). `Enter` on a staged file opens the
/// same inspector through the shared `Enter` binding that the Viewport remaps in
/// [`resolve_viewport_action`].
pub static VIEWPORT_COMMAND_MAP: CommandMap<ReplAction> = CommandMap::new(&[CommandSpec::new(
    KeyChord::Bare(KeyCode::Char('v')),
    ReplAction::OpenDiffInspector,
    Interface,
    "cmd.repl.open_diff_inspector",
)]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplAction {
    // Editor buffer mutations & navigation
    InsertChar(char),
    InsertNewline,
    InsertSpaces,
    DeleteBackward,
    DeleteForward,
    KillLine,
    KillToBeginningOfLine,
    KillWordBackward,
    KillWordForward,
    MoveBeginningOfLine,
    MoveEndOfLine,
    MoveBufferStart,
    MoveBufferEnd,
    MoveCharBackward,
    MoveCharForward,
    MoveWordBackward,
    MoveWordForward,
    MoveLineUp,
    MoveLineDown,
    Yank,
    ClearPromptSaving,
    HistoryPrev,
    HistoryNext,
    LineUpOrHistoryPrev,
    LineDownOrHistoryNext,

    // Workflow actions
    SendPrompt,
    OpenSquashDialog,
    CopyModelResponse,
    PrevModifiedFile,
    NextModifiedFile,
    ToggleSelectedFileFold,
    /// Recenter the Viewport on the selected staged file block.
    RecenterSelectedFile,
    /// Open the fullscreen audit of the staged change set.
    OpenDiffInspector,

    // Viewport scrolling
    ScrollResponseUp,
    ScrollResponseDown,
    ScrollResponsePageUp,
    ScrollResponsePageDown,
    ScrollResponseHome,
    ScrollResponseEnd,

    // Focus switching and Viewport navigation
    ToggleFocus,
    ViewportPrev,
    ViewportNext,
}

/// True for actions that edit or move the prompt cursor. When the Viewport has focus,
/// these actions return focus to the prompt before they are applied. The Help
/// surface reuses this predicate so the shared editor vocabulary stays on the
/// TextEditor tab instead of being duplicated in the active mode's tab.
pub fn is_editor_action(action: ReplAction) -> bool {
    matches!(
        action,
        ReplAction::InsertChar(_)
            | ReplAction::InsertNewline
            | ReplAction::InsertSpaces
            | ReplAction::DeleteBackward
            | ReplAction::DeleteForward
            | ReplAction::KillLine
            | ReplAction::KillToBeginningOfLine
            | ReplAction::KillWordBackward
            | ReplAction::KillWordForward
            | ReplAction::MoveBeginningOfLine
            | ReplAction::MoveEndOfLine
            | ReplAction::MoveBufferStart
            | ReplAction::MoveBufferEnd
            | ReplAction::MoveCharBackward
            | ReplAction::MoveCharForward
            | ReplAction::MoveWordBackward
            | ReplAction::MoveWordForward
            | ReplAction::MoveLineUp
            | ReplAction::MoveLineDown
            | ReplAction::Yank
    )
}

/// Resolves a key event while the Viewport has focus.
///
/// Navigation keys act on the response and the staged file diffs; editing keys
/// fall through unchanged so the caller can return focus to the prompt. The
/// Viewport-only chords (such as the bare `v` that opens the diff inspector) are
/// resolved first, because no text buffer owns the keyboard here.
///
/// Chords resolve through the complete workstation [`InputConfig`], so the
/// configured `layout` and `langmap` reach the same bare and modified commands
/// (Conventions §5.1). Translation applies to chord matching only: a bare letter
/// still falls through as an insertion.
pub fn resolve_viewport_action(key: KeyEvent, input: &InputConfig) -> Option<ReplAction> {
    if let Some(spec) = VIEWPORT_COMMAND_MAP.resolve(key, input, false) {
        return Some(spec.action);
    }

    let action = resolve_repl_action(key, input)?;
    Some(match action {
        ReplAction::ClearPromptSaving => ReplAction::ToggleFocus,
        ReplAction::SendPrompt
            if key.code == KeyCode::Enter && !input.is_primary(key.modifiers) =>
        {
            ReplAction::OpenDiffInspector
        }
        ReplAction::HistoryPrev | ReplAction::LineUpOrHistoryPrev | ReplAction::MoveLineUp => {
            ReplAction::ViewportPrev
        }
        ReplAction::HistoryNext | ReplAction::LineDownOrHistoryNext | ReplAction::MoveLineDown => {
            ReplAction::ViewportNext
        }
        ReplAction::InsertSpaces => ReplAction::ToggleSelectedFileFold,
        ReplAction::MoveBeginningOfLine if key.code == KeyCode::Home => {
            ReplAction::ScrollResponseHome
        }
        ReplAction::MoveEndOfLine if key.code == KeyCode::End => ReplAction::ScrollResponseEnd,
        other => other,
    })
}

/// Resolves a key event into a deterministic `ReplAction`, eliminating shortcut
/// shadowing between the primary command modifier (`C-`) and the Alt-based
/// Readline navigation.
///
/// Chords resolve through the complete workstation [`InputConfig`], so the
/// configured keyboard `layout` and `langmap` reach the same bare and modified
/// commands. Only chord matching is translated: a printable character typed
/// without a command modifier is inserted verbatim, in the language it was
/// typed in (Conventions §5.1).
pub fn resolve_repl_action(key: KeyEvent, input: &InputConfig) -> Option<ReplAction> {
    if let Some(spec) = REPL_COMMAND_MAP.resolve(key, input, false) {
        return Some(spec.action);
    }

    // Printable characters typed without a command modifier are inserted
    // unconditionally into the prompt buffer.
    if let KeyCode::Char(c) = key.code {
        if crate::input::is_char_typing(key.modifiers) {
            return Some(ReplAction::InsertChar(c));
        }
    }

    None
}

pub async fn handle_repl_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    // The whole workstation input configuration travels with the key, so the
    // configured `layout` and `langmap` reach the prompt chords and the
    // Viewport navigation alike (c-1, c-4).
    let (input, in_viewport) = {
        let st = state.lock().await;
        (
            st.tui_config.input.clone(),
            st.model.focus == ReplFocus::Viewport,
        )
    };

    let resolved = if in_viewport {
        resolve_viewport_action(key, &input)
    } else {
        resolve_repl_action(key, &input)
    };
    let action = match resolved {
        Some(act) => act,
        None => return Ok(InputResult::Continue),
    };

    let mut st = state.lock().await;
    let view_height = st.last_model_height;

    if in_viewport && is_editor_action(action) {
        st.model.focus = ReplFocus::Editor;
    }

    // Any action other than `Esc` disarms a pending two-press prompt clear.
    if action != ReplAction::ClearPromptSaving {
        st.model.esc_clear_armed = None;
    }

    match action {
        ReplAction::InsertChar(c) => {
            st.input_editor.insert_char(c);
        }
        ReplAction::InsertNewline => {
            st.input_editor.insert_char('\n');
        }
        ReplAction::InsertSpaces => {
            st.input_editor.insert_str("  ");
        }
        ReplAction::DeleteBackward => {
            st.input_editor.delete_backward();
        }
        ReplAction::DeleteForward => {
            st.input_editor.delete_forward();
        }
        ReplAction::KillLine => {
            st.input_editor.kill_line();
        }
        ReplAction::KillToBeginningOfLine => {
            st.input_editor.kill_to_beginning_of_line();
        }
        ReplAction::KillWordBackward => {
            st.input_editor.kill_word_backward();
        }
        ReplAction::KillWordForward => {
            st.input_editor.kill_word_forward();
        }
        ReplAction::MoveBeginningOfLine => {
            st.input_editor.move_beginning_of_line();
        }
        ReplAction::MoveEndOfLine => {
            st.input_editor.move_end_of_line();
        }
        ReplAction::MoveBufferStart => {
            st.input_editor.move_buffer_start();
        }
        ReplAction::MoveBufferEnd => {
            st.input_editor.move_buffer_end();
        }
        ReplAction::MoveCharBackward => {
            st.input_editor.move_backward();
        }
        ReplAction::MoveCharForward => {
            st.input_editor.move_forward();
        }
        ReplAction::MoveWordBackward => {
            st.input_editor.move_word_backward();
        }
        ReplAction::MoveWordForward => {
            st.input_editor.move_word_forward();
        }
        ReplAction::Yank => {
            st.input_editor.yank();
        }
        ReplAction::ClearPromptSaving => {
            if st.model.is_batch && st.model.is_busy() {
                st.confirm_batch_cancel = true;
                st.batch_cancel_button = crate::app::BatchCancelButton::Continue;
            } else if st.model.is_busy() {
                st.confirm_cancel = true;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            } else if !st.input_editor.is_empty() {
                let armed = st
                    .model
                    .esc_clear_armed
                    .is_some_and(|t| t.elapsed() < ESC_CLEAR_WINDOW);
                if armed {
                    st.model.esc_clear_armed = None;
                    st.input_editor.clear_saving();
                    st.notify_info(active_t("notify.prompt_cleared"));
                } else {
                    st.model.esc_clear_armed = Some(std::time::Instant::now());
                    st.notify_info(active_t("notify.prompt_clear_arm"));
                }
            } else if st.discussion_focus.is_active() {
                st.confirm_detach_focus = true;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        ReplAction::SendPrompt => {
            st.model.focus = ReplFocus::Editor;
            if let Some(prompt) = st.take_prompt() {
                drop(st);
                let params = ModelAskParams::Prompt {
                    text: prompt.clone(),
                };
                if let Err(err) = send_request(
                    server_writer,
                    methods::MODEL_ASK,
                    serde_json::to_value(params)?,
                )
                .await
                {
                    let mut st = state.lock().await;
                    st.input_editor.clear();
                    st.input_editor.insert_str(&prompt);
                    st.model.status = TurnStatus::Error;
                    st.notify_error(
                        active_t("notify.prompt_send_failed").replace("{}", &err.to_string()),
                    );
                }
            }
        }
        ReplAction::OpenSquashDialog => {
            // Opening the dialog ends with a preview request, so the guard is
            // released before the helper is awaited (c-1).
            drop(st);
            crate::input::open_squash_dialog(state, server_writer).await?;
        }
        ReplAction::CopyModelResponse => {
            if st.model.is_busy() {
                st.notify_warning(active_t("notify.copy_busy"));
            } else if st.model.text.trim().is_empty() {
                st.notify_warning(active_t("notify.copy_no_response"));
            } else {
                let text = st.model.text.clone();
                drop(st);
                let res = crate::clipboard::copy_to_clipboard(&text);
                let mut st = state.lock().await;
                match res {
                    crate::clipboard::CopyResult::Native => {
                        st.notify_success(active_t("notify.response_copied"));
                    }
                    crate::clipboard::CopyResult::Osc52Only => {
                        st.notify_info(active_t("notify.response_copied_osc52"));
                    }
                    crate::clipboard::CopyResult::Failed => {
                        st.notify_error(active_t("notify.copy_failed"));
                    }
                }
            }
        }
        ReplAction::PrevModifiedFile => {
            if !st.model.files.is_empty() {
                st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                st.model.scroll_to_selected_file(view_height);
            }
        }
        ReplAction::NextModifiedFile => {
            if !st.model.files.is_empty() && st.model.selected_file_index + 1 < st.model.files.len()
            {
                st.model.selected_file_index += 1;
                st.model.scroll_to_selected_file(view_height);
            }
        }
        ReplAction::ToggleSelectedFileFold => {
            let sel_idx = st.model.selected_file_index;
            if let Some(file) = st.model.files.get_mut(sel_idx) {
                file.expanded = !file.expanded;
                st.model.scroll_to_selected_file(view_height);
            }
        }
        ReplAction::OpenDiffInspector => {
            if st.model.files.is_empty() {
                st.notify_warning(active_t("notify.diff_inspector_empty"));
            } else {
                // The audit captures the staged set when it opens, so reading it
                // never depends on edits that are still arriving, and the
                // highlight starts on the file the Viewport had selected (c-3).
                let selected_file = st
                    .model
                    .selected_file_index
                    .min(st.model.files.len().saturating_sub(1));
                st.diff_inspector = Some(DiffInspectorState {
                    files: st.model.files.clone(),
                    selected_file,
                    diff_scroll: 0,
                });
            }
        }
        ReplAction::RecenterSelectedFile => {
            st.model.recenter_selected_file(view_height);
        }
        ReplAction::ScrollResponseUp => {
            st.model.auto_scroll = false;
            st.model.scroll = st.model.scroll.saturating_sub(1);
        }
        ReplAction::ScrollResponseDown => {
            let max = st.model.max_scroll(view_height);
            st.model.scroll = (st.model.scroll.saturating_add(1)).min(max);
            if st.model.scroll >= max {
                st.model.auto_scroll = true;
            }
        }
        ReplAction::ScrollResponsePageUp => {
            let page = view_height.saturating_sub(2).max(1);
            st.model.auto_scroll = false;
            st.model.scroll = st.model.scroll.saturating_sub(page);
        }
        ReplAction::ScrollResponsePageDown => {
            let page = view_height.saturating_sub(2).max(1);
            let max = st.model.max_scroll(view_height);
            st.model.scroll = (st.model.scroll.saturating_add(page)).min(max);
            if st.model.scroll >= max {
                st.model.auto_scroll = true;
            }
        }
        ReplAction::ScrollResponseHome => {
            st.model.auto_scroll = false;
            st.model.scroll = 0;
        }
        ReplAction::ScrollResponseEnd => {
            st.model.auto_scroll = true;
            st.model.scroll = st.model.max_scroll(view_height);
        }
        ReplAction::MoveLineUp => {
            st.input_editor.move_line_up();
        }
        ReplAction::MoveLineDown => {
            st.input_editor.move_line_down();
        }
        ReplAction::HistoryPrev => {
            st.input_editor.history_prev();
        }
        ReplAction::HistoryNext => {
            st.input_editor.history_next();
        }
        ReplAction::LineUpOrHistoryPrev => {
            if !st.input_editor.move_line_up() {
                st.input_editor.history_prev();
            }
        }
        ReplAction::LineDownOrHistoryNext => {
            if !st.input_editor.move_line_down() {
                st.input_editor.history_next();
            }
        }
        ReplAction::ToggleFocus => {
            st.model.focus = match st.model.focus {
                ReplFocus::Editor => ReplFocus::Viewport,
                ReplFocus::Viewport => ReplFocus::Editor,
            };
        }
        ReplAction::ViewportPrev => {
            step_viewport_up(&mut st.model, view_height);
        }
        ReplAction::ViewportNext => {
            step_viewport_down(&mut st.model, view_height);
        }
    }

    Ok(InputResult::Continue)
}

/// Moves the Viewport one line up.
///
/// While the selected file is expanded the movement stays inside that file's
/// block; the selection leaves it only after the block start has been reached,
/// so a long patch is read backward line by line instead of being skipped by a
/// single keystroke (c-2).
fn step_viewport_up(model: &mut ReplView, view_height: u16) {
    if !model.files.is_empty() {
        let selected = model.selected_file_index;
        let expanded = model.files.get(selected).is_some_and(|file| file.expanded);
        let window = if expanded {
            model.file_scroll_window(selected, view_height)
        } else {
            None
        };
        if window.is_some_and(|(block_start, _)| model.scroll > block_start) {
            model.auto_scroll = false;
            model.scroll = model.scroll.saturating_sub(1);
            return;
        }
        if selected > 0 {
            model.auto_scroll = false;
            model.selected_file_index = selected - 1;
            let previous = model.file_scroll_window(selected - 1, view_height);
            if let Some((_, block_end)) = previous {
                model.scroll = block_end;
            }
            model.clamp_scroll(view_height);
            return;
        }
    }
    model.auto_scroll = false;
    model.scroll = model.scroll.saturating_sub(1);
}

/// Moves the Viewport one line down.
///
/// Inside an expanded file the movement scrolls the diff body until its last
/// line reaches the bottom edge; only then does the selection advance to the
/// next file, which keeps the reading position stable for long patches (c-2).
fn step_viewport_down(model: &mut ReplView, view_height: u16) {
    if !model.files.is_empty() {
        let selected = model.selected_file_index;
        let expanded = model.files.get(selected).is_some_and(|file| file.expanded);
        let window = if expanded {
            model.file_scroll_window(selected, view_height)
        } else {
            None
        };
        if window.is_some_and(|(_, block_end)| model.scroll < block_end) {
            model.auto_scroll = false;
            model.scroll = model.scroll.saturating_add(1);
            return;
        }
        if selected + 1 < model.files.len() {
            model.auto_scroll = false;
            model.selected_file_index = selected + 1;
            let next = model.file_scroll_window(selected + 1, view_height);
            if let Some((block_start, _)) = next {
                model.scroll = block_start;
            }
            model.clamp_scroll(view_height);
            return;
        }
    }
    model.auto_scroll = false;
    let max = model.max_scroll(view_height);
    model.scroll = model.scroll.saturating_add(1).min(max);
    if model.scroll >= max {
        model.auto_scroll = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    use crate::config::{LayoutPreset, MetaModifier, PrimaryModifier};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// Workstation input configuration with a fixed `C-` / `M-` assignment and
    /// the default layout.
    fn input_with(primary_mod: PrimaryModifier) -> InputConfig {
        InputConfig {
            primary_modifier: primary_mod,
            meta_modifier: MetaModifier::Alt,
            ..InputConfig::default()
        }
    }

    fn resolve_repl_action(key: KeyEvent, primary_mod: PrimaryModifier) -> Option<ReplAction> {
        super::resolve_repl_action(key, &input_with(primary_mod))
    }

    fn resolve_viewport_action(key: KeyEvent, primary_mod: PrimaryModifier) -> Option<ReplAction> {
        super::resolve_viewport_action(key, &input_with(primary_mod))
    }

    #[test]
    fn test_resolve_repl_action_ctrl_primary() {
        let p = PrimaryModifier::Ctrl;

        // Copy response
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('c'), KeyModifiers::ALT), p),
            Some(ReplAction::CopyModelResponse)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('y'), KeyModifiers::ALT), p),
            Some(ReplAction::CopyModelResponse)
        );

        // C-z belongs to the text editor: it never opens an undo dialog.
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('z'), KeyModifiers::CONTROL), p),
            None
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('u'), KeyModifiers::ALT), p),
            None
        );

        // Yank via C-y and Shift+Insert
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('y'), KeyModifiers::CONTROL), p),
            Some(ReplAction::Yank)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Insert, KeyModifiers::SHIFT), p),
            Some(ReplAction::Yank)
        );

        // Line navigation, prompt history and diff folding
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveLineUp)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveLineDown)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('p'), KeyModifiers::ALT), p),
            Some(ReplAction::HistoryPrev)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('n'), KeyModifiers::ALT), p),
            Some(ReplAction::HistoryNext)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('t'), KeyModifiers::CONTROL), p),
            Some(ReplAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('t'), KeyModifiers::ALT), p),
            Some(ReplAction::ToggleSelectedFileFold)
        );

        // Arrows always move between prompt lines, independent of buffer content
        assert_eq!(
            resolve_repl_action(key(KeyCode::Up, KeyModifiers::NONE), p),
            Some(ReplAction::LineUpOrHistoryPrev)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Down, KeyModifiers::NONE), p),
            Some(ReplAction::LineDownOrHistoryNext)
        );

        // Readline text editing
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('u'), KeyModifiers::CONTROL), p),
            Some(ReplAction::KillToBeginningOfLine)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('b'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveCharBackward)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('b'), KeyModifiers::ALT), p),
            Some(ReplAction::MoveWordBackward)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('f'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveCharForward)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('f'), KeyModifiers::ALT), p),
            Some(ReplAction::MoveWordForward)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('d'), KeyModifiers::CONTROL), p),
            Some(ReplAction::DeleteForward)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('d'), KeyModifiers::ALT), p),
            Some(ReplAction::KillWordForward)
        );
    }

    #[test]
    fn test_resolve_repl_action_alt_primary() {
        let p = PrimaryModifier::Alt;

        // Alt+C copies response without shadowing
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('c'), KeyModifiers::ALT), p),
            Some(ReplAction::CopyModelResponse)
        );

        // The undo chord is gone: neither Alt+U nor Alt+Z resolve to an action.
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('u'), KeyModifiers::ALT), p),
            None
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('z'), KeyModifiers::ALT), p),
            None
        );

        // Ctrl+U still kills to beginning of line (Readline compatibility)
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('u'), KeyModifiers::CONTROL), p),
            Some(ReplAction::KillToBeginningOfLine)
        );

        // Alt+B, Alt+F, Alt+D retain word semantics
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('b'), KeyModifiers::ALT), p),
            Some(ReplAction::MoveWordBackward)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('f'), KeyModifiers::ALT), p),
            Some(ReplAction::MoveWordForward)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('d'), KeyModifiers::ALT), p),
            Some(ReplAction::KillWordForward)
        );

        // Ctrl+B, Ctrl+F, Ctrl+D retain char semantics
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('b'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveCharBackward)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('f'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveCharForward)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('d'), KeyModifiers::CONTROL), p),
            Some(ReplAction::DeleteForward)
        );

        // Alt+P / Alt+N trigger HistoryPrev / HistoryNext; Ctrl+P / Ctrl+N move lines
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('p'), KeyModifiers::ALT), p),
            Some(ReplAction::HistoryPrev)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('n'), KeyModifiers::ALT), p),
            Some(ReplAction::HistoryNext)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveLineUp)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveLineDown)
        );
    }

    #[test]
    fn test_editor_keys_do_not_depend_on_buffer_content() {
        let p = PrimaryModifier::Ctrl;

        // Tab, Home and End keep their editing meaning regardless of buffer content.
        assert_eq!(
            resolve_repl_action(key(KeyCode::Tab, KeyModifiers::NONE), p),
            Some(ReplAction::InsertSpaces)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Home, KeyModifiers::NONE), p),
            Some(ReplAction::MoveBeginningOfLine)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::End, KeyModifiers::NONE), p),
            Some(ReplAction::MoveEndOfLine)
        );

        // The Viewport is the sole place that maps those keys to navigation.
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Tab, KeyModifiers::NONE), p),
            Some(ReplAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Home, KeyModifiers::NONE), p),
            Some(ReplAction::ScrollResponseHome)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::End, KeyModifiers::NONE), p),
            Some(ReplAction::ScrollResponseEnd)
        );

        // C-a / C-e remain editing commands even in the Viewport.
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('a'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveBeginningOfLine)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('e'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveEndOfLine)
        );
    }

    #[test]
    fn test_focus_toggle_and_viewport_navigation() {
        let p = PrimaryModifier::Ctrl;

        // Focus toggle works from the prompt
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char(' '), KeyModifiers::CONTROL), p),
            Some(ReplAction::ToggleFocus)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::BackTab, KeyModifiers::SHIFT), p),
            Some(ReplAction::ToggleFocus)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Esc, KeyModifiers::NONE), p),
            Some(ReplAction::ClearPromptSaving)
        );

        // Viewport navigation
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(ReplAction::ViewportNext)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL), p),
            Some(ReplAction::ViewportPrev)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Down, KeyModifiers::NONE), p),
            Some(ReplAction::ViewportNext)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Up, KeyModifiers::NONE), p),
            Some(ReplAction::ViewportPrev)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Tab, KeyModifiers::NONE), p),
            Some(ReplAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Enter, KeyModifiers::NONE), p),
            Some(ReplAction::OpenDiffInspector)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('v'), KeyModifiers::NONE), p),
            Some(ReplAction::OpenDiffInspector)
        );
        // The focused command chord keeps sending the prompt from the Viewport
        // and leaves folding to `Tab` / `C-t`.
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Enter, KeyModifiers::CONTROL), p),
            Some(ReplAction::SendPrompt)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('t'), KeyModifiers::CONTROL), p),
            Some(ReplAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Esc, KeyModifiers::NONE), p),
            Some(ReplAction::ToggleFocus)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Enter, KeyModifiers::CONTROL), p),
            Some(ReplAction::SendPrompt)
        );

        // Typing (including Space) is an editor action that returns focus to the prompt
        let typed =
            resolve_viewport_action(key(KeyCode::Char('x'), KeyModifiers::NONE), p).unwrap();
        assert_eq!(typed, ReplAction::InsertChar('x'));
        assert!(is_editor_action(typed));
        let space =
            resolve_viewport_action(key(KeyCode::Char(' '), KeyModifiers::NONE), p).unwrap();
        assert_eq!(space, ReplAction::InsertChar(' '));
        assert!(is_editor_action(space));
    }

    #[test]
    fn recenter_chord_reaches_the_file_block_from_the_prompt_and_the_viewport() {
        let p = PrimaryModifier::Ctrl;

        // C-l is a structural chord rather than text editing: it reaches the
        // file recenter from the prompt, and the Viewport keeps it unchanged.
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('l'), KeyModifiers::CONTROL), p),
            Some(ReplAction::RecenterSelectedFile)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('l'), KeyModifiers::CONTROL), p),
            Some(ReplAction::RecenterSelectedFile)
        );

        // With Alt as the primary modifier the global locale picker owns
        // M-l at the dispatcher level, so the raw Ctrl spelling keeps the
        // command reachable, mirroring the History, Plans, and Review maps.
        assert_eq!(
            resolve_repl_action(
                key(KeyCode::Char('l'), KeyModifiers::CONTROL),
                PrimaryModifier::Alt
            ),
            Some(ReplAction::RecenterSelectedFile)
        );
    }

    #[test]
    fn newline_insertion_is_reserved_to_dedicated_chords() {
        let p = PrimaryModifier::Ctrl;

        // C-j is the electric-newline; newline insertion is reserved for Enter,
        // Shift+Enter, Alt+Enter and C-j only.
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('j'), KeyModifiers::CONTROL), p),
            Some(ReplAction::InsertNewline)
        );

        // C-p / C-n navigate symmetrically and never insert a newline.
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveLineUp)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(ReplAction::MoveLineDown)
        );
        assert_ne!(
            resolve_repl_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(ReplAction::InsertNewline)
        );
    }

    #[test]
    fn test_command_map_chords_are_unique() {
        let chords: Vec<KeyChord> = REPL_COMMAND_MAP
            .specs()
            .iter()
            .map(|spec| spec.chord)
            .collect();
        assert!(!chords.is_empty());
        for (index, chord) in chords.iter().enumerate() {
            for other in chords.iter().skip(index + 1) {
                assert_ne!(chord, other, "duplicate key chord in REPL_COMMAND_MAP");
            }
        }
    }

    /// Proves the Develop resolver honours the configured keyboard layout for
    /// both the prompt chords and the Viewport navigation, while a typed
    /// character keeps its original form (c-3, c-4).
    #[test]
    fn cyrillic_layout_reaches_the_viewport_chords_and_types_verbatim() {
        let input = InputConfig {
            layout: LayoutPreset::RuJcuken,
            ..input_with(PrimaryModifier::Ctrl)
        };

        // `м` sits on `v`: the Viewport-only inspector chord answers to it.
        assert_eq!(
            super::resolve_viewport_action(key(KeyCode::Char('м'), KeyModifiers::NONE), &input),
            Some(ReplAction::OpenDiffInspector)
        );

        // `т` and `з` sit on `n` and `p`: the navigation chords follow the
        // configured layout instead of demanding a Latin key.
        assert_eq!(
            super::resolve_viewport_action(key(KeyCode::Char('т'), KeyModifiers::CONTROL), &input),
            Some(ReplAction::ViewportNext)
        );
        assert_eq!(
            super::resolve_viewport_action(key(KeyCode::Char('з'), KeyModifiers::CONTROL), &input),
            Some(ReplAction::ViewportPrev)
        );

        // `е` sits on `t`: the modified folding chord follows it too.
        assert_eq!(
            super::resolve_repl_action(key(KeyCode::Char('е'), KeyModifiers::CONTROL), &input),
            Some(ReplAction::ToggleSelectedFileFold)
        );

        // A printable character typed into the prompt is inserted verbatim, in
        // the language it was typed in, and never translated (Conventions §5.1).
        let typed = super::resolve_repl_action(key(KeyCode::Char('ч'), KeyModifiers::NONE), &input)
            .expect("a printable character always resolves to insertion");
        assert_eq!(typed, ReplAction::InsertChar('ч'));
        assert!(is_editor_action(typed));
    }

    #[tokio::test]
    async fn empty_prompt_esc_arms_detach_confirmation_for_active_focus() {
        let mut st = crate::app::test_state();
        st.discussion_focus =
            tauqe_protocol::DiscussionFocus::plan("plan-1".to_string(), vec!["s-1".to_string()]);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_repl_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(
            st.confirm_detach_focus,
            "Esc on an empty prompt with an active focus must arm the detach confirmation"
        );
        assert_eq!(st.confirm_button, crate::app::ConfirmDialogButton::Cancel);
    }

    #[tokio::test]
    async fn test_esc_during_batch_execution_opens_batch_cancel_dialog() {
        let mut st = crate::app::test_state();
        st.model.status = TurnStatus::Responding;
        st.model.is_batch = true;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_repl_key(key, &state, &mut conn).await.unwrap();

        let guard = state.lock().await;
        assert!(
            guard.confirm_batch_cancel,
            "Esc during batch must open three-way batch cancel dialog"
        );
        assert!(
            !guard.confirm_cancel,
            "Esc during batch must not open binary hard cancel dialog"
        );
        assert_eq!(
            guard.batch_cancel_button,
            crate::app::BatchCancelButton::Continue
        );
    }

    #[tokio::test]
    async fn test_esc_during_single_turn_opens_hard_cancel_dialog() {
        let mut st = crate::app::test_state();
        st.model.status = TurnStatus::Responding;
        st.model.is_batch = false;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_repl_key(key, &state, &mut conn).await.unwrap();

        let guard = state.lock().await;
        assert!(!guard.confirm_batch_cancel);
        assert!(guard.confirm_cancel);
        assert_eq!(
            guard.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
    }

    #[tokio::test]
    async fn empty_prompt_esc_is_inert_without_active_focus() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_repl_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(!st.confirm_detach_focus);
        assert!(!st.confirm_cancel);
    }
}

#[cfg(test)]
mod viewport_scroll_tests {
    use super::super::state::{EditOp, FileEditStatus, StreamingFileEdit, StreamingHunk};
    use super::*;

    fn diff_file(path: &str, expanded: bool) -> StreamingFileEdit {
        StreamingFileEdit {
            path: path.to_string(),
            op_type: EditOp::Replace,
            status: FileEditStatus::Ok,
            error: None,
            hunks: vec![StreamingHunk {
                hunk_index: 0,
                old_text: "let a = 1;".to_string(),
                new_text: "let a = 2;".to_string(),
            }],
            expanded,
            retry_info: None,
        }
    }

    /// One answer line, the proposed-edits header, an expanded first file with
    /// a three-line diff body and a collapsed second file:
    ///
    /// ```text
    /// 0  answer
    /// 1  blank
    /// 2  proposed edits header
    /// 3  file 0 summary
    /// 4  @@ diff @@
    /// 5  - let a = 1;
    /// 6  + let a = 2;
    /// 7  file 1 summary
    /// ```
    fn viewport_model() -> ReplView {
        ReplView {
            text: "answer".to_string(),
            files: vec![
                diff_file("crates/tui/src/a.rs", true),
                diff_file("crates/tui/src/b.rs", false),
            ],
            ..ReplView::default()
        }
    }

    #[test]
    fn the_selection_leaves_an_expanded_file_only_after_its_last_diff_line() {
        let mut model = viewport_model();
        let height = 3;

        model.scroll = 3;
        step_viewport_down(&mut model, height);
        assert_eq!((model.selected_file_index, model.scroll), (0, 4));

        step_viewport_down(&mut model, height);
        assert_eq!(model.selected_file_index, 1);
        assert_eq!(model.scroll, 5);
    }

    #[test]
    fn the_selection_returns_into_the_previous_diff_body_line_by_line() {
        let mut model = viewport_model();
        let height = 3;

        model.selected_file_index = 1;
        model.scroll = 5;

        step_viewport_up(&mut model, height);
        assert_eq!((model.selected_file_index, model.scroll), (0, 4));

        step_viewport_up(&mut model, height);
        assert_eq!((model.selected_file_index, model.scroll), (0, 3));

        step_viewport_up(&mut model, height);
        assert_eq!(model.scroll, 2);
    }

    #[test]
    fn the_last_file_hands_the_viewport_back_to_plain_scrolling() {
        let mut model = viewport_model();
        let height = 3;

        model.selected_file_index = 1;
        model.scroll = 4;

        step_viewport_down(&mut model, height);
        assert_eq!(model.selected_file_index, 1);
        assert_eq!(model.scroll, 5);
    }
    /// Proves the Develop handler carries the configured keyboard layout into
    /// chord resolution, so the Viewport answers to a Cyrillic press exactly as
    /// it answers to the Latin chord (c-1, c-4).
    #[tokio::test]
    async fn the_configured_layout_reaches_the_viewport_through_the_handler() {
        let mut st = crate::app::test_state();
        st.tui_config.input.layout = crate::config::LayoutPreset::RuJcuken;
        st.model.focus = ReplFocus::Viewport;
        st.model.files = vec![
            diff_file("crates/tui/src/a.rs", false),
            diff_file("crates/tui/src/b.rs", false),
        ];
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // `м` sits on `v`: the Viewport-only inspector chord must answer to the
        // configured layout instead of typing a character.
        handle_repl_key(
            KeyEvent::new(KeyCode::Char('м'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        assert!(
            state.lock().await.diff_inspector.is_some(),
            "the translated `v` chord must open the diff inspector"
        );
    }

    /// C-l re-centers the Viewport on the expanded diff of the selected
    /// file: the first beat aligns the top of the tall block, and manual
    /// recentering disables auto-scroll (Conventions.md §5.2).
    #[tokio::test]
    async fn ctrl_l_recenters_the_viewport_on_the_selected_file_block() {
        let mut st = crate::app::test_state();
        st.model = viewport_model();
        st.model.focus = ReplFocus::Viewport;
        st.last_model_height = 3;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_repl_key(
            KeyEvent::new(KeyCode::Char('l'), crossterm::event::KeyModifiers::CONTROL),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let st = state.lock().await;
        assert_eq!(
            st.model.scroll, 3,
            "the center beat aligns the tall block's top"
        );
        assert!(
            !st.model.auto_scroll,
            "manual recentering disables auto-scroll"
        );
    }
}
