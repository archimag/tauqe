use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, Mutex};
use tauqe_protocol::{
    events, methods, ConfigState, ContextState, EditFileDoneEvent, EditFileRetryingEvent,
    EditFileStartedEvent, EditFinishedEvent, EditHunkEvent, Event, GitCommitCreatedEvent,
    GitSquashApplyResult, GitSquashGenerateMessageResult, GitSquashPreviewResult, GitUndoResult,
    HistoryEntryAddedEvent, HistoryGetResult, InitializeParams, InitializeResult, Message,
    ModelDeltaEvent, ModelErrorEvent, ModelFinishedEvent, ModelRef, ModelResultEvent,
    ModelStartedEvent, ModelUsageEvent, RepositoryListFilesResult, RepositoryState, Request,
    RequestId, Response, ReviewContentDeltaEvent, ReviewErrorEvent, ReviewFinishedEvent,
    ReviewGetResult, ReviewReasoningDeltaEvent, ReviewUpdateItemResult, PROTOCOL_VERSION,
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(10);
static PENDING_REQUESTS: std::sync::Mutex<Option<std::collections::HashMap<u64, String>>> =
    std::sync::Mutex::new(None);

pub fn record_pending_request(id: u64, method: &str) {
    if let Ok(mut lock) = PENDING_REQUESTS.lock() {
        lock.get_or_insert_with(std::collections::HashMap::new)
            .insert(id, method.to_string());
    }
}

pub fn take_pending_request(id: u64) -> Option<String> {
    if let Ok(mut lock) = PENDING_REQUESTS.lock() {
        lock.as_mut().and_then(|m| m.remove(&id))
    } else {
        None
    }
}

use crate::app::{AppState, OnboardingStep, ViewMode};
use crate::ui::develop::{DevelopView, StreamingFileEdit, StreamingHunk};

pub fn find_server_binary() -> PathBuf {
    let exe_name = if cfg!(windows) {
        "tauqe-server.exe"
    } else {
        "tauqe-server"
    };

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let candidate = parent.join(exe_name);
            if candidate.exists() {
                return candidate;
            }
        }
    }

    PathBuf::from(exe_name)
}

pub async fn start_server() -> anyhow::Result<(Child, ChildStdin, Lines<BufReader<ChildStdout>>)> {
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

    let reader = BufReader::new(child_stdout).lines();
    Ok((server_child, child_stdin, reader))
}

pub async fn initialize_connection(
    writer: &mut ChildStdin,
    reader: &mut Lines<BufReader<ChildStdout>>,
) -> anyhow::Result<(
    InitializeResult,
    ContextState,
    Vec<String>,
    crate::app::OnboardingState,
    ViewMode,
)> {
    // 1. Handshake: client/initialize
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
    writer.write_all(req_str.as_bytes()).await?;
    writer.flush().await?;

    let init_resp_line = reader
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

    // 2. Initial context fetch
    let ctx_req = Request {
        id: RequestId::Number(2),
        method: methods::CONTEXT_GET.to_string(),
        params: None,
    };
    let mut ctx_req_str = serde_json::to_string(&ctx_req)?;
    ctx_req_str.push('\n');
    writer.write_all(ctx_req_str.as_bytes()).await?;
    writer.flush().await?;

    let initial_context = match reader.next_line().await {
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

    // 3. Initial repository file list fetch
    let list_req = Request {
        id: RequestId::Number(3),
        method: methods::REPOSITORY_LIST_FILES.to_string(),
        params: None,
    };
    let mut list_req_str = serde_json::to_string(&list_req)?;
    list_req_str.push('\n');
    writer.write_all(list_req_str.as_bytes()).await?;
    writer.flush().await?;

    let initial_files = match reader.next_line().await {
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

    let mut initial_view_mode = ViewMode::Develop;
    let mut onboarding_state = crate::app::OnboardingState::default();

    // 4. Query system status for onboarding check
    let status_req = Request {
        id: RequestId::Number(4),
        method: "system/status".to_string(),
        params: None,
    };
    let mut status_req_str = serde_json::to_string(&status_req)?;
    status_req_str.push('\n');
    writer.write_all(status_req_str.as_bytes()).await?;
    writer.flush().await?;

    if let Ok(Some(line)) = reader.next_line().await {
        if let Ok(resp) = serde_json::from_str::<Response>(&line) {
            if let Some(val) = resp.result {
                let has_git = val.get("has_git").and_then(|v| v.as_bool()).unwrap_or(true);
                let has_config = val.get("has_config").and_then(|v| v.as_bool()).unwrap_or(false);
                let has_api_key = val.get("has_api_key").and_then(|v| v.as_bool()).unwrap_or(false);
                let config_path = val.get("config_path").and_then(|v| v.as_str()).map(|s| s.to_string());
                let credentials_path = val.get("credentials_path").and_then(|v| v.as_str()).map(|s| s.to_string());
                let default_cfg = val.get("default_config_path").and_then(|v| v.as_str()).unwrap_or("tauqe.toml").to_string();
                let default_creds = val.get("default_credentials_path").and_then(|v| v.as_str()).unwrap_or("~/.config/tauqe/credentials.toml").to_string();
                let model = val
                    .get("model")
                    .and_then(|v| serde_json::from_value::<ModelRef>(v.clone()).ok())
                    .map(|m| m.name)
                    .unwrap_or_else(|| "anthropic/claude-3.7-sonnet".to_string());

                onboarding_state.has_git = has_git;
                onboarding_state.has_config = has_config;
                onboarding_state.has_api_key = has_api_key;
                onboarding_state.config_path = config_path;
                onboarding_state.credentials_path = credentials_path;
                onboarding_state.default_config_path = default_cfg;
                onboarding_state.default_credentials_path = default_creds;
                onboarding_state.selected_model = model;

                if !has_git || !has_config || !has_api_key {
                    initial_view_mode = ViewMode::Onboarding;
                    if !has_git {
                        onboarding_state.step = OnboardingStep::Git;
                    } else if !has_config {
                        onboarding_state.step = OnboardingStep::Config;
                    } else {
                        onboarding_state.step = OnboardingStep::Credentials;
                    }
                }
            }
        }
    }

    Ok((
        init_result,
        initial_context,
        initial_files,
        onboarding_state,
        initial_view_mode,
    ))
}

pub fn spawn_message_reader(mut reader: Lines<BufReader<ChildStdout>>) -> mpsc::Receiver<Message> {
    let (msg_tx, msg_rx) = mpsc::channel::<Message>(100);
    tokio::spawn(async move {
        loop {
            match reader.next_line().await {
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
    msg_rx
}

pub async fn send_request(
    writer: &mut ChildStdin,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<()> {
    let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    record_pending_request(id, method);
    let req = Request {
        id: RequestId::Number(id),
        method: method.to_string(),
        params: Some(params),
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

pub fn fail_safe_reject_edits(model: &mut DevelopView, reason: &str) {
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

fn apply_system_status_response(st: &mut AppState, val: &serde_json::Value) {
    let ready = val.get("ready").and_then(|v| v.as_bool()).unwrap_or(false);
    let has_git = val.get("has_git").and_then(|v| v.as_bool()).unwrap_or(true);
    let has_config = val.get("has_config").and_then(|v| v.as_bool()).unwrap_or(false);
    let has_api_key = val.get("has_api_key").and_then(|v| v.as_bool()).unwrap_or(false);
    let config_path = val.get("config_path").and_then(|v| v.as_str()).map(|s| s.to_string());
    let credentials_path = val
        .get("credentials_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(m) = val
        .get("model")
        .and_then(|v| serde_json::from_value::<ModelRef>(v.clone()).ok())
    {
        st.onboarding.selected_model = m.name.clone();
        st.active_model = m;
    }
    if let Some(av) = val
        .get("available_models")
        .and_then(|v| serde_json::from_value::<Vec<ModelRef>>(v.clone()).ok())
    {
        if !av.is_empty() {
            st.available_models = av;
        }
    }

    st.onboarding.has_git = has_git;
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
        st.onboarding.status_message =
            Some("Ключ OpenRouter успешно сохранён.".to_string());
        st.onboarding.step = OnboardingStep::Ready;
        st.onboarding.selected_index = 0;
    } else if val.get("reloaded").and_then(|v| v.as_bool()).unwrap_or(false) {
        if ready {
            st.onboarding.status_message = Some(
                "Конфигурация перезагружена. Все проверки пройдены!".to_string(),
            );
            if st.view_mode == ViewMode::Onboarding {
                st.onboarding.step = OnboardingStep::Ready;
                st.onboarding.selected_index = 0;
            }
        } else {
            st.onboarding.error_message = Some(
                "Ключ по-прежнему не обнаружен. Проверьте переменную OPENROUTER_API_KEY или файл credentials.toml".to_string(),
            );
        }
    }
}

fn apply_fallback_response(st: &mut AppState, val: &serde_json::Value) {
    if val.get("squashed_commit").is_some() {
        if let Ok(applied) = serde_json::from_value::<GitSquashApplyResult>(val.clone()) {
            st.squash_dialog = None;
            let first_line = applied.message.lines().next().unwrap_or("Squashed commit");
            st.model.git_notification = Some(format!(
                "Squashed commits into {} ('{}')",
                applied.squashed_commit, first_line
            ));
            st.model.last_commit_hash = Some(applied.squashed_commit);
            st.model.last_commit_summary = Some(first_line.to_string());
        }
    } else if val.get("diff_stat").is_some() {
        if let Ok(preview) = serde_json::from_value::<GitSquashPreviewResult>(val.clone()) {
            if let Some(ref mut dialog) = st.squash_dialog {
                dialog.loading = false;
                dialog.base_ref = preview.base_ref;
                dialog.session_base = preview.session_base;
                dialog.upstream_base = preview.upstream_base;
                dialog.commits = preview.commits;
                dialog.diff_stat = preview.diff_stat;
                dialog.files = preview
                    .files
                    .into_iter()
                    .map(|f| crate::app::SquashFileItem {
                        path: f.path,
                        diff: f.diff,
                        expanded: true,
                    })
                    .collect();
                dialog.selected_file_index = 0;
                dialog.diff_scroll = 0;
                if dialog.message_buffer.is_empty() {
                    if let Some(msg) = preview.suggested_message {
                        dialog.message_buffer = msg;
                    }
                }
                dialog.status_message = None;
            }
        }
    } else if val.get("message").is_some()
        && st
            .squash_dialog
            .as_ref()
            .map(|d| d.generating_message)
            .unwrap_or(false)
    {
        if let Ok(gen_res) = serde_json::from_value::<GitSquashGenerateMessageResult>(val.clone()) {
            if let Some(ref mut dialog) = st.squash_dialog {
                dialog.generating_message = false;
                dialog.message_buffer = gen_res.message;
                dialog.focus = crate::app::SquashDialogFocus::MessageEditor;
                dialog.status_message = Some(
                    "Message generated via AI. Review or edit, then press Enter to apply."
                        .to_string(),
                );
            }
        }
    } else if val.get("has_more").is_some() && val.get("total_count").is_some() {
        if let Ok(history_res) = serde_json::from_value::<HistoryGetResult>(val.clone()) {
            st.history_view.loading = false;
            st.history_view.has_more = history_res.has_more;
            st.history_view.total_count = history_res.total_count;

            if st.history_view.pending_before_id.take().is_some()
                && !st.history_view.items.is_empty()
            {
                let added_lines = crate::ui::history::compute_history_items_line_count(
                    &history_res.items,
                    None,
                );
                let mut combined = history_res.items;
                combined.append(&mut st.history_view.items);
                st.history_view.items = combined;
                st.history_view.scroll =
                    st.history_view.scroll.saturating_add(added_lines as u16);
            } else {
                st.history_view.items = history_res.items;
                st.history_view.auto_scroll = true;
            }
        }
    } else if val.get("added_tokens").is_some() {
        if let Ok(pattern_res) =
            serde_json::from_value::<tauqe_protocol::ContextAddPatternResult>(val.clone())
        {
            st.context = pattern_res.state;
            st.context_view.status_message = Some(format!(
                "Added {} files (~{} tokens)",
                pattern_res.added_count, pattern_res.added_tokens
            ));
            if st.context_view.adding_file {
                st.update_filtered_candidates();
            }
        }
    } else if val.get("files").is_some() && val.get("total_estimated_tokens").is_none() {
        if let Ok(file_res) = serde_json::from_value::<RepositoryListFilesResult>(val.clone()) {
            st.all_repo_files = file_res.files;
            if st.context_view.adding_file {
                st.update_filtered_candidates();
            }
        }
    } else if val.get("total_estimated_tokens").is_some() {
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
    } else if val.get("initial_commit").is_some() {
        if let Ok(init_res) =
            serde_json::from_value::<tauqe_protocol::RepositoryInitResult>(val.clone())
        {
            st.repo_state = Some(init_res.repository);
            st.onboarding.has_git = true;
            st.onboarding.status_message =
                Some("Git-репозиторий успешно инициализирован.".to_string());
            if !st.onboarding.has_config {
                st.onboarding.step = OnboardingStep::Config;
                st.onboarding.selected_index = 0;
            } else if !st.onboarding.has_api_key {
                st.onboarding.step = OnboardingStep::Credentials;
                st.onboarding.selected_index = 0;
            } else {
                st.onboarding.step = OnboardingStep::Ready;
                st.onboarding.selected_index = 0;
            }
        }
    } else if val.get("workflow").is_some() && val.get("model").is_some() {
        if let Ok(cfg) = serde_json::from_value::<ConfigState>(val.clone()) {
            st.workflow = cfg.workflow;
            st.active_model = cfg.model;
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
    } else if val.get("undone_commit").is_some()
        || val.get("reverted_commit").is_some()
        || (val.get("success").is_some() && val.get("message").is_some())
    {
        if let Ok(undo_res) = serde_json::from_value::<GitUndoResult>(val.clone()) {
            st.model.git_notification = Some(undo_res.message.clone());
            st.model.last_commit_hash = None;
            st.model.last_commit_summary = None;
            st.model.edit_final_applied = None;
            st.model.files.clear();
        }
    } else if val.get("ready").is_some() {
        apply_system_status_response(st, val);
    }
}

pub async fn handle_response(resp: Response, state: &Arc<Mutex<AppState>>) {
    let method = match &resp.id {
        RequestId::Number(id) => take_pending_request(*id),
        _ => None,
    };

    let mut st = state.lock().await;
    if let Some(err) = resp.error {
        if method.as_deref() == Some(methods::REVIEW_START) {
            st.review.running = false;
            st.review.error = Some(err.message);
            return;
        }
        if let Some(ref mut dialog) = st.squash_dialog {
            dialog.loading = false;
            dialog.status_message = Some(format!("Error: {}", err.message));
        } else {
            st.context_view.status_message = Some(format!("Error: {}", err.message));
            if st.view_mode == ViewMode::Develop {
                st.model.git_notification = Some(format!("Error: {}", err.message));
            }
        }
        return;
    }

    let Some(val) = resp.result else {
        return;
    };

    if let Some(method_name) = method.as_deref() {
        match method_name {
            methods::GIT_SQUASH_APPLY => {
                if let Ok(applied) = serde_json::from_value::<GitSquashApplyResult>(val.clone()) {
                    st.squash_dialog = None;
                    let first_line = applied.message.lines().next().unwrap_or("Squashed commit");
                    st.model.git_notification = Some(format!(
                        "Squashed commits into {} ('{}')",
                        applied.squashed_commit, first_line
                    ));
                    st.model.last_commit_hash = Some(applied.squashed_commit);
                    st.model.last_commit_summary = Some(first_line.to_string());
                }
                return;
            }
            methods::GIT_SQUASH_PREVIEW => {
                if let Ok(preview) = serde_json::from_value::<GitSquashPreviewResult>(val.clone()) {
                    if let Some(ref mut dialog) = st.squash_dialog {
                        dialog.loading = false;
                        dialog.base_ref = preview.base_ref;
                        dialog.session_base = preview.session_base;
                        dialog.upstream_base = preview.upstream_base;
                        dialog.commits = preview.commits;
                        dialog.diff_stat = preview.diff_stat;
                        dialog.files = preview
                            .files
                            .into_iter()
                            .map(|f| crate::app::SquashFileItem {
                                path: f.path,
                                diff: f.diff,
                                expanded: true,
                            })
                            .collect();
                        dialog.selected_file_index = 0;
                        dialog.diff_scroll = 0;
                        if dialog.message_buffer.is_empty() {
                            if let Some(msg) = preview.suggested_message {
                                dialog.message_buffer = msg;
                            }
                        }
                        dialog.status_message = None;
                    }
                }
                return;
            }
            methods::GIT_SQUASH_GENERATE_MESSAGE => {
                if let Ok(gen_res) = serde_json::from_value::<GitSquashGenerateMessageResult>(val.clone()) {
                    if let Some(ref mut dialog) = st.squash_dialog {
                        dialog.generating_message = false;
                        dialog.message_buffer = gen_res.message;
                        dialog.focus = crate::app::SquashDialogFocus::MessageEditor;
                        dialog.status_message = Some(
                            "Message generated via AI. Review or edit, then press Enter to apply."
                                .to_string(),
                        );
                    }
                }
                return;
            }
            methods::GIT_UNDO => {
                if let Ok(undo_res) = serde_json::from_value::<GitUndoResult>(val.clone()) {
                    st.model.git_notification = Some(undo_res.message.clone());
                    st.model.last_commit_hash = None;
                    st.model.last_commit_summary = None;
                    st.model.edit_final_applied = None;
                    st.model.files.clear();
                }
                return;
            }
            methods::HISTORY_GET => {
                if let Ok(history_res) = serde_json::from_value::<HistoryGetResult>(val.clone()) {
                    st.history_view.loading = false;
                    st.history_view.has_more = history_res.has_more;
                    st.history_view.total_count = history_res.total_count;

                    if st.history_view.pending_before_id.take().is_some()
                        && !st.history_view.items.is_empty()
                    {
                        let added_lines = crate::ui::history::compute_history_items_line_count(
                            &history_res.items,
                            None,
                        );
                        let mut combined = history_res.items;
                        combined.append(&mut st.history_view.items);
                        st.history_view.items = combined;
                        st.history_view.scroll =
                            st.history_view.scroll.saturating_add(added_lines as u16);
                    } else {
                        st.history_view.items = history_res.items;
                        st.history_view.auto_scroll = true;
                    }
                }
                return;
            }
            methods::CONTEXT_ADD_PATTERN => {
                if let Ok(pattern_res) =
                    serde_json::from_value::<tauqe_protocol::ContextAddPatternResult>(val.clone())
                {
                    st.context = pattern_res.state;
                    st.context_view.status_message = Some(format!(
                        "Added {} files (~{} tokens)",
                        pattern_res.added_count, pattern_res.added_tokens
                    ));
                    if st.context_view.adding_file {
                        st.update_filtered_candidates();
                    }
                }
                return;
            }
            methods::REPOSITORY_LIST_FILES => {
                if let Ok(file_res) = serde_json::from_value::<RepositoryListFilesResult>(val.clone()) {
                    st.all_repo_files = file_res.files;
                    if st.context_view.adding_file {
                        st.update_filtered_candidates();
                    }
                }
                return;
            }
            methods::CONTEXT_GET
            | methods::CONTEXT_ADD
            | methods::CONTEXT_REMOVE
            | methods::CONTEXT_SET_ACCESS
            | methods::CONTEXT_CLEAR => {
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
                return;
            }
            methods::REPOSITORY_INIT => {
                if let Ok(init_res) =
                    serde_json::from_value::<tauqe_protocol::RepositoryInitResult>(val.clone())
                {
                    st.repo_state = Some(init_res.repository);
                    st.onboarding.has_git = true;
                    st.onboarding.status_message =
                        Some("Git-репозиторий успешно инициализирован.".to_string());
                    if !st.onboarding.has_config {
                        st.onboarding.step = OnboardingStep::Config;
                        st.onboarding.selected_index = 0;
                    } else if !st.onboarding.has_api_key {
                        st.onboarding.step = OnboardingStep::Credentials;
                        st.onboarding.selected_index = 0;
                    } else {
                        st.onboarding.step = OnboardingStep::Ready;
                        st.onboarding.selected_index = 0;
                    }
                }
                return;
            }
            methods::CONFIG_GET | methods::CONFIG_SET => {
                if let Ok(cfg) = serde_json::from_value::<ConfigState>(val.clone()) {
                    st.workflow = cfg.workflow;
                    st.active_model = cfg.model;
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
                return;
            }
            methods::REVIEW_GET => {
                if let Ok(res) = serde_json::from_value::<ReviewGetResult>(val.clone()) {
                    if let Some(session) = res.session {
                        st.review.set_session(session);
                    }
                }
                return;
            }
            methods::REVIEW_UPDATE_ITEM => {
                if let Ok(res) = serde_json::from_value::<ReviewUpdateItemResult>(val.clone()) {
                    st.review.apply_item(res.item);
                }
                return;
            }
            methods::REVIEW_START | methods::REVIEW_CANCEL => {
                return;
            }
            methods::SYSTEM_STATUS
            | methods::CONFIG_RELOAD
            | methods::CONFIG_CREATE
            | methods::CREDENTIALS_SAVE
            | methods::CREDENTIALS_CREATE_STUB => {
                apply_system_status_response(&mut st, &val);
                return;
            }
            _ => {}
        }
    }

    apply_fallback_response(&mut st, &val);
}

pub async fn handle_event(ev: Event, state: &Arc<Mutex<AppState>>, is_reasoning: &mut bool) {
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
                        let total_lines = crate::ui::history::compute_history_items_line_count(&st.history_view.items, None) as u16;
                        st.history_view.scroll = total_lines.saturating_sub(view_height);
                    }
                }
            }
        }
        events::CONFIG_CHANGED => {
            if let Some(params) = ev.params {
                if let Ok(cfg) = serde_json::from_value::<ConfigState>(params) {
                    st.workflow = cfg.workflow;
                    st.active_model = cfg.model;
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
                }
            }
        }
        events::MODEL_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelDeltaEvent>(params) {
                    if !data.delta.is_empty() {
                        st.model.status = "thinking".to_string();
                        if !*is_reasoning {
                            *is_reasoning = true;
                            st.model.reasoning.show = true;
                            if !st.model.reasoning.is_empty() && !st.model.reasoning.text.ends_with("\n\n") {
                                st.model.reasoning.text.push_str("\n\n---\n\n");
                            }
                            let h = st.last_model_height;
                            st.model.clamp_scroll(h);
                        }
                    }
                    st.model.reasoning.append_delta(&data.delta);
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
                        if st.model.edits_active {
                            st.model.status = "editing".to_string();
                        } else {
                            st.model.status = "responding".to_string();
                        }
                        if *is_reasoning {
                            *is_reasoning = false;
                            st.model.reasoning.show = false;
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
            st.model.edits_active = true;
            st.model.status = "editing".to_string();
        }
        events::EDIT_FILE_STARTED => {
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
            events::MODEL_USAGE => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ModelUsageEvent>(params) {
                    st.model.session_total_cost = data.session_total_cost;
                    if let Some(curr) = data.current_cost {
                        st.model.current_cost = Some(curr);
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
                        if let Some(curr) = data.current_cost {
                            st.model.current_cost = Some(curr);
                        }
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
        events::REVIEW_STARTED => {
            st.review.running = true;
        }
        events::REVIEW_REASONING_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewReasoningDeltaEvent>(params) {
                    st.review.reasoning.append_delta(&data.delta);
                }
            }
        }
        events::REVIEW_CONTENT_DELTA => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewContentDeltaEvent>(params) {
                    st.review.content.push_str(&data.delta);
                }
            }
        }
        events::REVIEW_FINISHED => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewFinishedEvent>(params) {
                    if let Some(total) = data.session_total_cost {
                        st.model.session_total_cost = total;
                    }
                    if let Some(cost) = data.current_cost {
                        st.model.prev_cost = Some(cost);
                    }
                    st.review.set_session(data.session);
                }
            }
        }
        events::REVIEW_CANCELLED => {
            st.review.running = false;
            st.review.error = Some("Review cancelled".to_string());
        }
        events::REVIEW_ERROR => {
            if let Some(params) = ev.params {
                if let Ok(data) = serde_json::from_value::<ReviewErrorEvent>(params) {
                    st.review.running = false;
                    st.review.error = Some(data.message);
                }
            }
        }
        _ => {}
    }
}
