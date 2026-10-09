use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::{
    methods, GitSquashApplyParams, GitSquashGenerateMessageParams, GitSquashPreviewParams,
};
use tokio::process::ChildStdin;
use tokio::sync::MutexGuard;

use crate::app::{
    AppState, ConfirmDialogButton, SquashBaseMode, SquashConfirm, SquashDialogFocus,
    SquashDialogState,
};
use crate::config::PrimaryModifier;
use crate::input::{is_char_typing, InputResult};
use crate::rpc::send_request;

/// Side effect requested by a key press; performed after the state lock is released.
enum SquashAction {
    None,
    Close,
    Preview(String),
    Generate(String),
    Apply(GitSquashApplyParams),
}

pub async fn handle_squash_key(
    key: KeyEvent,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let primary = st.tui_config.input.primary_modifier;
    let Some(dialog) = st.squash_dialog.as_mut() else {
        return Ok(InputResult::Continue);
    };

    match handle_key(dialog, key, primary) {
        SquashAction::None => {}
        SquashAction::Close => st.squash_dialog = None,
        SquashAction::Preview(base_ref) => {
            drop(st);
            let params = GitSquashPreviewParams {
                base_ref: Some(base_ref),
            };
            send_request(
                server_writer,
                methods::GIT_SQUASH_PREVIEW,
                serde_json::to_value(params)?,
            )
            .await?;
        }
        SquashAction::Generate(base_ref) => {
            drop(st);
            let params = GitSquashGenerateMessageParams { base_ref };
            send_request(
                server_writer,
                methods::GIT_SQUASH_GENERATE_MESSAGE,
                serde_json::to_value(params)?,
            )
            .await?;
        }
        SquashAction::Apply(params) => {
            drop(st);
            send_request(
                server_writer,
                methods::GIT_SQUASH_APPLY,
                serde_json::to_value(params)?,
            )
            .await?;
        }
    }
    Ok(InputResult::Continue)
}

fn handle_key(
    dialog: &mut SquashDialogState,
    key: KeyEvent,
    primary: PrimaryModifier,
) -> SquashAction {
    // The dialog stays open (and inert) until the server answers, so a failed
    // apply never loses the commit message.
    if dialog.applying {
        return SquashAction::None;
    }
    if let Some(kind) = dialog.confirm {
        return handle_confirm(dialog, kind, key);
    }

    let is_ctrl_alt = key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT);
    let is_cmd = !is_ctrl_alt
        && (primary.matches(key.modifiers) || key.modifiers.contains(KeyModifiers::CONTROL));

    if dialog.custom_input_active {
        handle_custom_input(dialog, key, is_cmd)
    } else if dialog.focus == SquashDialogFocus::MessageEditor {
        handle_message_editor(dialog, key, is_cmd)
    } else {
        handle_browse(dialog, key, is_cmd)
    }
}

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
        KeyCode::Enter => dialog.confirm_button == ConfirmDialogButton::Confirm,
        KeyCode::Char('y') | KeyCode::Char('Y') if is_char_typing(key.modifiers) => true,
        KeyCode::Char('n') | KeyCode::Char('N') if is_char_typing(key.modifiers) => false,
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

fn handle_custom_input(dialog: &mut SquashDialogState, key: KeyEvent, is_cmd: bool) -> SquashAction {
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
            dialog.status_message = Some(format!("Checking base '{}'...", custom));
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
) -> SquashAction {
    match key.code {
        KeyCode::Esc | KeyCode::Tab => dialog.focus = SquashDialogFocus::FileList,
        KeyCode::Enter | KeyCode::Char('s') if is_cmd => request_apply_confirm(dialog),
        KeyCode::Char('g') if is_cmd => return begin_generate(dialog),
        _ => {
            crate::input::handle_editor_key(&mut dialog.message_editor, key, is_cmd, true);
        }
    }
    SquashAction::None
}

fn handle_browse(dialog: &mut SquashDialogState, key: KeyEvent, is_cmd: bool) -> SquashAction {
    match key.code {
        KeyCode::Esc => request_close(dialog),
        KeyCode::Char('1') => {
            let base = dialog.session_base.clone();
            switch_base(
                dialog,
                SquashBaseMode::Session,
                base,
                "Session base commit not available (no AI commits in session).",
            )
        }
        KeyCode::Char('2') => {
            let base = dialog.upstream_base.clone();
            switch_base(
                dialog,
                SquashBaseMode::Upstream,
                base,
                "No upstream branch configured or detected.",
            )
        }
        KeyCode::Char('3') => {
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
        KeyCode::Char('g') => begin_generate(dialog),
        KeyCode::Char('e') | KeyCode::Char('m') => {
            dialog.focus = SquashDialogFocus::MessageEditor;
            SquashAction::None
        }
        KeyCode::Enter | KeyCode::Char('s') if is_cmd => {
            request_apply_confirm(dialog);
            SquashAction::None
        }
        KeyCode::F(6) => {
            request_apply_confirm(dialog);
            SquashAction::None
        }
        KeyCode::Char(' ') | KeyCode::Enter => {
            if let Some(file) = dialog.files.get_mut(dialog.selected_file_index) {
                file.expanded = !file.expanded;
            }
            SquashAction::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if dialog.focus == SquashDialogFocus::DiffView {
                dialog.diff_scroll = dialog.diff_scroll.saturating_sub(1);
            } else if dialog.selected_file_index > 0 {
                dialog.selected_file_index -= 1;
                dialog.diff_scroll = 0;
            }
            SquashAction::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if dialog.focus == SquashDialogFocus::DiffView {
                dialog.diff_scroll = dialog.diff_scroll.saturating_add(1);
            } else if !dialog.files.is_empty()
                && dialog.selected_file_index + 1 < dialog.files.len()
            {
                dialog.selected_file_index += 1;
                dialog.diff_scroll = 0;
            }
            SquashAction::None
        }
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
            dialog.status_message = Some(format!("Switching base to '{}'...", base_ref));
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
    dialog.status_message = Some("Generating Conventional Commit message with AI...".to_string());
    SquashAction::Generate(dialog.base_ref.clone())
}

fn begin_apply(dialog: &mut SquashDialogState) -> GitSquashApplyParams {
    dialog.applying = true;
    dialog.status_message = Some("Applying squash...".to_string());
    GitSquashApplyParams {
        base_ref: dialog.base_ref.clone(),
        message: dialog.message_editor.get_text().trim().to_string(),
    }
}

fn request_apply_confirm(dialog: &mut SquashDialogState) {
    if dialog.loading {
        dialog.status_message = Some("Diff is still loading, please wait...".to_string());
        return;
    }
    if dialog.generating_message {
        dialog.status_message = Some("Commit message is generating, please wait...".to_string());
        return;
    }
    if dialog.base_ref.trim().is_empty() {
        dialog.status_message = Some("Invalid or empty base ref.".to_string());
        return;
    }
    if dialog.commits.is_empty() {
        dialog.status_message = Some("No commits ahead of base to squash.".to_string());
        return;
    }
    if dialog.message_editor.get_text().trim().is_empty() {
        dialog.status_message =
            Some("Commit message cannot be empty (Ctrl+G to generate, or type one).".to_string());
        dialog.focus = SquashDialogFocus::MessageEditor;
        return;
    }
    dialog.confirm = Some(SquashConfirm::Apply);
    dialog.confirm_button = ConfirmDialogButton::Cancel;
}

    
#[cfg(test)]
mod tests {
    use super::*;

    fn press(dialog: &mut SquashDialogState, code: KeyCode) -> SquashAction {
        handle_key(
            dialog,
            KeyEvent::new(code, KeyModifiers::NONE),
            PrimaryModifier::Ctrl,
        )
    }

    fn ready_dialog() -> SquashDialogState {
        SquashDialogState {
            loading: false,
            ..Default::default()
        }
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
        assert!(matches!(press(&mut dialog, KeyCode::Esc), SquashAction::Close));
    }

    #[test]
    fn closing_with_message_requires_discard_confirmation() {
        let mut dialog = ready_dialog();
        dialog.message_editor.insert_str("feat: keep me");
        assert!(matches!(press(&mut dialog, KeyCode::Esc), SquashAction::None));
        assert_eq!(dialog.confirm, Some(SquashConfirm::Discard));
        assert_eq!(dialog.confirm_button, ConfirmDialogButton::Cancel);

        // Enter on the default Cancel button keeps the dialog and the message.
        assert!(matches!(press(&mut dialog, KeyCode::Enter), SquashAction::None));
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
        assert!(matches!(press(&mut dialog, KeyCode::Esc), SquashAction::None));
        assert_eq!(dialog.confirm, None);
    }
}
