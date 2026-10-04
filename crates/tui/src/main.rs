use std::io::stdout;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{self, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use workbench_protocol::{
    events, methods, Event, InitializeParams, InitializeResult, Message, ModelAskParams,
    ModelDeltaEvent, ModelErrorEvent, ModelFinishedEvent, ModelStartedEvent, ModelUsageEvent,
    RepositoryState, Request, RequestId, Response, PROTOCOL_VERSION,
};

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

struct ModelView {
    operation_id: Option<String>,
    model: Option<String>,
    reasoning: String,
    text: String,
    usage: Option<ModelUsageEvent>,
    session_total_cost: f64,
    status: String, // "idle", "starting", "streaming", "done", "cancelled", "error"
    error: Option<String>,
    scroll: u16,
    show_reasoning: bool,
}

impl Default for ModelView {
    fn default() -> Self {
        Self {
            operation_id: None,
            model: None,
            reasoning: String::new(),
            text: String::new(),
            usage: None,
            session_total_cost: 0.0,
            status: String::new(),
            error: None,
            scroll: 0,
            show_reasoning: true,
        }
    }
}

struct AppState {
    protocol_version: String,
    repo_state: Option<RepositoryState>,
    model: ModelView,
}

fn find_server_binary() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let exe_name = if cfg!(windows) {
                "workbench-server.exe"
            } else {
                "workbench-server"
            };
            let candidate = parent.join(exe_name);
            if candidate.exists() {
                return candidate;
            }
        }
    }

    for dir in &["target/debug", "target/release"] {
        let exe_name = if cfg!(windows) {
            "workbench-server.exe"
        } else {
            "workbench-server"
        };
        let candidate = Path::new(dir).join(exe_name);
        if candidate.exists() {
            return candidate;
        }
    }

    PathBuf::from(if cfg!(windows) {
        "workbench-server.exe"
    } else {
        "workbench-server"
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
                "Failed to spawn workbench-server at '{}'. Make sure to run 'cargo build' first.",
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
            client_name: "workbench-tui".to_string(),
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

    // Shared UI state
    let state = Arc::new(Mutex::new(AppState {
        protocol_version,
        repo_state,
        model: ModelView::default(),
    }));

    // Channel for incoming server messages (both Events and Responses)
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<Message>(100);

    // Spawn background reader
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

    // Channel for key events
    let (key_tx, mut key_rx) = tokio::sync::mpsc::channel::<event::KeyEvent>(100);
    {
        tokio::spawn(async move {
            loop {
                if event::poll(Duration::from_millis(10)).unwrap() {
                    if let crossterm::event::Event::Key(k) = event::read().unwrap() {
                        if key_tx.send(k).await.is_err() {
                            break;
                        }
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        });
    }

    // Terminal initialization
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut input_buffer = String::new();

    // Main UI loop
    loop {
        {
            let st = state.lock().await;
            terminal.draw(|f| render_ui(f, &st, &input_buffer))?;
        }

        tokio::select! {
            Some(key) = key_rx.recv() => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('c') => {
                            send_request(&mut server_writer, methods::MODEL_CANCEL, serde_json::json!({})).await?;
                        }
                        KeyCode::Char('l') => {
                            {
                                let mut st = state.lock().await;
                                let cost = st.model.session_total_cost;
                                st.model = ModelView {
                                    session_total_cost: cost,
                                    ..Default::default()
                                };
                            }
                            send_request(&mut server_writer, methods::MODEL_CLEAR_HISTORY, serde_json::json!({})).await?;
                        }
                        KeyCode::Char('r') => {
                            let mut st = state.lock().await;
                            st.model.show_reasoning = !st.model.show_reasoning;
                        }
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => {
                            let st = state.lock().await;
                            if st.model.status == "streaming" || st.model.status == "starting" {
                                drop(st);
                                send_request(&mut server_writer, methods::MODEL_CANCEL, serde_json::json!({})).await?;
                            } else {
                                break;
                            }
                        }
                        KeyCode::Enter => {
                            if !input_buffer.trim().is_empty() {
                                let prompt = input_buffer.clone();
                                input_buffer.clear();
                                
                                {
                                    let mut st = state.lock().await;
                                    st.model.reasoning.clear();
                                    st.model.text.clear();
                                    st.model.error = None;
                                    st.model.usage = None;
                                    st.model.scroll = 0;
                                    st.model.status = "starting".to_string();
                                    st.model.show_reasoning = true;
                                }

                                let params = ModelAskParams { prompt };
                                send_request(&mut server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await?;
                            }
                        }
                        KeyCode::Char(c) => {
                            input_buffer.push(c);
                        }
                        KeyCode::Backspace => {
                            input_buffer.pop();
                        }
                        KeyCode::PageUp => {
                            let mut st = state.lock().await;
                            st.model.scroll = st.model.scroll.saturating_sub(10);
                        }
                        KeyCode::PageDown => {
                            let mut st = state.lock().await;
                            st.model.scroll = st.model.scroll.saturating_add(10);
                        }
                        KeyCode::Up => {
                            let mut st = state.lock().await;
                            st.model.scroll = st.model.scroll.saturating_sub(1);
                        }
                        KeyCode::Down => {
                            let mut st = state.lock().await;
                            st.model.scroll = st.model.scroll.saturating_add(1);
                        }
                        _ => {}
                    }
                }
            }
            Some(msg) = msg_rx.recv() => {
                match msg {
                    Message::Event(ev) => handle_event(ev, &state).await,
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
    if let Some(err) = resp.error {
        let mut st = state.lock().await;
        st.model.status = "error".to_string();
        st.model.error = Some(format!("{}: {}", err.code, err.message));
    }
}

async fn handle_event(ev: Event, state: &Arc<Mutex<AppState>>) {
    let mut st = state.lock().await;
    match ev.method.as_str() {
        events::MODEL_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelStartedEvent>(params) {
                    st.model.operation_id = Some(data.operation_id);
                    st.model.model = Some(data.model);
                    st.model.status = "streaming".to_string();
                }
            }
        }
        events::MODEL_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    st.model.reasoning.push_str(&data.delta);
                }
            }
        }
        events::MODEL_TEXT_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    if st.model.text.is_empty() && !data.delta.is_empty() {
                        // Hide reasoning automatically when the actual answer starts
                        st.model.show_reasoning = false;
                    }
                    st.model.text.push_str(&data.delta);
                }
            }
        }
        events::MODEL_USAGE => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelUsageEvent>(params) {
                    st.model.session_total_cost = data.session_total_cost;
                    st.model.usage = Some(data);
                }
            }
        }
        events::MODEL_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelFinishedEvent>(params) {
                    st.model.text = data.full_text;
                    st.model.status = "done".to_string();
                }
            }
        }
        events::MODEL_CANCELLED => {
            st.model.status = "cancelled".to_string();
        }
        events::MODEL_ERROR => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelErrorEvent>(params) {
                    st.model.status = "error".to_string();
                    st.model.error = Some(data.message);
                }
            }
        }
        _ => {}
    }
}

fn render_ui(
    frame: &mut ratatui::Frame,
    state: &AppState,
    input_buffer: &str,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(frame.area());

    // Top Header
    let (project_name, branch, head, dirty_status) = match &state.repo_state {
        Some(repo) => {
            let name = Path::new(&repo.root)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&repo.root);
            let dirty = if repo.dirty {
                Span::styled(" [DIRTY]", Style::default().fg(Color::Yellow).bold())
            } else {
                Span::styled(" [CLEAN]", Style::default().fg(Color::Green))
            };
            (name.to_string(), repo.branch.clone(), repo.head.clone(), dirty)
        }
        None => ("no repository".to_string(), "-".to_string(), "-".to_string(), Span::raw("")),
    };

    let header_line = Line::from(vec![
        Span::styled(" WORKBENCH ", Style::default().bg(Color::Blue).fg(Color::White).bold()),
        Span::raw("  Project: "),
        Span::styled(project_name, Style::default().bold()),
        Span::raw("  Branch: "),
        Span::styled(branch, Style::default().fg(Color::Cyan)),
        Span::raw("  HEAD: "),
        Span::styled(head, Style::default().fg(Color::Magenta)),
        dirty_status,
        Span::raw("  |  Server: "),
        Span::styled("CONNECTED", Style::default().fg(Color::Green).bold()),
        Span::raw(format!(" (v{})", state.protocol_version)),
    ]);

    let header = Paragraph::new(header_line)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    // Model Output / Reasoning
    let mut model_lines: Vec<Line> = Vec::new();
    model_lines.push(Line::from(Span::styled(
        match state.model.status.as_str() {
            "starting" => "Starting...",
            "streaming" => "Streaming...",
            "done" => "Done",
            "cancelled" => "Cancelled",
            "error" => "Error",
            _ => "Idle",
        },
        Style::default().bold().fg(match state.model.status.as_str() {
            "starting" => Color::Yellow,
            "streaming" => Color::Yellow,
            "done" => Color::Green,
            "cancelled" => Color::Red,
            "error" => Color::Red,
            _ => Color::Gray,
        }),
    )));

    if let Some(model) = &state.model.model {
        model_lines.push(Line::from(vec![Span::raw("Model: "), Span::styled(model, Style::default().fg(Color::Cyan))]));
    }

    if let Some(err) = &state.model.error {
        model_lines.push(Line::from(vec![Span::raw("Error: "), Span::styled(err, Style::default().fg(Color::Red))]));
    }

    if !state.model.reasoning.is_empty() {
        if state.model.show_reasoning {
            model_lines.push(Line::from(Span::styled("Reasoning (Ctrl+R to hide):", Style::default().bold().fg(Color::Magenta))));
            let reasoning_lines: Vec<&str> = state.model.reasoning.lines().collect();
            for line in reasoning_lines {
                model_lines.push(Line::from(Span::styled(line.to_string(), Style::default().fg(Color::Magenta))));
            }
        } else {
            model_lines.push(Line::from(Span::styled("[+] Reasoning hidden (Ctrl+R to show)", Style::default().bold().fg(Color::DarkGray))));
        }
    }

    if !state.model.text.is_empty() {
        model_lines.push(Line::from(Span::styled("Answer:", Style::default().bold().fg(Color::Green))));
        let answer_lines: Vec<&str> = state.model.text.lines().collect();
        for line in answer_lines {
            model_lines.push(Line::from(Span::raw(line.to_string())));
        }
    }

    if let Some(usage) = &state.model.usage {
        model_lines.push(Line::from(vec![
            Span::raw("Usage: "),
            Span::styled(
                format!(
                    "prompt {} tokens, completion {} tokens, reason {} tokens, cost ${:.5}",
                    usage.usage.prompt_tokens,
                    usage.usage.completion_tokens,
                    usage.usage.reasoning_tokens.unwrap_or(0),
                    usage.usage.cost.unwrap_or(0.0)
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    
    if state.model.session_total_cost > 0.0 {
        model_lines.push(Line::from(vec![
            Span::raw("Session total cost: "),
            Span::styled(
                format!("${:.5}", state.model.session_total_cost),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    let model_paragraph = Paragraph::new(model_lines)
        .block(Block::default().title(" Model ").borders(Borders::ALL))
        .wrap(Wrap { trim: true })
        .scroll((state.model.scroll, 0));
    frame.render_widget(model_paragraph, chunks[1]);

    // Input buffer display
    let input_line = Line::from(vec![
        Span::styled(" > ", Style::default().fg(Color::Cyan).bold()),
        Span::raw(input_buffer),
    ]);
    let input_paragraph = Paragraph::new(input_line)
        .block(Block::default().title(" Intent ").borders(Borders::ALL));
    frame.render_widget(input_paragraph, chunks[2]);

    // Footer
    let footer_line = Line::from(vec![
        Span::styled(" Enter ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Send  "),
        Span::styled(" Esc ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Cancel/Quit  "),
        Span::styled(" Ctrl+L ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Clear  "),
        Span::styled(" Ctrl+R ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Reasoning  "),
        Span::styled(" ↑/↓/PgUp/PgDn ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Scroll"),
    ]);

    let footer = Paragraph::new(footer_line)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[3]);
}
