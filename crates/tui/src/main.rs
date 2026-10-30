#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod app;
pub mod clipboard;
pub mod command;
pub mod components;
pub mod config;
pub mod dialogs;
pub mod editor;
pub mod i18n;
pub mod input;
pub mod markdown;
pub mod model_display;
pub mod rpc;
pub mod terminal;
pub mod theme;
pub mod ui;
pub mod views;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::i18n::active_t;
use tauqe_protocol::{methods, HistoryGetParams, Message};
use tokio::sync::Mutex;

use crate::app::{AppInit, AppState, ViewMode};
use crate::editor::InputEditor;
use crate::input::InputResult;
use crate::ui::render_ui;
use crate::views::repl::SPINNER_FRAMES;

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
    let workflow = handshake.workflow.unwrap_or_default();
    let edit_protocol = handshake.edit_protocol.unwrap_or_default();
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
            tauqe_protocol::EditProtocolKind::Mixed,
        ]
    } else {
        handshake.available_edit_protocols
    };
    let active_model = handshake.model;

    let (tui_config, tui_config_error) = match config::TuiConfig::load_checked() {
        Ok(cfg) => (cfg, None),
        Err(err) => (config::TuiConfig::default(), Some(err)),
    };

    let mut input_editor = InputEditor::default();
    if let Some(history_path) = editor::default_prompt_history_path() {
        input_editor.set_history_path(history_path);
    }

    // Load the single custom locale (if any) so the interface starts localized
    // and falls through to English for every missing key (c-4, c-5).
    let i18n = crate::i18n::I18nEngine::new(crate::i18n::load_custom_locale());

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
        context: init.context,
        onboarding: init.onboarding,
        input_editor,
        tui_config,
        i18n,
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

    // Review presets declared by the project, so the launch dialog offers them
    // without the client re-reading `tauqe.toml` (c-5).
    rpc::send_request(&mut conn, methods::CONFIG_GET, serde_json::json!({})).await?;

    // Initial plans fetch
    rpc::send_request(&mut conn, methods::PLAN_LIST, serde_json::json!({})).await?;
    rpc::send_request(&mut conn, methods::PLAN_GET, serde_json::json!({})).await?;

    // Safety net: restore the terminal before reporting an abnormal exit. The
    // panic hook and the termination handler run the same routine as the
    // terminal guard, so every exit path undoes exactly the same changes.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        crate::terminal::restore_terminal();
        original_hook(panic_info);
    }));
    terminal::install_termination_restore();

    let mut msg_rx = rpc::spawn_message_reader(server_reader);
    let (mut terminal, _guard) = terminal::init_terminal()?;
    terminal.clear()?;
    let mut event_rx = terminal::spawn_event_reader();
    let mut last_spinner_tick = Instant::now();
    let mut last_draw: Option<Instant> = None;
    let mut dirty = true;
    let mut was_spinning = false;

    loop {
        let (needs_spin, has_active_notif) = {
            let mut st = state.lock().await;
            let spin = (if st.view_mode == ViewMode::Repl {
                st.model.needs_spinner()
            } else {
                st.model.background_active()
            }) || st.review.running;

            if spin && last_spinner_tick.elapsed() >= SPINNER_INTERVAL {
                st.model.spinner_frame = (st.model.spinner_frame + 1) % SPINNER_FRAMES.len();
                last_spinner_tick = Instant::now();
                dirty = true;
            } else if was_spinning && !spin {
                // When animation settles (e.g. splash typewriter completes),
                // request a final draw to render the resting state.
                dirty = true;
            }
            was_spinning = spin;

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
                        let message = active_t("notify.command_error");
                        st.notify_error(message.replacen("{}", &err.to_string(), 1));
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
                                Some(status) => active_t("notify.server_disconnected.terminated")
                                    .replacen("{}", &status.to_string(), 1),
                                None => active_t("notify.server_disconnected.unexpected"),
                            };
                            let banner = server_disconnect_banner(&status_text, &server_log_path);
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
                        let status_template = active_t("notify.server_disconnected.exited");
                        let status_text = status_template.replacen("{}", &status.to_string(), 1);
                        let banner = server_disconnect_banner(&status_text, &server_log_path);
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
    // Dropping the connection closes the daemon's stdin, which is what asks it
    // to stop. It answers by cancelling the active turn and rolling its Git
    // transaction back, so the wait is the shared protocol budget instead of a
    // local literal: the client must never kill the daemon inside the cleanup
    // the drain exists to protect. A daemon that already exited resolves `wait`
    // immediately, so an ordinary quit never pays the budget.
    if tokio::time::timeout(tauqe_protocol::CLIENT_SHUTDOWN_WAIT, server_child.wait())
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
/// Renders the daemon disconnect banner shown in the server-disconnected
/// modal.
///
/// The status line arrives pre-localized; the banner appends the log path and
/// the quit hint from the catalog, so a hot-swapped locale covers the whole
/// modal (Conventions §9).
fn server_disconnect_banner(status_text: &str, log_path: &std::path::Path) -> String {
    active_t("notify.server_disconnected.banner")
        .replacen("{}", status_text, 1)
        .replacen("{}", &log_path.display().to_string(), 1)
}

#[cfg(test)]
mod tests {
    /// The client owns the spawned daemon and is the only side that can kill
    /// it, so its wait must outlast the daemon's own drain budget: a shorter
    /// wait would terminate the process while it still cancels the active turn
    /// and rolls its Git transaction back. Reading the shared contract keeps a
    /// local literal from silently reintroducing that race.
    #[test]
    fn client_shutdown_wait_outlasts_the_server_drain_budget() {
        assert!(
            tauqe_protocol::CLIENT_SHUTDOWN_WAIT > tauqe_protocol::SHUTDOWN_DRAIN_BUDGET,
            "the client would kill the daemon before its drain completes"
        );
    }
}
