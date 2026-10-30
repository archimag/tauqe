use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::{methods, ModelAskParams};
use tokio::sync::Mutex;

use crate::app::AppState;
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
    // Interface: focus, file navigation, scrolling, and editor navigation.
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
    // The inspector is a command, not a buffer edit: the same `C-v` / `M-v`
    // press opens the audit from the prompt and from the Viewport, so reaching
    // it never requires the `C-Space` / `Shift+Tab` focus switch (Conventions
    // §5.1, §5.2; c-1). The bare `v` spelling stays a Viewport-only chord,
    // because a printable character typed into the prompt is always inserted.
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('v')),
        ReplAction::OpenDiffInspector,
        Interface,
        "cmd.repl.open_diff_inspector",
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
    // The marker detail level is presentation state rather than a buffer edit,
    // so the toggle keeps the configured command modifiers and stays reachable
    // from the prompt and the Viewport alike (Conventions §5.2).
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('x')),
        ReplAction::ToggleWorkMarker,
        Interface,
        "cmd.repl.toggle_work_marker",
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

/// Viewport-exclusive chords of the Develop tab.
///
/// The bare letter `v` belongs to this map alone: the prompt editor must insert
/// every printable character unconditionally, so a bare letter can never be a
/// chord of the text-input scope (Conventions §5.1). The dedicated inspector
/// chord `C-v` / `M-v` is declared once in [`REPL_COMMAND_MAP`], because it is a
/// command chord reachable from the prompt and from the Viewport alike. `Enter`
/// on a staged file opens the same audit through the shared `Enter` binding that
/// the Viewport remaps in [`resolve_viewport_action`].
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
    /// Recenter the Viewport on the selected staged file block.
    RecenterSelectedFile,
    /// Open the fullscreen audit of the staged change set.
    OpenDiffInspector,
    /// Toggle the work marker between its compact breadcrumb and the expanded
    /// sliding step window of the queued work.
    ToggleWorkMarker,

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
/// Navigation keys act on the response and the compact staged file list; editing keys
/// fall through unchanged so the caller can return focus to the prompt. The
/// Viewport-exclusive chords are resolved first, because no text buffer owns the
/// keyboard here: the bare `v` and the `Enter` remap of a staged file both open
/// the audit, while the shared `C-v` / `M-v` chord arrives through the prompt
/// command map and therefore reaches the same action from either focus (c-1).
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
        ReplAction::OpenDiffInspector => {
            // The snapshot belongs to the view, so the prompt chord and the
            // Viewport chords open exactly the same audit (c-1).
            match st.model.inspector_snapshot() {
                Some(inspector) => st.diff_inspector = Some(inspector),
                None => st.notify_warning(active_t("notify.diff_inspector_empty")),
            }
        }
        ReplAction::RecenterSelectedFile => {
            st.model.recenter_selected_file(view_height);
        }
        ReplAction::ToggleWorkMarker => {
            // The chosen detail level belongs to the session: the per-turn reset
            // keeps it until the developer flips it again (c-4, c-5).
            st.model.work_marker_expanded = !st.model.work_marker_expanded;
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

/// Moves the Viewport selection to the previous staged file row.
///
/// The compact list renders one summary row per file, so the selection walks
/// the rows themselves and the viewport follows it; the patch body is read in
/// the diff inspector instead of being scrolled inside the canvas (c-4). At
/// the first row the same chord scrolls the response.
fn step_viewport_up(model: &mut ReplView, view_height: u16) {
    if !model.files.is_empty() && model.selected_file_index > 0 {
        model.auto_scroll = false;
        model.selected_file_index -= 1;
        model.scroll_to_selected_file(view_height);
        return;
    }
    model.auto_scroll = false;
    model.scroll = model.scroll.saturating_sub(1);
}

/// Moves the Viewport selection to the next staged file row.
///
/// The selection leaves the last file only after every row has been walked;
/// afterwards the same chord keeps scrolling the response and re-arms
/// auto-scroll at its lower edge (c-4).
fn step_viewport_down(model: &mut ReplView, view_height: u16) {
    if !model.files.is_empty() && model.selected_file_index + 1 < model.files.len() {
        model.auto_scroll = false;
        model.selected_file_index += 1;
        model.scroll_to_selected_file(view_height);
        return;
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
        // Folding left the canvas together with the inline patch bodies (c-4).
        let control_t = key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(resolve_repl_action(control_t, p), None);
        let meta_t = key(KeyCode::Char('t'), KeyModifiers::ALT);
        assert_eq!(resolve_repl_action(meta_t, p), None);

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

        // The Viewport maps only the boundary keys to navigation: `Tab` stays a
        // plain insertion now that the canvas holds no expandable patch bodies
        // (c-4).
        let tab = key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(
            resolve_viewport_action(tab, p),
            Some(ReplAction::InsertSpaces)
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
        let tab = key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(
            resolve_viewport_action(tab, p),
            Some(ReplAction::InsertSpaces)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Enter, KeyModifiers::NONE), p),
            Some(ReplAction::OpenDiffInspector)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('v'), KeyModifiers::NONE), p),
            Some(ReplAction::OpenDiffInspector)
        );
        // The focused command chord keeps sending the prompt from the Viewport;
        // folding left the canvas with the inline patch bodies (c-4).
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Enter, KeyModifiers::CONTROL), p),
            Some(ReplAction::SendPrompt)
        );
        let toggle = key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(resolve_viewport_action(toggle, p), None);
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

    /// Proves the work-marker toggle answers to the symmetric command modifiers,
    /// reaches the Viewport, and is never treated as a buffer edit (c-5).
    ///
    /// `C-x` and `M-x` name the abstract primary and meta modifiers, so each
    /// workstation configuration is resolved with the modifier pair it actually
    /// declares. Pinning the meta modifier to `Alt` would leave the meta
    /// spelling of the `Alt`-primary configuration unreachable, and the test,
    /// not the application, would fail.
    #[test]
    fn work_marker_toggle_answers_to_both_command_modifiers() {
        for (primary_mod, primary, meta) in [
            (
                PrimaryModifier::Ctrl,
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
            ),
            (
                PrimaryModifier::Alt,
                KeyModifiers::ALT,
                KeyModifiers::CONTROL,
            ),
        ] {
            let input = InputConfig {
                primary_modifier: primary_mod,
                meta_modifier: match primary_mod {
                    PrimaryModifier::Ctrl => MetaModifier::Alt,
                    PrimaryModifier::Alt => MetaModifier::Ctrl,
                },
                ..InputConfig::default()
            };

            for physical in [primary, meta] {
                let chord = key(KeyCode::Char('x'), physical);
                assert_eq!(
                    super::resolve_repl_action(chord, &input),
                    Some(ReplAction::ToggleWorkMarker),
                    "{physical:?} must reach the work-marker toggle from the prompt"
                );
                // The Viewport keeps its focus while the detail level flips,
                // because the toggle edits no text.
                assert_eq!(
                    super::resolve_viewport_action(chord, &input),
                    Some(ReplAction::ToggleWorkMarker),
                    "{physical:?} must reach the work-marker toggle from the Viewport"
                );
            }
            assert!(!is_editor_action(ReplAction::ToggleWorkMarker));
        }
    }

    /// Proves the dedicated inspector chord reaches the audit from the prompt and
    /// from the Viewport under either command-modifier assignment (c-1).
    ///
    /// The chord is a command rather than a buffer edit, so opening the audit
    /// never rewrites the prompt, and the bare `v` spelling keeps its two
    /// distinct meanings: it is inserted into the prompt and opens the audit only
    /// while the Viewport owns the keyboard.
    #[test]
    fn diff_inspector_chord_reaches_both_focuses() {
        for (primary_mod, primary, meta) in [
            (
                PrimaryModifier::Ctrl,
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
            ),
            (
                PrimaryModifier::Alt,
                KeyModifiers::ALT,
                KeyModifiers::CONTROL,
            ),
        ] {
            let input = InputConfig {
                primary_modifier: primary_mod,
                meta_modifier: match primary_mod {
                    PrimaryModifier::Ctrl => MetaModifier::Alt,
                    PrimaryModifier::Alt => MetaModifier::Ctrl,
                },
                ..InputConfig::default()
            };

            for physical in [primary, meta] {
                let chord = key(KeyCode::Char('v'), physical);
                assert_eq!(
                    super::resolve_repl_action(chord, &input),
                    Some(ReplAction::OpenDiffInspector),
                    "{physical:?} must open the audit from the prompt"
                );
                assert_eq!(
                    super::resolve_viewport_action(chord, &input),
                    Some(ReplAction::OpenDiffInspector),
                    "{physical:?} must open the audit from the Viewport"
                );
            }
            assert!(!is_editor_action(ReplAction::OpenDiffInspector));
        }

        // The bare letter keeps its two distinct meanings: it is inserted into
        // the prompt and opens the audit only while the Viewport owns the keys.
        let input = input_with(PrimaryModifier::Ctrl);
        assert_eq!(
            super::resolve_repl_action(key(KeyCode::Char('v'), KeyModifiers::NONE), &input),
            Some(ReplAction::InsertChar('v'))
        );
        assert_eq!(
            super::resolve_viewport_action(key(KeyCode::Char('v'), KeyModifiers::NONE), &input),
            Some(ReplAction::OpenDiffInspector)
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

        // `е` sits on `t`, whose folding chord left the canvas with the inline
        // patch bodies (c-4): the translated chord resolves to nothing instead
        // of typing a stray character.
        let folding = key(KeyCode::Char('е'), KeyModifiers::CONTROL);
        assert_eq!(super::resolve_repl_action(folding, &input), None);

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

    #[tokio::test]
    async fn work_marker_toggle_flips_the_detail_level_of_the_declared_work() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let chord = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);

        {
            let st = state.lock().await;
            assert!(
                !st.model.work_marker_expanded,
                "the marker opens in its compact single-row form"
            );
        }

        handle_repl_key(chord, &state, &mut conn).await.unwrap();
        assert!(
            state.lock().await.model.work_marker_expanded,
            "the chord unfolds the sliding step window"
        );

        handle_repl_key(chord, &state, &mut conn).await.unwrap();
        assert!(
            !state.lock().await.model.work_marker_expanded,
            "the same chord folds the window back into the breadcrumb"
        );
    }
}

#[cfg(test)]
mod viewport_scroll_tests {
    use super::super::state::{EditOp, FileEditStatus, StreamingFileEdit, StreamingHunk};
    use super::*;

    /// One staged file with a single accepted patch.
    ///
    /// The canvas renders exactly one summary row per file and never inlines a
    /// patch body, so the fixture keeps its hunk only for the count printed on
    /// that row (c-4).
    fn diff_file(path: &str) -> StreamingFileEdit {
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
            retry_info: None,
        }
    }

    /// One answer line, the proposed-edits header, and one summary row per file:
    ///
    /// ```text
    /// 0  answer
    /// 1  blank
    /// 2  proposed edits header
    /// 3  file 0 summary
    /// 4  file 1 summary
    /// ```
    fn viewport_model() -> ReplView {
        ReplView {
            text: "answer".to_string(),
            files: vec![
                diff_file("crates/tui/src/a.rs"),
                diff_file("crates/tui/src/b.rs"),
            ],
            ..ReplView::default()
        }
    }

    /// The canvas keeps one summary row per staged file and never paints a patch
    /// body, so a change set of any size leaves the conversation intact (c-4).
    #[test]
    fn the_canvas_never_inlines_a_patch_body() {
        let mut model = viewport_model();

        let lines = crate::views::repl::compute_model_lines(&mut model);
        assert_eq!(lines.len(), 5, "one summary row per staged file");

        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("let a = 1;"), "{rendered}");
        assert!(!rendered.contains("let a = 2;"), "{rendered}");
        assert!(rendered.contains("crates/tui/src/a.rs"), "{rendered}");
        assert!(rendered.contains("crates/tui/src/b.rs"), "{rendered}");
    }

    /// The selection walks the compact summary rows, and the patch body never
    /// participates in the layout (c-4).
    #[test]
    fn the_selection_walks_the_compact_summary_rows() {
        let mut model = viewport_model();
        let height = 3;

        model.scroll = 0;
        step_viewport_down(&mut model, height);
        assert_eq!((model.selected_file_index, model.scroll), (1, 2));
        assert!(!model.auto_scroll);

        // The last row hands the viewport back to plain scrolling, which re-arms
        // auto-scroll at the lower edge.
        step_viewport_down(&mut model, height);
        assert_eq!(model.selected_file_index, 1);
        assert_eq!(model.scroll, 2);
        assert!(model.auto_scroll);
    }

    /// Walking back up the rows leaves the response scroll to the first row.
    #[test]
    fn the_selection_steps_back_up_the_rows_before_the_response() {
        let mut model = viewport_model();
        let height = 3;

        model.selected_file_index = 1;
        model.scroll = 2;
        step_viewport_up(&mut model, height);
        assert_eq!((model.selected_file_index, model.scroll), (0, 2));
        assert!(!model.auto_scroll);

        step_viewport_up(&mut model, height);
        assert_eq!(model.selected_file_index, 0);
        assert_eq!(model.scroll, 1);
    }

    /// Proves the Develop handler carries the configured keyboard layout into
    /// chord resolution, so the Viewport answers to a Cyrillic press exactly as
    /// it answers to the Latin chord (c-1, c-2).
    #[tokio::test]
    async fn the_configured_layout_reaches_the_viewport_through_the_handler() {
        let mut st = crate::app::test_state();
        st.tui_config.input.layout = crate::config::LayoutPreset::RuJcuken;
        st.model.focus = ReplFocus::Viewport;
        st.model.files = vec![
            diff_file("crates/tui/src/a.rs"),
            diff_file("crates/tui/src/b.rs"),
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

    /// C-l re-centers the Viewport on the summary row of the selected file: the
    /// first beat reveals it and manual recentering disables auto-scroll
    /// (Conventions.md §5.2, c-4).
    #[tokio::test]
    async fn ctrl_l_recenters_the_viewport_on_the_selected_file_row() {
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
        assert_eq!(st.model.scroll, 2, "the center beat reveals the row");
        assert!(
            !st.model.auto_scroll,
            "manual recentering disables auto-scroll"
        );
    }

    /// Proves the dedicated chord opens the audit while the prompt owns the
    /// keyboard, capturing the staged set and the Viewport highlight (c-1).
    #[tokio::test]
    async fn the_inspector_chord_opens_the_audit_from_the_prompt() {
        let mut st = crate::app::test_state();
        st.model.files = vec![
            diff_file("crates/tui/src/a.rs"),
            diff_file("crates/tui/src/b.rs"),
        ];
        st.model.selected_file_index = 1;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_repl_key(
            KeyEvent::new(KeyCode::Char('v'), crossterm::event::KeyModifiers::CONTROL),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let st = state.lock().await;
        assert_eq!(
            st.model.focus,
            ReplFocus::Editor,
            "the prompt keeps the keyboard while the audit opens over it"
        );
        let inspector = st.diff_inspector.as_ref().expect("the audit must open");
        assert_eq!(inspector.files.len(), 2);
        assert_eq!(
            inspector.selected_file, 1,
            "the highlight follows the file the Viewport had selected"
        );
        assert_eq!(inspector.diff_scroll, 0);
    }
}
