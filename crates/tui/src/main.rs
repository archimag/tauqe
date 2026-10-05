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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use workbench_protocol::matches_glob_pattern;
use workbench_protocol::{
    events, methods, ContextAccess, ContextAddParams, ContextAddPatternParams,
    ContextAddPatternResult, ContextRemoveParams, ContextSetAccessParams, ContextState,
    EditFileDoneEvent, EditFileStartedEvent, EditFinishedEvent, EditHunkEvent, Event,
    InitializeParams, InitializeResult, Message, ModelAskParams, ModelDeltaEvent,
    ModelErrorEvent, ModelFinishedEvent, ModelResult, ModelResultEvent, ModelStartedEvent,
    ModelUsageEvent, RepositoryListFilesResult, RepositoryState, Request, RequestId, Response,
    PROTOCOL_VERSION,
};

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Model,
    Context,
}

#[derive(Debug, Clone)]
struct StreamingHunk {
    hunk_index: usize,
    old_text: String,
    new_text: String,
}

#[derive(Debug, Clone)]
struct StreamingFileEdit {
    path: String,
    op_type: String, // "replace", "create", "delete"
    status: String,  // "running", "ok", "error"
    error: Option<String>,
    hunks: Vec<StreamingHunk>,
    expanded: bool,
}

struct ModelView {
    operation_id: Option<String>,
    model: Option<String>,
    reasoning: String,
    text: String,
    usage: Option<ModelUsageEvent>,
    last_op_cost: Option<f64>,
    session_total_cost: f64,
    status: String, // "idle", "starting", "streaming", "done", "cancelled", "error"
    error: Option<String>,
    result: Option<ModelResult>,
    scroll: u16,
    show_reasoning: bool,
    auto_scroll: bool,

    // Structured Org-Mode Edits State
    edits_active: bool,
    files: Vec<StreamingFileEdit>,
    selected_file_index: usize,
    edit_final_applied: Option<bool>,
    edit_final_error: Option<String>,
    spinner_frame: usize,
}

impl Default for ModelView {
    fn default() -> Self {
        Self {
            operation_id: None,
            model: None,
            reasoning: String::new(),
            text: String::new(),
            usage: None,
            last_op_cost: None,
            session_total_cost: 0.0,
            status: String::new(),
            error: None,
            result: None,
            scroll: 0,
            show_reasoning: true,
            auto_scroll: true,
            edits_active: false,
            files: Vec::new(),
            selected_file_index: 0,
            edit_final_applied: None,
            edit_final_error: None,
            spinner_frame: 0,
        }
    }
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl ModelView {
    fn max_scroll(&self, view_height: u16) -> u16 {
        let lines = compute_model_lines(self);
        let count = lines.len() as u16;
        count.saturating_sub(view_height)
    }

    fn clamp_scroll(&mut self, view_height: u16) {
        let max = self.max_scroll(view_height);
        if self.scroll > max {
            self.scroll = max;
        }
    }
}

struct ContextViewState {
    cursor_index: usize,
    adding_file: bool,
    add_input: String,
    filtered_candidates: Vec<String>,
    selected_candidate_index: usize,
    status_message: Option<String>,
}

impl Default for ContextViewState {
    fn default() -> Self {
        Self {
            cursor_index: 0,
            adding_file: false,
            add_input: String::new(),
            filtered_candidates: Vec::new(),
            selected_candidate_index: 0,
            status_message: None,
        }
    }
}

struct AppState {
    view_mode: ViewMode,
    protocol_version: String,
    repo_state: Option<RepositoryState>,
    all_repo_files: Vec<String>,
    model: ModelView,
    context: ContextState,
    context_view: ContextViewState,
    show_help: bool,
    last_model_height: u16,
}

impl AppState {
    fn update_filtered_candidates(&mut self) {
        let query = self.context_view.add_input.trim();
        let existing: std::collections::HashSet<&str> = self
            .context
            .items
            .iter()
            .map(|it| it.path.as_str())
            .collect();

        let mut candidates = Vec::new();

        if !query.is_empty() {
            // Check if query behaves as pattern or directory prefix
            let is_pattern_query = query.contains('*')
                || query.contains('?')
                || query.ends_with('/')
                || !query.contains('.');

            let matching_pattern_count = self
                .all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()) && matches_glob_pattern(query, f))
                .count();

            if matching_pattern_count > 0 && is_pattern_query {
                candidates.push(format!(
                    "[+] Add all matching '{}' ({} files)",
                    query, matching_pattern_count
                ));
            }

            let query_lower = query.to_lowercase();
            let file_candidates: Vec<String> = self
                .all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()))
                .filter(|f| {
                    f.to_lowercase().contains(&query_lower) || matches_glob_pattern(query, f)
                })
                .take(15)
                .cloned()
                .collect();

            candidates.extend(file_candidates);
        } else {
            candidates = self
                .all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()))
                .take(15)
                .cloned()
                .collect();
        }

        self.context_view.filtered_candidates = candidates;

        if self.context_view.filtered_candidates.is_empty() {
            self.context_view.selected_candidate_index = 0;
        } else if self.context_view.selected_candidate_index >= self.context_view.filtered_candidates.len() {
            self.context_view.selected_candidate_index =
                self.context_view.filtered_candidates.len() - 1;
        }
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
    let protocol_version = init_result.protocol_version.clone();

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

    // Shared UI state
    let state = Arc::new(Mutex::new(AppState {
        view_mode: ViewMode::Model,
        protocol_version,
        repo_state,
        all_repo_files: initial_files,
        model: ModelView::default(),
        context: initial_context,
        context_view: ContextViewState::default(),
        show_help: false,
        last_model_height: 10,
    }));

    // Channel for incoming server messages
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
            let mut st = state.lock().await;
            st.model.spinner_frame = (st.model.spinner_frame + 1) % SPINNER_FRAMES.len();
            terminal.draw(|f| render_ui(f, &mut st, &input_buffer))?;
        }

        tokio::select! {
            Some(key) = key_rx.recv() => {
                let mut st = state.lock().await;

                // Help overlay toggle / dismissal
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
                        KeyCode::Char('c') => {
                            drop(st);
                            send_request(&mut server_writer, methods::MODEL_CANCEL, serde_json::json!({})).await?;
                            continue;
                        }
                        KeyCode::Char('l') => {
                            let cost = st.model.session_total_cost;
                            let last_cost = st.model.last_op_cost;
                            st.model = ModelView {
                                session_total_cost: cost,
                                last_op_cost: last_cost,
                                ..Default::default()
                            };
                            drop(st);
                            send_request(&mut server_writer, methods::MODEL_CLEAR_HISTORY, serde_json::json!({})).await?;
                            continue;
                        }
                        KeyCode::Char('r') => {
                            st.model.show_reasoning = !st.model.show_reasoning;
                            let h = st.last_model_height;
                            st.model.clamp_scroll(h);
                            continue;
                        }
                        _ => {}
                    }
                }

                // Global Help toggle
                if key.code == KeyCode::Char('?') && !st.context_view.adding_file && (st.view_mode != ViewMode::Model || input_buffer.is_empty()) {
                    st.show_help = true;
                    continue;
                }

                // View Mode switching via Tab (when not typing in add input or intent prompt)
                if key.code == KeyCode::Tab && !st.context_view.adding_file && (input_buffer.is_empty() || st.view_mode == ViewMode::Context) {
                    st.view_mode = match st.view_mode {
                        ViewMode::Model => ViewMode::Context,
                        ViewMode::Context => ViewMode::Model,
                    };
                    st.context_view.status_message = None;
                    continue;
                }

                match st.view_mode {
                    ViewMode::Model => {
                        let view_height = st.last_model_height;
                        match key.code {
                            KeyCode::Esc => {
                                if st.model.status == "streaming" || st.model.status == "starting" {
                                    drop(st);
                                    send_request(&mut server_writer, methods::MODEL_CANCEL, serde_json::json!({})).await?;
                                } else {
                                    break;
                                }
                            }
                            KeyCode::Char('[') => {
                                if !st.model.files.is_empty() {
                                    st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                                }
                            }
                            KeyCode::Char(']') => {
                                if !st.model.files.is_empty() && st.model.selected_file_index + 1 < st.model.files.len() {
                                    st.model.selected_file_index += 1;
                                }
                            }
                            KeyCode::Char(' ') if input_buffer.is_empty() => {
                                let sel_idx = st.model.selected_file_index;
                                if let Some(file) = st.model.files.get_mut(sel_idx) {
                                    file.expanded = !file.expanded;
                                    let h = st.last_model_height;
                                    st.model.clamp_scroll(h);
                                }
                            }
                            KeyCode::Enter => {
                                if !input_buffer.trim().is_empty() {
                                    let prompt = input_buffer.clone();
                                    input_buffer.clear();

                                    st.model.reasoning.clear();
                                    st.model.text.clear();
                                    st.model.error = None;
                                    st.model.result = None;
                                    st.model.usage = None;
                                    st.model.scroll = 0;
                                    st.model.status = "starting".to_string();
                                    st.model.show_reasoning = true;
                                    st.model.auto_scroll = true;

                                    st.model.edits_active = false;
                                    st.model.files.clear();
                                    st.model.selected_file_index = 0;
                                    st.model.edit_final_applied = None;
                                    st.model.edit_final_error = None;

                                    let params = ModelAskParams { prompt };
                                    drop(st);
                                    send_request(&mut server_writer, methods::MODEL_ASK, serde_json::to_value(params)?).await?;
                                } else if !st.model.files.is_empty() {
                                    let sel_idx = st.model.selected_file_index;
                                    if let Some(file) = st.model.files.get_mut(sel_idx) {
                                        file.expanded = !file.expanded;
                                        let h = st.last_model_height;
                                        st.model.clamp_scroll(h);
                                    }
                                }
                            }
                            KeyCode::Char(c) => {
                                input_buffer.push(c);
                            }
                            KeyCode::Backspace => {
                                input_buffer.pop();
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
                            KeyCode::Up => {
                                if input_buffer.is_empty() && !st.model.files.is_empty() {
                                    st.model.selected_file_index = st.model.selected_file_index.saturating_sub(1);
                                } else {
                                    st.model.auto_scroll = false;
                                    st.model.scroll = st.model.scroll.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                if input_buffer.is_empty() && !st.model.files.is_empty() && st.model.selected_file_index + 1 < st.model.files.len() {
                                    st.model.selected_file_index += 1;
                                } else {
                                    let max = st.model.max_scroll(view_height);
                                    st.model.scroll = (st.model.scroll.saturating_add(1)).min(max);
                                    if st.model.scroll >= max {
                                        st.model.auto_scroll = true;
                                    }
                                }
                            }
                            KeyCode::Home => {
                                st.model.auto_scroll = false;
                                st.model.scroll = 0;
                            }
                            KeyCode::End => {
                                st.model.auto_scroll = true;
                                st.model.scroll = st.model.max_scroll(view_height);
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

                                    let is_pattern_entry = selected_candidate.as_ref().map_or(false, |c| {
                                        c.starts_with("[+] Add all matching '")
                                    });

                                    let raw_input = st.context_view.add_input.trim().to_string();
                                    let is_glob_direct = raw_input.contains('*')
                                        || raw_input.contains('?')
                                        || raw_input.ends_with('/');

                                    if is_pattern_entry || is_glob_direct {
                                        st.context_view.adding_file = false;
                                        st.context_view.add_input.clear();
                                        drop(st);

                                        let params = ContextAddPatternParams {
                                            pattern: raw_input,
                                            access: ContextAccess::ReadOnly,
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
                                                access: ContextAccess::ReadOnly,
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
                            let total_items = st.context.items.len();
                            match key.code {
                                KeyCode::Esc | KeyCode::Char('q') => {
                                    st.view_mode = ViewMode::Model;
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    if total_items > 0 {
                                        st.context_view.cursor_index = st.context_view.cursor_index.saturating_sub(1);
                                    }
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if total_items > 0 && st.context_view.cursor_index + 1 < total_items {
                                        st.context_view.cursor_index += 1;
                                    }
                                }
                                KeyCode::Char('a') => {
                                    st.context_view.adding_file = true;
                                    st.context_view.add_input.clear();
                                    st.context_view.selected_candidate_index = 0;
                                    st.context_view.status_message = None;
                                    st.update_filtered_candidates();

                                    // Refresh repository files list in background
                                    drop(st);
                                    send_request(
                                        &mut server_writer,
                                        methods::REPOSITORY_LIST_FILES,
                                        serde_json::json!({}),
                                    )
                                    .await?;
                                }
                                KeyCode::Char('e') => {
                                    if let Some(item) = st.context.items.get(st.context_view.cursor_index) {
                                        let path = item.path.clone();
                                        drop(st);
                                        let params = ContextSetAccessParams {
                                            path,
                                            access: ContextAccess::Editable,
                                        };
                                        send_request(&mut server_writer, methods::CONTEXT_SET_ACCESS, serde_json::to_value(params)?).await?;
                                    }
                                }
                                KeyCode::Char('r') => {
                                    if let Some(item) = st.context.items.get(st.context_view.cursor_index) {
                                        let path = item.path.clone();
                                        drop(st);
                                        let params = ContextSetAccessParams {
                                            path,
                                            access: ContextAccess::ReadOnly,
                                        };
                                        send_request(&mut server_writer, methods::CONTEXT_SET_ACCESS, serde_json::to_value(params)?).await?;
                                    }
                                }
                                KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => {
                                    if let Some(item) = st.context.items.get(st.context_view.cursor_index) {
                                        let path = item.path.clone();
                                        if st.context_view.cursor_index > 0 && st.context_view.cursor_index >= total_items.saturating_sub(1) {
                                            st.context_view.cursor_index -= 1;
                                        }
                                        drop(st);
                                        let params = ContextRemoveParams { path };
                                        send_request(&mut server_writer, methods::CONTEXT_REMOVE, serde_json::to_value(params)?).await?;
                                    }
                                }
                                _ => {}
                            }
                        }
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
    let mut st = state.lock().await;
    if let Some(err) = resp.error {
        st.context_view.status_message = Some(format!("Error: {}", err.message));
        if st.view_mode == ViewMode::Model {
            st.model.status = "error".to_string();
            st.model.error = Some(format!("{}: {}", err.code, err.message));
        }
    } else if let Some(val) = resp.result {
        if let Ok(pattern_res) = serde_json::from_value::<ContextAddPatternResult>(val.clone()) {
            st.context = pattern_res.state;
            st.context_view.status_message = Some(format!(
                "Added {} files (~{} tokens)",
                pattern_res.added_count, pattern_res.added_tokens
            ));
            if st.context_view.adding_file {
                st.update_filtered_candidates();
            }
        } else if let Ok(file_res) = serde_json::from_value::<RepositoryListFilesResult>(val.clone()) {
            st.all_repo_files = file_res.files;
            if st.context_view.adding_file {
                st.update_filtered_candidates();
            }
        } else if let Ok(ctx) = serde_json::from_value::<ContextState>(val) {
            st.context = ctx;
            if st.context.items.is_empty() {
                st.context_view.cursor_index = 0;
            } else if st.context_view.cursor_index >= st.context.items.len() {
                st.context_view.cursor_index = st.context.items.len() - 1;
            }
            if st.context_view.adding_file {
                st.update_filtered_candidates();
            }
        }
    }
}

async fn handle_event(ev: Event, state: &Arc<Mutex<AppState>>) {
    let mut st = state.lock().await;
    match ev.method.as_str() {
        events::CONTEXT_CHANGED => {
            if let Some(params) = ev.params {
                if let Some(val) = params.get("state") {
                    if let Ok(ctx) = serde_json::from_value::<ContextState>(val.clone()) {
                        st.context = ctx;
                        if st.context.items.is_empty() {
                            st.context_view.cursor_index = 0;
                        } else if st.context_view.cursor_index >= st.context.items.len() {
                            st.context_view.cursor_index = st.context.items.len() - 1;
                        }
                        if st.context_view.adding_file {
                            st.update_filtered_candidates();
                        }
                    }
                }
            }
        }
        events::MODEL_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelStartedEvent>(params) {
                    st.model.operation_id = Some(data.operation_id);
                    st.model.model = Some(data.model);
                    st.model.status = "streaming".to_string();
                    st.model.auto_scroll = true;
                }
            }
        }
        events::MODEL_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    st.model.reasoning.push_str(&data.delta);
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
                    if st.model.text.is_empty() && !data.delta.is_empty() {
                        st.model.show_reasoning = false;
                        let h = st.last_model_height;
                        st.model.clamp_scroll(h);
                    }
                    st.model.text.push_str(&data.delta);
                    if st.model.auto_scroll {
                        let h = st.last_model_height;
                        st.model.scroll = st.model.max_scroll(h);
                    }
                }
            }
        }
        events::EDIT_STARTED => {
            st.model.edits_active = true;
        }
        events::EDIT_FILE_STARTED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFileStartedEvent>(params) {
                    st.model.edits_active = true;
                    if let Some(existing) = st.model.files.iter_mut().find(|f| f.path == data.path) {
                        existing.status = "running".to_string();
                        existing.op_type = data.op_type;
                    } else {
                        st.model.files.push(StreamingFileEdit {
                            path: data.path,
                            op_type: data.op_type,
                            status: "running".to_string(),
                            error: None,
                            hunks: Vec::new(),
                            expanded: false,
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
                    }
                }
            }
        }
        events::EDIT_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<EditFinishedEvent>(params) {
                    st.model.edit_final_applied = Some(data.applied);
                    st.model.edit_final_error = data.error;
                }
            }
        }
        events::MODEL_USAGE => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelUsageEvent>(params) {
                    st.model.session_total_cost = data.session_total_cost;
                    if let Some(c) = data.usage.cost {
                        st.model.last_op_cost = Some(c);
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
                        let total_cost = data.session_total_cost.unwrap_or(st.model.session_total_cost);
                        st.model.session_total_cost = total_cost;
                        if let Some(c) = usage.cost {
                            st.model.last_op_cost = Some(c);
                        }
                        st.model.usage = Some(ModelUsageEvent {
                            operation_id: data.operation_id.clone(),
                            usage,
                            session_total_cost: total_cost,
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
            if let Some(params) = ev.params {
                if let Ok(_data) = serde_json::from_value::<ModelFinishedEvent>(params) {
                    st.model.status = "done".to_string();
                    let h = st.last_model_height;
                    st.model.clamp_scroll(h);
                }
            }
        }
        events::MODEL_CANCELLED => {
            st.model.status = "cancelled".to_string();
            let h = st.last_model_height;
            st.model.clamp_scroll(h);
        }
        events::MODEL_ERROR => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelErrorEvent>(params) {
                    st.model.status = "error".to_string();
                    st.model.error = Some(data.message);
                    let h = st.last_model_height;
                    st.model.clamp_scroll(h);
                }
            }
        }
        _ => {}
    }
}

fn compute_model_lines(model: &ModelView) -> Vec<Line<'static>> {
    let mut model_lines: Vec<Line<'static>> = Vec::new();

    model_lines.push(Line::from(Span::styled(
        match model.status.as_str() {
            "starting" => "Starting...",
            "streaming" => "Streaming...",
            "done" => "Done",
            "cancelled" => "Cancelled",
            "error" => "Error",
            _ => "Idle",
        },
        Style::default().bold().fg(match model.status.as_str() {
            "starting" => Color::Yellow,
            "streaming" => Color::Yellow,
            "done" => Color::Green,
            "cancelled" => Color::Red,
            "error" => Color::Red,
            _ => Color::Gray,
        }),
    )));

    if let Some(m) = &model.model {
        model_lines.push(Line::from(vec![
            Span::raw("Model: "),
            Span::styled(m.clone(), Style::default().fg(Color::Cyan)),
        ]));
    }

    if let Some(err) = &model.error {
        model_lines.push(Line::from(vec![
            Span::raw("Error: "),
            Span::styled(err.clone(), Style::default().fg(Color::Red)),
        ]));
    }

    if !model.reasoning.is_empty() {
        if model.show_reasoning {
            model_lines.push(Line::from(Span::styled(
                "Reasoning (Ctrl+R to hide):",
                Style::default().bold().fg(Color::Magenta),
            )));
            for line in model.reasoning.lines() {
                model_lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Magenta),
                )));
            }
        } else {
            model_lines.push(Line::from(Span::styled(
                "[+] Reasoning hidden (Ctrl+R to show)",
                Style::default().bold().fg(Color::DarkGray),
            )));
        }
    }

    if !model.text.is_empty() {
        model_lines.push(Line::raw(""));
        for line in model.text.lines() {
            model_lines.push(Line::from(Span::raw(line.to_string())));
        }
    }

    if model.edits_active || !model.files.is_empty() {
        model_lines.push(Line::raw(""));

        let total_files = model.files.len();
        let ok_files = model.files.iter().filter(|f| f.status == "ok").count();

        let header_badge = match model.edit_final_applied {
            Some(true) => Span::styled(" [APPLIED] ", Style::default().bg(Color::Green).fg(Color::Black).bold()),
            Some(false) => Span::styled(" [REJECTED] ", Style::default().bg(Color::Red).fg(Color::White).bold()),
            None => {
                let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
                Span::styled(format!(" [{}] Modifying ", spin), Style::default().bg(Color::Yellow).fg(Color::Black).bold())
            }
        };

        model_lines.push(Line::from(vec![
            header_badge,
            Span::raw(" "),
            Span::styled(
                format!("Proposed Edits ({}/{} files) - [ / ] Navigate, Space/Enter to Fold/Unfold", ok_files, total_files),
                Style::default().bold().fg(Color::Cyan),
            ),
        ]));

        for (idx, file) in model.files.iter().enumerate() {
            let is_selected = idx == model.selected_file_index;
            let fold_icon = if file.expanded { "▼ " } else { "▶ " };
            let cursor_prefix = if is_selected { "● " } else { "  " };

            let (status_icon, status_style) = match file.status.as_str() {
                "ok" => ("✓", Style::default().fg(Color::Green).bold()),
                "error" => ("✗", Style::default().fg(Color::Red).bold()),
                _ => (
                    SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()],
                    Style::default().fg(Color::Yellow).bold(),
                ),
            };

            let op_label = match file.op_type.as_str() {
                "create" => "[NEW] ",
                "delete" => "[DEL] ",
                _ => "",
            };

            let hunk_count = file.hunks.len();
            let hunk_label = if hunk_count == 1 {
                "1 hunk".to_string()
            } else {
                format!("{} hunks", hunk_count)
            };

            let file_line_style = if is_selected {
                Style::default().bg(Color::DarkGray).bold()
            } else {
                Style::default()
            };

            model_lines.push(
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(cursor_prefix, Style::default().fg(Color::Cyan)),
                    Span::styled(fold_icon, Style::default().fg(Color::DarkGray)),
                    Span::styled(op_label, Style::default().fg(Color::Magenta).bold()),
                    Span::styled(file.path.clone(), Style::default().bold()),
                    Span::raw(" "),
                    Span::styled(format!("({})", hunk_label), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(status_icon, status_style),
                ])
                .style(file_line_style),
            );

            if let Some(err_msg) = &file.error {
                model_lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled("Validation Error: ", Style::default().fg(Color::Red).bold()),
                    Span::styled(err_msg.clone(), Style::default().fg(Color::Yellow)),
                ]));
            }

            if file.expanded {
                for hunk in &file.hunks {
                    model_lines.push(Line::from(vec![
                        Span::raw("      "),
                        Span::styled(
                            format!("@@ hunk {} @@", hunk.hunk_index + 1),
                            Style::default().fg(Color::DarkGray).italic(),
                        ),
                    ]));

                    for line in hunk.old_text.lines() {
                        model_lines.push(Line::from(vec![
                            Span::raw("      "),
                            Span::styled(format!("- {}", line), Style::default().fg(Color::Red)),
                        ]));
                    }
                    for line in hunk.new_text.lines() {
                        model_lines.push(Line::from(vec![
                            Span::raw("      "),
                            Span::styled(format!("+ {}", line), Style::default().fg(Color::Green)),
                        ]));
                    }
                }
            }
        }
    }

    if let Some(usage) = &model.usage {
        model_lines.push(Line::raw(""));
        let cost_val = usage.usage.cost.unwrap_or(0.0);
        let cost_str = format!("${:.5}", cost_val);
        let cost_style = if cost_val > 0.0 {
            Style::default().fg(Color::Yellow).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let mut usage_spans = vec![
            Span::styled("Tokens: ", Style::default().fg(Color::DarkGray).bold()),
            Span::styled(
                format!(
                    "{} prompt, {} completion",
                    usage.usage.prompt_tokens,
                    usage.usage.completion_tokens,
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ];

        if let Some(reason_tokens) = usage.usage.reasoning_tokens {
            if reason_tokens > 0 {
                usage_spans.push(Span::styled(
                    format!(", {} reasoning", reason_tokens),
                    Style::default().fg(Color::Magenta),
                ));
            }
        }

        if let Some(cached_tokens) = usage.usage.cached_tokens {
            if cached_tokens > 0 {
                usage_spans.push(Span::styled(
                    format!(", {} cached", cached_tokens),
                    Style::default().fg(Color::Cyan),
                ));
            }
        }

        usage_spans.push(Span::raw(" | "));
        usage_spans.push(Span::styled("Operation cost: ", Style::default().fg(Color::Yellow).bold()));
        usage_spans.push(Span::styled(cost_str, cost_style));

        model_lines.push(Line::from(usage_spans));
    }

    if model.session_total_cost > 0.0 {
        model_lines.push(Line::from(vec![
            Span::styled("Session total cost: ", Style::default().fg(Color::Yellow).bold()),
            Span::styled(
                format!("${:.5}", model.session_total_cost),
                Style::default().fg(Color::Yellow).bold(),
            ),
        ]));
    }

    model_lines
}

fn render_ui(frame: &mut ratatui::Frame, state: &mut AppState, input_buffer: &str) {
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

    let model_tab_style = if state.view_mode == ViewMode::Model {
        Style::default().bg(Color::Blue).fg(Color::White).bold()
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let context_tab_style = if state.view_mode == ViewMode::Context {
        Style::default().bg(Color::Blue).fg(Color::White).bold()
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let header_line = Line::from(vec![
        Span::styled(" WORKBENCH ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
        Span::raw("  "),
        Span::styled(" 1: Model ", model_tab_style),
        Span::raw(" "),
        Span::styled(
            format!(" 2: Context ({}) ", state.context.items.len()),
            context_tab_style,
        ),
        Span::raw(" | Project: "),
        Span::styled(project_name, Style::default().bold()),
        Span::raw("  Branch: "),
        Span::styled(branch, Style::default().fg(Color::Cyan)),
        Span::raw("  HEAD: "),
        Span::styled(head, Style::default().fg(Color::Magenta)),
        dirty_status,
        Span::raw("  | Session: "),
        Span::styled(
            format!("${:.5}", state.model.session_total_cost),
            if state.model.session_total_cost > 0.0 {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::raw("  Last: "),
        Span::styled(
            match state.model.last_op_cost {
                Some(cost) => format!("${:.5}", cost),
                None => "-".to_string(),
            },
            if state.model.last_op_cost.unwrap_or(0.0) > 0.0 {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::raw("  | v"),
        Span::raw(&state.protocol_version),
    ]);

    let header = Paragraph::new(header_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    match state.view_mode {
        ViewMode::Model => {
            let content_height = chunks[1].height.saturating_sub(2);
            state.last_model_height = content_height;

            let model_lines = compute_model_lines(&state.model);
            let total_lines = model_lines.len() as u16;
            let max_scroll = total_lines.saturating_sub(content_height);
            if state.model.scroll > max_scroll {
                state.model.scroll = max_scroll;
            }

            let scroll_indicator = if max_scroll > 0 {
                format!(" [{}/{}]", state.model.scroll + 1, total_lines)
            } else {
                String::new()
            };

            let title = format!(" Model View{} ", scroll_indicator);
            let model_paragraph = Paragraph::new(model_lines)
                .block(Block::default().title(title).borders(Borders::ALL))
                .wrap(Wrap { trim: true })
                .scroll((state.model.scroll, 0));
            frame.render_widget(model_paragraph, chunks[1]);

            let input_line = Line::from(vec![
                Span::styled(" > ", Style::default().fg(Color::Cyan).bold()),
                Span::raw(input_buffer),
            ]);
            let input_paragraph = Paragraph::new(input_line)
                .block(Block::default().title(" Intent ").borders(Borders::ALL));
            frame.render_widget(input_paragraph, chunks[2]);

            let footer_line = Line::from(vec![
                Span::styled(" Ctrl+1/2 ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Switch  "),
                Span::styled(" Enter ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Send  "),
                Span::styled(" [/] ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Files  "),
                Span::styled(" Space ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Fold/Unfold  "),
                Span::styled(" Esc ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Cancel  "),
                Span::styled(" ? ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Help"),
            ]);
            let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
            frame.render_widget(footer, chunks[3]);
        }
        ViewMode::Context => {
            let mut context_lines: Vec<Line> = Vec::new();

            context_lines.push(Line::from(vec![
                Span::raw("Total size: "),
                Span::styled(
                    format!("~{} tokens", state.context.total_estimated_tokens),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::raw(format!(" | Files: {}", state.context.items.len())),
                Span::raw(format!(" | Revision: #{}", state.context.revision)),
            ]));
            context_lines.push(Line::raw(""));

            if state.context.items.is_empty() {
                context_lines.push(Line::from(Span::styled(
                    "No files in context yet. Press 'a' to add files or glob patterns.",
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                for (idx, item) in state.context.items.iter().enumerate() {
                    let is_selected = idx == state.context_view.cursor_index;
                    let cursor_prefix = if is_selected { " ▶ " } else { "   " };

                    let (access_badge, access_style) = match item.access {
                        ContextAccess::Editable => ("[EDITABLE] ", Style::default().fg(Color::Yellow).bold()),
                        ContextAccess::ReadOnly => ("[READ-ONLY]", Style::default().fg(Color::Green)),
                    };

                    let line_style = if is_selected {
                        Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };

                    let item_line = Line::from(vec![
                        Span::styled(cursor_prefix, Style::default().fg(Color::Cyan)).bold(),
                        Span::styled(access_badge, access_style),
                        Span::raw(" "),
                        Span::styled(&item.path, Style::default().bold()),
                        Span::styled(
                            format!("  (~{} tokens, {} B)", item.estimated_tokens, item.size_bytes),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]).style(line_style);

                    context_lines.push(item_line);
                }
            }

            let ctx_paragraph = Paragraph::new(context_lines)
                .block(Block::default().title(" Project Context ").borders(Borders::ALL))
                .wrap(Wrap { trim: false });
            frame.render_widget(ctx_paragraph, chunks[1]);

            let info_line = if let Some(msg) = &state.context_view.status_message {
                Line::from(Span::styled(msg, Style::default().fg(Color::Green).bold()))
            } else {
                Line::from(Span::styled(
                    "Press 'a' to add file or glob pattern (e.g. *.rs, src/), 'e' for editable, 'r' for read-only, 'd'/'x' to remove",
                    Style::default().fg(Color::DarkGray),
                ))
            };
            let prompt_widget = Paragraph::new(info_line)
                .block(Block::default().title(" Context Actions ").borders(Borders::ALL));
            frame.render_widget(prompt_widget, chunks[2]);

            let footer_line = Line::from(vec![
                Span::styled(" Ctrl+1/2 ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Switch  "),
                Span::styled(" a ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Add  "),
                Span::styled(" e ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Editable  "),
                Span::styled(" r ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Read-only  "),
                Span::styled(" d/x ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Remove  "),
                Span::styled(" ? ", Style::default().bg(Color::DarkGray).fg(Color::White).bold()),
                Span::raw(" Help"),
            ]);
            let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
            frame.render_widget(footer, chunks[3]);

            if state.context_view.adding_file {
                render_add_file_picker(frame, state);
            }
        }
    }

    if state.show_help {
        render_help_popup(frame, state.view_mode);
    }
}

fn render_add_file_picker(frame: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(70, 50, frame.area());
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let input_line = Line::from(vec![
        Span::styled(" File / Pattern: ", Style::default().fg(Color::Cyan).bold()),
        Span::raw(&state.context_view.add_input),
        Span::styled("█", Style::default().fg(Color::Yellow)),
    ]);
    let input_block = Paragraph::new(input_line).block(
        Block::default()
            .title(" Add to Context (Path, directory or glob like *.rs) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(input_block, chunks[0]);

    let mut candidate_lines = Vec::new();
    if state.context_view.filtered_candidates.is_empty() {
        candidate_lines.push(Line::from(Span::styled(
            "  No matching files found.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (idx, candidate) in state.context_view.filtered_candidates.iter().enumerate() {
            let is_sel = idx == state.context_view.selected_candidate_index;
            let (prefix, style) = if is_sel {
                (
                    " ▶ ",
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
                )
            } else {
                ("   ", Style::default().fg(Color::Gray))
            };

            let is_pattern_entry = candidate.starts_with("[+] Add all matching '");

            let line = if is_pattern_entry {
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Green).bold()),
                    Span::styled(
                        candidate,
                        if is_sel {
                            Style::default().bg(Color::Green).fg(Color::Black).bold()
                        } else {
                            Style::default().fg(Color::Green).bold()
                        },
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Cyan).bold()),
                    Span::styled(candidate, style),
                ])
            };

            candidate_lines.push(line);
        }
    }

    let list_block = Paragraph::new(candidate_lines).block(
        Block::default()
            .title(" Matching Files / Actions (Enter: Add, Tab: Complete, ↑/↓: Navigate, Esc: Cancel) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(list_block, chunks[1]);
}

fn render_help_popup(frame: &mut ratatui::Frame, mode: ViewMode) {
    let area = centered_rect(65, 55, frame.area());
    frame.render_widget(Clear, area);

    let (title, help_lines) = match mode {
        ViewMode::Model => (
            " Help: Model View ",
            vec![
                Line::from(Span::styled("Global Navigation", Style::default().fg(Color::Cyan).bold())),
                Line::from("  Ctrl+1        Switch directly to Model view"),
                Line::from("  Ctrl+2        Switch directly to Context view (preserves input)"),
                Line::from("  Tab           Switch between Model and Context views (if input empty)"),
                Line::from("  Esc / q       Close help / Cancel operation / Exit"),
                Line::from("  ?             Toggle this help popup"),
                Line::raw(""),
                Line::from(Span::styled("Model & File Review", Style::default().fg(Color::Yellow).bold())),
                Line::from("  Enter         Send prompt / Toggle selected file diff"),
                Line::from("  Space         Fold / Unfold selected file diff"),
                Line::from("  [ / ]         Navigate between modified files"),
                Line::from("  Ctrl+C        Cancel active streaming / thinking"),
                Line::from("  Ctrl+L        Clear conversation history & model view"),
                Line::from("  Ctrl+R        Toggle reasoning / thinking visibility"),
                Line::from("  ↑/↓           Scroll output / Navigate files if input empty"),
                Line::from("  PgUp/PgDn     Scroll output by page"),
            ],
        ),
        ViewMode::Context => (
            " Help: Context View ",
            vec![
                Line::from(Span::styled("Global Navigation", Style::default().fg(Color::Cyan).bold())),
                Line::from("  Ctrl+1        Switch directly to Model view"),
                Line::from("  Ctrl+2        Switch directly to Context view"),
                Line::from("  Tab           Switch between Model and Context views"),
                Line::from("  Esc / q       Back to Model view / Close help"),
                Line::from("  ?             Toggle this help popup"),
                Line::raw(""),
                Line::from(Span::styled("Context Management", Style::default().fg(Color::Yellow).bold())),
                Line::from("  ↑/↓ or k/j    Navigate through context files"),
                Line::from("  a             Open autocomplete file & glob pattern picker"),
                Line::from("  e             Make selected file EDITABLE (write permissions)"),
                Line::from("  r             Make selected file READ-ONLY"),
                Line::from("  d / x / Del   Remove selected file from context"),
                Line::raw(""),
                Line::from(Span::styled("Add File / Pattern Picker", Style::default().fg(Color::Cyan).bold())),
                Line::from("  Type pattern  Filter by substring or glob (e.g. *.rs, src/)"),
                Line::from("  ↑ / ↓         Select matching file or '[+] Add all' action"),
                Line::from("  Tab           Autocomplete path into input"),
                Line::from("  Enter         Add highlighted file or all matching files"),
                Line::from("  Esc           Cancel picker"),
            ],
        ),
    };

    let popup_block = Paragraph::new(help_lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(popup_block, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
