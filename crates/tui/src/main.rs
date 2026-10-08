#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod app;
pub mod clipboard;
pub mod config;
pub mod editor;
pub mod input;
pub mod markdown;
pub mod rpc;
pub mod terminal;
pub mod ui;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tauqe_protocol::{methods, HistoryGetParams, Message};
use tokio::sync::Mutex;

use crate::app::AppState;
use crate::editor::InputEditor;
use crate::input::InputResult;
use crate::ui::context::ContextViewState;
use crate::ui::develop::{DevelopView, SPINNER_FRAMES};
use crate::ui::history::HistoryViewState;
use crate::ui::plans::PlansViewState;
use crate::ui::review::ReviewViewState;
use crate::ui::render_ui;

const SPINNER_INTERVAL: Duration = Duration::from_millis(100);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut server_child, mut server_writer, mut server_reader, server_log_path) =
        rpc::start_server().await?;

    let (
        init_result,
        initial_context,
        initial_files,
        onboarding_state,
        initial_view_mode,
        init_events,
    ) = rpc::initialize_connection(&mut server_writer, &mut server_reader).await?;

    let repo_state = init_result.repository;
    let protocol_version = init_result.protocol_version.clone();
    let workflow = init_result
        .workflow
        .unwrap_or_else(|| "toolchain".to_string());
    let edit_protocol = init_result
        .edit_protocol
        .unwrap_or_else(|| "xml".to_string());
    let available_workflows = if init_result.available_workflows.is_empty() {
        vec![
            "toolchain".to_string(),
            "git".to_string(),
            "naive".to_string(),
        ]
    } else {
        init_result.available_workflows
    };
    let available_edit_protocols = if init_result.available_edit_protocols.is_empty() {
        vec!["xml".to_string(), "structured".to_string()]
    } else {
        init_result.available_edit_protocols
    };
    let active_model = init_result.model;
    let available_models = if init_result.available_models.is_empty() {
        vec![active_model.clone()]
    } else {
        init_result.available_models
    };

    let tui_config = config::TuiConfig::load();

    let state = Arc::new(Mutex::new(AppState {
        view_mode: initial_view_mode,
        protocol_version,
        repo_state,
        all_repo_files: initial_files,
        workflow,
        edit_protocol,
        available_workflows,
        available_edit_protocols,
        active_model,
        available_models,
        model: DevelopView::default(),
        context: initial_context,
        context_view: ContextViewState::default(),
        history_view: HistoryViewState::default(),
        review: ReviewViewState::default(),
        plans_view: PlansViewState::default(),
        review_dialog: None,
        notification: None,
        turn_started_at: None,
        onboarding: onboarding_state,
        input_editor: InputEditor::default(),
        tui_config,
        show_help: false,
        help_scroll: 0,
        confirm_cancel: false,
        confirm_quit: false,
        confirm_undo: false,
        confirm_clear_history: false,
        confirm_delete_plan: None,
        confirm_button: crate::app::ConfirmDialogButton::Cancel,
        selection_dialog: None,
        squash_dialog: None,
        server_disconnected: None,
        server_log_path: server_log_path.clone(),
        last_model_height: 10,
        header_clicks: crate::app::HeaderClickAreas::default(),
    }));

    let mut is_reasoning = false;
    for ev in init_events {
        rpc::handle_event(ev, &state, &mut is_reasoning).await;
    }

    // Initial history fetch
    rpc::send_request(
        &mut server_writer,
        methods::HISTORY_GET,
        serde_json::to_value(HistoryGetParams {
            limit: Some(10),
            before_id: None,
        })?,
    )
    .await?;

    // Restore the latest saved review (if any)
    rpc::send_request(&mut server_writer, methods::REVIEW_GET, serde_json::json!({})).await?;

    // Initial plans fetch
    rpc::send_request(&mut server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
    rpc::send_request(&mut server_writer, methods::PLAN_GET, serde_json::json!({})).await?;

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

    loop {
        {
            let mut st = state.lock().await;
            if st.model.needs_spinner() && last_spinner_tick.elapsed() >= SPINNER_INTERVAL {
                st.model.spinner_frame = (st.model.spinner_frame + 1) % SPINNER_FRAMES.len();
                last_spinner_tick = Instant::now();
            }
            st.model.flush_pending_markdown();
            terminal.draw(|f| render_ui(f, &mut st))?;
        }

        tokio::select! {
            Some(terminal_event) = event_rx.recv() => {
                match input::handle_terminal_event(terminal_event, &state, &mut server_writer).await {
                    Ok(InputResult::Continue) => {}
                    Ok(InputResult::Exit) => break,
                    Err(err) => {
                        let mut st = state.lock().await;
                        st.notify_error(format!("Command error: {}", err));
                    }
                }
            }
            msg = msg_rx.recv() => {
                match msg {
                    Some(Message::Event(ev)) => rpc::handle_event(ev, &state, &mut is_reasoning).await,
                    Some(Message::Response(resp)) => rpc::handle_response(resp, &state).await,
                    Some(_) => {}
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
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if let Ok(Some(status)) = server_child.try_wait() {
                    let mut st = state.lock().await;
                    if st.server_disconnected.is_none() {
                        let banner = format!("Server process exited ({}). Logs: {}. Press 'q' or Ctrl+Q to exit.", status, server_log_path.display());
                        st.server_disconnected = Some(banner.clone());
                        st.notify_error(banner);
                    }
                }
            }
        }
    }

    let _ = server_child.kill().await;

    Ok(())
}
