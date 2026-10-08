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
        return dialog.custom_input_active || dialog.focus == SquashDialogFocus::MessageEditor;
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

/// Routes terminal paste events strictly by modal stack priority, ensuring background
/// editors are not polluted and active modal input targets receive the pasted text.
pub fn handle_paste(st: &mut AppState, text: &str) {
    // 1. Squash dialog modal layer
    if let Some(dialog) = st.squash_dialog.as_mut() {
        if dialog.custom_input_active {
            dialog.custom_input.push_str(text.trim());
        } else if dialog.focus == SquashDialogFocus::MessageEditor {
            dialog.message_buffer.push_str(text);
        }
        return;
    }

    // 2. Review dialog modal layer
    if let Some(dialog) = st.review_dialog.as_mut() {
        dialog.prompt.push_str(&text.replace(['\n', '\r'], " "));
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
            let has_layout_mapping = layout_preset != crate::config::LayoutPreset::None
                || custom_langmap.is_some();
            if has_layout_mapping
                && (!text_input_active
                    || primary_modifier.matches(key.modifiers)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT))
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
                if st.model.is_busy() || st.review.running {
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
            if let Some(res) = handle_global_shortcuts(key, state, server_writer).await? {
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
) -> anyhow::Result<Option<InputResult>> {
    let mut st = state.lock().await;
    let primary_mod = st.tui_config.input.primary_modifier;
    let is_primary = primary_mod.matches(key.modifiers);

    // Tab switching via F1..F5 (universal hardware fallback) or C-1..5 (Primary Command Modifier) or Alt+1..5
    let switch_tab = match key.code {
        KeyCode::F(1) => {
            if st.view_mode == ViewMode::Develop {
                st.show_help = true;
                st.help_scroll = 0;
                return Ok(Some(InputResult::Continue));
            } else {
                Some(ViewMode::Develop)
            }
        }
        KeyCode::F(2) => Some(ViewMode::Context),
        KeyCode::F(3) => Some(ViewMode::Review),
        KeyCode::F(4) => Some(ViewMode::Plans),
        KeyCode::F(5) => Some(ViewMode::History),
        KeyCode::Char('1') if is_primary || key.modifiers.contains(KeyModifiers::ALT) => {
            Some(ViewMode::Develop)
        }
        KeyCode::Char('2') if is_primary || key.modifiers.contains(KeyModifiers::ALT) => {
            Some(ViewMode::Context)
        }
        KeyCode::Char('3') if is_primary || key.modifiers.contains(KeyModifiers::ALT) => {
            Some(ViewMode::Review)
        }
        KeyCode::Char('4') if is_primary || key.modifiers.contains(KeyModifiers::ALT) => {
            Some(ViewMode::Plans)
        }
        KeyCode::Char('5') if is_primary || key.modifiers.contains(KeyModifiers::ALT) => {
            Some(ViewMode::History)
        }
        _ => None,
    };

    if let Some(target_mode) = switch_tab {
        if target_mode == ViewMode::History {
            st.view_mode = ViewMode::History;
            st.history_view.auto_scroll = true;
            if !st.history_view.items.is_empty() {
                st.history_view.selected_item_index =
                    st.history_view.items.len().saturating_sub(1);
            } else if !st.history_view.loading {
                st.history_view.loading = true;
                drop(st);
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
                return Ok(Some(InputResult::Continue));
            }
            st.context_view.status_message = None;
            return Ok(Some(InputResult::Continue));
        } else {
            st.view_mode = target_mode;
            st.context_view.status_message = None;
            return Ok(Some(InputResult::Continue));
        }
    }

    // Model selection dialog via C-M / C-Y (Primary Command Modifier) or Alt+M (universal terminal fallback)
    let is_model_shortcut = (is_primary
        && (key.code == KeyCode::Char('m') || key.code == KeyCode::Char('M') || key.code == KeyCode::Char('y')))
        || (key.modifiers.contains(KeyModifiers::ALT)
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
            st.selection_dialog = Some(crate::app::SelectionDialogState {
                kind: crate::app::SelectionDialogKind::Model,
                items: st.available_models.clone(),
                selected_index: cur_idx,
            });
            return Ok(Some(InputResult::Continue));
        }
    }

    let is_cancel_shortcut = (is_primary && (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('C')))
        || (key.modifiers.contains(KeyModifiers::CONTROL) && (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('C')));

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
                st.notify_info("Reloading configuration from disk...");
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

    if key.code == KeyCode::Char('?')
        && !st.context_view.adding_file
        && (st.view_mode != ViewMode::Develop || st.input_editor.is_empty())
    {
        st.show_help = true;
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
            squash_dialog: None,
            server_disconnected: None,
            server_log_path: std::path::PathBuf::from(".tauqe/server.log"),
            last_model_height: 10,
            header_clicks: HeaderClickAreas::default(),
        }
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
            st.squash_dialog.as_ref().unwrap().custom_input,
            "feature/my-branch"
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
            st.squash_dialog.as_ref().unwrap().message_buffer,
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
}
