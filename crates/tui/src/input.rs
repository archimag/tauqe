use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, GitSquashPreviewParams};

use crate::app::{AppState, ViewMode};
use crate::rpc::send_request;

pub mod context;
pub mod develop;
pub mod dialogs;
pub mod history;
pub mod mouse;
pub mod onboarding;
pub mod plans;
pub mod review;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputResult {
    Continue,
    Exit,
}

pub async fn handle_terminal_event(
    terminal_event: crossterm::event::Event,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    match terminal_event {
        crossterm::event::Event::Paste(text) => {
            let mut st = state.lock().await;
            if let Some(dialog) = st.review_dialog.as_mut() {
                dialog.prompt.push_str(&text.replace(['\n', '\r'], " "));
            } else if st.view_mode == ViewMode::Develop
                && !st.show_help
                && !st.confirm_undo
                && !st.confirm_clear_history
                && st.selection_dialog.is_none()
            {
                st.input_editor.insert_paste(&text);
            } else if st.view_mode == ViewMode::Onboarding && st.onboarding.input_active {
                st.onboarding.input_buffer.push_str(text.trim());
            }
            Ok(InputResult::Continue)
        }
        crossterm::event::Event::Mouse(mouse) => {
            mouse::handle_mouse_event(mouse, state, server_writer).await
        }
        crossterm::event::Event::Key(key)
            if key.kind != crossterm::event::KeyEventKind::Release =>
        {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
                return Ok(InputResult::Exit);
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

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('1') => {
                st.view_mode = ViewMode::Develop;
                st.context_view.status_message = None;
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Char('2') => {
                st.view_mode = ViewMode::Context;
                st.context_view.status_message = None;
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Char('3') => {
                st.view_mode = ViewMode::Review;
                st.context_view.status_message = None;
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Char('4') => {
                st.view_mode = ViewMode::Plans;
                st.context_view.status_message = None;
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Char('5') => {
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
            }
            KeyCode::Char('o') => {
                st.model.git_notification =
                    Some("Reloading configuration from disk...".to_string());
                drop(st);
                send_request(server_writer, methods::CONFIG_RELOAD, serde_json::json!({})).await?;
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Char('c') => {
                if st.model.is_busy() {
                    st.confirm_cancel = true;
                }
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Char('l') => {
                if st.model.is_busy() {
                    st.model.git_notification =
                        Some("Cannot clear history while model is generating".to_string());
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
            KeyCode::Char('m') => {
                if st.model.is_busy() {
                    st.model.git_notification =
                        Some("Cannot change model while model is generating".to_string());
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
            _ => {}
        }
    }

    if key.code == KeyCode::F(6) {
        if st.model.is_busy() {
            st.model.git_notification =
                Some("Cannot squash commits while model is generating".to_string());
        } else {
            st.squash_dialog = Some(crate::app::SquashDialogState::default());
            drop(st);
            let params = GitSquashPreviewParams { base_ref: None };
            send_request(
                server_writer,
                methods::GIT_SQUASH_PREVIEW,
                serde_json::to_value(params)?,
            )
            .await?;
        }
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
