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
use crate::command::translate_code;
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

/// Projects one incoming key event through the configured keyboard layout.
///
/// This is the single layout gate of the interface. `handle_key_event` calls it
/// before the keyboard-quit chord, any modal, any global command, and any view
/// handler reads the event, so no surface can bypass the projection
/// (Conventions §3, §5).
///
/// The projection is governed by the capture boundary:
///
/// - the AltGr compose chord (`Ctrl+Alt`) is never projected, in either mode,
///   so a composed character never turns into a command;
/// - a character key carrying one command modifier (`Ctrl` or `Alt`) is always
///   projected, including while a text editor captures the keyboard, so the
///   `C-` and `M-` commands stay reachable while the developer types;
/// - a character key without a command modifier is projected only when no
///   editor captures the keyboard, which preserves the unconditional character
///   insertion invariant: a typed letter reaches the buffer exactly as pressed.
pub fn canonicalize_key_event(key: KeyEvent, input: &InputConfig, capturing: bool) -> KeyEvent {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // The AltGr / compose chord belongs to neither command modifier, so it is
    // never a command and is never projected (Conventions §5.2).
    if ctrl && alt {
        return key;
    }

    let code = translate_code(key.code, input);
    if code == key.code {
        return key;
    }

    // A character without a command modifier belongs to the text buffer while
    // an editor captures the keyboard, so it is inserted exactly as pressed.
    if capturing && !ctrl && !alt {
        return key;
    }

    let mut canonical = key;
    canonical.code = code;
    canonical
}

/// Routes one key event into the shared text editor.
///
/// `multiline` selects the field shape: the prompt, a discussion comment, and
/// the squash message are multiline buffers, while a path or a language field
/// stays a single-line input whose `Enter` confirms the dialog instead of
/// inserting a newline.
///
/// The router performs no I/O, so a caller may route keys while it holds the
/// application state guard: the editor restores its own kill ring and never
/// reads the system clipboard. A surface that wants the paste-buffer fallback
/// for a yank asks [`InputEditor::takes_yank_chord`] first, reads the buffer
/// with the guard released, and applies it through [`InputEditor::yank_with`];
/// `views::context::input` is the reference implementation.
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
        st.modals.squash_dialog = Some(SquashDialogState::default());
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

/// Opens the external documentation cache browser over RPC.
pub async fn open_outer_docs_cache(
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    {
        let mut st = state.lock().await;
        st.modals.outer_docs_cache = Some(crate::app::OuterDocsCacheDialogState {
            loading: true,
            ..Default::default()
        });
        st.modals.confirm_button = ConfirmDialogButton::Cancel;
    }
    send_request(
        server_writer,
        methods::OUTER_DOCS_CACHE_LIST,
        serde_json::json!({}),
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
        st.modals.confirm_button = ConfirmDialogButton::Cancel;
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
/// A modal receives the text in the field it exposes, and the REPL prompt
/// takes it when no modal is open. Every other surface ignores a paste, so a
/// bracketed paste can never mutate a view the developer is not editing.
pub fn handle_paste(st: &mut AppState, text: &str) {
    if text.is_empty() {
        return;
    }
    match st.active_modal() {
        Some(ActiveModal::Squash) => {
            if let Some(dialog) = st.modals.squash_dialog.as_mut() {
                if dialog.custom_input_active {
                    dialog.custom_editor.insert_paste_single_line(text);
                } else {
                    dialog.message_editor.insert_paste(text);
                }
            }
        }
        Some(ActiveModal::Prompt) => {
            if let Some(modal) = st.modals.prompt_modal.as_mut() {
                modal.prompt_editor.insert_paste(text);
            }
        }
        Some(ActiveModal::Review) => {
            if let Some(dialog) = st.modals.review_dialog.as_mut() {
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
            if let Some(dialog) = st.modals.locale_dialog.as_mut() {
                dialog.language_editor.insert_paste_single_line(text);
            }
        }
        // Every remaining surface owns no field the paste path feeds: its body
        // is a list, a confirmation, or a read-only area, so the payload is
        // dropped instead of being routed to a guessed field. The variants are
        // enumerated one by one, so a surface added later must state whether a
        // paste reaches it instead of inheriting a silent wildcard
        // (Conventions §5.1, c-text-capture-boundary). The plan-refinement and
        // review-execution forms do expose an instructions editor; this path
        // feeds only the four dialogs above, and that split stays as it is.
        Some(ActiveModal::ServerDisconnected)
        | Some(ActiveModal::ReviewTruncation)
        | Some(ActiveModal::ConfirmCancel)
        | Some(ActiveModal::ConfirmQuit)
        | Some(ActiveModal::BatchCancel)
        | Some(ActiveModal::ClearAuto)
        | Some(ActiveModal::ClearUser)
        | Some(ActiveModal::RemoveContextFile)
        | Some(ActiveModal::OuterDocsCacheRemove)
        | Some(ActiveModal::OuterDocsCache)
        | Some(ActiveModal::ConfirmDetachFocus)
        | Some(ActiveModal::ClearHorizonFinal)
        | Some(ActiveModal::ClearHorizon)
        | Some(ActiveModal::DeletePlanFinal)
        | Some(ActiveModal::DeletePlan)
        | Some(ActiveModal::DeleteReview)
        | Some(ActiveModal::ReviewAnalyze)
        | Some(ActiveModal::ExecuteReview)
        | Some(ActiveModal::ExecutePipeline)
        | Some(ActiveModal::ExecuteScope)
        | Some(ActiveModal::CreatePlan)
        | Some(ActiveModal::RefineDialog)
        | Some(ActiveModal::RefineBlocked)
        | Some(ActiveModal::ClearHistory)
        | Some(ActiveModal::Status)
        | Some(ActiveModal::Selection)
        | Some(ActiveModal::DiffInspector)
        | Some(ActiveModal::ThemeSelector)
        | Some(ActiveModal::UsageSummary)
        | Some(ActiveModal::Help) => {}
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

    // The layout gate runs first: the modifier configuration and the capture
    // boundary are read in one short lock, and every later step — the
    // keyboard-quit chord, the modal layer, the global command map, and the
    // active view handler — reads the canonical event. No surface can bypass
    // the projection (Conventions §3, §5).
    let (input, capturing, modal_open, disconnected, mode) = {
        let st = state.lock().await;
        (
            input_config(&st),
            text_input_active(&st),
            st.active_modal().is_some(),
            st.modals.server_disconnected.is_some(),
            st.view_mode,
        )
    };
    let key = canonicalize_key_event(key, &input, capturing);

    // `C-g` is the universal abort chord: normalize it once, after the layout
    // projection and before any view or modal handler runs, so every surface
    // reacts exactly as it reacts to `Esc` (Conventions §5.5).
    let key = if is_keyboard_quit(&key) {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    } else {
        key
    };

    // The command is resolved from the snapshot read under the single lock
    // above, so dispatch never takes the state lock a second time.
    let command = GLOBAL_COMMAND_MAP
        .resolve(key, &input, capturing)
        .map(|spec| spec.action);

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
        // The raw quit chord is resolved before any dialog handler runs: an
        // open modal owns the keyboard, so `Ctrl+Q` would otherwise be
        // swallowed by the dialog and merely dismiss it. Quitting here behaves
        // exactly as it does with no dialog open — immediately when nothing
        // would be lost, and through the `ConfirmQuit` card, which outranks the
        // dialog, when a draft is at stake. Every other resolved global command
        // stays with the open modal (Conventions §5.5).
        if matches!(command, Some(GlobalCommand::Quit)) {
            return execute_global_command(GlobalCommand::Quit, state, server_writer).await;
        }
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
            if st.modals.server_disconnected.is_some() || !st.has_lossy_work() {
                return Ok(InputResult::Exit);
            }
            st.modals.confirm_quit = true;
            st.modals.confirm_button = ConfirmDialogButton::Cancel;
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
        GlobalCommand::OpenOuterDocsCache => {
            open_outer_docs_cache(state, server_writer).await?;
        }
        GlobalCommand::ToggleReasoning => {
            let mut st = state.lock().await;
            st.model.reasoning.toggle_fold();
            st.model.update_reasoning_markdown();
        }
        GlobalCommand::OpenHelp => {
            let mut st = state.lock().await;
            let open = !st.modals.show_help;
            st.modals.show_help = open;
            st.modals.help_scroll = 0;
            if open {
                st.modals.help_tab = HelpTab::Global;
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
        st.modals.confirm_batch_cancel = true;
        st.modals.batch_cancel_button = BatchCancelButton::Continue;
    } else if st.model.is_busy() || st.review.running {
        st.modals.confirm_cancel = true;
        st.modals.confirm_button = ConfirmDialogButton::Cancel;
    }
}

/// Reloads the workstation configuration from disk and repaints the interface.
///
/// Reading and parsing `tui.toml` is blocking filesystem work, so it runs on the
/// blocking pool with no lock held; the state is locked only to apply the loaded
/// configuration or to surface the failure, so rendering and RPC processing never
/// queue behind the disk (c-1).
async fn reload_configuration(state: &Arc<Mutex<AppState>>) {
    let loaded = match tokio::task::spawn_blocking(TuiConfig::load_checked).await {
        Ok(loaded) => loaded,
        Err(err) => Err(err.to_string()),
    };
    let mut st = state.lock().await;
    match loaded {
        Ok(config) => {
            st.tui_config = config;
            st.sync_theme_mode();
            st.notify_success(active_t("notify.config_reloaded"));
        }
        Err(err) => {
            st.notify_error(active_t("notify.config_reload_error").replace("{}", &err));
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
/// insertion invariant holds (Conventions §5.1). The answer itself is owned by
/// [`AppState::text_input_capturing`], the single source of truth over the
/// surface that owns the keyboard, so the dispatcher reads one definition of the
/// capture boundary instead of re-deriving the capture states
/// (c-text-capture-boundary).
fn text_input_active(st: &AppState) -> bool {
    st.text_input_capturing()
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
        let mut repl_state = crate::app::test_state();
        repl_state.view_mode = ViewMode::Repl;
        handle_paste(&mut repl_state, "док");
        assert_eq!(repl_state.input_editor.get_text(), "док");

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
    /// commands of a structural view, while in the REPL prompt the very same
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

    #[tokio::test]
    async fn quit_exits_immediately_when_no_lossy_work() {
        let st = crate::app::test_state();
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let res = execute_global_command(GlobalCommand::Quit, &state, &mut conn)
            .await
            .expect("quit command");
        assert_eq!(res, InputResult::Exit);
        assert!(!state.lock().await.modals.confirm_quit);
    }

    #[tokio::test]
    async fn quit_asks_confirmation_when_lossy_work_present() {
        let mut st = crate::app::test_state();
        st.input_editor.insert_str("important draft");
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let res = execute_global_command(GlobalCommand::Quit, &state, &mut conn)
            .await
            .expect("quit command");
        assert_eq!(res, InputResult::Continue);
        assert!(state.lock().await.modals.confirm_quit);
    }

    /// The layout gate projects a command chord before any surface reads the
    /// event, so the abort chord is reachable from the JCUKEN layout: `п` sits
    /// on the physical position of `g`, and `Ctrl+п` must end the operation
    /// exactly like `Ctrl+g` (c-1, c-2).
    #[test]
    fn ctrl_cyrillic_pe_becomes_the_keyboard_quit_after_the_layout_gate() {
        use crate::config::LayoutPreset;

        let input = InputConfig {
            layout: LayoutPreset::RuJcuken,
            ..InputConfig::default()
        };
        let raw = KeyEvent::new(KeyCode::Char('п'), KeyModifiers::CONTROL);
        assert!(
            !is_keyboard_quit(&raw),
            "the raw Cyrillic press is not the canonical abort chord"
        );

        let canonical = canonicalize_key_event(raw, &input, false);
        assert_eq!(canonical.code, KeyCode::Char('g'));
        assert_eq!(canonical.modifiers, KeyModifiers::CONTROL);
        assert!(is_keyboard_quit(&canonical));

        // The dispatcher replaces the recognized chord with `Esc`, so the
        // normalized event aborts exactly as the Latin `C-g` does.
        let normalized = if is_keyboard_quit(&canonical) {
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        } else {
            canonical
        };
        assert_eq!(normalized.code, KeyCode::Esc);
        assert_eq!(normalized.modifiers, KeyModifiers::NONE);
    }

    /// The AltGr compose chord belongs to neither command modifier, so it is
    /// never projected and a composed character never becomes a command (c-2).
    #[test]
    fn layout_gate_never_projects_the_altgr_compose_chord() {
        use crate::config::LayoutPreset;

        let input = InputConfig {
            layout: LayoutPreset::RuJcuken,
            ..InputConfig::default()
        };
        let compose = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let key = KeyEvent::new(KeyCode::Char('п'), compose);

        for capturing in [false, true] {
            let canonical = canonicalize_key_event(key, &input, capturing);
            assert_eq!(canonical.code, KeyCode::Char('п'));
            assert_eq!(canonical.modifiers, compose);
        }
    }

    /// A typed character stays literal while an editor captures the keyboard,
    /// while the same physical key reaches the structural binding of a view
    /// that captures nothing (c-2, Conventions §5.1).
    #[test]
    fn layout_gate_keeps_typed_characters_literal_inside_an_editor() {
        use crate::config::LayoutPreset;

        let input = InputConfig {
            layout: LayoutPreset::RuJcuken,
            ..InputConfig::default()
        };
        let typed = KeyEvent::new(KeyCode::Char('т'), KeyModifiers::NONE);

        let literal = canonicalize_key_event(typed, &input, true);
        assert_eq!(
            literal.code,
            KeyCode::Char('т'),
            "the prompt receives the character the developer pressed"
        );

        let projected = canonicalize_key_event(typed, &input, false);
        assert_eq!(projected.code, KeyCode::Char('n'));
    }

    /// A command chord is projected even while an editor captures the keyboard,
    /// so the `C-` / `M-` commands never depend on the active layout (c-2).
    #[test]
    fn layout_gate_projects_command_chords_while_text_is_captured() {
        use crate::config::LayoutPreset;

        let input = InputConfig {
            layout: LayoutPreset::RuJcuken,
            ..InputConfig::default()
        };

        // `ч` sits on `x` and `а` on `f`.
        let execute = canonicalize_key_event(
            KeyEvent::new(KeyCode::Char('ч'), KeyModifiers::CONTROL),
            &input,
            true,
        );
        assert_eq!(execute.code, KeyCode::Char('x'));
        assert_eq!(execute.modifiers, KeyModifiers::CONTROL);

        let forward = canonicalize_key_event(
            KeyEvent::new(KeyCode::Char('а'), KeyModifiers::ALT),
            &input,
            true,
        );
        assert_eq!(forward.code, KeyCode::Char('f'));
        assert_eq!(forward.modifiers, KeyModifiers::ALT);
    }
    /// Under the JCUKEN layout the Develop prompt receives exactly the
    /// Under the JCUKEN layout the REPL prompt receives exactly the
    /// same layout still reaches its command even though the editor owns the
    /// keyboard: the capture boundary protects typed text, not command chords
    /// (c-text-capture-boundary).
    #[tokio::test]
    async fn jcuken_typing_stays_verbatim_and_command_chords_reach_the_prompt() {
        use crate::config::LayoutPreset;

        let mut st = crate::app::test_state();
        st.tui_config.input.layout = LayoutPreset::RuJcuken;
        st.view_mode = ViewMode::Repl;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        for character in ['т', 'е', 'с', 'т'] {
            let typed = KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
            handle_terminal_event(Event::Key(typed), &state, &mut conn)
                .await
                .expect("in-memory dispatch never fails");
        }
        assert_eq!(state.lock().await.input_editor.get_text(), "тест");

        // `C-ф` sits on the physical `a` position: the chord is normalized even
        // while the prompt captures text, so the cursor returns to the start of
        // the line instead of typing a Cyrillic character.
        let chord = KeyEvent::new(KeyCode::Char('ф'), KeyModifiers::CONTROL);
        handle_terminal_event(Event::Key(chord), &state, &mut conn)
            .await
            .expect("in-memory dispatch never fails");
        let typed = KeyEvent::new(KeyCode::Char('т'), KeyModifiers::NONE);
        handle_terminal_event(Event::Key(typed), &state, &mut conn)
            .await
            .expect("in-memory dispatch never fails");

        assert_eq!(
            state.lock().await.input_editor.get_text(),
            "ттест",
            "the chord moved the cursor and the typed character stayed literal"
        );
        assert!(
            conn.written().is_empty(),
            "typing and cursor movement never talk to the server"
        );
    }
    /// A command-only modal owns the keyboard and feeds no text field, so a
    /// pasted payload reaches no buffer behind it: the prompt keeps its draft and
    /// the status picker keeps its highlighted row (Conventions §5.1,
    /// c-text-capture-boundary).
    #[test]
    fn paste_is_dropped_by_a_command_only_modal() {
        let mut st = crate::app::test_state();
        st.input_editor.insert_str("draft");
        st.modals.status_dialog = Some(crate::app::StatusDialogState::new(
            crate::app::StatusDialogTarget::PlanItem {
                plan_id: "plan-1".to_string(),
                item_id: "1".to_string(),
                item_title: "Step".to_string(),
                current_status: tauqe_protocol::PlanItemStatus::Todo,
            },
            1,
        ));

        handle_paste(&mut st, "док");

        assert_eq!(
            st.input_editor.get_text(),
            "draft",
            "the prompt behind the card never receives a pasted payload"
        );
        assert_eq!(
            st.modals
                .status_dialog
                .as_ref()
                .map(|dialog| dialog.selector.selected_index()),
            Some(1)
        );

        // The reading surfaces are enumerated one by one as well, so neither
        // Help nor the usage breakdown forwards a paste to the prompt.
        st.modals.status_dialog = None;
        st.modals.show_help = true;
        handle_paste(&mut st, "док");
        assert_eq!(st.input_editor.get_text(), "draft");
        st.modals.show_help = false;

        st.open_usage_summary();
        handle_paste(&mut st, "док");
        assert_eq!(st.input_editor.get_text(), "draft");
        assert!(st.modals.usage_summary.is_some());
    }

    /// The explicit list of command surfaces never swallows a dialog that does
    /// expose a field, so a paste still reaches the squash commit message
    /// (Conventions §5.1).
    #[test]
    fn paste_still_reaches_a_dialog_that_exposes_a_field() {
        let mut st = crate::app::test_state();
        st.modals.squash_dialog = Some(SquashDialogState::default());

        handle_paste(&mut st, "fix(tui): keep the wheel modal-scoped");
        let dialog = st
            .modals
            .squash_dialog
            .as_ref()
            .expect("the dialog stays open");
        assert_eq!(
            dialog.message_editor.get_text(),
            "fix(tui): keep the wheel modal-scoped"
        );
    }

    /// The raw quit chord is resolved before any dialog handler runs: with a
    /// read-only dialog open and no lossy work, `Ctrl+Q` exits the application
    /// instead of merely dismissing the card (Conventions §5.5).
    #[tokio::test]
    async fn ctrl_q_quits_through_an_open_read_only_dialog() {
        for surface in ["help", "status", "theme"] {
            let mut st = crate::app::test_state();
            match surface {
                "help" => st.modals.show_help = true,
                "status" => {
                    st.modals.status_dialog = Some(crate::app::StatusDialogState::new(
                        crate::app::StatusDialogTarget::PlanItem {
                            plan_id: "plan-1".to_string(),
                            item_id: "1".to_string(),
                            item_title: "Step 1".to_string(),
                            current_status: tauqe_protocol::PlanItemStatus::Todo,
                        },
                        0,
                    ));
                }
                _ => st.open_theme_selector(),
            }
            let state = Arc::new(Mutex::new(st));
            let mut conn = RpcConnection::new(Vec::<u8>::new());

            let res = handle_terminal_event(
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                &state,
                &mut conn,
            )
            .await
            .expect("in-memory dispatch never fails");

            assert_eq!(res, InputResult::Exit, "{surface} swallows no quit");
            assert!(conn.written().is_empty(), "{surface} writes no request");
        }
    }

    /// A draft keeps the quit behind the shared confirmation card, which
    /// outranks the dialog that was open (Conventions §5.4, §5.5).
    #[tokio::test]
    async fn ctrl_q_with_a_draft_raises_the_quit_confirmation_over_a_dialog() {
        let mut st = crate::app::test_state();
        st.modals.show_help = true;
        st.input_editor.insert_str("important draft");
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let res = handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            &state,
            &mut conn,
        )
        .await
        .expect("in-memory dispatch never fails");

        assert_eq!(res, InputResult::Continue);
        let guard = state.lock().await;
        assert!(guard.modals.confirm_quit);
        assert_eq!(guard.modals.confirm_button, ConfirmDialogButton::Cancel);
        assert_eq!(guard.active_modal(), Some(ActiveModal::ConfirmQuit));
        assert!(guard.modals.show_help, "the dialog underneath stays open");
    }

    /// The preemption fires on the resolved chord, never on the bare letter: a
    /// bare `q` with Help open only dismisses the modal (Conventions §5.2).
    #[tokio::test]
    async fn bare_q_with_help_open_dismisses_without_quitting() {
        let mut st = crate::app::test_state();
        st.modals.show_help = true;
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let res = handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            &state,
            &mut conn,
        )
        .await
        .expect("in-memory dispatch never fails");

        assert_eq!(res, InputResult::Continue);
        let guard = state.lock().await;
        assert!(!guard.modals.show_help);
        assert!(!guard.modals.confirm_quit);
    }
}
