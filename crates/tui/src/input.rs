use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, GitSquashPreviewParams};

use crate::app::{AppState, SquashDialogFocus, ViewMode};
use crate::rpc::send_request;

pub mod context;
pub mod develop;
pub mod dialogs;
pub mod history;
pub mod layout;
pub mod mouse;
pub mod onboarding;
pub mod plans;
pub mod review;
pub mod squash;

pub async fn open_squash_dialog(
    st: &mut AppState,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<()> {
    if st.model.is_busy() {
        st.notify_warning("Cannot squash commits while model is generating");
    } else {
        st.squash_dialog = Some(crate::app::SquashDialogState::default());
        let params = GitSquashPreviewParams { base_ref: None };
        send_request(
            server_writer,
            methods::GIT_SQUASH_PREVIEW,
            serde_json::to_value(params)?,
        )
        .await?;
    }
    Ok(())
}

/// Centralized view switching for both keyboard shortcuts and mouse clicks.
/// Prevents accidental onboarding bypass and handles lazy History data loading.
pub async fn switch_view(
    st: &mut AppState,
    target_mode: ViewMode,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<()> {
    if st.view_mode == ViewMode::Onboarding {
        st.notify_warning("Please complete initial setup first");
        return Ok(());
    }

    if st.view_mode == target_mode {
        return Ok(());
    }

    if target_mode == ViewMode::History {
        st.view_mode = ViewMode::History;
        st.history_view.auto_scroll = true;
        if !st.history_view.items.is_empty() {
            st.history_view.selected_item_index =
                st.history_view.items.len().saturating_sub(1);
        } else if !st.history_view.loading {
            st.history_view.loading = true;
            let params = tauqe_protocol::HistoryGetParams {
                limit: Some(20),
                before_id: None,
            };
            send_request(
                server_writer,
                methods::HISTORY_GET,
                serde_json::to_value(params)?,
            )
            .await?;
        }
    } else {
        st.view_mode = target_mode;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputResult {
    Continue,
    Exit,
}

/// Returns whether text input is actively being entered (where bare keystrokes must
/// be inserted literally without translation).
pub fn is_text_input_active(st: &AppState) -> bool {
    if st.server_disconnected.is_some() {
        return false;
    }
    if let Some(dialog) = &st.squash_dialog {
        return dialog.confirm.is_none()
            && !dialog.applying
            && (dialog.custom_input_active || dialog.focus == SquashDialogFocus::MessageEditor);
    }
    if st.review_dialog.is_some() {
        return true;
    }
    if st.show_help
        || st.confirm_undo
        || st.confirm_cancel
        || st.confirm_quit
        || st.confirm_clear_history
        || st.confirm_delete_plan.is_some()
        || st.selection_dialog.is_some()
        || st.status_dialog.is_some()
        || st.context_view.confirm_clear_auto
    {
        return false;
    }
    match st.view_mode {
        ViewMode::Develop => true,
        ViewMode::Context => st.context_view.adding_file,
        ViewMode::Onboarding => st.onboarding.input_active,
        ViewMode::Review | ViewMode::Plans | ViewMode::History => false,
    }
}

/// Returns whether the key event's modifiers correspond to normal character typing:
/// either no modifiers, Shift only, or AltGr (Ctrl+Alt / Ctrl+Alt+Shift) on Windows/X11.
pub fn is_char_typing(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty()
        || modifiers == KeyModifiers::SHIFT
        || modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT)
        || modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
}

/// Helper to remove the last word from a simple string buffer (equivalent to Ctrl+W).
pub fn pop_word_backward(buf: &mut String) {
    while buf.ends_with(char::is_whitespace) {
        buf.pop();
    }
    while let Some(c) = buf.chars().last() {
        if c.is_whitespace() {
            break;
        }
        buf.pop();
    }
}

/// Emacs and Readline keyboard handling for modal text editors (`InputEditor`).
/// Printable characters without command modifiers are unconditionally inserted.
pub fn handle_editor_key(
    editor: &mut crate::editor::InputEditor,
    key: KeyEvent,
    is_cmd: bool,
    multiline: bool,
) -> bool {
    let has_alt = key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::CONTROL);

    if is_cmd {
        if let KeyCode::Char(c) = key.code {
            match c.to_ascii_lowercase() {
                'w' => { editor.kill_word_backward(); return true; }
                'u' => { editor.kill_to_beginning_of_line(); return true; }
                'k' => { editor.kill_line(); return true; }
                'y' => { editor.yank(); return true; }
                'a' => { editor.move_beginning_of_line(); return true; }
                'e' => { editor.move_end_of_line(); return true; }
                'b' => { editor.move_backward(); return true; }
                'f' => { editor.move_forward(); return true; }
                'd' => { editor.delete_forward(); return true; }
                'j' if multiline => { editor.insert_char('\n'); return true; }
                _ => {}
            }
        }
    }

    if has_alt {
        match key.code {
            KeyCode::Char('b') | KeyCode::Left => { editor.move_word_backward(); return true; }
            KeyCode::Char('f') | KeyCode::Right => { editor.move_word_forward(); return true; }
            KeyCode::Char('d') => { editor.kill_word_forward(); return true; }
            KeyCode::Backspace => { editor.kill_word_backward(); return true; }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char(c) if is_char_typing(key.modifiers) => {
            editor.insert_char(c);
            true
        }
        KeyCode::Backspace => {
            editor.delete_backward();
            true
        }
        KeyCode::Delete => {
            editor.delete_forward();
            true
        }
        KeyCode::Left => {
            editor.move_backward();
            true
        }
        KeyCode::Right => {
            editor.move_forward();
            true
        }
        KeyCode::Home => {
            editor.move_beginning_of_line();
            true
        }
        KeyCode::End => {
            editor.move_end_of_line();
            true
        }
        KeyCode::Up if multiline => {
            editor.move_line_up();
            true
        }
        KeyCode::Down if multiline => {
            editor.move_line_down();
            true
        }
        KeyCode::Enter if multiline => {
            editor.insert_char('\n');
            true
        }
        _ => false,
    }
}

/// Routes terminal paste events strictly by modal stack priority, ensuring background
/// editors are not polluted and active modal input targets receive the pasted text.
pub fn handle_paste(st: &mut AppState, text: &str) {
    // 1. Squash dialog modal layer
    if let Some(dialog) = st.squash_dialog.as_mut() {
        if dialog.applying || dialog.confirm.is_some() {
            return;
        }
        if dialog.custom_input_active {
            dialog
                .custom_editor
                .insert_str(&text.trim().replace(['\n', '\r'], " "));
        } else if dialog.focus == SquashDialogFocus::MessageEditor {
            dialog.message_editor.insert_paste(text);
        }
        return;
    }

    // 2. Review dialog modal layer
    if let Some(dialog) = st.review_dialog.as_mut() {
        dialog.prompt_editor.insert_paste(text);
        return;
    }

    // 3. Blocking modals absorb paste without forwarding to background
    if st.show_help
        || st.confirm_undo
        || st.confirm_cancel
        || st.confirm_quit
        || st.confirm_clear_history
        || st.confirm_delete_plan.is_some()
        || st.selection_dialog.is_some()
        || st.status_dialog.is_some()
        || st.context_view.confirm_clear_auto
    {
        return;
    }

    // 4. View-specific active text targets
    match st.view_mode {
        ViewMode::Develop => {
            st.input_editor.insert_paste(text);
        }
        ViewMode::Context => {
            if st.context_view.adding_file {
                st.context_view.add_input.push_str(text.trim());
                st.context_view.selected_candidate_index = 0;
                st.update_filtered_candidates();
            }
        }
        ViewMode::Onboarding => {
            if st.onboarding.input_active {
                st.onboarding.input_buffer.push_str(text.trim());
            }
        }
        ViewMode::Review | ViewMode::Plans | ViewMode::History => {}
    }
}

pub async fn handle_terminal_event(
    terminal_event: crossterm::event::Event,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    match terminal_event {
        crossterm::event::Event::FocusGained => {
            let mut st = state.lock().await;
            st.terminal_focused = true;
            Ok(InputResult::Continue)
        }
        crossterm::event::Event::FocusLost => {
            let mut st = state.lock().await;
            st.terminal_focused = false;
            Ok(InputResult::Continue)
        }
        crossterm::event::Event::Paste(text) => {
            let mut st = state.lock().await;
            handle_paste(&mut st, &text);
            Ok(InputResult::Continue)
        }
        crossterm::event::Event::Mouse(mouse) => {
            mouse::handle_mouse_event(mouse, state, server_writer).await
        }
        crossterm::event::Event::Key(mut key)
            if key.kind != crossterm::event::KeyEventKind::Release =>
        {
            let (text_input_active, layout_preset, custom_langmap, primary_modifier) = {
                let st = state.lock().await;
                (
                    is_text_input_active(&st),
                    st.tui_config.input.layout,
                    st.tui_config.input.langmap.clone(),
                    st.tui_config.input.primary_modifier,
                )
            };

            // Translate characters only if a layout mapping or custom langmap is explicitly configured:
            // 1. Command modifiers (PrimaryModifier, Ctrl or Alt) are active, OR
            // 2. The user is in a non-text navigation mode (Vim style).
            let is_ctrl_alt = key.modifiers.contains(KeyModifiers::CONTROL)
                && key.modifiers.contains(KeyModifiers::ALT);
            let is_primary_mod = !is_ctrl_alt && primary_modifier.matches(key.modifiers);
            let has_layout_mapping = layout_preset != crate::config::LayoutPreset::None
                || custom_langmap.is_some();
            if has_layout_mapping
                && (!text_input_active
                    || (!is_char_typing(key.modifiers)
                        && (is_primary_mod
                            || key.modifiers.contains(KeyModifiers::CONTROL)
                            || key.modifiers.contains(KeyModifiers::ALT))))
            {
                if let KeyCode::Char(c) = key.code {
                    key.code = KeyCode::Char(layout::translate_char(
                        c,
                        layout_preset,
                        custom_langmap.as_deref(),
                    ));
                }
            }

            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
                let mut st = state.lock().await;
                if st.confirm_quit {
                    return Ok(InputResult::Exit);
                }
                let has_unsaved_work = st.model.is_busy()
                    || st.review.running
                    || !st.input_editor.is_empty()
                    || st.squash_dialog.as_ref().is_some_and(|d| {
                        d.applying || !d.message_editor.is_empty()
                    });
                if has_unsaved_work {
                    st.confirm_quit = true;
                    st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    return Ok(InputResult::Continue);
                }
                return Ok(InputResult::Exit);
            }

            if state.lock().await.server_disconnected.is_some() {
                if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc {
                    return Ok(InputResult::Exit);
                }
                return Ok(InputResult::Continue);
            }

            // 1. Modals & Dialogs (Squash, Confirmations, Model Picker, Help)
            if let Some(res) = dialogs::handle_dialog_event(key, state, server_writer).await? {
                return Ok(res);
            }

            // 2. Global application-level shortcuts
            if let Some(res) =
                handle_global_shortcuts(key, state, server_writer, text_input_active).await?
            {
                return Ok(res);
            }

            // 3. View-specific handlers
            let mode = { state.lock().await.view_mode };
            match mode {
                ViewMode::Develop => develop::handle_develop_key(key, state, server_writer).await,
                ViewMode::Context => context::handle_context_key(key, state, server_writer).await,
                ViewMode::Onboarding => onboarding::handle_onboarding_key(key, state, server_writer).await,
                ViewMode::Review => review::handle_review_key(key, state, server_writer).await,
                ViewMode::Plans => plans::handle_plans_key(key, state, server_writer).await,
                ViewMode::History => history::handle_history_key(key, state, server_writer).await,
            }
        }
        _ => Ok(InputResult::Continue),
    }
}

async fn handle_global_shortcuts(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
    text_input_active: bool,
) -> anyhow::Result<Option<InputResult>> {
    // Zero modal ambiguity (§6 Conventions): when typing into any active text buffer
    // (Develop prompt, Onboarding fields, Context file search, Squash editor), printable
    // characters typed without command modifiers (including Shift and AltGr) must be passed
    // directly to the text input handler without interference from global shortcuts.
    if text_input_active && is_char_typing(key.modifiers) {
        if let KeyCode::Char(_) = key.code {
            return Ok(None);
        }
    }

    let mut st = state.lock().await;
    let primary_mod = st.tui_config.input.primary_modifier;
    let is_ctrl_alt = key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT);
    let is_primary = !is_ctrl_alt && primary_mod.matches(key.modifiers);
    let is_alt_tab = key.modifiers == KeyModifiers::ALT;

    // Tab switching via F1..F5 (universal hardware fallback) or C-1..5 (Primary Command Modifier) or Alt+1..5
    let switch_tab = match key.code {
        KeyCode::F(1) => Some(ViewMode::Develop),
        KeyCode::F(2) => Some(ViewMode::Context),
        KeyCode::F(3) => Some(ViewMode::Review),
        KeyCode::F(4) => Some(ViewMode::Plans),
        KeyCode::F(5) => Some(ViewMode::History),
        KeyCode::Char('1') if is_primary || is_alt_tab => Some(ViewMode::Develop),
        KeyCode::Char('2') if is_primary || is_alt_tab => Some(ViewMode::Context),
        KeyCode::Char('3') if is_primary || is_alt_tab => Some(ViewMode::Review),
        KeyCode::Char('4') if is_primary || is_alt_tab => Some(ViewMode::Plans),
        KeyCode::Char('5') if is_primary || is_alt_tab => Some(ViewMode::History),
        _ => None,
    };

    if let Some(target_mode) = switch_tab {
        switch_view(&mut st, target_mode, server_writer).await?;
        return Ok(Some(InputResult::Continue));
    }

    // Model selection dialog via C-M (Primary Command Modifier) or Alt+M (universal terminal fallback)
    let is_alt_only = key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::CONTROL);
    let is_model_shortcut = (is_primary
        && (key.code == KeyCode::Char('m') || key.code == KeyCode::Char('M')))
        || (is_alt_only
            && (key.code == KeyCode::Char('m') || key.code == KeyCode::Char('M')));

    if is_model_shortcut {
        if st.model.is_busy() {
            st.notify_warning("Cannot change model while model is generating");
            return Ok(Some(InputResult::Continue));
        }
        if !st.available_models.is_empty() {
            let cur_idx = st
                .available_models
                .iter()
                .position(|m| m == &st.active_model)
                .unwrap_or(0);
            st.selection_dialog = Some(crate::app::SelectionDialogState::new(
                crate::app::SelectionDialogKind::Model,
                st.available_models.clone(),
                cur_idx,
            ));
            return Ok(Some(InputResult::Continue));
        }
    }

    let is_cancel_shortcut = !is_ctrl_alt
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('C'));

    if is_cancel_shortcut {
        if st.model.is_busy() || st.review.running {
            st.confirm_cancel = true;
        } else {
            st.notify_info("Press Ctrl+Q to quit TAUQE, or ? for help");
        }
        return Ok(Some(InputResult::Continue));
    }

    if is_primary {
        match key.code {
            KeyCode::Char('o') => {
                match crate::config::TuiConfig::load_checked() {
                    Ok(cfg) => {
                        st.tui_config = cfg;
                        st.notify_info("Reloading configuration from disk...");
                    }
                    Err(err) => {
                        st.notify_error(format!("{}; keeping previous TUI settings", err));
                    }
                }
                drop(st);
                send_request(server_writer, methods::CONFIG_RELOAD, serde_json::json!({})).await?;
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Char('l') => {
                if st.model.is_busy() {
                    st.notify_warning("Cannot clear history while model is generating");
                } else {
                    st.confirm_clear_history = true;
                }
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Char('r') => {
                match st.view_mode {
                    ViewMode::Review => {
                        st.review.reasoning.toggle_fold();
                    }
                    _ => {
                        st.model.reasoning.toggle_fold();
                        let h = st.last_model_height;
                        st.model.clamp_scroll(h);
                    }
                }
                return Ok(Some(InputResult::Continue));
            }
            _ => {}
        }
    }

    if key.code == KeyCode::F(6) {
        open_squash_dialog(&mut st, server_writer).await?;
        return Ok(Some(InputResult::Continue));
    }

    // Help trigger:
    // - In text input mode: C-h (or Alt+h / Ctrl+h) or C-?
    // - In non-text navigation mode: bare '?', C-h, or C-?
    let is_c_h = (is_primary || is_alt_only || key.modifiers.contains(KeyModifiers::CONTROL))
        && (key.code == KeyCode::Char('h') || key.code == KeyCode::Char('H'));

    let is_help_shortcut = if text_input_active {
        is_c_h || (is_primary && key.code == KeyCode::Char('?'))
    } else {
        is_c_h
            || (key.code == KeyCode::Char('?')
                && !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT))
            || (is_primary && key.code == KeyCode::Char('?'))
    };

    if is_help_shortcut {
        st.show_help = true;
        st.help_scroll = 0;
        return Ok(Some(InputResult::Continue));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::*;
    use tauqe_protocol::ModelRef;

    fn test_state() -> AppState {
        AppState {
            view_mode: ViewMode::Develop,
            protocol_version: "1.0".to_string(),
            repo_state: None,
            all_repo_files: Vec::new(),
            workflow: "git".to_string(),
            edit_protocol: "xml".to_string(),
            active_model: ModelRef::openrouter("test-model"),
            available_models: Vec::new(),
            available_workflows: Vec::new(),
            available_edit_protocols: Vec::new(),
            model: crate::ui::develop::DevelopView::default(),
            context: tauqe_protocol::ContextState::default(),
            context_view: crate::ui::context::ContextViewState::default(),
            history_view: crate::ui::history::HistoryViewState::default(),
            review: crate::ui::review::ReviewViewState::default(),
            plans_view: crate::ui::plans::PlansViewState::default(),
            review_dialog: None,
            notification: None,
            turn_started_at: None,
            onboarding: OnboardingState::default(),
            input_editor: crate::editor::InputEditor::default(),
            tui_config: crate::config::TuiConfig::default(),
            show_help: false,
            help_scroll: 0,
            confirm_cancel: false,
            confirm_quit: false,
            confirm_undo: false,
            confirm_clear_history: false,
            confirm_delete_plan: None,
            confirm_button: ConfirmDialogButton::Cancel,
            selection_dialog: None,
            status_dialog: None,
            squash_dialog: None,
            server_disconnected: None,
            server_log_path: std::path::PathBuf::from(".tauqe/server.log"),
            last_model_height: 10,
            header_clicks: HeaderClickAreas::default(),
            terminal_focused: true,
            shift_tip_shown: false,
        }
    }

    #[tokio::test]
    async fn test_focus_events_update_terminal_focus_state() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        assert!(state.lock().await.terminal_focused);

        let res = handle_terminal_event(crossterm::event::Event::FocusLost, &state, &mut stdin)
            .await
            .unwrap();
        assert_eq!(res, InputResult::Continue);
        assert!(!state.lock().await.terminal_focused);

        let res = handle_terminal_event(crossterm::event::Event::FocusGained, &state, &mut stdin)
            .await
            .unwrap();
        assert_eq!(res, InputResult::Continue);
        assert!(state.lock().await.terminal_focused);
    }

    #[test]
    fn test_handle_paste_routes_to_develop_editor() {
        let mut st = test_state();
        handle_paste(&mut st, "hello world");
        assert_eq!(st.input_editor.get_text(), "hello world");
    }

    #[test]
    fn test_handle_paste_blocked_by_confirm_cancel() {
        let mut st = test_state();
        st.confirm_cancel = true;
        handle_paste(&mut st, "should not appear");
        assert_eq!(st.input_editor.get_text(), "");
    }

    #[test]
    fn test_handle_paste_blocked_by_squash_modal_when_browsing() {
        let mut st = test_state();
        st.squash_dialog = Some(SquashDialogState::default());
        handle_paste(&mut st, "should not appear");
        assert_eq!(st.input_editor.get_text(), "");
    }

    #[test]
    fn test_handle_paste_into_squash_custom_input() {
        let mut st = test_state();
        let dialog = SquashDialogState {
            custom_input_active: true,
            ..Default::default()
        };
        st.squash_dialog = Some(dialog);
        handle_paste(&mut st, " feature/my-branch \n");
        assert_eq!(
            st.squash_dialog.as_ref().unwrap().custom_editor.get_text(),
            "feature/my-branch"
        );
        assert_eq!(st.input_editor.get_text(), "");
    }

    #[test]
    fn test_handle_paste_into_review_dialog_editor() {
        let mut st = test_state();
        let dialog = ReviewDialogState {
            files_count: 2,
            estimated_tokens: 100,
            models: vec![ModelRef::openrouter("test-model")],
            model_index: 0,
            prompt_editor: crate::editor::InputEditor::default(),
        };
        st.review_dialog = Some(dialog);
        handle_paste(&mut st, "focus on security and memory leaks");
        assert_eq!(
            st.review_dialog.as_ref().unwrap().prompt_editor.get_text(),
            "focus on security and memory leaks"
        );
        assert_eq!(st.input_editor.get_text(), "");
    }

    #[test]
    fn test_handle_paste_into_squash_message_editor() {
        let mut st = test_state();
        let dialog = SquashDialogState {
            focus: SquashDialogFocus::MessageEditor,
            ..Default::default()
        };
        st.squash_dialog = Some(dialog);
        handle_paste(&mut st, "feat: implement paste routing\n\nFull details.");
        assert_eq!(
            st.squash_dialog.as_ref().unwrap().message_editor.get_text(),
            "feat: implement paste routing\n\nFull details."
        );
        assert_eq!(st.input_editor.get_text(), "");
    }

    #[test]
    fn test_handle_paste_into_context_file_picker() {
        let mut st = test_state();
        st.view_mode = ViewMode::Context;
        st.context_view.adding_file = true;
        st.all_repo_files = vec!["crates/core/src/lib.rs".to_string()];
        handle_paste(&mut st, " crates/core/src/lib.rs ");
        assert_eq!(st.context_view.add_input, "crates/core/src/lib.rs");
        assert_eq!(st.context_view.selected_candidate_index, 0);
    }

    #[tokio::test]
    async fn test_bare_question_mark_does_not_open_help_in_empty_develop() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        let res = handle_global_shortcuts(key, &state, &mut stdin, true).await.unwrap();
        assert_eq!(res, None);
        assert!(!state.lock().await.show_help);
    }

    #[tokio::test]
    async fn test_bare_question_mark_opens_help_in_plans_view() {
        let mut st = test_state();
        st.view_mode = ViewMode::Plans;
        let state = Arc::new(Mutex::new(st));
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        let res = handle_global_shortcuts(key, &state, &mut stdin, false).await.unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.show_help);
    }

    #[tokio::test]
    async fn test_ctrl_h_opens_help_in_develop_view() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        let key = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        let res = handle_global_shortcuts(key, &state, &mut stdin, true).await.unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.show_help);
    }

    #[tokio::test]
    async fn test_f1_in_develop_does_not_open_help() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        let key = KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE);
        let res = handle_global_shortcuts(key, &state, &mut stdin, true).await.unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(!state.lock().await.show_help);
        assert_eq!(state.lock().await.view_mode, ViewMode::Develop);
    }

    #[tokio::test]
    async fn test_switch_view_blocks_leaving_onboarding() {
        let mut st = test_state();
        st.view_mode = ViewMode::Onboarding;
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        switch_view(&mut st, ViewMode::Develop, &mut stdin).await.unwrap();
        assert_eq!(st.view_mode, ViewMode::Onboarding);
        assert!(st.notification.is_some());
        let notif = st.notification.unwrap();
        assert_eq!(notif.level, NotificationLevel::Warning);
        assert!(notif.text.contains("Please complete initial setup first"));
    }

    #[tokio::test]
    async fn test_altgr_digits_do_not_switch_tabs_in_text_input() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        // AltGr on Windows/X11: CONTROL | ALT
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let key = KeyEvent::new(KeyCode::Char('2'), altgr);
        let res = handle_global_shortcuts(key, &state, &mut stdin, true).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(state.lock().await.view_mode, ViewMode::Develop);
    }

    #[tokio::test]
    async fn test_ctrl_q_with_prompt_draft_triggers_confirm_quit() {
        let mut st = test_state();
        st.input_editor.insert_str("important unsubmitted prompt");
        let state = Arc::new(Mutex::new(st));
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let res = handle_terminal_event(crossterm::event::Event::Key(key), &state, &mut stdin)
            .await
            .unwrap();
        assert_eq!(res, InputResult::Continue);
        assert!(state.lock().await.confirm_quit);
    }

    #[tokio::test]
    async fn test_ctrl_q_empty_prompt_and_idle_exits_directly() {
        let st = test_state();
        assert!(st.input_editor.is_empty());
        let state = Arc::new(Mutex::new(st));
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let res = handle_terminal_event(crossterm::event::Event::Key(key), &state, &mut stdin)
            .await
            .unwrap();
        assert_eq!(res, InputResult::Exit);
        assert!(!state.lock().await.confirm_quit);
    }

    #[tokio::test]
    async fn test_altgr_digits_do_not_switch_tabs_in_navigation_view() {
        let mut st = test_state();
        st.view_mode = ViewMode::Plans;
        let state = Arc::new(Mutex::new(st));
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let key = KeyEvent::new(KeyCode::Char('2'), altgr);
        let res = handle_global_shortcuts(key, &state, &mut stdin, false).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(state.lock().await.view_mode, ViewMode::Plans);
    }
}
