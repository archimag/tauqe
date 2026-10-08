use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{
    methods, ConfigSetParams, GitSquashApplyParams, GitSquashGenerateMessageParams,
    GitSquashPreviewParams, ReviewStartParams,
};

use crate::app::{AppState, SquashBaseMode, SquashDialogFocus};
use crate::input::InputResult;
use crate::rpc::send_request;
use crate::ui::develop::DevelopView;
use crate::ui::history::HistoryViewState;

fn try_trigger_squash_confirm(dialog: &mut crate::app::SquashDialogState) {
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
    if dialog.message_buffer.trim().is_empty() {
        dialog.status_message = Some("Commit message cannot be empty (press 'g' to generate).".to_string());
        dialog.focus = SquashDialogFocus::MessageEditor;
        return;
    }
    dialog.confirm_apply = true;
    dialog.confirm_button = crate::app::ConfirmDialogButton::Cancel;
}

pub async fn handle_dialog_event(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<Option<InputResult>> {
    let mut st = state.lock().await;

    // 1. Squash dialog modal
    if let Some(mut dialog) = st.squash_dialog.take() {
        if dialog.confirm_apply {
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    dialog.confirm_button = match dialog.confirm_button {
                        crate::app::ConfirmDialogButton::Cancel => crate::app::ConfirmDialogButton::Confirm,
                        crate::app::ConfirmDialogButton::Confirm => crate::app::ConfirmDialogButton::Cancel,
                    };
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Enter if dialog.confirm_button == crate::app::ConfirmDialogButton::Confirm => {
                    let msg = dialog.message_buffer.trim().to_string();
                    let base_ref = dialog.base_ref.clone();
                    st.squash_dialog = None;
                    drop(st);
                    let params = GitSquashApplyParams { base_ref, message: msg };
                    send_request(server_writer, methods::GIT_SQUASH_APPLY, serde_json::to_value(params)?).await?;
                }
                KeyCode::Enter => {
                    dialog.confirm_apply = false;
                    dialog.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Char('y') | KeyCode::Char('Y') if crate::input::is_char_typing(key.modifiers) => {
                    let msg = dialog.message_buffer.trim().to_string();
                    let base_ref = dialog.base_ref.clone();
                    st.squash_dialog = None;
                    drop(st);
                    let params = GitSquashApplyParams { base_ref, message: msg };
                    send_request(server_writer, methods::GIT_SQUASH_APPLY, serde_json::to_value(params)?).await?;
                }
                KeyCode::Esc => {
                    dialog.confirm_apply = false;
                    dialog.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Char('n') | KeyCode::Char('N') if crate::input::is_char_typing(key.modifiers) => {
                    dialog.confirm_apply = false;
                    dialog.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    st.squash_dialog = Some(dialog);
                }
                _ => {
                    st.squash_dialog = Some(dialog);
                }
            }
            return Ok(Some(InputResult::Continue));
        }

        if dialog.custom_input_active {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('w') => {
                        crate::input::pop_word_backward(&mut dialog.custom_input);
                        st.squash_dialog = Some(dialog);
                        return Ok(Some(InputResult::Continue));
                    }
                    KeyCode::Char('u') => {
                        dialog.custom_input.clear();
                        st.squash_dialog = Some(dialog);
                        return Ok(Some(InputResult::Continue));
                    }
                    _ => {}
                }
            }

            match key.code {
                KeyCode::Esc => {
                    dialog.custom_input_active = false;
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Enter => {
                    let custom = dialog.custom_input.trim().to_string();
                    if !custom.is_empty() {
                        dialog.custom_input_active = false;
                        dialog.pending_base = Some((SquashBaseMode::Custom, custom.clone()));
                        dialog.loading = true;
                        dialog.status_message = Some(format!("Checking base '{}'...", custom));
                        st.squash_dialog = Some(dialog);
                        drop(st);
                        let params = GitSquashPreviewParams { base_ref: Some(custom) };
                        send_request(server_writer, methods::GIT_SQUASH_PREVIEW, serde_json::to_value(params)?).await?;
                    } else {
                        dialog.custom_input_active = false;
                        st.squash_dialog = Some(dialog);
                    }
                }
                KeyCode::Backspace => {
                    dialog.custom_input.pop();
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Char(c) if crate::input::is_char_typing(key.modifiers) => {
                    dialog.custom_input.push(c);
                    st.squash_dialog = Some(dialog);
                }
                _ => {
                    st.squash_dialog = Some(dialog);
                }
            }
            return Ok(Some(InputResult::Continue));
        }

        if dialog.focus == SquashDialogFocus::MessageEditor {
            let primary_mod = st.tui_config.input.primary_modifier;
            let is_primary = primary_mod.matches(key.modifiers);

            if is_primary || key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('w') => {
                        crate::input::pop_word_backward(&mut dialog.message_buffer);
                        st.squash_dialog = Some(dialog);
                        return Ok(Some(InputResult::Continue));
                    }
                    KeyCode::Char('u') => {
                        dialog.message_buffer.clear();
                        st.squash_dialog = Some(dialog);
                        return Ok(Some(InputResult::Continue));
                    }
                    _ => {}
                }
            }

            match key.code {
                KeyCode::Esc => {
                    dialog.focus = SquashDialogFocus::FileList;
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Tab => {
                    dialog.focus = SquashDialogFocus::FileList;
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Enter if is_primary || key.modifiers.contains(KeyModifiers::CONTROL) => {
                    try_trigger_squash_confirm(&mut dialog);
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Char('s') if is_primary || key.modifiers.contains(KeyModifiers::CONTROL) => {
                    try_trigger_squash_confirm(&mut dialog);
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Enter => {
                    dialog.message_buffer.push('\n');
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Char('g') if is_primary || key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.generating_message = true;
                    dialog.status_message = Some("Generating Conventional Commit message with AI...".to_string());
                    let base_ref = dialog.base_ref.clone();
                    st.squash_dialog = Some(dialog);
                    drop(st);
                    let params = GitSquashGenerateMessageParams { base_ref };
                    send_request(server_writer, methods::GIT_SQUASH_GENERATE_MESSAGE, serde_json::to_value(params)?).await?;
                }
                KeyCode::Backspace => {
                    dialog.message_buffer.pop();
                    st.squash_dialog = Some(dialog);
                }
                KeyCode::Char(c) if crate::input::is_char_typing(key.modifiers) => {
                    dialog.message_buffer.push(c);
                    st.squash_dialog = Some(dialog);
                }
                _ => {
                    st.squash_dialog = Some(dialog);
                }
            }
            return Ok(Some(InputResult::Continue));
        }

        match key.code {
            KeyCode::Esc => {
                st.squash_dialog = None;
            }
            KeyCode::Char('1') => {
                if let Some(ref base_ref) = dialog.session_base {
                    let base_ref = base_ref.clone();
                    dialog.pending_base = Some((SquashBaseMode::Session, base_ref.clone()));
                    dialog.loading = true;
                    dialog.status_message = Some("Switching base to Tauqe session...".to_string());
                    st.squash_dialog = Some(dialog);
                    drop(st);
                    let params = GitSquashPreviewParams { base_ref: Some(base_ref) };
                    send_request(server_writer, methods::GIT_SQUASH_PREVIEW, serde_json::to_value(params)?).await?;
                } else {
                    dialog.status_message = Some("Session base commit not available (no AI commits in session).".to_string());
                    st.squash_dialog = Some(dialog);
                }
            }
            KeyCode::Char('2') => {
                if let Some(ref base_ref) = dialog.upstream_base {
                    let base_ref = base_ref.clone();
                    dialog.pending_base = Some((SquashBaseMode::Upstream, base_ref.clone()));
                    dialog.loading = true;
                    dialog.status_message = Some(format!("Switching base to upstream '{}'...", base_ref));
                    st.squash_dialog = Some(dialog);
                    drop(st);
                    let params = GitSquashPreviewParams { base_ref: Some(base_ref) };
                    send_request(server_writer, methods::GIT_SQUASH_PREVIEW, serde_json::to_value(params)?).await?;
                } else {
                    dialog.status_message = Some("No upstream branch configured or detected.".to_string());
                    st.squash_dialog = Some(dialog);
                }
            }
            KeyCode::Char('3') => {
                dialog.custom_input_active = true;
                dialog.custom_input = dialog.base_ref.clone();
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Tab => {
                dialog.focus = match dialog.focus {
                    SquashDialogFocus::FileList => SquashDialogFocus::DiffView,
                    SquashDialogFocus::DiffView => SquashDialogFocus::MessageEditor,
                    SquashDialogFocus::MessageEditor => SquashDialogFocus::FileList,
                };
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Char('g') => {
                dialog.generating_message = true;
                dialog.status_message = Some("Generating Conventional Commit message with AI...".to_string());
                let base_ref = dialog.base_ref.clone();
                st.squash_dialog = Some(dialog);
                drop(st);
                let params = GitSquashGenerateMessageParams { base_ref };
                send_request(server_writer, methods::GIT_SQUASH_GENERATE_MESSAGE, serde_json::to_value(params)?).await?;
            }
            KeyCode::Char('e') | KeyCode::Char('m') => {
                dialog.focus = SquashDialogFocus::MessageEditor;
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                try_trigger_squash_confirm(&mut dialog);
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                try_trigger_squash_confirm(&mut dialog);
                st.squash_dialog = Some(dialog);
            }
            KeyCode::F(6) => {
                try_trigger_squash_confirm(&mut dialog);
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                if let Some(file) = dialog.files.get_mut(dialog.selected_file_index) {
                    file.expanded = !file.expanded;
                }
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if dialog.focus == SquashDialogFocus::DiffView {
                    dialog.diff_scroll = dialog.diff_scroll.saturating_sub(1);
                } else if dialog.selected_file_index > 0 {
                    dialog.selected_file_index -= 1;
                    dialog.diff_scroll = 0;
                }
                st.squash_dialog = Some(dialog);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if dialog.focus == SquashDialogFocus::DiffView {
                    dialog.diff_scroll = dialog.diff_scroll.saturating_add(1);
                } else if !dialog.files.is_empty() && dialog.selected_file_index + 1 < dialog.files.len() {
                    dialog.selected_file_index += 1;
                    dialog.diff_scroll = 0;
                }
                st.squash_dialog = Some(dialog);
            }
            KeyCode::PageUp => {
                dialog.diff_scroll = dialog.diff_scroll.saturating_sub(10);
                st.squash_dialog = Some(dialog);
            }
            KeyCode::PageDown => {
                dialog.diff_scroll = dialog.diff_scroll.saturating_add(10);
                st.squash_dialog = Some(dialog);
            }
            _ => {
                st.squash_dialog = Some(dialog);
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 1b. Review launch dialog
    if let Some(mut dialog) = st.review_dialog.take() {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('w') => {
                    crate::input::pop_word_backward(&mut dialog.prompt);
                    st.review_dialog = Some(dialog);
                    return Ok(Some(InputResult::Continue));
                }
                KeyCode::Char('u') => {
                    dialog.prompt.clear();
                    st.review_dialog = Some(dialog);
                    return Ok(Some(InputResult::Continue));
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let prompt = dialog.prompt.trim().to_string();
                let params = ReviewStartParams {
                    model: dialog.models.get(dialog.model_index).cloned(),
                    user_prompt: if prompt.is_empty() { None } else { Some(prompt) },
                };
                st.review.begin();
                drop(st);
                send_request(
                    server_writer,
                    methods::REVIEW_START,
                    serde_json::to_value(params)?,
                )
                .await?;
                return Ok(Some(InputResult::Continue));
            }
            code => {
                let count = dialog.models.len();
                match code {
                    KeyCode::Up | KeyCode::BackTab if count > 0 => {
                        dialog.model_index = (dialog.model_index + count - 1) % count;
                    }
                    KeyCode::Down | KeyCode::Tab if count > 0 => {
                        dialog.model_index = (dialog.model_index + 1) % count;
                    }
                    KeyCode::Backspace => {
                        dialog.prompt.pop();
                    }
                    KeyCode::Char(c) if crate::input::is_char_typing(key.modifiers) => {
                        dialog.prompt.push(c);
                    }
                    _ => {}
                }
                st.review_dialog = Some(dialog);
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 2. Cancel confirmation
    if st.confirm_cancel {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                st.confirm_button = match st.confirm_button {
                    crate::app::ConfirmDialogButton::Cancel => crate::app::ConfirmDialogButton::Confirm,
                    crate::app::ConfirmDialogButton::Confirm => crate::app::ConfirmDialogButton::Cancel,
                };
            }
            KeyCode::Enter if st.confirm_button == crate::app::ConfirmDialogButton::Confirm => {
                st.confirm_cancel = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                if st.review.running {
                    st.review.running = false;
                }
                drop(st);
                send_request(server_writer, methods::MODEL_CANCEL, serde_json::json!({})).await?;
            }
            KeyCode::Enter => {
                st.confirm_cancel = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if crate::input::is_char_typing(key.modifiers) => {
                st.confirm_cancel = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                if st.review.running {
                    st.review.running = false;
                }
                drop(st);
                send_request(server_writer, methods::MODEL_CANCEL, serde_json::json!({})).await?;
            }
            _ => {
                st.confirm_cancel = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 2a. Quit confirmation
    if st.confirm_quit {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                st.confirm_button = match st.confirm_button {
                    crate::app::ConfirmDialogButton::Cancel => crate::app::ConfirmDialogButton::Confirm,
                    crate::app::ConfirmDialogButton::Confirm => crate::app::ConfirmDialogButton::Cancel,
                };
            }
            KeyCode::Enter if st.confirm_button == crate::app::ConfirmDialogButton::Confirm => {
                st.confirm_quit = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                return Ok(Some(InputResult::Exit));
            }
            KeyCode::Enter => {
                st.confirm_quit = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if crate::input::is_char_typing(key.modifiers) => {
                st.confirm_quit = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                return Ok(Some(InputResult::Exit));
            }
            _ => {
                st.confirm_quit = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 2b. Context clear auto confirmation
    if st.context_view.confirm_clear_auto {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                st.confirm_button = match st.confirm_button {
                    crate::app::ConfirmDialogButton::Cancel => crate::app::ConfirmDialogButton::Confirm,
                    crate::app::ConfirmDialogButton::Confirm => crate::app::ConfirmDialogButton::Cancel,
                };
            }
            KeyCode::Enter if st.confirm_button == crate::app::ConfirmDialogButton::Confirm => {
                let auto_count = st.context.items.iter().filter(|i| i.layer == tauqe_protocol::ContextLayer::Auto).count();
                st.context_view.confirm_clear_auto = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.notify_success(format!("Cleared {} auto file(s) from context", auto_count));
                drop(st);
                let params = tauqe_protocol::ContextClearParams {
                    layer: Some(tauqe_protocol::ContextLayer::Auto),
                };
                send_request(server_writer, methods::CONTEXT_CLEAR, serde_json::to_value(params)?).await?;
            }
            KeyCode::Enter => {
                st.context_view.confirm_clear_auto = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if crate::input::is_char_typing(key.modifiers) => {
                let auto_count = st.context.items.iter().filter(|i| i.layer == tauqe_protocol::ContextLayer::Auto).count();
                st.context_view.confirm_clear_auto = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.notify_success(format!("Cleared {} auto file(s) from context", auto_count));
                drop(st);
                let params = tauqe_protocol::ContextClearParams {
                    layer: Some(tauqe_protocol::ContextLayer::Auto),
                };
                send_request(server_writer, methods::CONTEXT_CLEAR, serde_json::to_value(params)?).await?;
            }
            _ => {
                st.context_view.confirm_clear_auto = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 3. Undo confirmation
    if st.confirm_undo {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                st.confirm_button = match st.confirm_button {
                    crate::app::ConfirmDialogButton::Cancel => crate::app::ConfirmDialogButton::Confirm,
                    crate::app::ConfirmDialogButton::Confirm => crate::app::ConfirmDialogButton::Cancel,
                };
            }
            KeyCode::Enter if st.confirm_button == crate::app::ConfirmDialogButton::Confirm => {
                st.confirm_undo = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                drop(st);
                send_request(server_writer, methods::GIT_UNDO, serde_json::json!({})).await?;
            }
            KeyCode::Enter => {
                st.confirm_undo = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if crate::input::is_char_typing(key.modifiers) => {
                st.confirm_undo = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                drop(st);
                send_request(server_writer, methods::GIT_UNDO, serde_json::json!({})).await?;
            }
            _ => {
                st.confirm_undo = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 4a. Delete plan confirmation
    if let Some(plan_id) = st.confirm_delete_plan.take() {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                st.confirm_button = match st.confirm_button {
                    crate::app::ConfirmDialogButton::Cancel => crate::app::ConfirmDialogButton::Confirm,
                    crate::app::ConfirmDialogButton::Confirm => crate::app::ConfirmDialogButton::Cancel,
                };
                st.confirm_delete_plan = Some(plan_id);
            }
            KeyCode::Enter if st.confirm_button == crate::app::ConfirmDialogButton::Confirm => {
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.plans_view.reset_view_for_new_plan();
                st.plans_view.current_plan = None;
                st.plans_view.active_plan_id = None;
                drop(st);
                send_request(server_writer, methods::PLAN_DELETE, serde_json::json!({ "id": plan_id })).await?;
                send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
                send_request(server_writer, methods::PLAN_GET, serde_json::json!({})).await?;
            }
            KeyCode::Enter => {
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if crate::input::is_char_typing(key.modifiers) => {
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.plans_view.reset_view_for_new_plan();
                st.plans_view.current_plan = None;
                st.plans_view.active_plan_id = None;
                drop(st);
                send_request(server_writer, methods::PLAN_DELETE, serde_json::json!({ "id": plan_id })).await?;
                send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
                send_request(server_writer, methods::PLAN_GET, serde_json::json!({})).await?;
            }
            _ => {
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 4. Clear history confirmation
    if st.confirm_clear_history {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                st.confirm_button = match st.confirm_button {
                    crate::app::ConfirmDialogButton::Cancel => crate::app::ConfirmDialogButton::Confirm,
                    crate::app::ConfirmDialogButton::Confirm => crate::app::ConfirmDialogButton::Cancel,
                };
            }
            KeyCode::Enter if st.confirm_button == crate::app::ConfirmDialogButton::Confirm => {
                st.confirm_clear_history = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                let cost = st.model.session_total_cost;
                let prev_cost = st.model.prev_cost;
                st.model = DevelopView {
                    session_total_cost: cost,
                    prev_cost,
                    ..Default::default()
                };
                st.history_view = HistoryViewState::default();
                drop(st);
                send_request(server_writer, methods::MODEL_CLEAR_HISTORY, serde_json::json!({})).await?;
            }
            KeyCode::Enter => {
                st.confirm_clear_history = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if crate::input::is_char_typing(key.modifiers) => {
                st.confirm_clear_history = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                let cost = st.model.session_total_cost;
                let prev_cost = st.model.prev_cost;
                st.model = DevelopView {
                    session_total_cost: cost,
                    prev_cost,
                    ..Default::default()
                };
                st.history_view = HistoryViewState::default();
                drop(st);
                send_request(server_writer, methods::MODEL_CLEAR_HISTORY, serde_json::json!({})).await?;
            }
            _ => {
                st.confirm_clear_history = false;
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 5. Selection dialog
    if let Some(mut dialog) = st.selection_dialog.take() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                dialog.selected_index = dialog.selected_index.saturating_sub(1);
                st.selection_dialog = Some(dialog);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !dialog.items.is_empty() && dialog.selected_index + 1 < dialog.items.len() {
                    dialog.selected_index += 1;
                }
                st.selection_dialog = Some(dialog);
            }
            KeyCode::Enter => {
                if let Some(chosen) = dialog.items.get(dialog.selected_index).cloned() {
                    st.active_model = chosen.clone();
                    let params = ConfigSetParams {
                        model: Some(chosen),
                        ..Default::default()
                    };
                    drop(st);
                    send_request(server_writer, methods::CONFIG_SET, serde_json::to_value(params)?).await?;
                }
            }
            _ => {
                st.selection_dialog = Some(dialog);
            }
        }
        return Ok(Some(InputResult::Continue));
    }

    // 6. Help popup
    if st.show_help {
        match key.code {
            KeyCode::F(1) | KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                st.show_help = false;
                st.help_scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                st.help_scroll = st.help_scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                st.help_scroll = st.help_scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                st.help_scroll = st.help_scroll.saturating_add(10);
            }
            KeyCode::PageUp => {
                st.help_scroll = st.help_scroll.saturating_sub(10);
            }
            KeyCode::Home => {
                st.help_scroll = 0;
            }
            _ => {}
        }
        return Ok(Some(InputResult::Continue));
    }

    Ok(None)
}
