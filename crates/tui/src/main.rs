pub mod app;
pub mod clipboard;
pub mod context_view;
pub mod editor;
pub mod markdown;
pub mod model_view;
pub mod ui;

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyCode, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tauqe_protocol::{
    events, methods, ConfigSetParams, ConfigState, ContextAccess, ContextAddParams,
    ContextAddPatternParams, ContextAddPatternResult, ContextLayer, ContextRemoveParams,
    ContextSetAccessParams, ContextState, EditFileDoneEvent, EditFileRetryingEvent,
    EditFileStartedEvent, EditFinishedEvent, EditHunkEvent, Event, GitCommitCreatedEvent,
    GitSquashApplyParams, GitSquashApplyResult, GitSquashPreviewParams, GitSquashPreviewResult,
    GitUndoResult, HistoryEntryAddedEvent, HistoryGetParams, HistoryGetResult, InitializeParams,
    InitializeResult, Message, ModelAskParams, ModelDeltaEvent, ModelErrorEvent, ModelFinishedEvent,
    ModelResultEvent, ModelStartedEvent, ModelUsageEvent, RepositoryListFilesResult,
    RepositoryState, Request, RequestId, Response, ToolchainResultEvent, ToolchainStartedEvent,
    PROTOCOL_VERSION,
};

use crate::app::{AppState, OnboardingStep, ViewMode};
use crate::context_view::{ContextRow, ContextViewState};
use crate::editor::InputEditor;
use crate::model_view::{ModelView, StreamingFileEdit, StreamingHunk, SPINNER_FRAMES};
use crate::ui::render_ui;

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
        );
        let _ = disable_raw_mode();
    }
}

fn find_server_binary() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let exe_name = if cfg!(windows) {
                "tauqe-server.exe"
            } else {
                "tauqe-server"
            };
            let candidate = parent.join(exe_name);
            if candidate.exists() {
                return candidate;
            }
        }
    }

    for dir in &["target/debug", "target/release"] {
        let exe_name = if cfg!(windows) {
            "tauqe-server.exe"
        } else {
            "tauqe-server"
        };
        let candidate = Path::new(dir).join(exe_name);
        if candidate.exists() {
            return candidate;
        }
    }

    PathBuf::from(if cfg!(windows) {
        "tauqe-server.exe"
    } else {
        "tauqe-server"
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server_path = find_server_binary();
    let mut server_child = tokio::process::Command::new(&server_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "Failed to spawn tauqe-server at '{}'. Make sure to run 'cargo build' first.",
                server_path.display()
            )
        })?;

    let child_stdin = server_child
        .stdin
        .take()
        .context("Failed to open child server stdin")?;
    let child_stdout = server_child
        .stdout
        .take()
        .context("Failed to open child server stdout")?;

    let mut server_writer = child_stdin;
    let mut server_reader = BufReader::new(child_stdout).lines();

    // Handshake: client/initialize
    let init_req = Request {
        id: RequestId::Number(1),
        method: methods::CLIENT_INITIALIZE.to_string(),
        params: Some(serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            client_name: "tauqe-tui".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        })?),
    };

    let mut req_str = serde_json::to_string(&init_req)?;
    req_str.push('\n');
    server_writer.write_all(req_str.as_bytes()).await?;
    server_writer.flush().await?;

    let init_resp_line = server_reader
        .next_line()
        .await?
        .context("Server closed connection during initialize handshake")?;

    let response: Response = serde_json::from_str(&init_resp_line)
        .context("Failed to parse server initialize response")?;

    let init_result: InitializeResult = match response.result {
        Some(val) => serde_json::from_value(val).context("Invalid initialize result")?,
        None => {
            let err_msg = response
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "Unknown server error".to_string());
            anyhow::bail!("Server initialization rejected: {}", err_msg);
        }
    };

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
    let active_model = init_result.model.unwrap_or_default();
    let available_models = if init_result.available_models.is_empty() {
        if active_model.is_empty() {
            Vec::new()
        } else {
            vec![active_model.clone()]
        }
    } else {
        init_result.available_models
    };

    // Initial context fetch
    let ctx_req = Request {
        id: RequestId::Number(2),
        method: methods::CONTEXT_GET.to_string(),
        params: None,
    };
    let mut ctx_req_str = serde_json::to_string(&ctx_req)?;
    ctx_req_str.push('\n');
    server_writer.write_all(ctx_req_str.as_bytes()).await?;
    server_writer.flush().await?;

    let initial_context = match server_reader.next_line().await {
        Ok(Some(line)) => {
            if let Ok(resp) = serde_json::from_str::<Response>(&line) {
                resp.result
                    .and_then(|v| serde_json::from_value::<ContextState>(v).ok())
                    .unwrap_or_default()
            } else {
                ContextState::default()
            }
        }
        _ => ContextState::default(),
    };

    // Initial repository file list fetch
    let list_req = Request {
        id: RequestId::Number(3),
        method: methods::REPOSITORY_LIST_FILES.to_string(),
        params: None,
    };
    let mut list_req_str = serde_json::to_string(&list_req)?;
    list_req_str.push('\n');
    server_writer.write_all(list_req_str.as_bytes()).await?;
    server_writer.flush().await?;

    let initial_files = match server_reader.next_line().await {
        Ok(Some(line)) => {
            if let Ok(resp) = serde_json::from_str::<Response>(&line) {
                resp.result
                    .and_then(|v| serde_json::from_value::<RepositoryListFilesResult>(v).ok())
                    .map(|r| r.files)
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };

    let mut initial_view_mode = ViewMode::Model;
    let mut onboarding_state = crate::app::OnboardingState::default();

    // Query system status for onboarding check
    let status_req = Request {
        id: RequestId::Number(4),
        method: "system/status".to_string(),
        params: None,
    };
    let mut status_req_str = serde_json::to_string(&status_req)?;
    status_req_str.push('\n');
    server_writer.write_all(status_req_str.as_bytes()).await?;
    server_writer.flush().await?;

    if let Ok(Some(line)) = server_reader.next_line().await {
        if let Ok(resp) = serde_json::from_str::<Response>(&line) {
            if let Some(val) = resp.result {
                let has_config = val.get("has_config").and_then(|v| v.as_bool()).unwrap_or(false);
                let has_api_key = val.get("has_api_key").and_then(|v| v.as_bool()).unwrap_or(false);
                let config_path = val.get("config_path").and_then(|v| v.as_str()).map(|s| s.to_string());
                let credentials_path = val.get("credentials_path").and_then(|v| v.as_str()).map(|s| s.to_string());
                let default_cfg = val.get("default_config_path").and_then(|v| v.as_str()).unwrap_or("tauqe.toml").to_string();
                let default_creds = val.get("default_credentials_path").and_then(|v| v.as_str()).unwrap_or("~/.config/tauqe/credentials.toml").to_string();
                let model = val.get("model").and_then(|v| v.as_str()).unwrap_or("anthropic/claude-3.7-sonnet").to_string();

                onboarding_state.has_config = has_config;
                onboarding_state.has_api_key = has_api_key;
                onboarding_state.config_path = config_path;
                onboarding_state.credentials_path = credentials_path;
                onboarding_state.default_config_path = default_cfg;
                onboarding_state.default_credentials_path = default_creds;
                onboarding_state.selected_model = model;

                if !has_config || !has_api_key {
                    initial_view_mode = ViewMode::Onboarding;
                    if !has_config {
                        onboarding_state.step = OnboardingStep::Config;
                    } else {
                        onboarding_state.step = OnboardingStep::Credentials;
                    }
                }
            }
        }
    }

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
        model: ModelView::default(),
        context: initial_context,
        context_view: ContextViewState::default(),
        history_view: crate::app::HistoryViewState::default(),
        onboarding: onboarding_state,
        input_editor: InputEditor::default(),
        show_help: false,
        confirm_cancel: false,
        confirm_undo: false,
        confirm_clear_history: false,
        selection_dialog: None,
        squash_dialog: None,
        last_model_height: 10,
    }));

    // Initial history fetch
    send_request(
        &mut server_writer,
        methods::HISTORY_GET,
        serde_json::to_value(HistoryGetParams {
            limit: Some(10),
            before_id: None,
        })?,
    )
    .await?;

    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<Message>(100);
    {
        let msg_tx = msg_tx.clone();
        tokio::spawn(async move {
            loop {
                match server_reader.next_line().await {
                    Ok(Some(line)) => {
                        if let Ok(msg) = serde_json::from_str::<Message>(&line) {
                            if msg_tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        });
    }

    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
    )?;

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<event::Event>(100);
    {
        // Terminal reads are blocking: keep them off the async executor.
        tokio::task::spawn_blocking(move || {
            while !event_tx.is_closed() {
                match event::poll(Duration::from_millis(10)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if event_tx.blocking_send(ev).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });
    }

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut is_reasoning = false;

    loop {
        {
            let mut st = state.lock().await;
            st.model.spinner_frame = (st.model.spinner_frame + 1) % SPINNER_FRAMES.len();
            terminal.draw(|f| render_ui(f, &mut st))?;
        }

        tokio::select! {
            Some(terminal_event) = event_rx.recv() => {
                let key = match terminal_event {
                    event::Event::Paste(text) => {
                        let mut st = state.lock().await;
                        if st.view_mode == ViewMode::Model
                            && !st.show_help
                            && !st.confirm_undo
                            && !st.confirm_clear_history
                            && st.selection_dialog.is_none()
                        {
                            st.input_editor.insert_paste(&text);
                        }
                        continue;
                    }
                    event::Event::Key(key) if key.kind != event::KeyEventKind::Release => key,
                    event::Event::Mouse(mouse) => {
                        let mut st = state.lock().await;
                        match mouse.kind {
                            crossterm::event::MouseEventKind::ScrollUp => {
                                match st.view_mode {
                                    ViewMode::Model => {
                                        st.model.auto_scroll = false;
                                        st.model.scroll = st.model.scroll.saturating_sub(3);
                                    }
                                    ViewMode::History => {
                                        st.history_view.auto_scroll = false;
                                        st.history_view.scroll = st.history_view.scroll.saturating_sub(3);
                                    }
                                    ViewMode::Context => {
                                        if st.context_view.cursor_index > 0 {
                                            st.context_view.cursor_index = st.context_view.cursor_index.saturating_sub(1);
                                        }
                                    }
                                    ViewMode::Onboarding => {}
                                }
                                continue;
                            }
                            crossterm::event::MouseEventKind::ScrollDown => {
                                match st.view_mode {
                                    ViewMode::Model => {
                                        let view_height = st.last_model_height;
                                        let max = st.model.max_scroll(view_height);
                                        st.model.scroll = (st.model.scroll.saturating_add(3)).min(max);
                                        if st.model.scroll >= max {
                                            st.model.auto_scroll = true;
                                        }
                                    }
                                    ViewMode::History => {
                                        let view_height = st.last_model_height;
                                        let total = st.history_view.rendered_lines_count as u16;
                                        let max = total.saturating_sub(view_height);
                                        st.history_view.scroll = (st.history_view.scroll.saturating_add(3)).min(max);
                                        if st.history_view.scroll >= max {
                                            st.history_view.auto_scroll = true;
                                        }
                                    }
                                    ViewMode::Context => {
                                        let rows_len = st.context_view.compute_rows(&st.context.items).len();
                                        if rows_len > 0 && st.context_view.cursor_index + 1 < rows_len {
                                            st.context_view.cursor_index += 1;
                                        }
                                    }
                                    ViewMode::Onboarding => {}
                                }
                                continue;
                            }
                            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                                if mouse.row == 1 {
                                    // Top tab navigation
                                    if mouse.column >= 9 && mouse.column <= 19 {
                                        st.view_mode = ViewMode::Model;
                                        st.context_view.status_message = None;
                                        continue;
                                    } else if mouse.column >= 20 && mouse.column <= 36 {
                                        st.view_mode = ViewMode::Context;
                                        st.context_view.status_message = None;
                                        continue;
                                    } else if mouse.column >= 37 && mouse.column <= 53 {
                                        st.view_mode = ViewMode::History;
                                        st.history_view.auto_scroll = true;
                                        st.context_view.status_message = None;
                                        continue;
                                    }
                                }

                                if st.view_mode == ViewMode::Model {
                                    let (cx, cy, cw, ch) = st.model.content_rect;
                                    if mouse.column >= cx && mouse.column < cx + cw && mouse.row >= cy && mouse.row < cy + ch {
                                        let relative_row = (mouse.row - cy) as usize;
                                        let clicked_visual_line = st.model.scroll as usize + relative_row;

                                        if let Some(block) = st.model.code_blocks.iter().find(|b| {
                                            clicked_visual_line >= b.visual_start_line && clicked_visual_line <= b.visual_end_line
                                        }).cloned() {
                                            let code_to_copy = block.code.clone();
                                            let block_id = block.id;
                                            crate::clipboard::copy_to_clipboard(&code_to_copy);
                                            st.model.copy_flash = Some((block_id, std::time::Instant::now()));
                                            let lines_count = code_to_copy.lines().count().max(1);
                                            st.model.copy_notification = Some((
                                                format!("Copied {} lines to clipboard", lines_count),
                                                std::time::Instant::now(),
                                            ));
                                            st.model.update_markdown();
                                        }
                                    }
                                }
                                continue;
                            }
                            _ => continue,
                        }
                    }
                    _ => continue,
                };

                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
                    break;
                }

                let mut st = state.lock().await;

                // Handle active squash dialog modal
                if let Some(mut dialog) = st.squash_dialog.take() {
                    match key.code {
                        KeyCode::Esc => {
                            st.squash_dialog = None;
                            continue;
                        }
                        KeyCode::Enter => {
                            if dialog.loading {
                                st.squash_dialog = Some(dialog);
                                continue;
                            }
                            let msg = dialog.message_buffer.trim().to_string();
                            if msg.is_empty() {
                                dialog.status_message = Some("Commit message cannot be empty".to_string());
                                st.squash_dialog = Some(dialog);
                                continue;
                            }
                            let base_ref = dialog.base_ref.clone();
                            st.squash_dialog = None;
                            drop(st);
                            let params = GitSquashApplyParams {
                                base_ref,
                                message: msg,
                            };
                            send_request(
                                &mut server_writer,
                                methods::GIT_SQUASH_APPLY,
                                serde_json::to_value(params)?,
                            )
                            .await?;
                            continue;
                        }
                        KeyCode::Backspace => {
                            dialog.message_buffer.pop();
                            st.squash_dialog = Some(dialog);
                            continue;
                        }
                        KeyCode::Char(c) => {
                            dialog.message_buffer.push(c);
                            st.squash_dialog = Some(dialog);
                            continue;
                        }
                        _ => {
                            st.squash_dialog = Some(dialog);
                            continue;
                        }
                    }
                }

                // Handle active cancel confirmation modal
                if st.confirm_cancel {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                            st.confirm_cancel = false;
                            drop(st);
                            send_request(&mut server_writer, methods::MODEL_CANCEL, serde_json::json!({})).await?;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            st.confirm_cancel = false;
                        }
                        _ => {}
                    }
                    continue;
                }

                // Handle active undo confirmation modal
                if st.confirm_undo {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                            st.confirm_undo = false;
                            drop(st);
                            send_request(&mut server_writer, methods::GIT_UNDO, serde_json::json!({})).await?;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            st.confirm_undo = false;
                        }
                        _ => {}
                    }
                    continue;
                }

                // Handle active clear history confirmation modal
                if st.confirm_clear_history {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                            st.confirm_clear_history = false;
                            let cost = st.model.session_total_cost;
                            let prev_cost = st.model.prev_cost;
                            st.model = ModelView {
                                session_total_cost: cost,
                                prev_cost,
                                ..Default::default()
                            };
                            st.history_view = crate::app::HistoryViewState::default();
                            drop(st);
                            send_request(&mut server_writer, methods::MODEL_CLEAR_HISTORY, serde_json::json!({})).await?;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            st.confirm_clear_history = false;
                        }
                        _ => {}
                    }
                    continue;
                }

                // Handle active selection dialog (workflow / edit protocol / model)
                if let Some(mut dialog) = st.selection_dialog.take() {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            continue;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            dialog.selected_index = dialog.selected_index.saturating_sub(1);
                            st.selection_dialog = Some(dialog);
                            continue;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if !dialog.items.is_empty() && dialog.selected_index + 1 < dialog.items.len() {
                                dialog.selected_index += 1;
                            }
                            st.selection_dialog = Some(dialog);
                            continue;
                        }
                        KeyCode::Enter => {
                            if let Some(chosen) = dialog.items.get(dialog.selected_index).cloned() {
                                match dialog.kind {
                                    crate::app::SelectionDialogKind::Workflow => {
                                        st.workflow = chosen.clone();
                                        let params = ConfigSetParams {
                                            workflow: Some(chosen),
                                            edit_protocol: None,
                                            model: None,
                                        };
                                        drop(st);
                                        send_request(&mut server_writer, methods::CONFIG_SET, serde_json::to_value(params)?).await?;
                                    }
                                    crate::app::SelectionDialogKind::EditProtocol => {
                                        st.edit_protocol = chosen.clone();
                                        let params = ConfigSetParams {
                                            workflow: None,
                                            edit_protocol: Some(chosen),
                                            model: None,
                                        };
                                        drop(st);
                                        send_request(&mut server_writer, methods::CONFIG_SET, serde_json::to_value(params)?).await?;
                                    }
                                    crate::app::SelectionDialogKind::Model => {
                                        st.active_model = chosen.clone();
                                        let params = ConfigSetParams {
                                            workflow: None,
                                            edit_protocol: None,
                                            model: Some(chosen),
                                        };
                                        drop(st);
                                        send_request(&mut server_writer, methods::CONFIG_SET, serde_json::to_value(params)?).await?;
                                    }
                                }
                            }
                            continue;
                        }
                        _ => {
                            st.selection_dialog = Some(dialog);
                            continue;
                        }
                    }
                }

                if st.show_help {
                    match key.code {
                        KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                            st.show_help = false;
                        }
                        _ => {}
                    }
                    continue;
                }

                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('1') => {
                            st.view_mode = ViewMode::Model;
                            st.context_view.status_message = None;
                            continue;
                        }
                        KeyCode::Char('2') => {
                            st.view_mode = ViewMode::Context;
                            st.context_view.status_message = None;
                            continue;
                        }
                        KeyCode::Char('3') => {
                            st.view_mode = ViewMode::History;
                            st.history_view.auto_scroll = true;
                            st.context_view.status_message = None;
                            continue;
                        }
                        KeyCode::Char('o') => {
                            st.model.git_notification = Some("Reloading configuration from disk...".to_string());
                            drop(st);
                            send_request(&mut server_writer, "config/reload", serde_json::json!({})).await?;
                            continue;
                        }
                        KeyCode::Char('c') => {
                            if st.model.is_busy() {
                                st.confirm_cancel = true;
                            }
                            continue;
                        }
                        KeyCode::Char('l') => {
                            if st.model.is_busy() {
                                st.model.git_notification = Some("Cannot clear history while model is generating".to_string());
                            } else {
                                st.confirm_clear_history = true;
                            }
                            continue;
                        }
                        KeyCode::Char('r') => {
                            st.model.show_reasoning = !st.model.show_reasoning;
                            let h = st.last_model_height;
                            st.model.clamp_scroll(h);
                            continue;
                        }
                        KeyCode::Char('w') => {
                            if st.model.is_busy() {
                                st.model.git_notification = Some("Cannot change workflow while model is generating".to_string());
                                continue;
                            }
                            if !st.available_workflows.is_empty() {
                                let cur_idx = st
                                    .available_workflows
                                    .iter()
                                    .position(|w| w == &st.workflow)
                                    .unwrap_or(0);
                                st.selection_dialog = Some(crate::app::SelectionDialogState {
                                    kind: crate::app::SelectionDialogKind::Workflow,
                                    items: st.available_workflows.clone(),
                                    selected_index: cur_idx,
                                });
                                continue;
                            }
                        }
                        KeyCode::Char('p') => {
                            if st.model.is_busy() {
                                st.model.git_notification = Some("Cannot change edit protocol while model is generating".to_string());
                                continue;
                            }
                            if !st.available_edit_protocols.is_empty() {
                                let cur_idx = st
                                    .available_edit_protocols
                                    .iter()
                                    .position(|p| p == &st.edit_protocol)
                                    .unwrap_or(0);
                                st.selection_dialog = Some(crate::app::SelectionDialogState {
                                    kind: crate::app::SelectionDialogKind::EditProtocol,
                                    items: st.available_edit_protocols.clone(),
                                    selected_index: cur_idx,
                                });
                                continue;
                            }
                        }
                        KeyCode::Char('m') => {
                            if st.model.is_busy() {
                                st.model.git_notification = Some("Cannot change model while model is generating".to_string());
                                continue;
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
                                continue;
                            }
                        }
                        KeyCode::Char('a') if st.view_mode == ViewMode::Model => {
                            st.input_editor.move_beginning_of_line();
                            continue;
                        }
                        KeyCode::Char('e') if st.view_mode == ViewMode::Model => {
                            st.input_editor.move_end_of_line();
                            continue;
                        }
                        KeyCode::Char('k') if st.view_mode == ViewMode::Model => {
                            st.input_editor.kill_line();
                            continue;
                        }
                        KeyCode::Char('u') if st.view_mode == ViewMode::Model => {
                            st.input_editor.kill_to_beginning_of_line();
                            continue;
                        }
                        KeyCode::Char('y') if st.view_mode == ViewMode::Model => {
                            st.input_editor.yank();
                            continue;
                        }
                        KeyCode::Char('d') if st.view_mode == ViewMode::Model => {
                            st.input_editor.delete_forward();
                            continue;
                        }
                        KeyCode::Char('b') if st.view_mode == ViewMode::Model => {
                            st.input_editor.move_backward();
                            continue;
                        }
                        KeyCode::Char('f') if st.view_mode == ViewMode::Model => {
                            st.input_editor.move_forward();
                            continue;
                        }
                        KeyCode::Left if st.view_mode == ViewMode::Model => {
                            st.input_editor.move_word_backward();
                            continue;
                        }
                        KeyCode::Right if st.view_mode == ViewMode::Model => {
                            st.input_editor.move_word_forward();
                            continue;
                        }
                        KeyCode::Char('s') if st.view_mode == ViewMode::Model => {
                            if st.model.is_busy() {
                                st.model.git_notification = Some("Cannot squash commits while model is generating".to_string());
                            } else {
                                st.squash_dialog = Some(crate::app::SquashDialogState {
                                    loading: true,
                                    base_ref: String::new(),
                                    commits: Vec::new(),
                                    diff_stat: String::new(),
                                    message_buffer: String::new(),
                                    status_message: Some("Querying commits ahead of upstream...".to_string()),
                                });
                                drop(st);
                                let params = GitSquashPreviewParams { base_ref: None };
                                send_request(
                                    &mut server_writer,
                                    methods::GIT_SQUASH_PREVIEW,
                                    serde_json::to_value(params)?,
                                )
                                .await?;
                                continue;
                            }
                        }
                        // Submit prompt via Ctrl+Enter
                        KeyCode::Enter if st.view_mode == ViewMode::Model => {
                            if let Some(prompt) = st.take_prompt() {
                                drop(st);
                                let params = ModelAskParams { prompt };
                                send_request(&mut server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await?;
                            }
                            continue;
                        }
                        // Ctrl+J inserts newline
                        KeyCode::Char('j') if st.view_mode == ViewMode::Model => {
                            st.input_editor.insert_char('\n');
                            continue;
                        }
                        _ => {}
                    }
                }

                if key.modifiers.contains(KeyModifiers::ALT) && st.view_mode == ViewMode::Model {
                    match key.code {
                        KeyCode::Up => {
                            st.model.auto_scroll = false;
                            st.model.scroll = st.model.scroll.saturating_sub(1);
                            continue;
                        }
                        KeyCode::Down => {
                            let view_height = st.last_model_height;
                            let max = st.model.max_scroll(view_height);
                            st.model.scroll = (st.model.scroll.saturating_add(1)).min(max);
                            if st.model.scroll >= max {
                                st.model.auto_scroll = true;
                            }
                            continue;
                        }
                        KeyCode::Char('b') | KeyCode::Left => {
                            st.input_editor.move_word_backward();
                            continue;
                        }
                        KeyCode::Char('f') | KeyCode::Right => {
                            st.input_editor.move_word_forward();
                            continue;
                        }
                        KeyCode::Char('d') => {
                            st.input_editor.kill_word_forward();
                            continue;
                        }
                        KeyCode::Backspace => {
                            st.input_editor.kill_word_backward();
                            continue;
                        }
                        KeyCode::Enter => {
                            if let Some(prompt) = st.take_prompt() {
                                drop(st);
                                let params = ModelAskParams { prompt };
                                send_request(&mut server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await?;
                            }
                            continue;
                        }
                        _ => {}
                    }
                }

                if key.code == KeyCode::Char('?') && !st.context_view.adding_file && (st.view_mode != ViewMode::Model || st.input_editor.is_empty()) {
                    st.show_help = true;
                    continue;
                }

                match st.view_mode {
                    ViewMode::Model => {
                        let view_height = st.last_model_height;
                        match key.code {
                            KeyCode::Esc => {
                                if st.model.is_busy() {
                                    st.confirm_cancel = true;
                                } else if !st.input_editor.is_empty() {
                                    st.input_editor.clear();
                                }
                            }
                            KeyCode::Char('u') if st.input_editor.is_empty() => {
                                if st.model.is_busy() {
                                    st.model.git_notification = Some("Cannot undo while model is generating".to_string());
                                } else {
                                    st.confirm_undo = true;
                                }
                            }
                            KeyCode::Char('s') if st.input_editor.is_empty() => {
                                if st.model.is_busy() {
                                    st.model.git_notification = Some("Cannot squash commits while model is generating".to_string());
                                } else {
                                    st.squash_dialog = Some(crate::app::SquashDialogState {
                                        loading: true,
                                        base_ref: String::new(),
                                        commits: Vec::new(),
                                        diff_stat: String::new(),
                                        message_buffer: String::new(),
                                        status_message: Some("Querying commits ahead of upstream...".to_string()),
                                    });
                                    drop(st);
                                    let params = GitSquashPreviewParams { base_ref: None };
                                    send_request(
                                        &mut server_writer,
                                        methods::GIT_SQUASH_PREVIEW,
                                        serde_json::to_value(params)?,
                                    )
                                    .await?;
                                    continue;
                                }
                            }
                            KeyCode::Char('[') if st.input_editor.is_empty() => {
                                if !st.model.files.is_empty() {
                                    st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                                    st.model.scroll_to_selected_file(view_height);
                                }
                            }
                            KeyCode::Char(']') if st.input_editor.is_empty() => {
                                if !st.model.files.is_empty() && st.model.selected_file_index + 1 < st.model.files.len() {
                                    st.model.selected_file_index += 1;
                                    st.model.scroll_to_selected_file(view_height);
                                }
                            }
                            KeyCode::Char(' ') if st.input_editor.is_empty() => {
                                let sel_idx = st.model.selected_file_index;
                                if let Some(file) = st.model.files.get_mut(sel_idx) {
                                    file.expanded = !file.expanded;
                                    st.model.scroll_to_selected_file(view_height);
                                }
                            }
                            KeyCode::Tab => {
                                if st.input_editor.is_empty() {
                                    if !st.model.files.is_empty() {
                                        let sel_idx = st.model.selected_file_index;
                                        if let Some(file) = st.model.files.get_mut(sel_idx) {
                                            file.expanded = !file.expanded;
                                            let h = st.last_model_height;
                                            st.model.clamp_scroll(h);
                                        }
                                    }
                                } else {
                                    st.input_editor.insert_str("  ");
                                }
                            }
                            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                                st.input_editor.insert_char('\n');
                            }
                            KeyCode::Enter => {
                                if let Some(prompt) = st.take_prompt() {
                                    drop(st);
                                    let params = ModelAskParams { prompt };
                                    send_request(&mut server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await?;
                                } else if !st.model.files.is_empty() {
                                    let sel_idx = st.model.selected_file_index;
                                    if let Some(file) = st.model.files.get_mut(sel_idx) {
                                        file.expanded = !file.expanded;
                                        st.model.scroll_to_selected_file(view_height);
                                    }
                                }
                            }
                            KeyCode::Char(c) => {
                                st.input_editor.insert_char(c);
                            }
                            KeyCode::Backspace => {
                                st.input_editor.delete_backward();
                            }
                            KeyCode::Delete => {
                                st.input_editor.delete_forward();
                            }
                            KeyCode::Left => {
                                st.input_editor.move_backward();
                            }
                            KeyCode::Right => {
                                st.input_editor.move_forward();
                            }
                            KeyCode::Home => {
                                if !st.input_editor.is_empty() {
                                    st.input_editor.move_beginning_of_line();
                                } else {
                                    st.model.auto_scroll = false;
                                    st.model.scroll = 0;
                                }
                            }
                            KeyCode::End => {
                                if !st.input_editor.is_empty() {
                                    st.input_editor.move_end_of_line();
                                } else {
                                    st.model.auto_scroll = true;
                                    st.model.scroll = st.model.max_scroll(view_height);
                                }
                            }
                            KeyCode::PageUp => {
                                st.model.auto_scroll = false;
                                st.model.scroll = st.model.scroll.saturating_sub(10);
                            }
                            KeyCode::PageDown => {
                                let max = st.model.max_scroll(view_height);
                                st.model.scroll = (st.model.scroll.saturating_add(10)).min(max);
                                if st.model.scroll >= max {
                                    st.model.auto_scroll = true;
                                }
                            }
                            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                                if st.input_editor.is_empty() {
                                    st.model.auto_scroll = false;
                                    st.model.scroll = st.model.scroll.saturating_sub(1);
                                }
                            }
                            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                                if st.input_editor.is_empty() {
                                    let max = st.model.max_scroll(view_height);
                                    st.model.scroll = (st.model.scroll.saturating_add(1)).min(max);
                                    if st.model.scroll >= max {
                                        st.model.auto_scroll = true;
                                    }
                                }
                            }
                            KeyCode::Up => {
                                if !st.input_editor.is_empty() {
                                    st.input_editor.move_line_up();
                                } else if !st.model.files.is_empty() {
                                    st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                                    st.model.scroll_to_selected_file(view_height);
                                } else {
                                    st.model.auto_scroll = false;
                                    st.model.scroll = st.model.scroll.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                if !st.input_editor.is_empty() {
                                    st.input_editor.move_line_down();
                                } else if !st.model.files.is_empty() {
                                    if st.model.selected_file_index + 1 < st.model.files.len() {
                                        st.model.selected_file_index += 1;
                                        st.model.scroll_to_selected_file(view_height);
                                    }
                                } else {
                                    let max = st.model.max_scroll(view_height);
                                    st.model.scroll = (st.model.scroll.saturating_add(1)).min(max);
                                    if st.model.scroll >= max {
                                        st.model.auto_scroll = true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    ViewMode::Context => {
                        if st.context_view.adding_file {
                            match key.code {
                                KeyCode::Esc => {
                                    st.context_view.adding_file = false;
                                    st.context_view.add_input.clear();
                                    st.context_view.status_message = None;
                                }
                                KeyCode::Up => {
                                    if !st.context_view.filtered_candidates.is_empty() {
                                        st.context_view.selected_candidate_index =
                                            st.context_view.selected_candidate_index.saturating_sub(1);
                                    }
                                }
                                KeyCode::Down => {
                                    if !st.context_view.filtered_candidates.is_empty()
                                        && st.context_view.selected_candidate_index + 1
                                            < st.context_view.filtered_candidates.len()
                                    {
                                        st.context_view.selected_candidate_index += 1;
                                    }
                                }
                                KeyCode::Tab => {
                                    if let Some(candidate) = st
                                        .context_view
                                        .filtered_candidates
                                        .get(st.context_view.selected_candidate_index)
                                    {
                                        if !candidate.starts_with("[+] Add all matching '") {
                                            st.context_view.add_input = candidate.clone();
                                            st.update_filtered_candidates();
                                        }
                                    }
                                }
                                KeyCode::Enter => {
                                    let selected_candidate = st
                                        .context_view
                                        .filtered_candidates
                                        .get(st.context_view.selected_candidate_index)
                                        .cloned();

                                    let is_pattern_entry = selected_candidate.as_ref().is_some_and(|c| {
                                        c.starts_with("[+] Add all matching '")
                                    });

                                    let raw_input = st.context_view.add_input.trim().to_string();
                                    let is_glob_direct = raw_input.contains('*')
                                        || raw_input.contains('?')
                                        || raw_input.ends_with('/');

                                    let access = st.context_view.add_access;
                                    if is_pattern_entry || is_glob_direct {
                                        st.context_view.adding_file = false;
                                        st.context_view.add_input.clear();
                                        drop(st);

                                        let params = ContextAddPatternParams {
                                            pattern: raw_input,
                                            access,
                                        };
                                        send_request(
                                            &mut server_writer,
                                            methods::CONTEXT_ADD_PATTERN,
                                            serde_json::to_value(params)?,
                                        )
                                        .await?;
                                    } else {
                                        let target_path = if let Some(candidate) = selected_candidate {
                                            candidate
                                        } else {
                                            raw_input
                                        };

                                        if !target_path.is_empty() {
                                            st.context_view.adding_file = false;
                                            st.context_view.add_input.clear();
                                            drop(st);
                                            let params = ContextAddParams {
                                                path: target_path,
                                                access,
                                                layer: Some(ContextLayer::User),
                                            };
                                            send_request(
                                                &mut server_writer,
                                                methods::CONTEXT_ADD,
                                                serde_json::to_value(params)?,
                                            )
                                            .await?;
                                        }
                                    }
                                }
                                KeyCode::Char(c) => {
                                    st.context_view.add_input.push(c);
                                    st.context_view.selected_candidate_index = 0;
                                    st.update_filtered_candidates();
                                }
                                KeyCode::Backspace => {
                                    st.context_view.add_input.pop();
                                    st.context_view.selected_candidate_index = 0;
                                    st.update_filtered_candidates();
                                }
                                _ => {}
                            }
                        } else {
                            let rows = st.context_view.compute_rows(&st.context.items);
                            let total_rows = rows.len();
                            if st.context_view.cursor_index >= total_rows && total_rows > 0 {
                                st.context_view.cursor_index = total_rows - 1;
                            }
                            let current_row = rows.get(st.context_view.cursor_index).cloned();

                            match key.code {
                                KeyCode::Esc | KeyCode::Char('q') => {
                                    st.view_mode = ViewMode::Model;
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    if total_rows > 0 {
                                        st.context_view.cursor_index = st.context_view.cursor_index.saturating_sub(1);
                                    }
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if total_rows > 0 && st.context_view.cursor_index + 1 < total_rows {
                                        st.context_view.cursor_index += 1;
                                    }
                                }
                                KeyCode::Tab | KeyCode::Char(' ') => {
                                    if let Some(ContextRow::Header(layer)) = current_row {
                                        st.context_view.toggle_section(layer);
                                    }
                                }
                                KeyCode::Enter => {
                                    match current_row {
                                        Some(ContextRow::Header(layer)) => {
                                            st.context_view.toggle_section(layer);
                                        }
                                        Some(ContextRow::Item(ref item)) if item.layer == ContextLayer::Auto => {
                                            let path = item.path.clone();
                                            let access = item.access;
                                            st.context_view.status_message = Some(format!("Promoting '{}' to User context...", path));
                                            drop(st);
                                            let params = ContextAddParams {
                                                path,
                                                access,
                                                layer: Some(ContextLayer::User),
                                            };
                                            send_request(&mut server_writer, methods::CONTEXT_ADD, serde_json::to_value(params)?).await?;
                                        }
                                        _ => {}
                                    }
                                }
                                KeyCode::Char('p') | KeyCode::Char('u') => {
                                    if let Some(ContextRow::Item(ref item)) = current_row {
                                        if item.layer == ContextLayer::Auto {
                                            let path = item.path.clone();
                                            let access = item.access;
                                            st.context_view.status_message = Some(format!("Promoting '{}' to User context...", path));
                                            drop(st);
                                            let params = ContextAddParams {
                                                path,
                                                access,
                                                layer: Some(ContextLayer::User),
                                            };
                                            send_request(&mut server_writer, methods::CONTEXT_ADD, serde_json::to_value(params)?).await?;
                                        } else {
                                            st.context_view.status_message = Some("Only Auto files can be promoted to User context".to_string());
                                        }
                                    }
                                }
                                KeyCode::Char('c') | KeyCode::Char('C') => {
                                    let auto_paths: Vec<String> = st.context.items.iter()
                                        .filter(|i| i.layer == ContextLayer::Auto)
                                        .map(|i| i.path.clone())
                                        .collect();
                                    if auto_paths.is_empty() {
                                        st.context_view.status_message = Some("No auto files to clear".to_string());
                                    } else {
                                        st.context_view.status_message = Some(format!("Clearing {} auto files...", auto_paths.len()));
                                        drop(st);
                                        for p in auto_paths {
                                            let params = ContextRemoveParams { path: p };
                                            send_request(&mut server_writer, methods::CONTEXT_REMOVE, serde_json::to_value(params)?).await?;
                                        }
                                    }
                                }
                                KeyCode::Char('e') => {
                                    st.context_view.adding_file = true;
                                    st.context_view.add_access = ContextAccess::Editable;
                                    st.context_view.add_input.clear();
                                    st.context_view.selected_candidate_index = 0;
                                    st.context_view.status_message = None;
                                    st.update_filtered_candidates();

                                    drop(st);
                                    send_request(
                                        &mut server_writer,
                                        methods::REPOSITORY_LIST_FILES,
                                        serde_json::json!({}),
                                    )
                                    .await?;
                                }
                                KeyCode::Char('r') | KeyCode::Char('a') => {
                                    st.context_view.adding_file = true;
                                    st.context_view.add_access = ContextAccess::ReadOnly;
                                    st.context_view.add_input.clear();
                                    st.context_view.selected_candidate_index = 0;
                                    st.context_view.status_message = None;
                                    st.update_filtered_candidates();

                                    drop(st);
                                    send_request(
                                        &mut server_writer,
                                        methods::REPOSITORY_LIST_FILES,
                                        serde_json::json!({}),
                                    )
                                    .await?;
                                }
                                KeyCode::Char('t') => {
                                    if let Some(ContextRow::Item(ref item)) = current_row {
                                        if item.layer == ContextLayer::Pinned {
                                            st.context_view.status_message = Some("Pinned files are read-only and cannot be changed".to_string());
                                        } else {
                                            let path = item.path.clone();
                                            let next_access = match item.access {
                                                ContextAccess::Editable => ContextAccess::ReadOnly,
                                                ContextAccess::ReadOnly => ContextAccess::Editable,
                                            };
                                            drop(st);
                                            let params = ContextSetAccessParams {
                                                path,
                                                access: next_access,
                                            };
                                            send_request(&mut server_writer, methods::CONTEXT_SET_ACCESS, serde_json::to_value(params)?).await?;
                                        }
                                    }
                                }
                                KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => {
                                    match current_row {
                                        Some(ContextRow::Item(ref item)) => {
                                            if item.layer == ContextLayer::Pinned {
                                                st.context_view.status_message = Some("Pinned files are protected and cannot be removed".to_string());
                                            } else {
                                                let path = item.path.clone();
                                                if st.context_view.cursor_index > 0 && st.context_view.cursor_index >= total_rows.saturating_sub(1) {
                                                    st.context_view.cursor_index -= 1;
                                                }
                                                drop(st);
                                                let params = ContextRemoveParams { path };
                                                send_request(&mut server_writer, methods::CONTEXT_REMOVE, serde_json::to_value(params)?).await?;
                                            }
                                        }
                                        Some(ContextRow::Header(ContextLayer::Auto)) => {
                                            let auto_paths: Vec<String> = st.context.items.iter()
                                                .filter(|i| i.layer == ContextLayer::Auto)
                                                .map(|i| i.path.clone())
                                                .collect();
                                            if auto_paths.is_empty() {
                                                st.context_view.status_message = Some("No auto files to clear".to_string());
                                            } else {
                                                st.context_view.status_message = Some(format!("Clearing {} auto files...", auto_paths.len()));
                                                drop(st);
                                                for p in auto_paths {
                                                    let params = ContextRemoveParams { path: p };
                                                    send_request(&mut server_writer, methods::CONTEXT_REMOVE, serde_json::to_value(params)?).await?;
                                                }
                                            }
                                        }
                                        Some(ContextRow::Header(ContextLayer::Pinned)) => {
                                            st.context_view.status_message = Some("Pinned section is protected and cannot be removed".to_string());
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    ViewMode::Onboarding => {
                        if st.onboarding.input_active {
                            match key.code {
                                KeyCode::Esc => {
                                    st.onboarding.input_active = false;
                                    st.onboarding.input_buffer.clear();
                                }
                                KeyCode::Char(c) => {
                                    st.onboarding.input_buffer.push(c);
                                }
                                KeyCode::Backspace => {
                                    st.onboarding.input_buffer.pop();
                                }
                                KeyCode::Enter => {
                                    let input = st.onboarding.input_buffer.trim().to_string();
                                    st.onboarding.input_active = false;
                                    st.onboarding.input_buffer.clear();

                                    match st.onboarding.step {
                                        OnboardingStep::Config if !input.is_empty() => {
                                            st.onboarding.selected_model = input.clone();
                                            drop(st);
                                            send_request(
                                                &mut server_writer,
                                                "config/create",
                                                serde_json::json!({ "model": input }),
                                            )
                                            .await?;
                                        }
                                        OnboardingStep::Credentials if !input.is_empty() => {
                                            drop(st);
                                            send_request(
                                                &mut server_writer,
                                                "credentials/save",
                                                serde_json::json!({ "api_key": input }),
                                            )
                                            .await?;
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            let total_options = match st.onboarding.step {
                                OnboardingStep::Config => 6, // 4 presets + custom + skip
                                OnboardingStep::Credentials => 4, // enter key + stub + check again + skip
                                OnboardingStep::Gatekeeper => 3, // try again + check again + exit
                                OnboardingStep::Ready => 1, // start
                            };

                            match key.code {
                                KeyCode::Esc | KeyCode::Char('q') => {
                                    if st.onboarding.step == OnboardingStep::Gatekeeper {
                                        break;
                                    } else if st.onboarding.has_api_key {
                                        st.view_mode = ViewMode::Model;
                                    } else {
                                        st.onboarding.step = OnboardingStep::Gatekeeper;
                                        st.onboarding.selected_index = 0;
                                    }
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    st.onboarding.selected_index =
                                        st.onboarding.selected_index.saturating_sub(1);
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if st.onboarding.selected_index + 1 < total_options {
                                        st.onboarding.selected_index += 1;
                                    }
                                }
                                KeyCode::Enter => {
                                    let sel = st.onboarding.selected_index;
                                    match st.onboarding.step {
                                        OnboardingStep::Config => match sel {
                                            0..=3 => {
                                                let model = st.onboarding.models_list.get(sel).cloned().unwrap_or_else(|| "anthropic/claude-3.7-sonnet".to_string());
                                                st.onboarding.selected_model = model.clone();
                                                drop(st);
                                                send_request(
                                                    &mut server_writer,
                                                    "config/create",
                                                    serde_json::json!({ "model": model }),
                                                )
                                                .await?;
                                            }
                                            4 => {
                                                st.onboarding.input_active = true;
                                                st.onboarding.input_buffer.clear();
                                            }
                                            _ => {
                                                // Skip
                                                if !st.onboarding.has_api_key {
                                                    st.onboarding.step = OnboardingStep::Credentials;
                                                } else {
                                                    st.onboarding.step = OnboardingStep::Ready;
                                                }
                                                st.onboarding.selected_index = 0;
                                            }
                                        },
                                        OnboardingStep::Credentials => match sel {
                                            0 => {
                                                st.onboarding.input_active = true;
                                                st.onboarding.input_buffer.clear();
                                            }
                                            1 => {
                                                drop(st);
                                                send_request(
                                                    &mut server_writer,
                                                    "credentials/create_stub",
                                                    serde_json::json!({}),
                                                )
                                                .await?;
                                            }
                                            2 => {
                                                drop(st);
                                                send_request(
                                                    &mut server_writer,
                                                    "config/reload",
                                                    serde_json::json!({}),
                                                )
                                                .await?;
                                            }
                                            _ => {
                                                // Skip
                                                if st.onboarding.has_api_key {
                                                    st.onboarding.step = OnboardingStep::Ready;
                                                } else {
                                                    st.onboarding.step = OnboardingStep::Gatekeeper;
                                                }
                                                st.onboarding.selected_index = 0;
                                            }
                                        },
                                        OnboardingStep::Gatekeeper => match sel {
                                            0 => {
                                                st.onboarding.step = OnboardingStep::Credentials;
                                                st.onboarding.selected_index = 0;
                                            }
                                            1 => {
                                                drop(st);
                                                send_request(
                                                    &mut server_writer,
                                                    "config/reload",
                                                    serde_json::json!({}),
                                                )
                                                .await?;
                                            }
                                            _ => {
                                                break;
                                            }
                                        },
                                        OnboardingStep::Ready => {
                                            st.view_mode = ViewMode::Model;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    ViewMode::History => {
                        let view_height = st.last_model_height;
                        let total_lines = st.history_view.rendered_lines_count as u16;
                        let max_scroll = total_lines.saturating_sub(view_height);

                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => {
                                st.view_mode = ViewMode::Model;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                st.history_view.auto_scroll = false;
                                if st.history_view.scroll > 0 {
                                    st.history_view.scroll = st.history_view.scroll.saturating_sub(1);
                                } else if st.history_view.has_more && !st.history_view.loading {
                                    if let Some(first_item) = st.history_view.items.first() {
                                        let before_id = first_item.id;
                                        st.history_view.loading = true;
                                        st.history_view.pending_before_id = Some(before_id);
                                        drop(st);
                                        let params = HistoryGetParams {
                                            limit: Some(10),
                                            before_id: Some(before_id),
                                        };
                                        send_request(&mut server_writer, methods::HISTORY_GET, serde_json::to_value(params)?).await?;
                                    }
                                }
                            }
                            KeyCode::PageUp => {
                                st.history_view.auto_scroll = false;
                                if st.history_view.scroll > 0 {
                                    st.history_view.scroll = st.history_view.scroll.saturating_sub(10);
                                } else if st.history_view.has_more && !st.history_view.loading {
                                    if let Some(first_item) = st.history_view.items.first() {
                                        let before_id = first_item.id;
                                        st.history_view.loading = true;
                                        st.history_view.pending_before_id = Some(before_id);
                                        drop(st);
                                        let params = HistoryGetParams {
                                            limit: Some(10),
                                            before_id: Some(before_id),
                                        };
                                        send_request(&mut server_writer, methods::HISTORY_GET, serde_json::to_value(params)?).await?;
                                    }
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if st.history_view.scroll < max_scroll {
                                    st.history_view.scroll = st.history_view.scroll.saturating_add(1);
                                }
                                if st.history_view.scroll >= max_scroll {
                                    st.history_view.auto_scroll = true;
                                }
                            }
                            KeyCode::PageDown => {
                                st.history_view.scroll = (st.history_view.scroll.saturating_add(10)).min(max_scroll);
                                if st.history_view.scroll >= max_scroll {
                                    st.history_view.auto_scroll = true;
                                }
                            }
                            KeyCode::Home => {
                                st.history_view.auto_scroll = false;
                                st.history_view.scroll = 0;
                                if st.history_view.has_more && !st.history_view.loading {
                                    if let Some(first_item) = st.history_view.items.first() {
                                        let before_id = first_item.id;
                                        st.history_view.loading = true;
                                        st.history_view.pending_before_id = Some(before_id);
                                        drop(st);
                                        let params = HistoryGetParams {
                                            limit: Some(10),
                                            before_id: Some(before_id),
                                        };
                                        send_request(&mut server_writer, methods::HISTORY_GET, serde_json::to_value(params)?).await?;
                                    }
                                }
                            }
                            KeyCode::End => {
                                st.history_view.auto_scroll = true;
                                st.history_view.scroll = max_scroll;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some(msg) = msg_rx.recv() => {
                match msg {
                    Message::Event(ev) => handle_event(ev, &state, &mut is_reasoning).await,
                    Message::Response(resp) => handle_response(resp, &state).await,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }

    let _ = server_child.kill().await;

    Ok(())
}

/// Fail-safe: if the edit block is still pending when the operation ends, reject it
/// instead of leaving the `[Modifying]` spinner running forever.
fn fail_safe_reject_edits(model: &mut ModelView, reason: &str) {
    if !model.edits_active || model.edit_final_applied.is_some() {
        return;
    }
    for f in model.files.iter_mut() {
        if f.status == "running" {
            f.status = "error".to_string();
            if f.error.is_none() {
                f.error = Some(reason.to_string());
            }
        }
    }
    model.edit_final_applied = Some(false);
    model.edit_final_error = Some(reason.to_string());
    model.git_notification = Some(reason.to_string());
}

async fn send_request(
    writer: &mut tokio::process::ChildStdin,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<()> {
    let req = Request {
        id: RequestId::Number(1),
        method: method.to_string(),
        params: Some(params),
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn handle_response(resp: Response, state: &Arc<Mutex<AppState>>) {
    let mut st = state.lock().await;
    if let Some(err) = resp.error {
        if let Some(ref mut dialog) = st.squash_dialog {
            dialog.loading = false;
            dialog.status_message = Some(format!("Error: {}", err.message));
        } else {
            st.context_view.status_message = Some(format!("Error: {}", err.message));
            if st.view_mode == ViewMode::Model {
                st.model.git_notification = Some(format!("Error: {}", err.message));
            }
        }
    } else if let Some(val) = resp.result {
        if let Ok(cfg) = serde_json::from_value::<ConfigState>(val.clone()) {
            st.workflow = cfg.workflow;
            if !cfg.model.is_empty() {
                st.active_model = cfg.model;
            }
            if !cfg.available_models.is_empty() {
                st.available_models = cfg.available_models;
            }
            st.edit_protocol = cfg.edit_protocol;
            if !cfg.available_workflows.is_empty() {
                st.available_workflows = cfg.available_workflows;
            }
            if !cfg.available_edit_protocols.is_empty() {
                st.available_edit_protocols = cfg.available_edit_protocols;
            }
        } else if let Ok(undo_res) = serde_json::from_value::<GitUndoResult>(val.clone()) {
            st.model.git_notification = Some(undo_res.message.clone());
            st.model.last_commit_hash = None;
            st.model.last_commit_summary = None;
            st.model.edit_final_applied = None;
            st.model.files.clear();
        } else if let Ok(preview) = serde_json::from_value::<GitSquashPreviewResult>(val.clone()) {
            if let Some(ref mut dialog) = st.squash_dialog {
                dialog.loading = false;
                dialog.base_ref = preview.base_ref;
                dialog.commits = preview.commits;
                dialog.diff_stat = preview.diff_stat;
                dialog.message_buffer = preview.suggested_message;
                dialog.status_message = None;
            }
        } else if let Ok(applied) = serde_json::from_value::<GitSquashApplyResult>(val.clone()) {
            st.squash_dialog = None;
            let first_line = applied.message.lines().next().unwrap_or("Squashed commit");
            st.model.git_notification = Some(format!(
                "Squashed commits into {} ('{}')",
                applied.squashed_commit, first_line
            ));
            st.model.last_commit_hash = Some(applied.squashed_commit);
            st.model.last_commit_summary = Some(first_line.to_string());
        } else if let Ok(history_res) = serde_json::from_value::<HistoryGetResult>(val.clone()) {
            st.history_view.loading = false;
            st.history_view.has_more = history_res.has_more;
            st.history_view.total_count = history_res.total_count;

            if st.history_view.pending_before_id.take().is_some() && !st.history_view.items.is_empty() {
                // Pagination prepend: calculate number of lines added to adjust scroll smoothly
                let added_lines = crate::ui::compute_history_items_line_count(&history_res.items, None);
                let mut combined = history_res.items;
                combined.append(&mut st.history_view.items);
                st.history_view.items = combined;
                st.history_view.scroll = st.history_view.scroll.saturating_add(added_lines as u16);
            } else {
                // Initial load or replacement
                st.history_view.items = history_res.items;
                st.history_view.auto_scroll = true;
            }
        } else if let Ok(pattern_res) =
            serde_json::from_value::<ContextAddPatternResult>(val.clone())
        {
            st.context = pattern_res.state;
            st.context_view.status_message = Some(format!(
                "Added {} files (~{} tokens)",
                pattern_res.added_count, pattern_res.added_tokens
            ));
            if st.context_view.adding_file {
                st.update_filtered_candidates();
            }
        } else if let Ok(file_res) =
            serde_json::from_value::<RepositoryListFilesResult>(val.clone())
        {
            st.all_repo_files = file_res.files;
            if st.context_view.adding_file {
                st.update_filtered_candidates();
            }
        } else if val.get("ready").is_some() {
            let ready = val.get("ready").and_then(|v| v.as_bool()).unwrap_or(false);
            // Onboarding / system status response
            let has_config = val.get("has_config").and_then(|v| v.as_bool()).unwrap_or(false);
            let has_api_key = val.get("has_api_key").and_then(|v| v.as_bool()).unwrap_or(false);
            let config_path = val.get("config_path").and_then(|v| v.as_str()).map(|s| s.to_string());
            let credentials_path = val.get("credentials_path").and_then(|v| v.as_str()).map(|s| s.to_string());
            if let Some(m) = val.get("model").and_then(|v| v.as_str()) {
                st.active_model = m.to_string();
                st.onboarding.selected_model = m.to_string();
            }
            if let Some(av) = val.get("available_models").and_then(|v| v.as_array()) {
                st.available_models = av.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
            }

            st.onboarding.has_config = has_config;
            st.onboarding.has_api_key = has_api_key;
            if config_path.is_some() {
                st.onboarding.config_path = config_path;
            }
            if credentials_path.is_some() {
                st.onboarding.credentials_path = credentials_path;
            }

            if val.get("stub_created").and_then(|v| v.as_bool()).unwrap_or(false) {
                st.onboarding.status_message = Some(format!(
                    "Файл-заглушка создан в '{}'. Отредактируйте его и нажмите 'Проверить снова'.",
                    st.onboarding.default_credentials_path
                ));
            } else if val.get("created").and_then(|v| v.as_bool()).unwrap_or(false) {
                st.onboarding.status_message = Some("Файл tauqe.toml успешно создан.".to_string());
                if !has_api_key {
                    st.onboarding.step = OnboardingStep::Credentials;
                    st.onboarding.selected_index = 0;
                } else {
                    st.onboarding.step = OnboardingStep::Ready;
                    st.onboarding.selected_index = 0;
                }
            } else if val.get("saved").and_then(|v| v.as_bool()).unwrap_or(false) {
                st.onboarding.status_message = Some("Ключ OpenRouter успешно сохранён.".to_string());
                st.onboarding.step = OnboardingStep::Ready;
                st.onboarding.selected_index = 0;
            } else if val.get("reloaded").and_then(|v| v.as_bool()).unwrap_or(false) {
                if ready {
                    st.onboarding.status_message = Some("Конфигурация перезагружена. Все проверки пройдены!".to_string());
                    if st.view_mode == ViewMode::Onboarding {
                        st.onboarding.step = OnboardingStep::Ready;
                        st.onboarding.selected_index = 0;
                    }
                } else {
                    st.onboarding.error_message = Some("Ключ по-прежнему не обнаружен. Проверьте переменную OPENROUTER_API_KEY или файл credentials.toml".to_string());
                }
            }
        } else if let Ok(ctx) = serde_json::from_value::<ContextState>(val) {
            st.context = ctx;
            let rows = st.context_view.compute_rows(&st.context.items);
            if rows.is_empty() {
                st.context_view.cursor_index = 0;
            } else if st.context_view.cursor_index >= rows.len() {
                st.context_view.cursor_index = rows.len() - 1;
            }
            if st.context_view.adding_file {
                st.update_filtered_candidates();
            }
        }
    }
}

async fn handle_event(ev: Event, state: &Arc<Mutex<AppState>>, is_reasoning: &mut bool) {
    let mut st = state.lock().await;
    match ev.method.as_str() {
        events::GIT_STATE_CHANGED => {
            if let Some(params) = ev.params {
                if let Some(val) = params.get("repository") {
                    if let Ok(repo) = serde_json::from_value::<RepositoryState>(val.clone()) {
                        st.repo_state = Some(repo);
                    }
                }
            }
        }
        events::GIT_COMMIT_CREATED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<GitCommitCreatedEvent>(params) {
                    st.model.last_commit_hash = Some(data.commit_hash);
                    st.model.last_commit_summary = Some(data.summary);
                }
            }
        }
        events::GIT_UNDO_COMPLETED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<GitUndoResult>(params) {
                    st.model.git_notification = Some(data.message);
                }
            }
        }
        events::GIT_SQUASH_COMPLETED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<GitSquashApplyResult>(params) {
                    st.squash_dialog = None;
                    let first_line = data.message.lines().next().unwrap_or("Squashed commit");
                    st.model.git_notification = Some(format!(
                        "Squashed commits into {} ('{}')",
                        data.squashed_commit, first_line
                    ));
                    st.model.last_commit_hash = Some(data.squashed_commit);
                    st.model.last_commit_summary = Some(first_line.to_string());
                }
            }
        }
        events::HISTORY_ENTRY_ADDED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<HistoryEntryAddedEvent>(params) {
                    st.history_view.items.push(data.item);
                    st.history_view.total_count += 1;
                    if st.history_view.auto_scroll {
                        let view_height = st.last_model_height;
                        let total_lines = crate::ui::compute_history_items_line_count(&st.history_view.items, None) as u16;
                        st.history_view.scroll = total_lines.saturating_sub(view_height);
                    }
                }
            }
        }
        events::CONFIG_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(cfg) = serde_json::from_value::<ConfigState>(params) {
                    st.workflow = cfg.workflow;
                    if !cfg.model.is_empty() {
                        st.active_model = cfg.model;
                    }
                    if !cfg.available_models.is_empty() {
                        st.available_models = cfg.available_models;
                    }
                    st.edit_protocol = cfg.edit_protocol;
                    if !cfg.available_workflows.is_empty() {
                        st.available_workflows = cfg.available_workflows;
                    }
                    if !cfg.available_edit_protocols.is_empty() {
                        st.available_edit_protocols = cfg.available_edit_protocols;
                    }
                }
            }
        }
        events::CONTEXT_CHANGED => {
            if let Some(params) = ev.params {
                if let Some(val) = params.get("state") {
                    if let Ok(ctx) = serde_json::from_value::<ContextState>(val.clone()) {
                        st.context = ctx;
                        let rows = st.context_view.compute_rows(&st.context.items);
                        if rows.is_empty() {
                            st.context_view.cursor_index = 0;
                        } else if st.context_view.cursor_index >= rows.len() {
                            st.context_view.cursor_index = rows.len() - 1;
                        }
                        if st.context_view.adding_file {
                            st.update_filtered_candidates();
                        }
                    }
                }
            }
        }
        events::MODEL_STARTED => {
            *is_reasoning = false;
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelStartedEvent>(params) {
                    st.model.operation_id = Some(data.operation_id);
                    st.model.model = Some(data.model);
                    st.model.status = "awaiting".to_string();
                    st.model.edits_active = false;
                    st.model.files.clear();
                    st.model.selected_file_index = 0;
                    st.model.edit_final_applied = None;
                    st.model.edit_final_error = None;
                    st.model.last_commit_hash = None;
                    st.model.last_commit_summary = None;
                    st.model.auto_scroll = true;
                    st.model.current_cost = Some(0.0);
                    st.model.round_usage_received = false;
                }
            }
        }
        events::MODEL_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    if !data.delta.is_empty() {
                        st.model.start_new_round_if_needed();
                        st.model.status = "thinking".to_string();
                        if !*is_reasoning {
                            *is_reasoning = true;
                            st.model.show_reasoning = true;
                            if !st.model.reasoning.is_empty() && !st.model.reasoning.ends_with("\n\n") {
                                st.model.reasoning.push_str("\n\n---\n\n");
                            }
                            let h = st.last_model_height;
                            st.model.clamp_scroll(h);
                        }
                    }
                    st.model.reasoning.push_str(&data.delta);
                    st.model.update_reasoning_markdown();
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::MODEL_TEXT_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    if !data.delta.is_empty() {
                        st.model.start_new_round_if_needed();
                        if st.model.edits_active {
                            st.model.status = "editing".to_string();
                        } else {
                            st.model.status = "responding".to_string();
                        }
                        if *is_reasoning {
                            *is_reasoning = false;
                            st.model.show_reasoning = false;
                            let h = st.last_model_height;
                            st.model.clamp_scroll(h);
                        }
                    }
                    st.model.text.push_str(&data.delta);
                    st.model.update_markdown();
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::EDIT_STARTED => {
            st.model.start_new_round_if_needed();
            st.model.edits_active = true;
            st.model.status = "editing".to_string();
        }
        events::EDIT_FILE_STARTED => {
            st.model.start_new_round_if_needed();
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileStartedEvent>(params) {
                    st.model.edits_active = true;
                    st.model.status = "editing".to_string();
                    if let Some(existing) = st.model.files.iter_mut().find(|f| f.path == data.path)
                    {
                        existing.status = "running".to_string();
                        existing.op_type = data.op_type;
                        existing.retry_info = None;
                    } else {
                        st.model.files.push(StreamingFileEdit {
                            path: data.path,
                            op_type: data.op_type,
                            status: "running".to_string(),
                            error: None,
                            hunks: Vec::new(),
                            expanded: false,
                            retry_info: None,
                        });
                        st.model.selected_file_index = st.model.files.len() - 1;
                    }
                }
            }
        }
        events::EDIT_HUNK => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditHunkEvent>(params) {
                    if let Some(f) = st.model.files.iter_mut().find(|f| f.path == data.path) {
                        f.retry_info = None;
                        f.hunks.push(StreamingHunk {
                            hunk_index: data.hunk_index,
                            old_text: data.old_text,
                            new_text: data.new_text,
                        });
                    }
                }
            }
        }
        events::EDIT_FILE_DONE => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileDoneEvent>(params) {
                    if let Some(f) = st.model.files.iter_mut().find(|f| f.path == data.path) {
                        f.status = data.status;
                        f.error = data.error;
                        if f.status == "ok" {
                            f.retry_info = None;
                            f.error = None;
                        }
                    }
                }
            }
        }
        events::EDIT_FILE_RETRYING => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileRetryingEvent>(params) {
                    st.model.status = "editing".to_string();
                    let retry_msg = format!("{}/{} retrying: {}", data.attempt, data.max_retries, data.reason);
                    if let Some(f) = st.model.files.iter_mut().find(|f| f.path == data.path) {
                        f.status = "retrying".to_string();
                        f.retry_info = Some(retry_msg);
                        f.error = None;
                    } else {
                        st.model.files.push(StreamingFileEdit {
                            path: data.path,
                            op_type: "replace".to_string(),
                            status: "retrying".to_string(),
                            error: None,
                            hunks: Vec::new(),
                            expanded: false,
                            retry_info: Some(retry_msg),
                        });
                        st.model.selected_file_index = st.model.files.len() - 1;
                    }
                }
            }
        }
        events::EDIT_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFinishedEvent>(params) {
                    st.model.edit_final_applied = Some(data.applied);
                    st.model.edit_final_error = data.error;
                    if let Some(hash) = data.commit_hash {
                        st.model.last_commit_hash = Some(hash);
                    }
                }
            }
        }
        events::TOOLCHAIN_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ToolchainStartedEvent>(params) {
                    st.model.status = "verifying".to_string();
                    st.model.toolchain_command = Some(data.command.clone());
                    st.model.toolchain_status = Some(format!(
                        "Running toolchain verification: {}...",
                        data.command
                    ));
                }
            }
        }
        events::TOOLCHAIN_RESULT => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ToolchainResultEvent>(params) {
                    st.model.toolchain_command = None;
                    if data.success {
                        st.model.toolchain_status =
                            Some(format!("Toolchain check passed ({})", data.command));
                    } else {
                        st.model.toolchain_status =
                            Some(format!("Toolchain check failed ({})", data.command));
                    }
                }
            }
        }
        events::MODEL_USAGE => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelUsageEvent>(params) {
                    st.model.session_total_cost = data.session_total_cost;
                    if let Some(curr) = data.current_cost {
                        st.model.current_cost = Some(curr);
                        st.model.round_usage_received = true;
                    }
                    st.model.usage = Some(data);
                }
            }
        }
        events::MODEL_RESULT => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelResultEvent>(params) {
                    st.model.result = Some(data.result);
                    if let Some(usage) = data.usage {
                        let total_cost = data
                            .session_total_cost
                            .unwrap_or(st.model.session_total_cost);
                        st.model.session_total_cost = total_cost;
                        st.model.usage = Some(ModelUsageEvent {
                            operation_id: data.operation_id.clone(),
                            usage,
                            session_total_cost: total_cost,
                            current_cost: data.current_cost,
                        });
                    }
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::MODEL_FINISHED => {
            *is_reasoning = false;
            st.confirm_cancel = false;
            if let Some(params) = ev.params {
                if let Ok(_data) = serde_json::from_value::<ModelFinishedEvent>(params) {
                    st.model.status = "done".to_string();
                    if let Some(c) = st.model.current_cost {
                        st.model.prev_cost = Some(c);
                    }
                    st.model.current_cost = None;
                    st.model.round_usage_received = false;
                    fail_safe_reject_edits(
                        &mut st.model,
                        "Edit state desynchronized: server finished without sending edit/finished",
                    );
                    st.model.update_markdown();
                    let h = st.last_model_height;
                    st.model.clamp_scroll(h);
                }
            }
        }
        events::MODEL_CANCELLED => {
            *is_reasoning = false;
            st.confirm_cancel = false;
            st.model.status = "cancelled".to_string();
            if let Some(c) = st.model.current_cost {
                if c > 0.0 {
                    st.model.prev_cost = Some(c);
                }
            }
            st.model.current_cost = None;
            st.model.round_usage_received = false;
            fail_safe_reject_edits(
                &mut st.model,
                "Operation cancelled before edits were applied",
            );
            let h = st.last_model_height;
            st.model.clamp_scroll(h);
        }
        events::MODEL_ERROR => {
            *is_reasoning = false;
            st.confirm_cancel = false;
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelErrorEvent>(params) {
                    st.model.status = "error".to_string();
                    if let Some(c) = st.model.current_cost {
                        if c > 0.0 {
                            st.model.prev_cost = Some(c);
                        }
                    }
                    st.model.current_cost = None;
                    st.model.round_usage_received = false;
                    fail_safe_reject_edits(
                        &mut st.model,
                        "Operation failed before edits were applied",
                    );
                    st.model.error = Some(data.message);
                    let h = st.last_model_height;
                    st.model.clamp_scroll(h);
                }
            }
        }
        _ => {}
    }
}
