use std::io::stdout;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use workbench_protocol::{
    methods, InitializeParams, InitializeResult, RepositoryState, Request, RequestId, Response,
    PROTOCOL_VERSION,
};

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
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

    // Terminal initialization
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    // Main UI loop
    loop {
        terminal.draw(|f| {
            render_ui(f, &init_result.protocol_version, repo_state.as_ref());
        })?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q')
                    || key.code == KeyCode::Esc
                    || (key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c'))
                {
                    break;
                }
            }
        }
    }

    let _ = server_child.kill().await;

    Ok(())
}

fn render_ui(
    frame: &mut ratatui::Frame,
    protocol_version: &str,
    repo_state: Option<&RepositoryState>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(frame.area());

    // Top Header
    let (project_name, branch, head, dirty_status) = match repo_state {
        Some(state) => {
            let name = Path::new(&state.root)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&state.root);
            let dirty = if state.dirty {
                Span::styled(" [DIRTY]", Style::default().fg(Color::Yellow).bold())
            } else {
                Span::styled(" [CLEAN]", Style::default().fg(Color::Green))
            };
            (name.to_string(), state.branch.clone(), state.head.clone(), dirty)
        }
        None => (
            "no repository".to_string(),
            "-".to_string(),
            "-".to_string(),
            Span::raw(""),
        ),
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
        Span::raw(format!(" (v{})", protocol_version)),
    ]);

    let header = Paragraph::new(header_line)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    // Center Workspace (Placeholder for Stage 1)
    let body_text = vec![
        Line::from(Span::styled("Stage 1: Live Wireframe", Style::default().bold())),
        Line::raw(""),
        Line::from(vec![
            Span::styled("✔ ", Style::default().fg(Color::Green)),
            Span::raw("Client & server connected via stdio transport"),
        ]),
        Line::from(vec![
            Span::styled("✔ ", Style::default().fg(Color::Green)),
            Span::raw(format!("Protocol handshake successful (v{})", protocol_version)),
        ]),
        Line::from(vec![
            Span::styled("✔ ", Style::default().fg(Color::Green)),
            Span::raw("Deterministic Git repository discovery loaded from core"),
        ]),
        Line::raw(""),
        Line::raw("Ready for Stage 2: Streaming LLM conversation & Intent Bar."),
    ];

    let body = Paragraph::new(body_text)
        .block(Block::default().title(" Status ").borders(Borders::ALL));
    frame.render_widget(body, chunks[1]);

    // Footer
    let footer_line = Line::from(vec![
        Span::styled(" q ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Quit   "),
        Span::styled(" Esc ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
        Span::raw(" Exit"),
    ]);

    let footer = Paragraph::new(footer_line)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[2]);
}
