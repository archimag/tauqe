//! Terminal input dispatch: keys, pointer events, and pasted text.
//!
//! The module is the single entry point the event loop calls for every terminal
//! event. A key press is normalized once — the Emacs `C-g` chord becomes `Esc` —
//! then routed to the top-most modal, then to the global command map, and finally
//! to the handler of the active view. The surface that owns the keyboard is
//! therefore always the surface that receives the keystroke, and an open dialog
//! can never leak an event into the view below it (Conventions §3, §5, §7).
//!
//! Every binding is declared once as a
//! [`CommandSpec`](crate::command::CommandSpec) and resolved through its map, so
//! this module carries no ad-hoc shortcut comparison of its own.

pub mod layout;
pub mod mouse;
pub mod squash;

use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tauqe_protocol::{methods, GitSquashPreviewParams, HistoryGetParams};
use tokio::sync::Mutex;

use crate::app::{
    ActiveModal, AppState, BatchCancelButton, ConfirmDialogButton, HelpTab, SquashDialogState,
    ViewMode,
};
use crate::command::global::{GlobalCommand, GLOBAL_COMMAND_MAP};
use crate::config::{InputConfig, TuiConfig};
use crate::editor::InputEditor;
use crate::i18n::active_t;
use crate::rpc::{send_request, RpcConnection};
use crate::views::{context, history, onboarding, plans, repl, review};

pub use crate::components::editor::is_char_typing;

/// Outcome of handling one terminal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputResult {
    /// The application keeps running.
    Continue,
    /// The developer asked to quit.
    Exit,
}

/// Reports whether the key event is the canonical Emacs keyboard-quit (`C-g`).
///
/// The chord is recognized before any handler runs because it is normalized to
/// `Esc` and never dispatched as a command of its own (Conventions §5.5). The
/// AltGr compose chord (`Ctrl+Alt`) is excluded, so a composed character never
/// aborts a running operation.
pub fn is_keyboard_quit(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('g'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

/// Routes one key event into the shared text editor.
///
/// `multiline` selects the field shape: the prompt, a discussion comment, and
/// the squash message are multiline buffers, while a path or a language field
/// stays a single-line input whose `Enter` confirms the dialog instead of
/// inserting a newline.
pub fn handle_editor_key(editor: &mut InputEditor, key: KeyEvent, is_cmd: bool, multiline: bool) {
    editor.multiline = multiline;
    editor.handle_key(key, is_cmd);
}

/// Opens the squash dialog and requests the preview of its default base.
pub async fn open_squash_dialog(
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    let params = {
        let mut st = state.lock().await;
        if st.model.is_busy() || st.review.running {
            st.notify_warning(active_t("notify.squash_busy"));
            return Ok(());
        }
        st.squash_dialog = Some(SquashDialogState::default());
        GitSquashPreviewParams { base_ref: None }
    };
    send_request(
        server_writer,
        methods::GIT_SQUASH_PREVIEW,
        serde_json::to_value(params)?,
    )
    .await?;
    Ok(())
}

/// Switches the active view and refreshes the data the target view renders.
///
/// The switch is a presentation transition, so the state lock is held for the
/// shortest possible window and released before any request leaves the
/// transport; a daemon that stopped draining its stdin therefore cannot freeze
/// the interface (c-1).
pub async fn switch_view(
    state: &Arc<Mutex<AppState>>,
    target: ViewMode,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    {
        let mut st = state.lock().await;
        st.view_mode = target;
        st.confirm_button = ConfirmDialogButton::Cancel;
    }

    match target {
        ViewMode::History => {
            let params = HistoryGetParams {
                limit: Some(10),
                before_id: None,
            };
            send_request(
                server_writer,
                methods::HISTORY_GET,
                serde_json::to_value(params)?,
            )
            .await?;
        }
        ViewMode::Plans => {
            send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
            send_request(server_writer, methods::PLAN_GET, serde_json::json!({})).await?;
        }
        ViewMode::Review => {
            send_request(server_writer, methods::REVIEW_LIST, serde_json::json!({})).await?;
            send_request(server_writer, methods::REVIEW_GET, serde_json::json!({})).await?;
        }
        ViewMode::Repl | ViewMode::Context | ViewMode::Onboarding => {}
    }
    Ok(())
}

/// Routes pasted text to the surface that currently owns the keyboard.
///
/// A modal receives the text in the field it exposes, and the Develop prompt
/// takes it when no modal is open. Every other surface ignores a paste, so a
/// bracketed paste can never mutate a view the developer is not editing.
pub fn handle_paste(st: &mut AppState, text: &str) {
    if text.is_empty() {
        return;
    }
    match st.active_modal() {
        Some(ActiveModal::Squash) => {
            if let Some(dialog) = st.squash_dialog.as_mut() {
                if dialog.custom_input_active {
                    dialog.custom_editor.insert_paste_single_line(text);
                } else {
                    dialog.message_editor.insert_paste(text);
                }
            }
        }
        Some(ActiveModal::Prompt) => {
            if let Some(modal) = st.prompt_modal.as_mut() {
                modal.prompt_editor.insert_paste(text);
            }
        }
        Some(ActiveModal::Review) => {
            if let Some(dialog) = st.review_dialog.as_mut() {
                if dialog.focus.is_path_editor() {
                    if let Some(editor) = dialog.active_path_editor_mut() {
                        editor.insert_paste_single_line(text);
                    }
                    dialog.refresh_path_completion();
                } else {
                    dialog.prompt_editor.insert_paste(text);
                }
            }
        }
        Some(ActiveModal::Locale) => {
            if let Some(dialog) = st.locale_dialog.as_mut() {
                dialog.language_editor.insert_paste_single_line(text);
            }
        }
        Some(_) => {}
        None => {
            if st.view_mode == ViewMode::Repl {
                st.input_editor.insert_paste(text);
            }
        }
    }
}

/// Handles one terminal event and reports whether the interface keeps running.
pub async fn handle_terminal_event(
    event: Event,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    match event {
        Event::Key(key) => handle_key_event(key, state, server_writer).await,
        Event::Mouse(mouse) => mouse::handle_mouse_event(mouse, state, server_writer).await,
        Event::Paste(text) => {
            let mut st = state.lock().await;
            handle_paste(&mut st, &text);
            Ok(InputResult::Continue)
        }
        Event::FocusGained => {
            state.lock().await.terminal_focused = true;
            Ok(InputResult::Continue)
        }
        Event::FocusLost => {
            state.lock().await.terminal_focused = false;
            Ok(InputResult::Continue)
        }
        // A resize is redrawn by the event loop itself, and any future event
        // kind is inert until the interface subscribes to it.
        _ => Ok(InputResult::Continue),
    }
}

/// Applies the canonical dispatch order to one key press.
async fn handle_key_event(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(InputResult::Continue);
    }

    // `C-g` is the universal abort chord: normalize it once, before any view or
    // modal handler runs, so every surface reacts exactly as it reacts to `Esc`
    // (Conventions §5.5).
    let key = if is_keyboard_quit(&key) {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    } else {
        key
    };

    let (modal_open, disconnected, command, mode) = {
        let st = state.lock().await;
        let input = input_config(&st);
        let command = GLOBAL_COMMAND_MAP
            .resolve(key, &input, text_input_active(&st))
            .map(|spec| spec.action);
        (
            st.active_modal().is_some(),
            st.server_disconnected.is_some(),
            command,
            st.view_mode,
        )
    };

    // A disconnected daemon leaves the interface read-only: the card offers the
    // quit chords and nothing else (Conventions §6).
    if disconnected {
        let quits = matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
            || matches!(command, Some(GlobalCommand::Quit));
        return Ok(if quits {
            InputResult::Exit
        } else {
            InputResult::Continue
        });
    }

    if modal_open {
        return crate::dialogs::input::handle_dialog_event(key, state, server_writer)
            .await
            .map(|handled| handled.unwrap_or(InputResult::Continue));
    }

    if let Some(command) = command {
        return execute_global_command(command, state, server_writer).await;
    }

    match mode {
        ViewMode::Repl => repl::input::handle_repl_key(key, state, server_writer).await,
        ViewMode::Plans => plans::input::handle_plans_key(key, state, server_writer).await,
        ViewMode::Review => review::input::handle_review_key(key, state, server_writer).await,
        ViewMode::Context => context::input::handle_context_key(key, state, server_writer).await,
        ViewMode::History => history::input::handle_history_key(key, state, server_writer).await,
        ViewMode::Onboarding => {
            onboarding::input::handle_onboarding_key(key, state, server_writer).await
        }
    }
}

/// Executes one resolved global command.
async fn execute_global_command(
    command: GlobalCommand,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    match command {
        GlobalCommand::Quit => {
            let mut st = state.lock().await;
            if st.server_disconnected.is_some() {
                return Ok(InputResult::Exit);
            }
            st.confirm_quit = true;
            st.confirm_button = ConfirmDialogButton::Cancel;
        }
        // The chord is normalized to `Esc` before dispatch, so this arm is
        // unreachable in practice; the binding stays declared so the Help
        // surface and the dispatcher read one vocabulary (Conventions §5.5).
        GlobalCommand::KeyboardQuit => {}
        GlobalCommand::CancelGeneration => {
            let mut st = state.lock().await;
            cancel_active_operation(&mut st);
        }
        GlobalCommand::SquashCommits => {
            open_squash_dialog(state, server_writer).await?;
        }
        GlobalCommand::SwitchView(target) => {
            switch_view(state, target, server_writer).await?;
        }
        GlobalCommand::SelectModel => {
            let mut st = state.lock().await;
            if st.model.is_busy() {
                st.notify_warning(active_t("notify.model_select_busy"));
            } else {
                st.open_model_dialog();
            }
        }
        GlobalCommand::SelectTheme => {
            state.lock().await.open_theme_selector();
        }
        GlobalCommand::SelectLocale => {
            state.lock().await.open_locale_dialog();
        }
        GlobalCommand::OpenUsageSummary => {
            state.lock().await.open_usage_summary();
        }
        GlobalCommand::ToggleReasoning => {
            let mut st = state.lock().await;
            st.model.reasoning.toggle_fold();
            st.model.update_reasoning_markdown();
        }
        GlobalCommand::OpenHelp => {
            let mut st = state.lock().await;
            let open = !st.show_help;
            st.show_help = open;
            st.help_scroll = 0;
            if open {
                st.help_tab = HelpTab::Global;
            }
        }
        GlobalCommand::ReloadConfig => reload_configuration(state).await,
    }
    Ok(InputResult::Continue)
}

/// Raises the confirmation that interrupts whatever owns the operation slot.
///
/// A running batch gets the three-way dialog whose safe default is `Continue`;
/// a single turn or a review gets the binary confirmation whose default is
/// `Cancel` (Conventions §6.3, §7).
fn cancel_active_operation(st: &mut AppState) {
    if st.model.is_batch && st.model.is_busy() {
        st.confirm_batch_cancel = true;
        st.batch_cancel_button = BatchCancelButton::Continue;
    } else if st.model.is_busy() || st.review.running {
        st.confirm_cancel = true;
        st.confirm_button = ConfirmDialogButton::Cancel;
    }
}

/// Reloads the workstation configuration from disk and repaints the interface.
async fn reload_configuration(state: &Arc<Mutex<AppState>>) {
    let mut st = state.lock().await;
    match TuiConfig::load_checked() {
        Ok(config) => {
            st.tui_config = config;
            st.sync_theme_mode();
            st.notify_success(active_t("notify.config_reloaded"));
        }
        Err(err) => {
            st.notify_error(active_t("notify.config_reload_error").replace("{}", &err.to_string()));
        }
    }
}

/// Modifier configuration the shared command maps resolve their chords with.
fn input_config(st: &AppState) -> InputConfig {
    // Forward the whole workstation input configuration verbatim: rebuilding it
    // field by field silently dropped `layout` and `langmap`, leaving the shared
    // command maps unaware of the configured keyboard layout.
    st.tui_config.input.clone()
}

/// Whether a text buffer currently captures printable characters.
///
/// The flag neutralizes bare structural chords (such as the bare `?` that opens
/// Help) while the developer is typing, so the unconditional character
/// insertion invariant holds (Conventions §5.1).
fn text_input_active(st: &AppState) -> bool {
    match st.view_mode {
        ViewMode::Repl => true,
        ViewMode::Onboarding => st.onboarding.input_active,
        ViewMode::Context => st.context_view.adding_file,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_g_is_the_keyboard_quit_and_altgr_is_not() {
        assert!(is_keyboard_quit(&KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_keyboard_quit(&KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
        assert!(!is_keyboard_quit(&KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn paste_reaches_the_develop_prompt_and_no_other_view() {
        let mut develop = crate::app::test_state();
        develop.view_mode = ViewMode::Repl;
        handle_paste(&mut develop, "док");
        assert_eq!(develop.input_editor.get_text(), "док");

        let mut plans_view = crate::app::test_state();
        plans_view.view_mode = ViewMode::Plans;
        handle_paste(&mut plans_view, "док");
        assert!(plans_view.input_editor.is_empty());
    }
    #[test]
    fn input_config_forwards_layout_and_langmap_from_workstation_config() {
        use crate::config::{LayoutPreset, MetaModifier, PrimaryModifier};

        let mut st = crate::app::test_state();
        st.tui_config.input.layout = LayoutPreset::RuJcuken;
        st.tui_config.input.langmap = Some("йцукен".to_string());
        st.tui_config.input.primary_modifier = PrimaryModifier::Alt;
        st.tui_config.input.meta_modifier = MetaModifier::Ctrl;

        let input = input_config(&st);
        assert_eq!(input.layout, LayoutPreset::RuJcuken);
        assert_eq!(input.langmap.as_deref(), Some("йцукен"));
        assert_eq!(input.primary_modifier, PrimaryModifier::Alt);
        assert_eq!(input.meta_modifier, MetaModifier::Ctrl);
        assert!(input.is_primary(KeyModifiers::ALT));
        assert!(input.is_meta(KeyModifiers::CONTROL));
    }

    #[test]
    fn input_config_tracks_the_selected_layout_preset() {
        use crate::config::LayoutPreset;

        let mut st = crate::app::test_state();
        st.tui_config.input.layout = LayoutPreset::None;
        assert_eq!(input_config(&st).layout, LayoutPreset::None);

        st.tui_config.input.layout = LayoutPreset::RuJcuken;
        assert_eq!(input_config(&st).layout, LayoutPreset::RuJcuken);
    }
    /// Runs the end-to-end JCUKEN scenario through the dispatcher inputs.
    ///
    /// The workstation state carries the selected layout, the dispatcher derives
    /// the `InputConfig` from it, and the shared command maps resolve the
    /// physical press: a Cyrillic key reaches the navigation and execution
    /// commands of a structural view, while in the Develop prompt the very same
    /// key is a literal character that never reaches a command (c-1, c-3, c-4).
    #[test]
    fn ru_jcuken_scenario_reaches_structural_actions_and_stays_literal_in_the_prompt() {
        use crate::config::LayoutPreset;
        use crate::views::context::input::{ContextAction, CONTEXT_COMMAND_MAP};
        use crate::views::plans::input::{PlansAction, PLANS_COMMAND_MAP};
        use crate::views::repl::input::REPL_COMMAND_MAP;
        use crate::views::review::input::{ReviewAction, REVIEW_COMMAND_MAP};

        let mut st = crate::app::test_state();
        st.tui_config.input.layout = LayoutPreset::RuJcuken;

        // `т`, `з`, and `ч` occupy the `n`, `p`, and `x` positions of JCUKEN.
        st.view_mode = ViewMode::Plans;
        assert!(!text_input_active(&st), "Plans captures no text");
        let input = input_config(&st);
        let structural = text_input_active(&st);
        for (cyrillic, expected) in [
            ('т', PlansAction::NavNext),
            ('з', PlansAction::NavPrev),
            ('ч', PlansAction::Execute),
        ] {
            let resolved = PLANS_COMMAND_MAP
                .resolve(
                    KeyEvent::new(KeyCode::Char(cyrillic), KeyModifiers::NONE),
                    &input,
                    structural,
                )
                .map(|spec| spec.action);
            assert_eq!(
                resolved,
                Some(expected),
                "Plans: {cyrillic:?} must resolve to {expected:?}"
            );
        }

        st.view_mode = ViewMode::Review;
        assert!(!text_input_active(&st), "Review captures no text");
        let input = input_config(&st);
        let structural = text_input_active(&st);
        for (cyrillic, expected) in [
            ('т', ReviewAction::NavNext),
            ('з', ReviewAction::NavPrev),
            ('ч', ReviewAction::Execute),
        ] {
            let resolved = REVIEW_COMMAND_MAP
                .resolve(
                    KeyEvent::new(KeyCode::Char(cyrillic), KeyModifiers::NONE),
                    &input,
                    structural,
                )
                .map(|spec| spec.action);
            assert_eq!(
                resolved,
                Some(expected),
                "Review: {cyrillic:?} must resolve to {expected:?}"
            );
        }

        st.view_mode = ViewMode::Context;
        st.context_view.adding_file = false;
        assert!(!text_input_active(&st), "the Context list captures no text");
        let input = input_config(&st);
        let structural = text_input_active(&st);
        for (cyrillic, expected) in [('т', ContextAction::NavNext), ('з', ContextAction::NavPrev)]
        {
            let resolved = CONTEXT_COMMAND_MAP
                .resolve(
                    KeyEvent::new(KeyCode::Char(cyrillic), KeyModifiers::NONE),
                    &input,
                    structural,
                )
                .map(|spec| spec.action);
            assert_eq!(
                resolved,
                Some(expected),
                "Context: {cyrillic:?} must resolve to {expected:?}"
            );
        }

        // The same physical presses are typed into the prompt instead of being
        // dispatched, so the unconditional insertion invariant holds (c-3).
        st.view_mode = ViewMode::Repl;
        assert!(
            text_input_active(&st),
            "the Develop prompt captures printable characters"
        );
        let input = input_config(&st);
        let capturing = text_input_active(&st);
        for cyrillic in ['т', 'з', 'ч'] {
            let resolved = REPL_COMMAND_MAP
                .resolve(
                    KeyEvent::new(KeyCode::Char(cyrillic), KeyModifiers::NONE),
                    &input,
                    capturing,
                )
                .map(|spec| spec.action);
            assert!(
                resolved.is_none(),
                "Develop: {cyrillic:?} must be typed into the prompt, not dispatched"
            );
        }

        // The buffer keeps the original characters: the layout translation lives
        // strictly inside chord matching and never rewrites typed text (c-3).
        handle_paste(&mut st, "тзч");
        assert_eq!(st.input_editor.get_text(), "тзч");
    }
}
