use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::Mutex;
use tauqe_protocol::{methods, GitSquashPreviewParams};

use crate::app::{ActiveModal, AppState, SquashDialogFocus, ViewMode};
use crate::command::global::{GlobalCommand, GLOBAL_COMMAND_MAP};
use crate::rpc::{send_request, RpcConnection};
use crate::views::{context, history, onboarding, plans, repl, review};

pub mod layout;
pub mod mouse;
pub mod squash;

pub async fn open_squash_dialog(
    st: &mut AppState,
    server_writer: &mut RpcConnection,
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
    server_writer: &mut RpcConnection,
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
    } else if target_mode == ViewMode::Review {
        st.view_mode = ViewMode::Review;
        if st.review.sessions.is_empty() {
            send_request(
                server_writer,
                methods::REVIEW_LIST,
                serde_json::json!({}),
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
    // Derive the answer from the canonical modal ordering: only the surfaces that
    // actually own a live text editor report an active text input.
    match st.active_modal() {
        Some(ActiveModal::Squash) => st.squash_dialog.as_ref().is_some_and(|dialog| {
            dialog.confirm.is_none()
                && !dialog.applying
                && (dialog.custom_input_active || dialog.focus == SquashDialogFocus::MessageEditor)
        }),
        Some(ActiveModal::Prompt) => {
            st.prompt_modal.as_ref().is_some_and(|modal| modal.focus.is_editor())
        }
        Some(ActiveModal::Review) => st
            .review_dialog
            .as_ref()
            .is_some_and(|d| d.focus.is_prompt_editor() || d.focus.is_path_editor()),
        Some(ActiveModal::RefineDialog) => {
            st.plans_view.refine_dialog.as_ref().is_some_and(|d| d.focus.is_instructions_editor())
        }
        Some(ActiveModal::ReviewRefine) => st
            .review
            .refine_dialog
            .as_ref()
            .is_some_and(|d| d.focus.is_instructions_editor()),
        Some(ActiveModal::Locale) => st
            .locale_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.focus.is_editor()),
        Some(_) => false,
        None => match st.view_mode {
            ViewMode::Repl => true,
            ViewMode::Context => st.context_view.adding_file,
            ViewMode::Onboarding => st.onboarding.input_active,
            ViewMode::Review | ViewMode::Plans | ViewMode::History => false,
        },
    }
}

/// Returns whether the key event's modifiers correspond to normal character typing:
/// either no modifiers, Shift only, or AltGr (Ctrl+Alt / Ctrl+Alt+Shift) on Windows/X11.
pub use crate::components::editor::is_char_typing;

/// Returns whether the key event is the canonical keyboard-quit chord: `C-g`
/// (Emacs `keyboard-quit`, ASCII BEL 0x07). The TUI treats it exactly like
/// `Esc`, so one chord always aborts the running operation or closes the active
/// surface without intercepting the system `SIGINT` signal.
pub fn is_keyboard_quit(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL)
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
    editor.multiline = multiline;
    editor.handle_key(key, is_cmd)
}

/// Routes terminal paste events strictly by modal stack priority, ensuring background
/// editors are not polluted and active modal input targets receive the pasted text.
pub fn handle_paste(st: &mut AppState, text: &str) {
    // Route paste through the canonical modal ordering so it can never leak into a
    // background editor that is not the visible, top-most surface.
    match st.active_modal() {
        // Text-editing modals receive the pasted text directly.
        Some(ActiveModal::Squash) => {
            if let Some(dialog) = st.squash_dialog.as_mut() {
                if dialog.applying || dialog.confirm.is_some() {
                    return;
                }
                if dialog.custom_input_active {
                    dialog.custom_editor.insert_paste_single_line(text);
                } else if dialog.focus == SquashDialogFocus::MessageEditor {
                    dialog.message_editor.insert_paste(text);
                }
            }
        }
        Some(ActiveModal::Review) => {
            if let Some(dialog) = st.review_dialog.as_mut() {
                // Paste targets the field that owns the keyboard, so a pasted
                // path never lands in the prompt editor and vice versa.
                if dialog.focus.is_path_editor() {
                    dialog.path_editor.insert_paste_single_line(text);
                } else {
                    dialog.prompt_editor.insert_paste(text);
                }
            }
        }
        Some(ActiveModal::Prompt) => {
            if let Some(modal) = st.prompt_modal.as_mut() {
                if modal.focus.is_editor() {
                    modal.prompt_editor.insert_paste(text);
                }
            }
        }
        Some(ActiveModal::RefineDialog) => {
            if let Some(dialog) = st.plans_view.refine_dialog.as_mut() {
                dialog.instructions_editor.insert_paste(text);
            }
        }
        Some(ActiveModal::ReviewRefine) => {
            if let Some(dialog) = st.review.refine_dialog.as_mut() {
                dialog.instructions_editor.insert_paste(text);
            }
        }
        Some(ActiveModal::Locale) => {
            if let Some(dialog) = st.locale_dialog.as_mut() {
                if dialog.focus.is_editor() {
                    dialog.language_editor.insert_paste_single_line(text);
                }
            }
        }
        // Blocking modals absorb paste without forwarding it to the background.
        Some(_) => {}
        // No modal: route to the active view's text target.
        None => match st.view_mode {
            ViewMode::Repl => {
                st.model.focus = repl::ReplFocus::Editor;
                st.input_editor.insert_paste(text);
            }
            ViewMode::Context => {
                if st.context_view.adding_file {
                    st.context_view.add_input.insert_paste_single_line(text);
                    st.context_view.selected_candidate_index = 0;
                    st.update_filtered_candidates();
                }
            }
            ViewMode::Onboarding => {
                if st.onboarding.input_active {
                    st.onboarding.input_buffer.insert_paste_single_line(text);
                }
            }
            ViewMode::Review | ViewMode::Plans | ViewMode::History => {}
        },
    }
}

pub async fn handle_terminal_event(
    terminal_event: crossterm::event::Event,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
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

            // C-g (keyboard-quit) is the canonical abort chord: normalize it to
            // `Esc` here so every view and modal reacts identically without a
            // per-handler binding (Conventions §5). It runs after layout
            // translation so a mapped key still resolves back to `C-g`.
            if is_keyboard_quit(&key) {
                key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
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
            if let Some(res) = crate::dialogs::input::handle_dialog_event(key, state, server_writer).await? {
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
                ViewMode::Repl => repl::handle_repl_key(key, state, server_writer).await,
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

/// Reloads the TUI configuration from disk and asks the server to reload its
/// own configuration, keeping both sides in sync. Shared by the workspace and
/// onboarding configuration-reload shortcuts.
async fn reload_configuration(
    st: &mut AppState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    match crate::config::TuiConfig::load_checked() {
        Ok(cfg) => {
            st.tui_config = cfg;
            st.sync_theme_mode();
            st.notify_info("Reloading configuration from disk...");
        }
        Err(err) => {
            st.notify_error(format!("{}; keeping previous TUI settings", err));
        }
    }
    send_request(server_writer, methods::CONFIG_RELOAD, serde_json::json!({})).await?;
    Ok(())
}

async fn handle_global_shortcuts(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
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

    // Resolve the key once against the single source of truth for global
    // bindings. The command map already blocks bare structural chords while a
    // text editor is active, so the unconditional character insertion
    // invariant holds without any per-key branching here (Conventions §3, §5).
    let Some(command) = GLOBAL_COMMAND_MAP
        .resolve(key, &st.tui_config.input, text_input_active)
        .map(|spec| spec.action)
    else {
        return Ok(None);
    };

    // Onboarding is a modal first-run flow. Workspace-level shortcuts (tab
    // switching, model picker, squash, reasoning toggle, cancel) must not fire
    // before setup completes: for example the squash chord would issue a squash
    // request with no repository. Only Help and configuration reload stay
    // available, because the onboarding footer advertises them.
    if st.view_mode == ViewMode::Onboarding
        && !matches!(command, GlobalCommand::OpenHelp | GlobalCommand::ReloadConfig)
    {
        return Ok(None);
    }

    match command {
        GlobalCommand::OpenHelp => {
            st.show_help = true;
            st.help_scroll = 0;
        }
        GlobalCommand::ReloadConfig => {
            reload_configuration(&mut st, server_writer).await?;
        }
        GlobalCommand::ToggleReasoning => {
            st.model.reasoning.toggle_fold();
            let h = st.last_model_height;
            st.model.clamp_scroll(h);
        }
        GlobalCommand::SwitchView(target_mode) => {
            switch_view(&mut st, target_mode, server_writer).await?;
        }
        GlobalCommand::SquashCommits => {
            open_squash_dialog(&mut st, server_writer).await?;
        }
        GlobalCommand::SelectTheme => {
            st.open_theme_selector();
        }
        GlobalCommand::SelectLocale => {
            st.open_locale_dialog();
        }
        GlobalCommand::SelectModel => {
            if st.model.is_busy() {
                st.notify_warning("Cannot change model while model is generating");
            } else if !st.open_model_dialog() {
                return Ok(None);
            }
        }
        GlobalCommand::CancelGeneration => {
            if st.model.is_batch && st.model.is_busy() {
                // A batch supervisor runs a bounded queue of isolated turns: offer
                // the three-way choice between continuing, stopping after the
                // current member, and aborting immediately instead of a plain
                // interrupt (Conventions.md §7).
                st.confirm_batch_cancel = true;
                st.batch_cancel_button = crate::app::BatchCancelButton::Continue;
            } else if st.model.is_busy() || st.review.running {
                st.confirm_cancel = true;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            } else {
                st.notify_info("Press Ctrl+Q to quit TAUQE, or ? for help");
            }
        }
        // Quit and keyboard-quit are consumed earlier in `handle_terminal_event`
        // (Ctrl+Q handling and the C-g normalization); the arms only satisfy the
        // exhaustive match over the global command set.
        GlobalCommand::Quit | GlobalCommand::KeyboardQuit => return Ok(None),
    }

    Ok(Some(InputResult::Continue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::*;
    use tauqe_protocol::ModelRef;

    #[tokio::test]
    async fn test_focus_events_update_terminal_focus_state() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        assert!(state.lock().await.terminal_focused);

        let res = handle_terminal_event(crossterm::event::Event::FocusLost, &state, &mut conn)
            .await
            .unwrap();
        assert_eq!(res, InputResult::Continue);
        assert!(!state.lock().await.terminal_focused);

        let res = handle_terminal_event(crossterm::event::Event::FocusGained, &state, &mut conn)
            .await
            .unwrap();
        assert_eq!(res, InputResult::Continue);
        assert!(state.lock().await.terminal_focused);
    }

    #[test]
    fn test_is_keyboard_quit_only_matches_ctrl_g() {
        assert!(is_keyboard_quit(&KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_keyboard_quit(&KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::NONE
        )));
        assert!(!is_keyboard_quit(&KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL
        )));
    }

    #[tokio::test]
    async fn test_ctrl_g_closes_active_modal_like_esc() {
        let mut st = test_state();
        st.show_help = true;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        let res = handle_terminal_event(crossterm::event::Event::Key(key), &state, &mut conn)
            .await
            .unwrap();
        assert_eq!(res, InputResult::Continue);
        assert!(!state.lock().await.show_help);
    }

    #[test]
    fn test_handle_paste_routes_to_develop_editor() {
        let mut st = test_state();
        handle_paste(&mut st, "hello world");
        assert_eq!(st.input_editor.get_text(), "hello world");
    }

    #[test]
    fn test_handle_paste_into_develop_returns_focus_to_editor() {
        let mut st = test_state();
        st.model.focus = crate::views::repl::ReplFocus::Viewport;
        handle_paste(&mut st, "hello world");
        assert!(matches!(
            &st.model.focus,
            crate::views::repl::ReplFocus::Editor
        ));
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
    fn test_is_text_input_active_in_prompt_modal() {
        let mut st = test_state();
        st.view_mode = ViewMode::Review;
        assert!(!is_text_input_active(&st));

        st.prompt_modal = Some(PromptModalState::new(PromptModalTarget::Review {
            review_id: "test".to_string(),
            review_title: "Test".to_string(),
            focused_items: Vec::new(),
        }));
        assert!(is_text_input_active(&st));

        st.prompt_modal.as_mut().unwrap().focus = ModalFocus::Actions;
        assert!(!is_text_input_active(&st));
    }

    #[test]
    fn test_handle_paste_into_prompt_modal() {
        let mut st = test_state();
        st.prompt_modal = Some(PromptModalState::new(PromptModalTarget::Review {
            review_id: "r1".to_string(),
            review_title: "Review 1".to_string(),
            focused_items: Vec::new(),
        }));
        handle_paste(&mut st, "pasted review comment");
        assert_eq!(
            st.prompt_modal.as_ref().unwrap().prompt_editor.get_text(),
            "pasted review comment"
        );
        assert_eq!(st.input_editor.get_text(), "");
    }

    #[test]
    fn test_is_text_input_active_in_plan_prompt_modal() {
        let mut st = test_state();
        st.view_mode = ViewMode::Plans;
        assert!(!is_text_input_active(&st));

        st.prompt_modal = Some(PromptModalState::new(PromptModalTarget::Plan {
            plan_id: "test".to_string(),
            plan_title: "Test".to_string(),
            focused_items: Vec::new(),
        }));
        assert!(is_text_input_active(&st));
    }

    #[test]
    fn test_handle_editor_key_newline_shortcuts() {
        let mut editor = crate::editor::InputEditor::default();
        editor.insert_str("line 1");

        // C-j inserts newline (electric-newline)
        let key_cj = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert!(handle_editor_key(&mut editor, key_cj, true, true));
        assert_eq!(editor.get_text(), "line 1\n");

        // Alt+Enter inserts newline
        let key_alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert!(handle_editor_key(&mut editor, key_alt_enter, false, true));
        assert_eq!(editor.get_text(), "line 1\n\n");

        // C-n must never insert a newline in multiline mode; it navigates one line down.
        let before = editor.get_text().to_string();
        let key_cn = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(handle_editor_key(&mut editor, key_cn, true, true));
        assert_eq!(editor.get_text(), before);
    }

    #[test]
    fn test_handle_editor_key_vertical_navigation_symmetry() {
        let mut editor = crate::editor::InputEditor::default();
        editor.insert_str("line 1\nline 2\nline 3");
        editor.cursor = 0;

        let original = editor.get_text().to_string();

        // C-n moves exactly one line down and leaves the buffer untouched.
        let key_cn = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(handle_editor_key(&mut editor, key_cn, true, true));
        assert_eq!(editor.cursor_line_col().0, 1);
        assert_eq!(editor.get_text(), original);

        // C-p moves back up, restoring strict C-p / C-n symmetry.
        let key_cp = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(handle_editor_key(&mut editor, key_cp, true, true));
        assert_eq!(editor.cursor_line_col().0, 0);
        assert_eq!(editor.get_text(), original);
    }

    #[test]
    fn test_handle_paste_into_plan_prompt_modal() {
        let mut st = test_state();
        st.view_mode = ViewMode::Plans;
        st.prompt_modal = Some(PromptModalState::new(PromptModalTarget::Plan {
            plan_id: "p1".to_string(),
            plan_title: "Plan 1".to_string(),
            focused_items: Vec::new(),
        }));
        handle_paste(&mut st, "русский текст с переносом\nвторая строка");
        assert_eq!(
            st.prompt_modal.as_ref().unwrap().prompt_editor.get_text(),
            "русский текст с переносом\nвторая строка"
        );
        assert_eq!(st.input_editor.get_text(), "");
    }

    #[test]
    fn test_handle_paste_into_review_dialog_editor() {
        let mut st = test_state();
        let dialog = ReviewDialogState::new(
            2,
            100,
            vec![tauqe_protocol::ModelEntry::new(
                ModelRef::openrouter("test-model"),
                None,
                None,
            )],
        );
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
        assert_eq!(st.context_view.add_input.get_text(), "crates/core/src/lib.rs");
        assert_eq!(st.context_view.selected_candidate_index, 0);
    }

    #[tokio::test]
    async fn test_bare_question_mark_does_not_open_help_in_empty_develop() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        let res = handle_global_shortcuts(key, &state, &mut conn, true).await.unwrap();
        assert_eq!(res, None);
        assert!(!state.lock().await.show_help);
    }

    #[tokio::test]
    async fn test_bare_question_mark_opens_help_in_plans_view() {
        let mut st = test_state();
        st.view_mode = ViewMode::Plans;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        let res = handle_global_shortcuts(key, &state, &mut conn, false).await.unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.show_help);
    }

    #[tokio::test]
    async fn test_ctrl_h_opens_help_in_develop_view() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        let res = handle_global_shortcuts(key, &state, &mut conn, true).await.unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.show_help);
    }

    #[tokio::test]
    async fn test_function_keys_are_ignored_by_global_shortcuts() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        for f in 1..=7 {
            let res = handle_global_shortcuts(
                KeyEvent::new(KeyCode::F(f), KeyModifiers::NONE),
                &state,
                &mut conn,
                true,
            )
            .await
            .unwrap();
            assert_eq!(res, None, "function key F{} must be ignored", f);
        }

        let st = state.lock().await;
        assert!(!st.show_help);
        assert_eq!(st.view_mode, ViewMode::Repl);
        assert!(st.squash_dialog.is_none());
        assert!(st.theme_selector.is_none());
    }

    #[tokio::test]
    async fn test_switch_view_blocks_leaving_onboarding() {
        let mut st = test_state();
        st.view_mode = ViewMode::Onboarding;
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        switch_view(&mut st, ViewMode::Repl, &mut conn).await.unwrap();
        assert_eq!(st.view_mode, ViewMode::Onboarding);
        assert!(st.notification.is_some());
        let notif = st.notification.unwrap();
        assert_eq!(notif.level, NotificationLevel::Warning);
        assert!(notif.text.contains("Please complete initial setup first"));
    }

    #[tokio::test]
    async fn test_altgr_digits_do_not_switch_tabs_in_text_input() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // AltGr on Windows/X11: CONTROL | ALT
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let key = KeyEvent::new(KeyCode::Char('2'), altgr);
        let res = handle_global_shortcuts(key, &state, &mut conn, true).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(state.lock().await.view_mode, ViewMode::Repl);
    }

    #[tokio::test]
    async fn test_ctrl_q_with_prompt_draft_triggers_confirm_quit() {
        let mut st = test_state();
        st.input_editor.insert_str("important unsubmitted prompt");
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let res = handle_terminal_event(crossterm::event::Event::Key(key), &state, &mut conn)
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
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let res = handle_terminal_event(crossterm::event::Event::Key(key), &state, &mut conn)
            .await
            .unwrap();
        assert_eq!(res, InputResult::Exit);
        assert!(!state.lock().await.confirm_quit);
    }

    #[tokio::test]
    async fn test_confirmation_modals_default_to_cancel_even_if_leaked() {
        let mut st = test_state();
        // A leaked non-default button focus must be forced back to the safe
        // Cancel whenever a global shortcut arms a confirmation.
        st.confirm_button = ConfirmDialogButton::Confirm;
        st.model.status = crate::views::repl::TurnStatus::Thinking;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // Cancel shortcut (Ctrl+C) when busy arms the confirmation and resets
        // the focused button to the safe Cancel.
        let key_ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let res = handle_global_shortcuts(key_ctrl_c, &state, &mut conn, true).await.unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.confirm_cancel);
        assert_eq!(state.lock().await.confirm_button, ConfirmDialogButton::Cancel);
    }

    #[tokio::test]
    async fn test_cancel_shortcut_opens_batch_dialog_for_running_batch() {
        let mut st = test_state();
        st.model.status = crate::views::repl::TurnStatus::Responding;
        st.model.is_batch = true;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let res = handle_global_shortcuts(key, &state, &mut conn, true)
            .await
            .unwrap();
        assert_eq!(res, Some(InputResult::Continue));

        let guard = state.lock().await;
        assert!(guard.confirm_batch_cancel);
        assert!(!guard.confirm_cancel);
        assert_eq!(guard.batch_cancel_button, crate::app::BatchCancelButton::Continue);
        assert!(!guard.batch_stopping);
    }

    #[tokio::test]
    async fn test_altgr_digits_do_not_switch_tabs_in_navigation_view() {
        let mut st = test_state();
        st.view_mode = ViewMode::Plans;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let key = KeyEvent::new(KeyCode::Char('2'), altgr);
        let res = handle_global_shortcuts(key, &state, &mut conn, false).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(state.lock().await.view_mode, ViewMode::Plans);
    }

    #[tokio::test]
    async fn test_global_shortcuts_blocked_during_onboarding() {
        let mut st = test_state();
        st.view_mode = ViewMode::Onboarding;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // The squash chord must not fire before a workspace exists.
        let key_cs = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let res = handle_global_shortcuts(key_cs, &state, &mut conn, false)
            .await
            .unwrap();
        assert_eq!(res, None);
        assert!(state.lock().await.squash_dialog.is_none());

        // Ctrl+L is no longer a global shortcut (history clearing is isolated
        // to the History view), so onboarding must stay unaffected by it.
        let key_ctrl_l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        let res = handle_global_shortcuts(key_ctrl_l, &state, &mut conn, false)
            .await
            .unwrap();
        assert_eq!(res, None);
        assert!(!state.lock().await.confirm_clear_history);

        // Tab switching is blocked too.
        let key_c2 = KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL);
        let res = handle_global_shortcuts(key_c2, &state, &mut conn, false)
            .await
            .unwrap();
        assert_eq!(res, None);
        assert_eq!(state.lock().await.view_mode, ViewMode::Onboarding);
    }

    #[tokio::test]
    async fn test_config_reload_shortcut_allowed_during_onboarding() {
        let mut st = test_state();
        st.view_mode = ViewMode::Onboarding;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        let res = handle_global_shortcuts(key, &state, &mut conn, false)
            .await
            .unwrap();
        assert_eq!(res, Some(InputResult::Continue));
    }

    #[tokio::test]
    async fn test_meta_t_opens_the_theme_selector() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT);
        let res = handle_global_shortcuts(key, &state, &mut conn, false)
            .await
            .unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.theme_selector.is_some());
    }

    #[tokio::test]
    async fn test_help_shortcut_allowed_during_onboarding() {
        let mut st = test_state();
        st.view_mode = ViewMode::Onboarding;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        let res = handle_global_shortcuts(key, &state, &mut conn, false)
            .await
            .unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.show_help);
    }

    #[tokio::test]
    async fn test_command_digits_switch_tabs() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // C-2 (Primary) switches to Plans.
        let res = handle_global_shortcuts(
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL),
            &state,
            &mut conn,
            false,
        )
        .await
        .unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert_eq!(state.lock().await.view_mode, ViewMode::Plans);

        // M-4 (Meta) switches to Context.
        let res = handle_global_shortcuts(
            KeyEvent::new(KeyCode::Char('4'), KeyModifiers::ALT),
            &state,
            &mut conn,
            false,
        )
        .await
        .unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert_eq!(state.lock().await.view_mode, ViewMode::Context);
    }

    #[tokio::test]
    async fn test_command_chords_open_squash_dialog() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // C-S (Primary).
        let res = handle_global_shortcuts(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &state,
            &mut conn,
            false,
        )
        .await
        .unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.squash_dialog.is_some());
    }

    #[tokio::test]
    async fn test_meta_s_opens_squash_dialog() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // M-S (Meta).
        let res = handle_global_shortcuts(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT),
            &state,
            &mut conn,
            false,
        )
        .await
        .unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.squash_dialog.is_some());
    }

    #[tokio::test]
    async fn test_meta_l_opens_the_locale_dialog() {
        let state = Arc::new(Mutex::new(test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // M-L (Meta) opens the client-only locale dialog.
        let res = handle_global_shortcuts(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT),
            &state,
            &mut conn,
            false,
        )
        .await
        .unwrap();
        assert_eq!(res, Some(InputResult::Continue));
        assert!(state.lock().await.locale_dialog.is_some());
    }

    fn review_refine_dialog_with_focus(
        focus: crate::views::plans::RefineDialogFocus,
    ) -> crate::views::review::ReviewRefineDialogState {
        let mut dialog = crate::views::review::ReviewRefineDialogState::new(
            "rev-1".to_string(),
            "Session rev-1".to_string(),
            2,
            "Missing auth guard".to_string(),
            Vec::new(),
        );
        dialog.focus = focus;
        dialog
    }

    #[test]
    fn test_is_text_input_active_in_review_refine_dialog() {
        let mut st = test_state();
        st.view_mode = ViewMode::Review;
        st.review.refine_dialog = Some(review_refine_dialog_with_focus(
            crate::views::plans::RefineDialogFocus::ModelList,
        ));
        assert!(!is_text_input_active(&st));

        st.review.refine_dialog = Some(review_refine_dialog_with_focus(
            crate::views::plans::RefineDialogFocus::InstructionsEditor,
        ));
        assert!(is_text_input_active(&st));

        st.review.refine_dialog = Some(review_refine_dialog_with_focus(
            crate::views::plans::RefineDialogFocus::Actions,
        ));
        assert!(!is_text_input_active(&st));
    }

    #[test]
    fn test_handle_paste_into_review_refine_instructions_editor() {
        let mut st = test_state();
        st.view_mode = ViewMode::Review;
        st.review.refine_dialog = Some(review_refine_dialog_with_focus(
            crate::views::plans::RefineDialogFocus::InstructionsEditor,
        ));

        handle_paste(&mut st, "split the finding into executable steps");

        assert_eq!(
            st.review
                .refine_dialog
                .as_ref()
                .expect("dialog stays open")
                .instructions_editor
                .get_text(),
            "split the finding into executable steps"
        );
        assert_eq!(st.input_editor.get_text(), "");
    }
}
