use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::{
    methods, GitSquashApplyParams, GitSquashGenerateMessageParams, GitSquashPreviewParams,
};
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{
    AppState, ConfirmDialogButton, SquashBaseMode, SquashConfirm, SquashDialogFocus,
    SquashDialogState,
};
use crate::command::is_bare_press;
use crate::config::{MetaModifier, PrimaryModifier};
use crate::input::InputResult;
use crate::rpc::{send_request, settle_squash_failure, RpcConnection};

/// Side effect requested by a key press; performed after the state lock is released.
enum SquashAction {
    None,
    /// A yank press: the paste buffer is read outside the state guard (c-1).
    Yank,
    Close,
    Preview(String),
    Generate(String),
    Apply(GitSquashApplyParams),
}

/// Rendered label of the chord that generates the commit message.
///
/// Generation is bound to the abstract meta chord (`M-g`), never to the primary
/// chord: `C-g` is the global keyboard-quit and is normalized to `Esc` before any
/// dialog sees it, so a `Ctrl+G` binding could only ever move focus
/// (Conventions §5.5). The label is projected through the configured modifier
/// instead of hardcoding `Ctrl+` (Conventions §3, §5.2).
pub(crate) fn generate_chord_label() -> String {
    format!("{}G", crate::config::active_meta_modifier().prefix_str())
}

/// Rendered label of the chord that applies the squash: the configured primary
/// command modifier applied to `Enter` and `S`, projected dynamically so the
/// advertised chord tracks the developer's `primary_modifier` (Conventions §5.2).
pub(crate) fn apply_chord_label() -> String {
    let prefix = crate::config::active_primary_modifier().prefix_str();
    format!("{prefix}Enter / {prefix}S")
}

pub async fn handle_squash_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let primary = st.tui_config.input.primary_modifier;
    let meta = st.tui_config.input.meta_modifier;
    let Some(dialog) = st.modals.squash_dialog.as_mut() else {
        return Ok(InputResult::Continue);
    };

    match handle_key(dialog, key, primary, meta) {
        SquashAction::None => {}
        SquashAction::Yank => {
            // The paste buffer is a native round trip bounded at
            // NATIVE_CLIPBOARD_TIMEOUT. It is read with the state guard released
            // and applied afterwards, so a hung clipboard owner can never freeze
            // rendering or block another locker of the state (c-1).
            drop(st);
            let clipboard = crate::clipboard::get_from_clipboard_async().await;
            let mut st = state.lock().await;
            if let Some(dialog) = st.modals.squash_dialog.as_mut() {
                if dialog.custom_input_active {
                    dialog.custom_editor.yank_with(clipboard);
                } else if dialog.focus == SquashDialogFocus::MessageEditor {
                    dialog.message_editor.yank_with(clipboard);
                }
            }
        }
        SquashAction::Close => st.modals.squash_dialog = None,
        SquashAction::Preview(base_ref) => {
            let params = GitSquashPreviewParams {
                base_ref: Some(base_ref),
            };
            let params = serde_json::to_value(params)?;
            drop(st);
            dispatch(state, server_writer, methods::GIT_SQUASH_PREVIEW, params).await;
        }
        SquashAction::Generate(base_ref) => {
            let params = GitSquashGenerateMessageParams { base_ref };
            let params = serde_json::to_value(params)?;
            drop(st);
            dispatch(
                state,
                server_writer,
                methods::GIT_SQUASH_GENERATE_MESSAGE,
                params,
            )
            .await;
        }
        SquashAction::Apply(params) => {
            let params = serde_json::to_value(params)?;
            drop(st);
            dispatch(state, server_writer, methods::GIT_SQUASH_APPLY, params).await;
        }
    }
    Ok(InputResult::Continue)
}

/// Dispatches one squash request after the state guard is released.
///
/// A write that never reaches the daemon produces no response, so the
/// in-flight flag the press raised (`loading`, `generating_message`, or
/// `applying`) would stay set and `handle_key` would refuse every later key:
/// an applying card would be wedged for good. The failure therefore settles
/// the dialog exactly as a refused request does and is reported in the
/// interface instead of propagating as a dispatch error (c-3).
async fn dispatch(
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
    method: &str,
    params: serde_json::Value,
) {
    if let Err(error) = send_request(server_writer, method, params).await {
        let mut st = state.lock().await;
        settle_squash_failure(&mut st, &error.to_string());
    }
}

fn handle_key(
    dialog: &mut SquashDialogState,
    key: KeyEvent,
    primary: PrimaryModifier,
    meta: MetaModifier,
) -> SquashAction {
    // The dialog stays open (and inert) until the server answers, so a failed
    // apply never loses the commit message.
    if dialog.applying {
        return SquashAction::None;
    }
    if let Some(kind) = dialog.confirm {
        return handle_confirm(dialog, kind, key);
    }

    let is_ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::ALT);
    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));
    // The meta chord never carries the `C-g` keyboard-quit, so generating a
    // commit message cannot be swallowed by the global abort normalization.
    let is_meta = !is_ctrl_alt && meta.matches(key.modifiers);

    if dialog.custom_input_active {
        handle_custom_input(dialog, key, is_cmd)
    } else if dialog.focus == SquashDialogFocus::MessageEditor {
        handle_message_editor(dialog, key, is_cmd, is_meta)
    } else {
        handle_browse(dialog, key, is_cmd)
    }
}

/// Resolves the apply or discard confirmation card.
///
/// The `y` / `n` accelerators and the `Enter` activation are bare presses: a
/// press carrying a command modifier or the AltGr compose chord confirms and
/// discards nothing, so a composed `y` can never commit the git-history rewrite
/// and `Alt+Enter` can never fire the focused button (Conventions §3, §5.1,
/// §5.2). `Esc` and the button walk stay unconditional.
fn handle_confirm(
    dialog: &mut SquashDialogState,
    kind: SquashConfirm,
    key: KeyEvent,
) -> SquashAction {
    let accepted = match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
            dialog.confirm_button = match dialog.confirm_button {
                ConfirmDialogButton::Cancel => ConfirmDialogButton::Confirm,
                ConfirmDialogButton::Confirm => ConfirmDialogButton::Cancel,
            };
            return SquashAction::None;
        }
        KeyCode::Enter if is_bare_press(key.modifiers) => {
            dialog.confirm_button == ConfirmDialogButton::Confirm
        }
        KeyCode::Char('y') | KeyCode::Char('Y') if is_bare_press(key.modifiers) => true,
        KeyCode::Char('n') | KeyCode::Char('N') if is_bare_press(key.modifiers) => false,
        KeyCode::Esc => false,
        _ => return SquashAction::None,
    };

    dialog.confirm = None;
    dialog.confirm_button = ConfirmDialogButton::Cancel;
    match (accepted, kind) {
        (true, SquashConfirm::Apply) => SquashAction::Apply(begin_apply(dialog)),
        (true, SquashConfirm::Discard) => SquashAction::Close,
        (false, _) => SquashAction::None,
    }
}

fn handle_custom_input(
    dialog: &mut SquashDialogState,
    key: KeyEvent,
    is_cmd: bool,
) -> SquashAction {
    // The paste buffer is read outside the state guard, so the press is
    // reported upward instead of being resolved here (c-1).
    if dialog.custom_editor.takes_yank_chord(key, is_cmd) {
        return SquashAction::Yank;
    }
    match key.code {
        KeyCode::Esc => {
            dialog.custom_input_active = false;
            SquashAction::None
        }
        KeyCode::Enter => {
            let custom = dialog.custom_editor.get_text().trim().to_string();
            dialog.custom_input_active = false;
            if custom.is_empty() {
                return SquashAction::None;
            }
            dialog.pending_base = Some((SquashBaseMode::Custom, custom.clone()));
            dialog.loading = true;
            dialog.status_message = Some(
                crate::i18n::active_t("dialog.squash.status_checking_base").replace("{}", &custom),
            );
            SquashAction::Preview(custom)
        }
        _ => {
            crate::input::handle_editor_key(&mut dialog.custom_editor, key, is_cmd, false);
            SquashAction::None
        }
    }
}

fn handle_message_editor(
    dialog: &mut SquashDialogState,
    key: KeyEvent,
    is_cmd: bool,
    is_meta: bool,
) -> SquashAction {
    // The paste buffer is read outside the state guard, so the press is
    // reported upward instead of being resolved here (c-1).
    if dialog.message_editor.takes_yank_chord(key, is_cmd) {
        return SquashAction::Yank;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Tab => dialog.focus = SquashDialogFocus::FileList,
        KeyCode::Enter | KeyCode::Char('s') if is_cmd => request_apply_confirm(dialog),
        // Generation is a meta chord: the primary chord for `g` is the global
        // keyboard-quit and never reaches the dialog (Conventions §5.5).
        KeyCode::Char('g') if is_meta => return begin_generate(dialog),
        _ => {
            crate::input::handle_editor_key(&mut dialog.message_editor, key, is_cmd, true);
        }
    }
    SquashAction::None
}

/// Browses the dialog while no text field owns the keyboard.
///
/// Every letter and digit accelerator is a bare press: a command chord and the
/// AltGr compose chord belong to the command surface or to a text buffer and
/// never fire one, so `C-1` cannot dispatch a base preview and `Alt+G` cannot
/// spend tokens on a generated message (Conventions §3, §5.1, §5.2). The
/// dedicated arrows, `Esc`, `Tab`, and the paging keys stay unconditional, and
/// the `C-Enter` / `C-S` apply request keeps its explicit command chord.
fn handle_browse(dialog: &mut SquashDialogState, key: KeyEvent, is_cmd: bool) -> SquashAction {
    let bare = is_bare_press(key.modifiers);
    match key.code {
        KeyCode::Esc => request_close(dialog),
        KeyCode::Char('1') if bare => {
            let base = dialog.session_base.clone();
            switch_base(
                dialog,
                SquashBaseMode::Session,
                base,
                &crate::i18n::active_t("dialog.squash.status_no_session_base"),
            )
        }
        KeyCode::Char('2') if bare => {
            let base = dialog.upstream_base.clone();
            switch_base(
                dialog,
                SquashBaseMode::Upstream,
                base,
                &crate::i18n::active_t("dialog.squash.status_no_upstream_base"),
            )
        }
        KeyCode::Char('3') if bare => {
            dialog.custom_input_active = true;
            dialog.custom_editor.clear();
            dialog.custom_editor.insert_str(&dialog.base_ref);
            SquashAction::None
        }
        KeyCode::Tab => {
            dialog.focus = match dialog.focus {
                SquashDialogFocus::FileList => SquashDialogFocus::DiffView,
                SquashDialogFocus::DiffView => SquashDialogFocus::MessageEditor,
                SquashDialogFocus::MessageEditor => SquashDialogFocus::FileList,
            };
            SquashAction::None
        }
        KeyCode::Char('g') if bare => begin_generate(dialog),
        KeyCode::Char('e') | KeyCode::Char('m') if bare => {
            dialog.focus = SquashDialogFocus::MessageEditor;
            SquashAction::None
        }
        KeyCode::Enter | KeyCode::Char('s') if is_cmd => {
            request_apply_confirm(dialog);
            SquashAction::None
        }
        KeyCode::Char(' ') | KeyCode::Enter if bare => {
            if let Some(file) = dialog.files.get_mut(dialog.selected_file_index) {
                file.expanded = !file.expanded;
            }
            SquashAction::None
        }
        KeyCode::Up => step_browse(dialog, false),
        KeyCode::Char('k') if bare => step_browse(dialog, false),
        KeyCode::Down => step_browse(dialog, true),
        KeyCode::Char('j') if bare => step_browse(dialog, true),
        KeyCode::PageUp => {
            dialog.diff_scroll = dialog.diff_scroll.saturating_sub(10);
            SquashAction::None
        }
        KeyCode::PageDown => {
            dialog.diff_scroll = dialog.diff_scroll.saturating_add(10);
            SquashAction::None
        }
        _ => SquashAction::None,
    }
}

/// Steps the browse cursor one file or scrolls the diff pane by one line.
///
/// The dedicated arrows and the bare `j` / `k` letters share this motion; the
/// letter spelling is guarded at the call site, so a command chord or a composed
/// character can never move the file cursor.
fn step_browse(dialog: &mut SquashDialogState, forward: bool) -> SquashAction {
    if dialog.focus == SquashDialogFocus::DiffView {
        dialog.diff_scroll = if forward {
            dialog.diff_scroll.saturating_add(1)
        } else {
            dialog.diff_scroll.saturating_sub(1)
        };
        return SquashAction::None;
    }

    let last = dialog.files.len().saturating_sub(1);
    if forward && dialog.selected_file_index < last {
        dialog.selected_file_index += 1;
        dialog.diff_scroll = 0;
    } else if !forward && dialog.selected_file_index > 0 {
        dialog.selected_file_index -= 1;
        dialog.diff_scroll = 0;
    }
    SquashAction::None
}

/// Closes immediately when there is nothing to lose, otherwise asks first.
fn request_close(dialog: &mut SquashDialogState) -> SquashAction {
    if dialog.message_editor.get_text().trim().is_empty() {
        return SquashAction::Close;
    }
    dialog.confirm = Some(SquashConfirm::Discard);
    dialog.confirm_button = ConfirmDialogButton::Cancel;
    SquashAction::None
}

fn switch_base(
    dialog: &mut SquashDialogState,
    mode: SquashBaseMode,
    base: Option<String>,
    unavailable: &str,
) -> SquashAction {
    match base {
        Some(base_ref) => {
            dialog.pending_base = Some((mode, base_ref.clone()));
            dialog.loading = true;
            dialog.status_message = Some(
                crate::i18n::active_t("dialog.squash.status_switching_base")
                    .replace("{}", &base_ref),
            );
            SquashAction::Preview(base_ref)
        }
        None => {
            dialog.status_message = Some(unavailable.to_string());
            SquashAction::None
        }
    }
}

fn begin_generate(dialog: &mut SquashDialogState) -> SquashAction {
    if dialog.generating_message {
        return SquashAction::None;
    }
    dialog.generating_message = true;
    dialog.status_message = Some(crate::i18n::active_t(
        "dialog.squash.status_generating_message",
    ));
    SquashAction::Generate(dialog.base_ref.clone())
}

fn begin_apply(dialog: &mut SquashDialogState) -> GitSquashApplyParams {
    dialog.applying = true;
    dialog.status_message = Some(crate::i18n::active_t("dialog.squash.status_applying"));
    GitSquashApplyParams {
        base_ref: dialog.base_ref.clone(),
        message: dialog.message_editor.get_text().trim().to_string(),
    }
}

fn request_apply_confirm(dialog: &mut SquashDialogState) {
    if dialog.loading {
        dialog.status_message = Some(crate::i18n::active_t("dialog.squash.status_loading_diff"));
        return;
    }
    if dialog.generating_message {
        dialog.status_message = Some(crate::i18n::active_t(
            "dialog.squash.status_message_generating",
        ));
        return;
    }
    if dialog.base_ref.trim().is_empty() {
        dialog.status_message = Some(crate::i18n::active_t("dialog.squash.status_invalid_base"));
        return;
    }
    if dialog.commits.is_empty() {
        dialog.status_message = Some(crate::i18n::active_t("dialog.squash.status_no_commits"));
        return;
    }
    if dialog.message_editor.get_text().trim().is_empty() {
        dialog.status_message = Some(
            crate::i18n::active_t("dialog.squash.status_empty_message")
                .replace("{}", &generate_chord_label()),
        );
        dialog.focus = SquashDialogFocus::MessageEditor;
        return;
    }
    dialog.confirm = Some(SquashConfirm::Apply);
    dialog.confirm_button = ConfirmDialogButton::Cancel;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SquashFileItem;

    fn press(dialog: &mut SquashDialogState, code: KeyCode) -> SquashAction {
        handle_key(
            dialog,
            KeyEvent::new(code, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
        )
    }

    /// Presses a key carrying the default meta modifier (`Alt`).
    fn press_meta(dialog: &mut SquashDialogState, code: KeyCode) -> SquashAction {
        handle_key(
            dialog,
            KeyEvent::new(code, KeyModifiers::ALT),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
        )
    }

    /// Presses a key carrying an arbitrary modifier set.
    fn press_with(
        dialog: &mut SquashDialogState,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> SquashAction {
        handle_key(
            dialog,
            KeyEvent::new(code, modifiers),
            PrimaryModifier::Ctrl,
            MetaModifier::Alt,
        )
    }

    /// Presses a key carrying the AltGr compose chord.
    fn press_compose(dialog: &mut SquashDialogState, code: KeyCode) -> SquashAction {
        press_with(dialog, code, KeyModifiers::CONTROL | KeyModifiers::ALT)
    }

    fn ready_dialog() -> SquashDialogState {
        SquashDialogState {
            loading: false,
            status_message: None,
            ..Default::default()
        }
    }

    #[test]
    fn meta_g_generates_the_message_from_the_editor() {
        let mut dialog = ready_dialog();
        dialog.focus = SquashDialogFocus::MessageEditor;
        dialog.base_ref = "origin/master".to_string();

        let action = press_meta(&mut dialog, KeyCode::Char('g'));
        assert!(matches!(
            action,
            SquashAction::Generate(base) if base == "origin/master"
        ));
        assert!(dialog.generating_message);
        assert_eq!(dialog.message_editor.get_text(), "");
    }

    #[test]
    fn primary_g_never_generates_because_it_is_the_global_keyboard_quit() {
        // `C-g` is normalized to `Esc` before the dialog runs, so the editor
        // must treat it as a dismissal (focus back to the file list) and never
        // as a generate request.
        let mut dialog = ready_dialog();
        dialog.focus = SquashDialogFocus::MessageEditor;

        assert!(matches!(
            press(&mut dialog, KeyCode::Esc),
            SquashAction::None
        ));
        assert_eq!(dialog.focus, SquashDialogFocus::FileList);
        assert!(!dialog.generating_message);
    }

    #[test]
    fn chord_labels_project_the_configured_modifiers() {
        // The default configuration pairs a Ctrl primary with an Alt meta chord.
        assert_eq!(generate_chord_label(), "Alt+G");
        assert_eq!(apply_chord_label(), "Ctrl+Enter / Ctrl+S");
    }

    #[test]
    fn bare_letters_are_typed_literally_in_message_editor() {
        let mut dialog = ready_dialog();
        dialog.focus = SquashDialogFocus::MessageEditor;
        press(&mut dialog, KeyCode::Char('g'));
        press(&mut dialog, KeyCode::Char('e'));
        assert_eq!(dialog.message_editor.get_text(), "ge");
    }

    #[test]
    fn message_editor_inserts_in_the_middle_after_cursor_movement() {
        let mut dialog = ready_dialog();
        dialog.focus = SquashDialogFocus::MessageEditor;
        for c in "helo".chars() {
            press(&mut dialog, KeyCode::Char(c));
        }
        press(&mut dialog, KeyCode::Left);
        press(&mut dialog, KeyCode::Char('l'));
        assert_eq!(dialog.message_editor.get_text(), "hello");
    }

    #[test]
    fn empty_message_closes_immediately() {
        let mut dialog = ready_dialog();
        assert!(matches!(
            press(&mut dialog, KeyCode::Esc),
            SquashAction::Close
        ));
    }

    #[test]
    fn closing_with_message_requires_discard_confirmation() {
        let mut dialog = ready_dialog();
        dialog.message_editor.insert_str("feat: keep me");
        assert!(matches!(
            press(&mut dialog, KeyCode::Esc),
            SquashAction::None
        ));
        assert_eq!(dialog.confirm, Some(SquashConfirm::Discard));
        assert_eq!(dialog.confirm_button, ConfirmDialogButton::Cancel);

        // Enter on the default Cancel button keeps the dialog and the message.
        assert!(matches!(
            press(&mut dialog, KeyCode::Enter),
            SquashAction::None
        ));
        assert_eq!(dialog.confirm, None);
        assert_eq!(dialog.message_editor.get_text(), "feat: keep me");
    }

    #[test]
    fn apply_keeps_dialog_open_in_applying_state() {
        let mut dialog = ready_dialog();
        dialog.base_ref = "origin/master".to_string();
        dialog.message_editor.insert_str("feat: x");
        dialog.confirm = Some(SquashConfirm::Apply);
        dialog.confirm_button = ConfirmDialogButton::Confirm;

        let action = press(&mut dialog, KeyCode::Enter);
        assert!(matches!(
            action,
            SquashAction::Apply(p) if p.message == "feat: x" && p.base_ref == "origin/master"
        ));
        assert!(dialog.applying);
        assert_eq!(dialog.message_editor.get_text(), "feat: x");

        // Input is ignored until the server responds.
        assert!(matches!(
            press(&mut dialog, KeyCode::Esc),
            SquashAction::None
        ));
        assert_eq!(dialog.confirm, None);
    }

    #[test]
    fn function_key_does_not_trigger_apply_in_browse() {
        let mut dialog = ready_dialog();
        assert!(dialog.status_message.is_none());

        // F6 is inert: the squash apply is reachable only through the C-S / M-S
        // chords, never through a hardware function key.
        assert!(matches!(
            press(&mut dialog, KeyCode::F(6)),
            SquashAction::None
        ));
        assert!(dialog.status_message.is_none());
        assert_eq!(dialog.confirm, None);
    }
    /// Asserts the press resolved to no dialog action at all.
    ///
    /// A command chord and an AltGr-composed character are never dialog
    /// accelerators, so a guarded arm answers with `SquashAction::None` and
    /// leaves every field of the dialog untouched.
    fn assert_no_action(action: SquashAction) {
        assert!(
            matches!(action, SquashAction::None),
            "a command chord or a composed character must not fire a dialog action"
        );
    }

    /// Builds a staged file entry without a rendered diff.
    fn squash_file(path: &str) -> SquashFileItem {
        SquashFileItem {
            path: path.to_string(),
            diff: String::new(),
            expanded: false,
        }
    }

    /// Modifier sets a command surface must refuse: both command chords, the
    /// AltGr compose chord they form together, and its shifted spelling.
    fn non_bare_modifiers() -> [KeyModifiers; 4] {
        [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
        ]
    }

    #[test]
    fn command_and_compose_chords_never_switch_the_squash_base() {
        for modifiers in non_bare_modifiers() {
            for code in [KeyCode::Char('1'), KeyCode::Char('2'), KeyCode::Char('3')] {
                let mut dialog = ready_dialog();
                dialog.session_base = Some("origin/master".to_string());
                dialog.upstream_base = Some("origin/main".to_string());
                dialog.base_ref = "origin/master".to_string();

                assert_no_action(press_with(&mut dialog, code, modifiers));
                assert!(dialog.pending_base.is_none());
                assert!(!dialog.loading);
                assert!(!dialog.custom_input_active);
                assert!(dialog.status_message.is_none());
            }
        }
    }

    #[test]
    fn command_and_compose_chords_never_generate_a_message() {
        for modifiers in non_bare_modifiers() {
            let mut dialog = ready_dialog();
            dialog.base_ref = "origin/master".to_string();

            assert_no_action(press_with(&mut dialog, KeyCode::Char('g'), modifiers));
            assert!(!dialog.generating_message);
            assert!(dialog.status_message.is_none());
        }
    }

    #[test]
    fn compose_chord_never_applies_or_discards_the_confirmation() {
        for (kind, code) in [
            (SquashConfirm::Apply, KeyCode::Char('y')),
            (SquashConfirm::Discard, KeyCode::Char('n')),
        ] {
            let mut dialog = ready_dialog();
            dialog.base_ref = "origin/master".to_string();
            dialog.message_editor.insert_str("feat: keep me");
            dialog.confirm = Some(kind);
            dialog.confirm_button = ConfirmDialogButton::Confirm;

            assert_no_action(press_compose(&mut dialog, code));
            assert_eq!(dialog.confirm, Some(kind));
            assert_eq!(dialog.confirm_button, ConfirmDialogButton::Confirm);
            assert!(!dialog.applying);
            assert_eq!(dialog.message_editor.get_text(), "feat: keep me");
        }
    }

    #[test]
    fn command_modified_enter_never_confirms_the_rewrite() {
        for modifiers in non_bare_modifiers() {
            let mut dialog = ready_dialog();
            dialog.base_ref = "origin/master".to_string();
            dialog.message_editor.insert_str("feat: x");
            dialog.confirm = Some(SquashConfirm::Apply);
            dialog.confirm_button = ConfirmDialogButton::Confirm;

            assert_no_action(press_with(&mut dialog, KeyCode::Enter, modifiers));
            assert_eq!(dialog.confirm, Some(SquashConfirm::Apply));
            assert_eq!(dialog.confirm_button, ConfirmDialogButton::Confirm);
            assert!(!dialog.applying);
        }
    }

    #[test]
    fn bare_presses_keep_their_squash_actions() {
        let mut dialog = ready_dialog();
        dialog.session_base = Some("origin/master".to_string());
        let _ = press(&mut dialog, KeyCode::Char('1'));
        assert_eq!(
            dialog.pending_base,
            Some((SquashBaseMode::Session, "origin/master".to_string()))
        );
        assert!(dialog.loading);

        let mut dialog = ready_dialog();
        dialog.base_ref = "origin/master".to_string();
        let _ = press(&mut dialog, KeyCode::Char('g'));
        assert!(dialog.generating_message);

        let mut dialog = ready_dialog();
        dialog.files = vec![squash_file("a.rs")];
        let _ = press(&mut dialog, KeyCode::Char(' '));
        assert!(dialog.files[0].expanded);

        for code in [KeyCode::Char('y'), KeyCode::Enter] {
            let mut dialog = ready_dialog();
            dialog.base_ref = "origin/master".to_string();
            dialog.message_editor.insert_str("feat: x");
            dialog.confirm = Some(SquashConfirm::Apply);
            dialog.confirm_button = ConfirmDialogButton::Confirm;

            let action = press(&mut dialog, code);
            assert!(
                matches!(action, SquashAction::Apply(params) if params.message == "feat: x"),
                "a bare {code:?} on the focused Confirm button applies the rewrite"
            );
            assert!(dialog.applying);
        }
    }

    #[test]
    fn command_modified_letters_never_move_focus_or_the_cursor() {
        for modifiers in non_bare_modifiers() {
            for code in [KeyCode::Char('e'), KeyCode::Char('m')] {
                let mut dialog = ready_dialog();
                dialog.focus = SquashDialogFocus::FileList;

                assert_no_action(press_with(&mut dialog, code, modifiers));
                assert_eq!(dialog.focus, SquashDialogFocus::FileList);
            }

            // The cursor rests on the second row, so `k` has somewhere to go.
            let mut dialog = ready_dialog();
            dialog.files = vec![squash_file("a.rs"), squash_file("b.rs")];
            dialog.selected_file_index = 1;
            assert_no_action(press_with(&mut dialog, KeyCode::Char('k'), modifiers));
            assert_eq!(dialog.selected_file_index, 1);

            // `j` is checked from the first row of the same two files.
            let mut dialog = ready_dialog();
            dialog.files = vec![squash_file("a.rs"), squash_file("b.rs")];
            assert_no_action(press_with(&mut dialog, KeyCode::Char('j'), modifiers));
            assert_eq!(dialog.selected_file_index, 0);

            // Inside the diff pane the same letters scroll the patch instead.
            for code in [KeyCode::Char('j'), KeyCode::Char('k')] {
                let mut dialog = ready_dialog();
                dialog.focus = SquashDialogFocus::DiffView;
                dialog.diff_scroll = 5;

                assert_no_action(press_with(&mut dialog, code, modifiers));
                assert_eq!(dialog.diff_scroll, 5);
            }
        }
    }

    #[test]
    fn command_modified_toggles_never_fold_a_file() {
        for modifiers in non_bare_modifiers() {
            let mut dialog = ready_dialog();
            dialog.files = vec![squash_file("a.rs")];

            assert_no_action(press_with(&mut dialog, KeyCode::Char(' '), modifiers));
            assert!(!dialog.files[0].expanded);
        }

        // `Ctrl+Enter` is the documented apply request of the browse surface, so
        // the fold guard is proved with the Alt and AltGr spellings that carry no
        // other meaning there.
        for modifiers in [
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
        ] {
            let mut dialog = ready_dialog();
            dialog.files = vec![squash_file("a.rs")];

            assert_no_action(press_with(&mut dialog, KeyCode::Enter, modifiers));
            assert!(!dialog.files[0].expanded);
        }
    }

    /// Async sink whose every write fails, standing in for a server pipe that
    /// closed before the press reached it.
    struct BrokenPipeWriter;

    impl tokio::io::AsyncWrite for BrokenPipeWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "server pipe closed",
            )))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Presses one key through the entry point the dialog dispatch calls, over
    /// a transport whose every write fails.
    async fn press_over_broken_pipe(
        state: &Arc<Mutex<AppState>>,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> anyhow::Result<InputResult> {
        let mut conn = RpcConnection::new(BrokenPipeWriter);
        let guard = state.lock().await;
        handle_squash_key(KeyEvent::new(code, modifiers), state, guard, &mut conn).await
    }

    /// Builds a state whose squash dialog awaits the apply confirmation of
    /// `feat: x` on `origin/master`.
    fn applying_state() -> Arc<Mutex<AppState>> {
        let mut st = crate::app::test_state();
        let mut dialog = ready_dialog();
        dialog.base_ref = "origin/master".to_string();
        dialog.message_editor.insert_str("feat: x");
        dialog.confirm = Some(SquashConfirm::Apply);
        dialog.confirm_button = ConfirmDialogButton::Confirm;
        st.modals.squash_dialog = Some(dialog);
        Arc::new(Mutex::new(st))
    }

    #[tokio::test]
    async fn failed_apply_write_settles_the_dialog_and_keeps_the_message() {
        let state = applying_state();

        let result = press_over_broken_pipe(&state, KeyCode::Char('y'), KeyModifiers::NONE).await;
        assert!(
            result.is_ok(),
            "a failed write is reported in the interface, not as a dispatch error"
        );

        {
            let guard = state.lock().await;
            let dialog = guard
                .modals
                .squash_dialog
                .as_ref()
                .expect("the dialog stays open so the message is not lost");
            assert!(
                !dialog.applying,
                "a failed write must not latch the applying flag"
            );
            assert_eq!(dialog.message_editor.get_text(), "feat: x");
            assert_eq!(
                guard
                    .active_notification()
                    .map(|notification| notification.level),
                Some(crate::app::NotificationLevel::Error)
            );
        }

        // The card is usable again: the next `Esc` reaches the dialog and asks
        // to discard the draft instead of being swallowed by the latch.
        press_over_broken_pipe(&state, KeyCode::Esc, KeyModifiers::NONE)
            .await
            .expect("an in-memory dispatch never fails");
        let guard = state.lock().await;
        let dialog = guard
            .modals
            .squash_dialog
            .as_ref()
            .expect("the dialog stays open");
        assert_eq!(dialog.confirm, Some(SquashConfirm::Discard));
    }

    #[tokio::test]
    async fn failed_preview_write_clears_the_loading_flag() {
        let mut st = crate::app::test_state();
        let mut dialog = ready_dialog();
        dialog.session_base = Some("origin/master".to_string());
        st.modals.squash_dialog = Some(dialog);
        let state = Arc::new(Mutex::new(st));

        press_over_broken_pipe(&state, KeyCode::Char('1'), KeyModifiers::NONE)
            .await
            .expect("an in-memory dispatch never fails");

        let guard = state.lock().await;
        let dialog = guard
            .modals
            .squash_dialog
            .as_ref()
            .expect("the dialog stays open");
        assert!(
            !dialog.loading,
            "a failed write must not latch the loading flag"
        );
        assert!(dialog.pending_base.is_none());
        assert_eq!(
            guard
                .active_notification()
                .map(|notification| notification.level),
            Some(crate::app::NotificationLevel::Error)
        );
    }

    #[tokio::test]
    async fn failed_generate_write_clears_the_generating_flag() {
        let mut st = crate::app::test_state();
        let mut dialog = ready_dialog();
        dialog.focus = SquashDialogFocus::MessageEditor;
        dialog.base_ref = "origin/master".to_string();
        st.modals.squash_dialog = Some(dialog);
        let state = Arc::new(Mutex::new(st));

        press_over_broken_pipe(&state, KeyCode::Char('g'), KeyModifiers::ALT)
            .await
            .expect("an in-memory dispatch never fails");

        let guard = state.lock().await;
        let dialog = guard
            .modals
            .squash_dialog
            .as_ref()
            .expect("the dialog stays open");
        assert!(
            !dialog.generating_message,
            "a failed write must not latch the generating flag"
        );
        assert_eq!(
            guard
                .active_notification()
                .map(|notification| notification.level),
            Some(crate::app::NotificationLevel::Error)
        );
    }
}
