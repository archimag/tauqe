use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, ModelAskParams};

use crate::app::{AppState, KeyCommand};
use crate::config::PrimaryModifier;
use crate::input::InputResult;
use crate::rpc::send_request;
use crate::ui::develop::{DevelopFocus, DevelopView};

pub const DEVELOP_COMMANDS: &[KeyCommand] = &[
    KeyCommand { key: "Enter / C-Enter", description: "Send prompt" },
    KeyCommand { key: "Shift+Enter / Alt+Enter / C-J", description: "Insert newline into prompt" },
    KeyCommand { key: "C-P / C-N / Alt+P / Alt+N", description: "Navigate prompt history backward / forward (select previous / next file in Viewport)" },
    KeyCommand { key: "↑ / ↓", description: "Navigate prompt lines (select modified file or scroll response when empty)" },
    KeyCommand { key: "C-Space / Shift+Tab", description: "Switch focus between prompt and Viewport (typing returns focus to prompt)" },
    KeyCommand { key: "C-T / Tab (empty prompt or Viewport) / Enter (Viewport)", description: "Fold / unfold selected file diff" },
    KeyCommand { key: "C-[ / C-] / Alt+[/]", description: "Select previous / next modified file" },
    KeyCommand { key: "C-Left / C-Right / Alt+B / Alt+F", description: "Move cursor word backward / forward" },
    KeyCommand { key: "C-A / C-E / Home / End", description: "Move cursor to beginning / end of line or response" },
    KeyCommand { key: "C-K / C-U", description: "Kill line to end / beginning into kill ring" },
    KeyCommand { key: "C-W / Alt+Backspace", description: "Kill word backward into kill ring" },
    KeyCommand { key: "Alt+D", description: "Kill word forward into kill ring" },
    KeyCommand { key: "C-D / Delete", description: "Delete character forward" },
    KeyCommand { key: "C-B / C-F / Left / Right", description: "Move cursor character backward / forward" },
    KeyCommand { key: "C-Y", description: "Yank (paste) text from clipboard or kill ring" },
    KeyCommand { key: "Esc", description: "Interrupt generation or clear prompt (saves to kill ring); leave Viewport" },
    KeyCommand { key: "C-Z / Alt+U", description: "Undo last AI commit" },
    KeyCommand { key: "F6 / C-S", description: "Squash commits dialog" },
    KeyCommand { key: "Alt+C / Alt+Y", description: "Copy model response to clipboard" },
    KeyCommand { key: "PgUp / PgDn", description: "Page scroll response view" },
    KeyCommand { key: "Alt+↑ / Alt+↓", description: "Scroll model response up / down" },
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
    Yank,
    ClearPromptSaving,
    HistoryPrev,
    HistoryNext,
    LineUpOrFilePrev,
    LineDownOrFileNext,

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
            | DevelopAction::Yank
    )
}

/// Resolves a key event while the Viewport has focus. Navigation keys act on the
/// response and file diffs; editing keys fall through unchanged so the caller can
/// return focus to the prompt.
pub fn resolve_viewport_action(key: KeyEvent, primary_mod: PrimaryModifier) -> Option<DevelopAction> {
    let action = resolve_develop_action(key, primary_mod, true)?;
    Some(match action {
        DevelopAction::ClearPromptSaving => DevelopAction::ToggleFocus,
        DevelopAction::SendPrompt
            if key.code == KeyCode::Enter && !primary_mod.matches(key.modifiers) =>
        {
            DevelopAction::ToggleSelectedFileFold
        }
        DevelopAction::HistoryPrev | DevelopAction::LineUpOrFilePrev => DevelopAction::ViewportPrev,
        DevelopAction::HistoryNext | DevelopAction::LineDownOrFileNext => DevelopAction::ViewportNext,
        DevelopAction::InsertSpaces => DevelopAction::ToggleSelectedFileFold,
        other => other,
    })
}

/// Resolves a key event into a deterministic `DevelopAction`, eliminating shortcut
/// shadowing between primary command modifier (`C-`) and Alt-based Readline navigation.
pub fn resolve_develop_action(
    key: KeyEvent,
    primary_mod: PrimaryModifier,
    is_editor_empty: bool,
) -> Option<DevelopAction> {
    let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let is_primary = primary_mod.matches(key.modifiers);

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
            KeyCode::Char('p') | KeyCode::Char('P') => return Some(DevelopAction::HistoryPrev),
            KeyCode::Char('n') | KeyCode::Char('N') => return Some(DevelopAction::HistoryNext),
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
            KeyCode::Char('p') | KeyCode::Char('P') => return Some(DevelopAction::HistoryPrev),
            KeyCode::Char('n') | KeyCode::Char('N') => return Some(DevelopAction::HistoryNext),
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

    // 7. Plain navigation & editing keys
    match key.code {
        KeyCode::Enter if has_shift => Some(DevelopAction::InsertNewline),
        KeyCode::Enter => Some(DevelopAction::SendPrompt),
        KeyCode::Tab => {
            if is_editor_empty {
                Some(DevelopAction::ToggleSelectedFileFold)
            } else {
                Some(DevelopAction::InsertSpaces)
            }
        }
        KeyCode::Backspace => Some(DevelopAction::DeleteBackward),
        KeyCode::Delete => Some(DevelopAction::DeleteForward),
        KeyCode::Left => Some(DevelopAction::MoveCharBackward),
        KeyCode::Right => Some(DevelopAction::MoveCharForward),
        KeyCode::Home => {
            if is_editor_empty {
                Some(DevelopAction::ScrollResponseHome)
            } else {
                Some(DevelopAction::MoveBeginningOfLine)
            }
        }
        KeyCode::End => {
            if is_editor_empty {
                Some(DevelopAction::ScrollResponseEnd)
            } else {
                Some(DevelopAction::MoveEndOfLine)
            }
        }
        KeyCode::PageUp => Some(DevelopAction::ScrollResponsePageUp),
        KeyCode::PageDown => Some(DevelopAction::ScrollResponsePageDown),
        KeyCode::Up if has_shift && is_editor_empty => Some(DevelopAction::ScrollResponseUp),
        KeyCode::Down if has_shift && is_editor_empty => Some(DevelopAction::ScrollResponseDown),
        KeyCode::Up => Some(DevelopAction::LineUpOrFilePrev),
        KeyCode::Down => Some(DevelopAction::LineDownOrFileNext),
        KeyCode::Char(c) if crate::input::is_char_typing(key.modifiers) => {
            Some(DevelopAction::InsertChar(c))
        }
        _ => None,
    }
}

pub async fn handle_develop_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let (primary_mod, is_editor_empty, in_viewport) = {
        let st = state.lock().await;
        (
            st.tui_config.input.primary_modifier,
            st.input_editor.is_empty(),
            st.model.focus == DevelopFocus::Viewport,
        )
    };

    let resolved = if in_viewport {
        resolve_viewport_action(key, primary_mod)
    } else {
        resolve_develop_action(key, primary_mod, is_editor_empty)
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
            if st.model.is_busy() {
                st.confirm_cancel = true;
            } else if !st.input_editor.is_empty() {
                st.input_editor.clear_saving();
                st.notify_info("Prompt cleared (C-Y to restore)");
            }
        }
        DevelopAction::SendPrompt => {
            st.model.focus = DevelopFocus::Editor;
            if let Some(prompt) = st.take_prompt() {
                drop(st);
                let params = ModelAskParams { prompt: prompt.clone() };
                if let Err(err) = send_request(server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await {
                    let mut st = state.lock().await;
                    st.input_editor.clear();
                    st.input_editor.insert_str(&prompt);
                    st.model.status = "error".to_string();
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
        DevelopAction::HistoryPrev => {
            st.input_editor.history_prev();
        }
        DevelopAction::HistoryNext => {
            st.input_editor.history_next();
        }
        DevelopAction::LineUpOrFilePrev => {
            if st.input_editor.line_count() > 1 && st.input_editor.move_line_up() {
                return Ok(InputResult::Continue);
            }
            if st.input_editor.is_empty() {
                step_viewport_up(&mut st.model, view_height);
            }
        }
        DevelopAction::LineDownOrFileNext => {
            if st.input_editor.line_count() > 1 && st.input_editor.move_line_down() {
                return Ok(InputResult::Continue);
            }
            if st.input_editor.is_empty() {
                step_viewport_down(&mut st.model, view_height);
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
            resolve_develop_action(key(KeyCode::Char('c'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::CopyModelResponse)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('y'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::CopyModelResponse)
        );

        // Undo
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('z'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::UndoCommit)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('u'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::UndoCommit)
        );

        // Prompt history and diff folding
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::HistoryPrev)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('n'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::HistoryNext)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('t'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::ToggleSelectedFileFold)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('t'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::ToggleSelectedFileFold)
        );

        // Arrows when editor is empty
        assert_eq!(
            resolve_develop_action(key(KeyCode::Up, KeyModifiers::NONE), p, true),
            Some(DevelopAction::LineUpOrFilePrev)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Down, KeyModifiers::NONE), p, true),
            Some(DevelopAction::LineDownOrFileNext)
        );

        // Readline text editing
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('u'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::KillToBeginningOfLine)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('b'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::MoveCharBackward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('b'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::MoveWordBackward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('f'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::MoveCharForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('f'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::MoveWordForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('d'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::DeleteForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('d'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::KillWordForward)
        );
    }

    #[test]
    fn test_resolve_develop_action_alt_primary() {
        let p = PrimaryModifier::Alt;

        // Alt+C copies response without shadowing
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('c'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::CopyModelResponse)
        );

        // Alt+U and Alt+Z both trigger Undo
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('u'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::UndoCommit)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('z'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::UndoCommit)
        );

        // Ctrl+U still kills to beginning of line (Readline compatibility)
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('u'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::KillToBeginningOfLine)
        );

        // Alt+B, Alt+F, Alt+D retain word semantics
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('b'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::MoveWordBackward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('f'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::MoveWordForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('d'), KeyModifiers::ALT), p, false),
            Some(DevelopAction::KillWordForward)
        );

        // Ctrl+B, Ctrl+F, Ctrl+D retain char semantics
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('b'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::MoveCharBackward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('f'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::MoveCharForward)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char('d'), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::DeleteForward)
        );
    }

    #[test]
    fn test_focus_toggle_and_viewport_navigation() {
        let p = PrimaryModifier::Ctrl;

        // Focus toggle works from the prompt
        assert_eq!(
            resolve_develop_action(key(KeyCode::Char(' '), KeyModifiers::CONTROL), p, false),
            Some(DevelopAction::ToggleFocus)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::BackTab, KeyModifiers::SHIFT), p, false),
            Some(DevelopAction::ToggleFocus)
        );
        assert_eq!(
            resolve_develop_action(key(KeyCode::Esc, KeyModifiers::NONE), p, false),
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
}
