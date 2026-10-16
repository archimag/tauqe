use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::Mutex;
use tauqe_protocol::{methods, ModelAskParams};

use crate::app::{AppState, KeyCommand};
use crate::config::PrimaryModifier;
use crate::input::{is_keyboard_quit, InputResult};
use crate::rpc::{send_request, RpcConnection};
use crate::views::develop::{DevelopFocus, DevelopView, TurnStatus};

/// Time window within which a second `Esc` confirms clearing a non-empty prompt.
const ESC_CLEAR_WINDOW: Duration = Duration::from_secs(2);

pub const DEVELOP_COMMANDS: &[KeyCommand] = &[
    // Core Actions
    KeyCommand { key: "Enter / C-Enter", description: "Send prompt" },
    KeyCommand { key: "C-Z / Alt+U", description: "Undo last AI commit" },
    KeyCommand { key: "F6 / C-S", description: "Squash commits dialog" },
    // Navigation & Inspection
    KeyCommand { key: "C-Space / Shift+Tab", description: "Switch focus between prompt and Viewport (typing returns focus to prompt)" },
    KeyCommand { key: "C-T / Enter (Viewport)", description: "Fold / unfold selected file diff" },
    KeyCommand { key: "C-[ / C-] / Alt+[/]", description: "Select previous / next modified file" },
    KeyCommand { key: "C-P / C-N", description: "Move cursor line up / down (Viewport: select previous / next file)" },
    KeyCommand { key: "↑ / ↓", description: "Move cursor between prompt lines; navigate history at the first / last line (Viewport: select previous / next file or scroll)" },
    KeyCommand { key: "PgUp / PgDn", description: "Page scroll response view" },
    KeyCommand { key: "Alt+↑ / Alt+↓", description: "Scroll model response up / down" },
    // Editing & Clipboard
    KeyCommand { key: "Alt+P / Alt+N", description: "Navigate prompt history backward / forward" },
    KeyCommand { key: "Shift+Enter / Alt+Enter / C-J", description: "Insert newline into prompt" },
    KeyCommand { key: "Tab", description: "Insert two spaces into the prompt (Viewport: fold / unfold selected file diff)" },
    KeyCommand { key: "C-Left / C-Right / Alt+B / Alt+F", description: "Move cursor word backward / forward" },
    KeyCommand { key: "C-B / C-F / Left / Right", description: "Move cursor character backward / forward" },
    KeyCommand { key: "C-A / C-E / Home / End", description: "Move cursor to beginning / end of line (Viewport: scroll to top / bottom)" },
    KeyCommand { key: "C-K / C-U", description: "Kill line to end / beginning into kill ring" },
    KeyCommand { key: "C-W / Alt+Backspace", description: "Kill word backward into kill ring" },
    KeyCommand { key: "Alt+D", description: "Kill word forward into kill ring" },
    KeyCommand { key: "C-D / Delete", description: "Delete character forward" },
    KeyCommand { key: "C-Y", description: "Yank (paste) text from the kill ring (falls back to the system clipboard when empty)" },
    KeyCommand { key: "Alt+C / Alt+Y", description: "Copy model response to clipboard" },
    // Auxiliary & Destructive
    KeyCommand { key: "Esc / C-G", description: "Interrupt generation (keyboard-quit); press twice to clear the prompt (saved to kill ring); detach discussion focus when empty; leave Viewport" },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevelopAction {
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
/// these actions return focus to the prompt before they are applied.
fn is_editor_action(action: DevelopAction) -> bool {
    matches!(
        action,
        DevelopAction::InsertChar(_)
            | DevelopAction::InsertNewline
            | DevelopAction::InsertSpaces
            | DevelopAction::DeleteBackward
            | DevelopAction::DeleteForward
            | DevelopAction::KillLine
            | DevelopAction::KillToBeginningOfLine
            | DevelopAction::KillWordBackward
            | DevelopAction::KillWordForward
            | DevelopAction::MoveBeginningOfLine
            | DevelopAction::MoveEndOfLine
            | DevelopAction::MoveCharBackward
            | DevelopAction::MoveCharForward
            | DevelopAction::MoveWordBackward
            | DevelopAction::MoveWordForward
            | DevelopAction::MoveLineUp
            | DevelopAction::MoveLineDown
            | DevelopAction::Yank
    )
}

/// Resolves a key event while the Viewport has focus. Navigation keys act on the
/// response and file diffs; editing keys fall through unchanged so the caller can
/// return focus to the prompt.
///
/// The base editor bindings are buffer-content independent, so the Viewport is the
/// sole place that maps `Tab`, `Home`, `End` and the arrow keys to response
/// navigation (Conventions §8).
pub fn resolve_viewport_action(key: KeyEvent, primary_mod: PrimaryModifier) -> Option<DevelopAction> {
    let action = resolve_develop_action(key, primary_mod)?;
    Some(match action {
        DevelopAction::ClearPromptSaving => DevelopAction::ToggleFocus,
        DevelopAction::SendPrompt
            if key.code == KeyCode::Enter && !primary_mod.matches(key.modifiers) =>
        {
            DevelopAction::ToggleSelectedFileFold
        }
        DevelopAction::HistoryPrev
        | DevelopAction::LineUpOrHistoryPrev
        | DevelopAction::MoveLineUp => DevelopAction::ViewportPrev,
        DevelopAction::HistoryNext
        | DevelopAction::LineDownOrHistoryNext
        | DevelopAction::MoveLineDown => DevelopAction::ViewportNext,
        DevelopAction::InsertSpaces => DevelopAction::ToggleSelectedFileFold,
        DevelopAction::MoveBeginningOfLine if key.code == KeyCode::Home => {
            DevelopAction::ScrollResponseHome
        }
        DevelopAction::MoveEndOfLine if key.code == KeyCode::End => DevelopAction::ScrollResponseEnd,
        other => other,
    })
}

/// Resolves a key event into a deterministic `DevelopAction`, eliminating shortcut
/// shadowing between primary command modifier (`C-`) and Alt-based Readline navigation.
///
/// Editor bindings never depend on buffer content: `Tab`, `Home`, `End` and the arrow
/// keys keep a single meaning whether the prompt is empty or not. Navigation over the
/// response and file diffs is the exclusive responsibility of the Viewport (§8).
pub fn resolve_develop_action(key: KeyEvent, primary_mod: PrimaryModifier) -> Option<DevelopAction> {
    let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let is_primary = primary_mod.matches(key.modifiers);

    // 0. C-g (keyboard-quit): the canonical abort chord is identical to Esc,
    // returning Viewport focus to the prompt, clearing the prompt, or arming
    // the detach confirmation on an empty prompt.
    if is_keyboard_quit(&key) {
        return Some(DevelopAction::ClearPromptSaving);
    }

    // 1. Esc: interrupt generation or clear prompt
    if key.code == KeyCode::Esc {
        return Some(DevelopAction::ClearPromptSaving);
    }

    // 2. Universal hardware function keys
    if key.code == KeyCode::F(6) {
        return Some(DevelopAction::OpenSquashDialog);
    }

    // Shift+Tab switches focus between the prompt and the Viewport
    if key.code == KeyCode::BackTab || (key.code == KeyCode::Tab && has_shift) {
        return Some(DevelopAction::ToggleFocus);
    }

    // 3. Alt+C and Alt+Y: Copy model response (Alt+C always; Alt+Y when not primary modifier)
    if has_alt && !has_ctrl {
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => {
                return Some(DevelopAction::CopyModelResponse);
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if primary_mod != PrimaryModifier::Alt => {
                return Some(DevelopAction::CopyModelResponse);
            }
            KeyCode::Char('u') | KeyCode::Char('U') => {
                return Some(DevelopAction::UndoCommit);
            }
            _ => {}
        }
    }

    // 4. Primary Command Modifier (C-x) chords
    if is_primary {
        match key.code {
            KeyCode::Enter => return Some(DevelopAction::SendPrompt),
            KeyCode::Char('s') => return Some(DevelopAction::OpenSquashDialog),
            KeyCode::Char('z') => return Some(DevelopAction::UndoCommit),
            KeyCode::Char('j') => return Some(DevelopAction::InsertNewline),
            KeyCode::Char('t') | KeyCode::Char('T') => return Some(DevelopAction::ToggleSelectedFileFold),
            KeyCode::Char('[') => return Some(DevelopAction::PrevModifiedFile),
            KeyCode::Char(']') => return Some(DevelopAction::NextModifiedFile),
            KeyCode::Char(' ') => return Some(DevelopAction::ToggleFocus),
            KeyCode::Char('a') => return Some(DevelopAction::MoveBeginningOfLine),
            KeyCode::Char('e') => return Some(DevelopAction::MoveEndOfLine),
            KeyCode::Char('k') => return Some(DevelopAction::KillLine),
            KeyCode::Char('w') => return Some(DevelopAction::KillWordBackward),
            KeyCode::Char('y') => return Some(DevelopAction::Yank),
            KeyCode::Left => return Some(DevelopAction::MoveWordBackward),
            KeyCode::Right => return Some(DevelopAction::MoveWordForward),
            // Mode-specific character actions
            KeyCode::Char('p') | KeyCode::Char('P') if primary_mod == PrimaryModifier::Ctrl => {
                return Some(DevelopAction::MoveLineUp);
            }
            KeyCode::Char('n') | KeyCode::Char('N') if primary_mod == PrimaryModifier::Ctrl => {
                return Some(DevelopAction::MoveLineDown);
            }
            KeyCode::Char('p') | KeyCode::Char('P') if primary_mod == PrimaryModifier::Alt => {
                return Some(DevelopAction::HistoryPrev);
            }
            KeyCode::Char('n') | KeyCode::Char('N') if primary_mod == PrimaryModifier::Alt => {
                return Some(DevelopAction::HistoryNext);
            }
            KeyCode::Char('u') if primary_mod == PrimaryModifier::Ctrl => {
                return Some(DevelopAction::KillToBeginningOfLine);
            }
            KeyCode::Char('b') if primary_mod == PrimaryModifier::Ctrl => {
                return Some(DevelopAction::MoveCharBackward);
            }
            KeyCode::Char('f') if primary_mod == PrimaryModifier::Ctrl => {
                return Some(DevelopAction::MoveCharForward);
            }
            KeyCode::Char('d') if primary_mod == PrimaryModifier::Ctrl => {
                return Some(DevelopAction::DeleteForward);
            }
            // When primary is Alt, keep Readline word semantics
            KeyCode::Char('b') if primary_mod == PrimaryModifier::Alt => {
                return Some(DevelopAction::MoveWordBackward);
            }
            KeyCode::Char('f') if primary_mod == PrimaryModifier::Alt => {
                return Some(DevelopAction::MoveWordForward);
            }
            KeyCode::Char('d') if primary_mod == PrimaryModifier::Alt => {
                return Some(DevelopAction::KillWordForward);
            }
            _ => {}
        }
    }

    // 5. Readline Ctrl chords (always active even if primary_modifier is Alt)
    if has_ctrl && !has_alt {
        match key.code {
            KeyCode::Char('a') => return Some(DevelopAction::MoveBeginningOfLine),
            KeyCode::Char('e') => return Some(DevelopAction::MoveEndOfLine),
            KeyCode::Char('k') => return Some(DevelopAction::KillLine),
            KeyCode::Char('u') => return Some(DevelopAction::KillToBeginningOfLine),
            KeyCode::Char('w') => return Some(DevelopAction::KillWordBackward),
            KeyCode::Char('y') => return Some(DevelopAction::Yank),
            KeyCode::Char('d') => return Some(DevelopAction::DeleteForward),
            KeyCode::Char('b') => return Some(DevelopAction::MoveCharBackward),
            KeyCode::Char('f') => return Some(DevelopAction::MoveCharForward),
            KeyCode::Char('j') => return Some(DevelopAction::InsertNewline),
            KeyCode::Char('p') | KeyCode::Char('P') => return Some(DevelopAction::MoveLineUp),
            KeyCode::Char('n') | KeyCode::Char('N') => return Some(DevelopAction::MoveLineDown),
            KeyCode::Char('t') | KeyCode::Char('T') => return Some(DevelopAction::ToggleSelectedFileFold),
            KeyCode::Left => return Some(DevelopAction::MoveWordBackward),
            KeyCode::Right => return Some(DevelopAction::MoveWordForward),
            KeyCode::Char('[') => return Some(DevelopAction::PrevModifiedFile),
            KeyCode::Char(']') => return Some(DevelopAction::NextModifiedFile),
            KeyCode::Char(' ') => return Some(DevelopAction::ToggleFocus),
            KeyCode::Char('s') => return Some(DevelopAction::OpenSquashDialog),
            KeyCode::Char('z') => return Some(DevelopAction::UndoCommit),
            _ => {}
        }
    }

    // 6. Readline Alt chords and response scrolling
    if has_alt && !has_ctrl {
        match key.code {
            KeyCode::Char('b') | KeyCode::Left => return Some(DevelopAction::MoveWordBackward),
            KeyCode::Char('f') | KeyCode::Right => return Some(DevelopAction::MoveWordForward),
            KeyCode::Char('d') => return Some(DevelopAction::KillWordForward),
            KeyCode::Char('p') | KeyCode::Char('P') => return Some(DevelopAction::HistoryPrev),
            KeyCode::Char('n') | KeyCode::Char('N') => return Some(DevelopAction::HistoryNext),
            KeyCode::Char('t') | KeyCode::Char('T') => return Some(DevelopAction::ToggleSelectedFileFold),
            KeyCode::Backspace => return Some(DevelopAction::KillWordBackward),
            KeyCode::Enter => return Some(DevelopAction::InsertNewline),
            KeyCode::Char('[') => return Some(DevelopAction::PrevModifiedFile),
            KeyCode::Char(']') => return Some(DevelopAction::NextModifiedFile),
            KeyCode::Char(' ') => return Some(DevelopAction::ToggleSelectedFileFold),
            KeyCode::Up => return Some(DevelopAction::ScrollResponseUp),
            KeyCode::Down => return Some(DevelopAction::ScrollResponseDown),
            _ => {}
        }
    }

    // 7. Plain navigation & editing keys (independent of buffer content)
    match key.code {
        KeyCode::Enter if has_shift => Some(DevelopAction::InsertNewline),
        KeyCode::Enter => Some(DevelopAction::SendPrompt),
        KeyCode::Tab => Some(DevelopAction::InsertSpaces),
        KeyCode::Backspace => Some(DevelopAction::DeleteBackward),
        KeyCode::Delete => Some(DevelopAction::DeleteForward),
        KeyCode::Left => Some(DevelopAction::MoveCharBackward),
        KeyCode::Right => Some(DevelopAction::MoveCharForward),
        KeyCode::Home => Some(DevelopAction::MoveBeginningOfLine),
        KeyCode::End => Some(DevelopAction::MoveEndOfLine),
        KeyCode::PageUp => Some(DevelopAction::ScrollResponsePageUp),
        KeyCode::PageDown => Some(DevelopAction::ScrollResponsePageDown),
        KeyCode::Up if has_shift => Some(DevelopAction::ScrollResponseUp),
        KeyCode::Down if has_shift => Some(DevelopAction::ScrollResponseDown),
        KeyCode::Up => Some(DevelopAction::LineUpOrHistoryPrev),
        KeyCode::Down => Some(DevelopAction::LineDownOrHistoryNext),
        KeyCode::Char(c) if crate::input::is_char_typing(key.modifiers) => {
            Some(DevelopAction::InsertChar(c))
        }
        _ => None,
    }
}

pub async fn handle_develop_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let (primary_mod, in_viewport) = {
        let st = state.lock().await;
        (
            st.tui_config.input.primary_modifier,
            st.model.focus == DevelopFocus::Viewport,
        )
    };

    let resolved = if in_viewport {
        resolve_viewport_action(key, primary_mod)
    } else {
        resolve_develop_action(key, primary_mod)
    };
    let action = match resolved {
        Some(act) => act,
        None => return Ok(InputResult::Continue),
    };

    let mut st = state.lock().await;
    let view_height = st.last_model_height;

    if in_viewport && is_editor_action(action) {
        st.model.focus = DevelopFocus::Editor;
    }

    // Any action other than `Esc` disarms a pending two-press prompt clear.
    if action != DevelopAction::ClearPromptSaving {
        st.model.esc_clear_armed = None;
    }

    match action {
        DevelopAction::InsertChar(c) => {
            st.input_editor.insert_char(c);
        }
        DevelopAction::InsertNewline => {
            st.input_editor.insert_char('\n');
        }
        DevelopAction::InsertSpaces => {
            st.input_editor.insert_str("  ");
        }
        DevelopAction::DeleteBackward => {
            st.input_editor.delete_backward();
        }
        DevelopAction::DeleteForward => {
            st.input_editor.delete_forward();
        }
        DevelopAction::KillLine => {
            st.input_editor.kill_line();
        }
        DevelopAction::KillToBeginningOfLine => {
            st.input_editor.kill_to_beginning_of_line();
        }
        DevelopAction::KillWordBackward => {
            st.input_editor.kill_word_backward();
        }
        DevelopAction::KillWordForward => {
            st.input_editor.kill_word_forward();
        }
        DevelopAction::MoveBeginningOfLine => {
            st.input_editor.move_beginning_of_line();
        }
        DevelopAction::MoveEndOfLine => {
            st.input_editor.move_end_of_line();
        }
        DevelopAction::MoveCharBackward => {
            st.input_editor.move_backward();
        }
        DevelopAction::MoveCharForward => {
            st.input_editor.move_forward();
        }
        DevelopAction::MoveWordBackward => {
            st.input_editor.move_word_backward();
        }
        DevelopAction::MoveWordForward => {
            st.input_editor.move_word_forward();
        }
        DevelopAction::Yank => {
            st.input_editor.yank();
        }
        DevelopAction::ClearPromptSaving => {
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
                // With an empty prompt and an active discussion focus, `Esc`
                // opens the same guarded detach confirmation as `C-G`, so both
                // routes share one detach path and the `[Esc to detach]` banner
                // reflects actual behavior.
                st.confirm_detach_focus = true;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        DevelopAction::SendPrompt => {
            st.model.focus = DevelopFocus::Editor;
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
        DevelopAction::UndoCommit => {
            if st.model.is_busy() {
                st.notify_warning("Cannot undo while model is generating");
            } else if st.model.last_commit_hash.is_none() {
                st.notify_warning("No AI commit to undo");
            } else {
                st.confirm_undo = true;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        DevelopAction::OpenSquashDialog => {
            crate::input::open_squash_dialog(&mut st, server_writer).await?;
        }
        DevelopAction::CopyModelResponse => {
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
        DevelopAction::PrevModifiedFile => {
            if !st.model.files.is_empty() {
                st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                st.model.scroll_to_selected_file(view_height);
            }
        }
        DevelopAction::NextModifiedFile => {
            if !st.model.files.is_empty() && st.model.selected_file_index + 1 < st.model.files.len() {
                st.model.selected_file_index += 1;
                st.model.scroll_to_selected_file(view_height);
            }
        }
        DevelopAction::ToggleSelectedFileFold => {
            let sel_idx = st.model.selected_file_index;
            if let Some(file) = st.model.files.get_mut(sel_idx) {
                file.expanded = !file.expanded;
                st.model.scroll_to_selected_file(view_height);
            }
        }
        DevelopAction::ScrollResponseUp => {
            st.model.auto_scroll = false;
            st.model.scroll = st.model.scroll.saturating_sub(1);
        }
        DevelopAction::ScrollResponseDown => {
            let max = st.model.max_scroll(view_height);
            st.model.scroll = (st.model.scroll.saturating_add(1)).min(max);
            if st.model.scroll >= max {
                st.model.auto_scroll = true;
            }
        }
        DevelopAction::ScrollResponsePageUp => {
            let page = view_height.saturating_sub(2).max(1);
            st.model.auto_scroll = false;
            st.model.scroll = st.model.scroll.saturating_sub(page);
        }
        DevelopAction::ScrollResponsePageDown => {
            let page = view_height.saturating_sub(2).max(1);
            let max = st.model.max_scroll(view_height);
            st.model.scroll = (st.model.scroll.saturating_add(page)).min(max);
            if st.model.scroll >= max {
                st.model.auto_scroll = true;
            }
        }
        DevelopAction::ScrollResponseHome => {
            st.model.auto_scroll = false;
            st.model.scroll = 0;
        }
        DevelopAction::ScrollResponseEnd => {
            st.model.auto_scroll = true;
            st.model.scroll = st.model.max_scroll(view_height);
        }
        DevelopAction::MoveLineUp => {
            st.input_editor.move_line_up();
        }
        DevelopAction::MoveLineDown => {
            st.input_editor.move_line_down();
        }
        DevelopAction::HistoryPrev => {
            st.input_editor.history_prev();
        }
        DevelopAction::HistoryNext => {
            st.input_editor.history_next();
        }
        DevelopAction::LineUpOrHistoryPrev => {
            if !st.input_editor.move_line_up() {
                st.input_editor.history_prev();
            }
        }
        DevelopAction::LineDownOrHistoryNext => {
            if !st.input_editor.move_line_down() {
                st.input_editor.history_next();
            }
        }
        DevelopAction::ToggleFocus => {
            st.model.focus = match st.model.focus {
                DevelopFocus::Editor => DevelopFocus::Viewport,
                DevelopFocus::Viewport => DevelopFocus::Editor,
            };
        }
        DevelopAction::ViewportPrev => {
            step_viewport_up(&mut st.model, view_height);
        }
        DevelopAction::ViewportNext => {
            step_viewport_down(&mut st.model, view_height);
        }
    }

    Ok(InputResult::Continue)
}

/// Selects the previous modified file, or scrolls the response up when there are no files.
fn step_viewport_up(model: &mut DevelopView, view_height: u16) {
    if !model.files.is_empty() {
        model.selected_file_index = model.selected_file_index.saturating_sub(1);
        model.scroll_to_selected_file(view_height);
    } else {
        model.auto_scroll = false;
        model.scroll = model.scroll.saturating_sub(1);
    }
}

/// Selects the next modified file, or scrolls the response down when there are no files.
fn step_viewport_down(model: &mut DevelopView, view_height: u16) {
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

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn test_resolve_develop_action_ctrl_primary() {
        let p = PrimaryModifier::Ctrl;

        // Copy response
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('c'), KeyModifiers::ALT), p),
            Some(DevelopAction::CopyModelResponse)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('y'), KeyModifiers::ALT), p),
            Some(DevelopAction::CopyModelResponse)
        );

        // Undo
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('z'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::UndoCommit)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('u'), KeyModifiers::ALT), p),
            Some(DevelopAction::UndoCommit)
        );

        // Line navigation, prompt history and diff folding
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveLineUp)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveLineDown)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('p'), KeyModifiers::ALT), p),
            Some(DevelopAction::HistoryPrev)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('n'), KeyModifiers::ALT), p),
            Some(DevelopAction::HistoryNext)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('t'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('t'), KeyModifiers::ALT), p),
            Some(DevelopAction::ToggleSelectedFileFold)
        );

        // Arrows always move between prompt lines, independent of buffer content
        assert_eq!(
            resolve_develop_action(key(KeyCode::Up, KeyModifiers::NONE), p),
            Some(DevelopAction::LineUpOrHistoryPrev)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Down, KeyModifiers::NONE), p),
            Some(DevelopAction::LineDownOrHistoryNext)
        );

        // Readline text editing
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('u'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::KillToBeginningOfLine)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('b'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveCharBackward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('b'), KeyModifiers::ALT), p),
            Some(DevelopAction::MoveWordBackward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('f'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveCharForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('f'), KeyModifiers::ALT), p),
            Some(DevelopAction::MoveWordForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('d'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::DeleteForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('d'), KeyModifiers::ALT), p),
            Some(DevelopAction::KillWordForward)
        );
    }

    #[test]
    fn test_resolve_develop_action_alt_primary() {
        let p = PrimaryModifier::Alt;

        // Alt+C copies response without shadowing
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('c'), KeyModifiers::ALT), p),
            Some(DevelopAction::CopyModelResponse)
        );

        // Alt+U and Alt+Z both trigger Undo
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('u'), KeyModifiers::ALT), p),
            Some(DevelopAction::UndoCommit)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('z'), KeyModifiers::ALT), p),
            Some(DevelopAction::UndoCommit)
        );

        // Ctrl+U still kills to beginning of line (Readline compatibility)
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('u'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::KillToBeginningOfLine)
        );

        // Alt+B, Alt+F, Alt+D retain word semantics
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('b'), KeyModifiers::ALT), p),
            Some(DevelopAction::MoveWordBackward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('f'), KeyModifiers::ALT), p),
            Some(DevelopAction::MoveWordForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('d'), KeyModifiers::ALT), p),
            Some(DevelopAction::KillWordForward)
        );

        // Ctrl+B, Ctrl+F, Ctrl+D retain char semantics
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('b'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveCharBackward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('f'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveCharForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('d'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::DeleteForward)
        );

        // Alt+P / Alt+N trigger HistoryPrev / HistoryNext; Ctrl+P / Ctrl+N move lines
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('p'), KeyModifiers::ALT), p),
            Some(DevelopAction::HistoryPrev)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('n'), KeyModifiers::ALT), p),
            Some(DevelopAction::HistoryNext)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveLineUp)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveLineDown)
        );
    }

    #[test]
    fn test_editor_keys_do_not_depend_on_buffer_content() {
        let p = PrimaryModifier::Ctrl;

        // Tab, Home and End keep their editing meaning regardless of buffer content.
        assert_eq!(
            resolve_develop_action(key(KeyCode::Tab, KeyModifiers::NONE), p),
            Some(DevelopAction::InsertSpaces)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Home, KeyModifiers::NONE), p),
            Some(DevelopAction::MoveBeginningOfLine)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::End, KeyModifiers::NONE), p),
            Some(DevelopAction::MoveEndOfLine)
        );

        // The Viewport is the sole place that maps those keys to navigation.
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Tab, KeyModifiers::NONE), p),
            Some(DevelopAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Home, KeyModifiers::NONE), p),
            Some(DevelopAction::ScrollResponseHome)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::End, KeyModifiers::NONE), p),
            Some(DevelopAction::ScrollResponseEnd)
        );

        // C-a / C-e remain editing commands even in the Viewport.
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('a'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveBeginningOfLine)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('e'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveEndOfLine)
        );
    }

    #[test]
    fn test_focus_toggle_and_viewport_navigation() {
        let p = PrimaryModifier::Ctrl;

        // Focus toggle works from the prompt
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char(' '), KeyModifiers::CONTROL), p),
            Some(DevelopAction::ToggleFocus)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::BackTab, KeyModifiers::SHIFT), p),
            Some(DevelopAction::ToggleFocus)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Esc, KeyModifiers::NONE), p),
            Some(DevelopAction::ClearPromptSaving)
        );

        // Viewport navigation
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::ViewportNext)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::ViewportPrev)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Down, KeyModifiers::NONE), p),
            Some(DevelopAction::ViewportNext)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Up, KeyModifiers::NONE), p),
            Some(DevelopAction::ViewportPrev)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Tab, KeyModifiers::NONE), p),
            Some(DevelopAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Enter, KeyModifiers::NONE), p),
            Some(DevelopAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Char('t'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Esc, KeyModifiers::NONE), p),
            Some(DevelopAction::ToggleFocus)
        );
        assert_eq!(
            resolve_viewport_action(key(KeyCode::Enter, KeyModifiers::CONTROL), p),
            Some(DevelopAction::SendPrompt)
        );

        // Typing (including Space) is an editor action that returns focus to the prompt
        let typed = resolve_viewport_action(key(KeyCode::Char('x'), KeyModifiers::NONE), p).unwrap();
        assert_eq!(typed, DevelopAction::InsertChar('x'));
        assert!(is_editor_action(typed));
        let space = resolve_viewport_action(key(KeyCode::Char(' '), KeyModifiers::NONE), p).unwrap();
        assert_eq!(space, DevelopAction::InsertChar(' '));
        assert!(is_editor_action(space));
    }

    #[test]
    fn test_develop_line_navigation_invariants() {
        let p = PrimaryModifier::Ctrl;

        // C-j is the electric-newline; newline insertion is reserved for Enter,
        // Shift+Enter, Alt+Enter and C-j only.
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('j'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::InsertNewline)
        );

        // C-p / C-n navigate symmetrically and never insert a newline.
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveLineUp)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::MoveLineDown)
        );
        assert_ne!(
            resolve_develop_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p),
            Some(DevelopAction::InsertNewline)
        );
    }

    #[test]
    fn test_command_bindings_are_unique() {
        use std::collections::HashSet;
        let keys: HashSet<&str> = DEVELOP_COMMANDS.iter().map(|c| c.key).collect();
        assert!(!DEVELOP_COMMANDS.is_empty());
        assert_eq!(
            keys.len(),
            DEVELOP_COMMANDS.len(),
            "duplicate key bindings in DEVELOP_COMMANDS"
        );
    }

    #[tokio::test]
    async fn empty_prompt_esc_arms_detach_confirmation_for_active_focus() {
        let mut st = crate::app::test_state();
        st.discussion_focus =
            tauqe_protocol::DiscussionFocus::plan("plan-1".to_string(), vec!["s-1".to_string()]);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_develop_key(key, &state, &mut conn).await.unwrap();

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
        handle_develop_key(key, &state, &mut conn).await.unwrap();

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
        handle_develop_key(key, &state, &mut conn).await.unwrap();

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
        handle_develop_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(!st.confirm_detach_focus);
        assert!(!st.confirm_cancel);
    }
}
