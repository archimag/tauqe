use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::Mutex;
use tauqe_protocol::{methods, ModelAskParams};

use crate::app::AppState;
use crate::command::CommandCategory::{Essential, Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::config::{InputConfig, MetaModifier, PrimaryModifier};
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
        "Send the prompt",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Enter),
        ReplAction::SendPrompt,
        Essential,
        "Send the prompt",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Enter),
        ReplAction::SendPrompt,
        Essential,
        "Send the prompt",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('u')),
        ReplAction::UndoCommit,
        Essential,
        "Undo the last AI commit",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('z')),
        ReplAction::UndoCommit,
        Essential,
        "Undo the last AI commit",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('z')),
        ReplAction::UndoCommit,
        Essential,
        "Undo the last AI commit",
    ),
    CommandSpec::new(
        KeyChord::PrimaryOrMeta(KeyCode::Char('s')),
        ReplAction::OpenSquashDialog,
        Essential,
        "Open the squash AI commits dialog",
    ),
    // Interface: focus, folding, file navigation, scrolling, and editor navigation.
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Tab),
        ReplAction::ToggleFocus,
        Interface,
        "Switch focus between the prompt and the Viewport",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::BackTab),
        ReplAction::ToggleFocus,
        Interface,
        "Switch focus between the prompt and the Viewport",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::BackTab),
        ReplAction::ToggleFocus,
        Interface,
        "Switch focus between the prompt and the Viewport",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char(' ')),
        ReplAction::ToggleFocus,
        Interface,
        "Switch focus between the prompt and the Viewport",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char(' ')),
        ReplAction::ToggleFocus,
        Interface,
        "Switch focus between the prompt and the Viewport",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char(' ')),
        ReplAction::ToggleSelectedFileFold,
        Interface,
        "Fold or unfold the selected file diff",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('t')),
        ReplAction::ToggleSelectedFileFold,
        Interface,
        "Fold or unfold the selected file diff",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('t')),
        ReplAction::ToggleSelectedFileFold,
        Interface,
        "Fold or unfold the selected file diff",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('t')),
        ReplAction::ToggleSelectedFileFold,
        Interface,
        "Fold or unfold the selected file diff",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('[')),
        ReplAction::PrevModifiedFile,
        Interface,
        "Select the previous modified file",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char(']')),
        ReplAction::NextModifiedFile,
        Interface,
        "Select the next modified file",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('[')),
        ReplAction::PrevModifiedFile,
        Interface,
        "Select the previous modified file",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char(']')),
        ReplAction::NextModifiedFile,
        Interface,
        "Select the next modified file",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Up),
        ReplAction::ScrollResponseUp,
        Interface,
        "Scroll the response up",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Down),
        ReplAction::ScrollResponseDown,
        Interface,
        "Scroll the response down",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Up),
        ReplAction::ScrollResponseUp,
        Interface,
        "Scroll the response up",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Down),
        ReplAction::ScrollResponseDown,
        Interface,
        "Scroll the response down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageUp),
        ReplAction::ScrollResponsePageUp,
        Interface,
        "Scroll the response up one page",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageDown),
        ReplAction::ScrollResponsePageDown,
        Interface,
        "Scroll the response down one page",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        ReplAction::LineUpOrHistoryPrev,
        Interface,
        "Move the cursor up one line; navigate history at the first line",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        ReplAction::LineDownOrHistoryNext,
        Interface,
        "Move the cursor down one line; navigate history at the last line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('p')),
        ReplAction::MoveLineUp,
        Interface,
        "Move the cursor up one line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('n')),
        ReplAction::MoveLineDown,
        Interface,
        "Move the cursor down one line",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('p')),
        ReplAction::HistoryPrev,
        Interface,
        "Navigate the prompt history backward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('n')),
        ReplAction::HistoryNext,
        Interface,
        "Navigate the prompt history forward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('a')),
        ReplAction::MoveBeginningOfLine,
        Interface,
        "Move the cursor to the beginning of the line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('a')),
        ReplAction::MoveBeginningOfLine,
        Interface,
        "Move the cursor to the beginning of the line",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('e')),
        ReplAction::MoveEndOfLine,
        Interface,
        "Move the cursor to the end of the line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('e')),
        ReplAction::MoveEndOfLine,
        Interface,
        "Move the cursor to the end of the line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('b')),
        ReplAction::MoveCharBackward,
        Interface,
        "Move the cursor one character backward",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('f')),
        ReplAction::MoveCharForward,
        Interface,
        "Move the cursor one character forward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('b')),
        ReplAction::MoveWordBackward,
        Interface,
        "Move the cursor one word backward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('f')),
        ReplAction::MoveWordForward,
        Interface,
        "Move the cursor one word forward",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Left),
        ReplAction::MoveWordBackward,
        Interface,
        "Move the cursor one word backward",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Right),
        ReplAction::MoveWordForward,
        Interface,
        "Move the cursor one word forward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Left),
        ReplAction::MoveWordBackward,
        Interface,
        "Move the cursor one word backward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Right),
        ReplAction::MoveWordForward,
        Interface,
        "Move the cursor one word forward",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Left),
        ReplAction::MoveCharBackward,
        Interface,
        "Move the cursor one character backward",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Right),
        ReplAction::MoveCharForward,
        Interface,
        "Move the cursor one character forward",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Home),
        ReplAction::MoveBeginningOfLine,
        Interface,
        "Move the cursor to the beginning of the line",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::End),
        ReplAction::MoveEndOfLine,
        Interface,
        "Move the cursor to the end of the line",
    ),
    // Helpers: editing and clipboard operations.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        ReplAction::InsertSpaces,
        Helpers,
        "Insert two spaces",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Enter),
        ReplAction::InsertNewline,
        Helpers,
        "Insert a newline",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Enter),
        ReplAction::InsertNewline,
        Helpers,
        "Insert a newline",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('j')),
        ReplAction::InsertNewline,
        Helpers,
        "Insert a newline",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Backspace),
        ReplAction::DeleteBackward,
        Helpers,
        "Delete the character backward",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Backspace),
        ReplAction::KillWordBackward,
        Helpers,
        "Kill the word before the cursor",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('d')),
        ReplAction::DeleteForward,
        Helpers,
        "Delete the character forward",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Delete),
        ReplAction::DeleteForward,
        Helpers,
        "Delete the character forward",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('k')),
        ReplAction::KillLine,
        Helpers,
        "Kill to the end of the line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('k')),
        ReplAction::KillLine,
        Helpers,
        "Kill to the end of the line",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('u')),
        ReplAction::KillToBeginningOfLine,
        Helpers,
        "Kill to the beginning of the line",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('w')),
        ReplAction::KillWordBackward,
        Helpers,
        "Kill the word before the cursor",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('w')),
        ReplAction::KillWordBackward,
        Helpers,
        "Kill the word before the cursor",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('d')),
        ReplAction::KillWordForward,
        Helpers,
        "Kill the word after the cursor",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('y')),
        ReplAction::Yank,
        Helpers,
        "Yank from the kill ring or the clipboard",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('y')),
        ReplAction::Yank,
        Helpers,
        "Yank from the kill ring or the clipboard",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Insert),
        ReplAction::Yank,
        Helpers,
        "Yank from the kill ring or the clipboard",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('y')),
        ReplAction::CopyModelResponse,
        Helpers,
        "Copy the model response to the clipboard",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('c')),
        ReplAction::CopyModelResponse,
        Helpers,
        "Copy the model response to the clipboard",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('g')),
        ReplAction::ClearPromptSaving,
        Helpers,
        "Interrupt, or clear the prompt (saving it to the kill ring)",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        ReplAction::ClearPromptSaving,
        Helpers,
        "Interrupt, or clear the prompt (saving it to the kill ring)",
    ),
]);

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
    UndoCommit,
    OpenSquashDialog,
    CopyModelResponse,
    PrevModifiedFile,
    NextModifiedFile,
    ToggleSelectedFileFold,

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
            | ReplAction::MoveCharBackward
            | ReplAction::MoveCharForward
            | ReplAction::MoveWordBackward
            | ReplAction::MoveWordForward
            | ReplAction::MoveLineUp
            | ReplAction::MoveLineDown
            | ReplAction::Yank
    )
}

/// Resolves a key event while the Viewport has focus. Navigation keys act on the
/// response and file diffs; editing keys fall through unchanged so the caller can
/// return focus to the prompt.
pub fn resolve_viewport_action(
    key: KeyEvent,
    primary_mod: PrimaryModifier,
    meta_mod: MetaModifier,
) -> Option<ReplAction> {
    let action = resolve_repl_action(key, primary_mod, meta_mod)?;
    Some(match action {
        ReplAction::ClearPromptSaving => ReplAction::ToggleFocus,
        ReplAction::SendPrompt
            if key.code == KeyCode::Enter && !primary_mod.matches(key.modifiers) =>
        {
            ReplAction::ToggleSelectedFileFold
        }
        ReplAction::HistoryPrev
        | ReplAction::LineUpOrHistoryPrev
        | ReplAction::MoveLineUp => ReplAction::ViewportPrev,
        ReplAction::HistoryNext
        | ReplAction::LineDownOrHistoryNext
        | ReplAction::MoveLineDown => ReplAction::ViewportNext,
        ReplAction::InsertSpaces => ReplAction::ToggleSelectedFileFold,
        ReplAction::MoveBeginningOfLine if key.code == KeyCode::Home => {
            ReplAction::ScrollResponseHome
        }
        ReplAction::MoveEndOfLine if key.code == KeyCode::End => ReplAction::ScrollResponseEnd,
        other => other,
    })
}

/// Resolves a key event into a deterministic `ReplAction`, eliminating shortcut
/// shadowing between primary command modifier (`C-`) and Alt-based Readline navigation.
pub fn resolve_repl_action(
    key: KeyEvent,
    primary_mod: PrimaryModifier,
    meta_mod: MetaModifier,
) -> Option<ReplAction> {
    let input = InputConfig {
        primary_modifier: primary_mod,
        meta_modifier: meta_mod,
        ..InputConfig::default()
    };

    if let Some(spec) = REPL_COMMAND_MAP.resolve(key, &input, false) {
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
    let (primary_mod, meta_mod, in_viewport) = {
        let st = state.lock().await;
        (
            st.tui_config.input.primary_modifier,
            st.tui_config.input.meta_modifier,
            st.model.focus == ReplFocus::Viewport,
        )
    };

    let resolved = if in_viewport {
        resolve_viewport_action(key, primary_mod, meta_mod)
    } else {
        resolve_repl_action(key, primary_mod, meta_mod)
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
                    st.notify_info("Prompt cleared (C-Y to restore)");
                } else {
                    st.model.esc_clear_armed = Some(std::time::Instant::now());
                    st.notify_info("Press Esc again to clear the prompt");
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
                if let Err(err) = send_request(server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await {
                    let mut st = state.lock().await;
                    st.input_editor.clear();
                    st.input_editor.insert_str(&prompt);
                    st.model.status = TurnStatus::Error;
                    st.notify_error(format!("Failed to send prompt: {}", err));
                }
            }
        }
        ReplAction::UndoCommit => {
            if st.model.is_busy() {
                st.notify_warning("Cannot undo while model is generating");
            } else if st.model.last_commit_hash.is_none() {
                st.notify_warning("No AI commit to undo");
            } else {
                st.confirm_undo = true;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        ReplAction::OpenSquashDialog => {
            crate::input::open_squash_dialog(&mut st, server_writer).await?;
        }
        ReplAction::CopyModelResponse => {
            if st.model.is_busy() {
                st.notify_warning("Cannot copy while model is generating");
            } else if st.model.text.trim().is_empty() {
                st.notify_warning("No model response to copy");
            } else {
                let text = st.model.text.clone();
                drop(st);
                let res = crate::clipboard::copy_to_clipboard(&text);
                let mut st = state.lock().await;
                match res {
                    crate::clipboard::CopyResult::Native => {
                        st.notify_success("Model response copied to clipboard");
                    }
                    crate::clipboard::CopyResult::Osc52Only => {
                        st.notify_info("Model response sent to terminal clipboard (OSC 52)");
                    }
                    crate::clipboard::CopyResult::Failed => {
                        st.notify_error("Failed to copy to clipboard");
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
            if !st.model.files.is_empty() && st.model.selected_file_index + 1 < st.model.files.len() {
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

fn step_viewport_up(model: &mut ReplView, view_height: u16) {
    if !model.files.is_empty() {
        model.selected_file_index = model.selected_file_index.saturating_sub(1);
        model.scroll_to_selected_file(view_height);
    } else {
        model.auto_scroll = false;
        model.scroll = model.scroll.saturating_sub(1);
    }
}

fn step_viewport_down(model: &mut ReplView, view_height: u16) {
    if !model.files.is_empty() {
        if model.selected_file_index + 1 < model.files.len() {
            model.selected_file_index += 1;
            model.scroll_to_selected_file(view_height);
        }
    } else {
        let max = model.max_scroll(view_height);
        model.scroll = (model.scroll.saturating_add(1)).min(max);
        if model.scroll >= max {
            model.auto_scroll = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn resolve_repl_action(key: KeyEvent, primary_mod: PrimaryModifier) -> Option<ReplAction> {
        super::resolve_repl_action(key, primary_mod, MetaModifier::Alt)
    }

    fn resolve_viewport_action(key: KeyEvent, primary_mod: PrimaryModifier) -> Option<ReplAction> {
        super::resolve_viewport_action(key, primary_mod, MetaModifier::Alt)
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

        // Undo
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('z'), KeyModifiers::CONTROL), p),
            Some(ReplAction::UndoCommit)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('u'), KeyModifiers::ALT), p),
            Some(ReplAction::UndoCommit)
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

        // Alt+U and Alt+Z both trigger Undo
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('u'), KeyModifiers::ALT), p),
            Some(ReplAction::UndoCommit)
        );
        assert_eq!(
            resolve_repl_action(key(KeyCode::Char('z'), KeyModifiers::ALT), p),
            Some(ReplAction::UndoCommit)
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
            Some(ReplAction::ToggleSelectedFileFold)
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
        let typed = resolve_viewport_action(key(KeyCode::Char('x'), KeyModifiers::NONE), p).unwrap();
        assert_eq!(typed, ReplAction::InsertChar('x'));
        assert!(is_editor_action(typed));
        let space = resolve_viewport_action(key(KeyCode::Char(' '), KeyModifiers::NONE), p).unwrap();
        assert_eq!(space, ReplAction::InsertChar(' '));
        assert!(is_editor_action(space));
    }

    #[test]
    fn test_develop_line_navigation_invariants() {
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
        assert!(guard.confirm_batch_cancel, "Esc during batch must open three-way batch cancel dialog");
        assert!(!guard.confirm_cancel, "Esc during batch must not open binary hard cancel dialog");
        assert_eq!(guard.batch_cancel_button, crate::app::BatchCancelButton::Continue);
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
        assert_eq!(guard.confirm_button, crate::app::ConfirmDialogButton::Cancel);
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
