#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod app;
pub mod clipboard;
pub mod components;
pub mod config;
pub mod dialogs;
pub mod editor;
pub mod input;
pub mod markdown;
pub mod rpc;
pub mod terminal;
pub mod ui;
pub mod views;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tauqe_protocol::{methods, HistoryGetParams, Message};
use tokio::sync::Mutex;

use crate::app::{AppInit, AppState};
use crate::editor::InputEditor;
use crate::input::InputResult;
use crate::ui::render_ui;
use crate::views::develop::SPINNER_FRAMES;

const SPINNER_INTERVAL: Duration = Duration::from_millis(100);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut server_child, server_stdin, mut server_reader, server_log_path) =
        rpc::start_server().await?;
    let mut conn = rpc::RpcConnection::new(server_stdin);

    let init = rpc::initialize_connection(&mut conn, &mut server_reader).await?;
    let handshake = init.handshake;

    let repo_state = handshake.repository;
    let protocol_version = handshake.protocol_version.clone();
    let workflow = handshake
        .workflow
        .unwrap_or_default();
    let edit_protocol = handshake
        .edit_protocol
        .unwrap_or_default();
    let available_workflows = if handshake.available_workflows.is_empty() {
        vec![
            tauqe_protocol::WorkflowKind::Git,
            tauqe_protocol::WorkflowKind::Naive,
        ]
    } else {
        handshake.available_workflows
    };
    let available_edit_protocols = if handshake.available_edit_protocols.is_empty() {
        vec![
            tauqe_protocol::EditProtocolKind::Xml,
            tauqe_protocol::EditProtocolKind::Structured,
        ]
    } else {
        handshake.available_edit_protocols
    };
    let active_model = handshake.model;
    let available_models = if handshake.available_models.is_empty() {
        vec![active_model.clone()]
    } else {
        handshake.available_models
    };

    let (tui_config, tui_config_error) = match config::TuiConfig::load_checked() {
        Ok(cfg) => (cfg, None),
        Err(err) => (config::TuiConfig::default(), Some(err)),
    };

    let mut input_editor = InputEditor::default();
    input_editor.set_history_path(editor::default_prompt_history_path());

    let state = Arc::new(Mutex::new(AppState::new(AppInit {
        view_mode: init.view_mode,
        protocol_version,
        repo_state,
        all_repo_files: init.files,
        workflow,
        edit_protocol,
        available_workflows,
        available_edit_protocols,
        active_model,
        available_models,
        context: init.context,
        onboarding: init.onboarding,
        input_editor,
        tui_config,
        server_log_path: server_log_path.clone(),
    })));

    {
        let mut st = state.lock().await;
        st.sync_theme_mode();
        if let Some(err) = tui_config_error {
            st.notify_error(format!("{}; using default TUI settings", err));
        }
    }

    let mut is_reasoning = false;
    for ev in init.events {
        rpc::handle_event(ev, &state, &mut is_reasoning).await;
    }

    // Initial history fetch
    rpc::send_request(
        &mut conn,
        methods::HISTORY_GET,
        serde_json::to_value(HistoryGetParams {
            limit: Some(10),
            before_id: None,
        })?,
    )
    .await?;

    // Restore saved reviews and the active session
    rpc::send_request(&mut conn, methods::REVIEW_LIST, serde_json::json!({})).await?;
    rpc::send_request(&mut conn, methods::REVIEW_GET, serde_json::json!({})).await?;

    // Initial model selection and tier assignments
    rpc::send_request(&mut conn, methods::MODEL_CURRENT, serde_json::json!({})).await?;

    // Initial plans fetch
    rpc::send_request(&mut conn, methods::PLAN_LIST, serde_json::json!({})).await?;
    rpc::send_request(&mut conn, methods::PLAN_GET, serde_json::json!({})).await?;

    // Safety net: restore the terminal before reporting any unexpected panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        original_hook(panic_info);
    }));

    let mut msg_rx = rpc::spawn_message_reader(server_reader);
    let (mut terminal, _guard) = terminal::init_terminal()?;
    let mut event_rx = terminal::spawn_event_reader();
    let mut last_spinner_tick = Instant::now();
    let mut last_draw: Option<Instant> = None;
    let mut dirty = true;

    loop {
        let (needs_spin, has_active_notif) = {
            let mut st = state.lock().await;
            let spin = st.model.needs_spinner() || st.review.running;
            if spin && last_spinner_tick.elapsed() >= SPINNER_INTERVAL {
                st.model.spinner_frame = (st.model.spinner_frame + 1) % SPINNER_FRAMES.len();
                last_spinner_tick = Instant::now();
                dirty = true;
            }

            if dirty && last_draw.is_none_or(|t| t.elapsed() >= FRAME_INTERVAL) {
                st.model.flush_pending_markdown();
                terminal.draw(|f| render_ui(f, &mut st))?;
                last_draw = Some(Instant::now());
                dirty = false;
            }

            (spin, st.active_notification().is_some())
        };

        let sleep_duration = if dirty {
            match last_draw {
                Some(t) => FRAME_INTERVAL
                    .saturating_sub(t.elapsed())
                    .max(Duration::from_millis(1)),
                None => Duration::from_millis(1),
            }
        } else if needs_spin {
            let elapsed = last_spinner_tick.elapsed();
            if elapsed >= SPINNER_INTERVAL {
                Duration::from_millis(5)
            } else {
                SPINNER_INTERVAL - elapsed
            }
        } else if has_active_notif {
            Duration::from_millis(200)
        } else {
            Duration::from_millis(500)
        };

        tokio::select! {
            Some(terminal_event) = event_rx.recv() => {
                match input::handle_terminal_event(terminal_event, &state, &mut conn).await {
                    Ok(InputResult::Continue) => {
                        dirty = true;
                    }
                    Ok(InputResult::Exit) => break,
                    Err(err) => {
                        let mut st = state.lock().await;
                        st.notify_error(format!("Command error: {}", err));
                        dirty = true;
                    }
                }
            }
            msg = msg_rx.recv() => {
                match msg {
                    Some(msg) => {
                        dispatch_server_message(msg, &state, &mut is_reasoning, &mut conn).await;
                        while let Ok(msg) = msg_rx.try_recv() {
                            dispatch_server_message(msg, &state, &mut is_reasoning, &mut conn).await;
                        }
                        dirty = true;
                    }
                    None => {
                        let mut st = state.lock().await;
                        if st.server_disconnected.is_none() {
                            let exit_status = server_child.try_wait().ok().flatten();
                            let status_text = match exit_status {
                                Some(status) => format!("Server process terminated ({})", status),
                                None => "Server disconnected unexpectedly".to_string(),
                            };
                            let banner = format!("{}. Logs: {}. Press 'q' or Ctrl+Q to exit.", status_text, server_log_path.display());
                            st.server_disconnected = Some(banner.clone());
                            st.notify_error(banner);
                            dirty = true;
                        }
                    }
                }
            }
            _ = tokio::time::sleep(sleep_duration) => {
                if let Ok(Some(status)) = server_child.try_wait() {
                    let mut st = state.lock().await;
                    if st.server_disconnected.is_none() {
                        let banner = format!("Server process exited ({}). Logs: {}. Press 'q' or Ctrl+Q to exit.", status, server_log_path.display());
                        st.server_disconnected = Some(banner.clone());
                        st.notify_error(banner);
                        dirty = true;
                    }
                } else if has_active_notif {
                    let st = state.lock().await;
                    if st.active_notification().is_none() {
                        dirty = true;
                    }
                }
            }
        }
    }

    drop(conn);
    let shutdown_timeout = Duration::from_millis(1500);
    if tokio::time::timeout(shutdown_timeout, server_child.wait())
        .await
        .is_err()
    {
        let _ = server_child.kill().await;
    }

    Ok(())
}

/// Applies a single server message to the shared application state.
///
/// Events and responses are dispatched in arrival order to keep streaming
/// updates consistent; requests are ignored because the server never
/// initiates RPC calls toward the client.
async fn dispatch_server_message(
    msg: Message,
    state: &Arc<Mutex<AppState>>,
    is_reasoning: &mut bool,
    conn: &mut rpc::RpcConnection,
) {
    match msg {
        Message::Event(ev) => {
            rpc::handle_event(ev, state, is_reasoning).await;
        }
        Message::Response(resp) => {
            rpc::handle_response(resp, state, conn).await;
        }
        Message::Request(_) => {}
    }
}
