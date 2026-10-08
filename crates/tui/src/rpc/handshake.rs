use anyhow::Context;
use tokio::io::{AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};
use tauqe_protocol::{
    methods, ContextState, Event, InitializeParams, InitializeResult, Message,
    RepositoryListFilesResult, Request, RequestId, Response, PROTOCOL_VERSION,
};

use crate::app::{OnboardingState, OnboardingStep, ViewMode};

pub async fn read_response_for_id(
    reader: &mut Lines<BufReader<ChildStdout>>,
    expected_id: u64,
    buffered_events: &mut Vec<Event>,
) -> anyhow::Result<Response> {
    loop {
        let line = reader
            .next_line()
            .await?
            .context("Server closed connection during initialization")?;
        match serde_json::from_str::<Message>(&line) {
            Ok(Message::Response(resp)) => {
                if resp.id == RequestId::Number(expected_id) {
                    return Ok(resp);
                }
            }
            Ok(Message::Event(ev)) => {
                buffered_events.push(ev);
            }
            Ok(Message::Request(_)) => {}
            Err(_) => {}
        }
    }
}

pub async fn initialize_connection(
    writer: &mut ChildStdin,
    reader: &mut Lines<BufReader<ChildStdout>>,
) -> anyhow::Result<(
    InitializeResult,
    ContextState,
    Vec<String>,
    OnboardingState,
    ViewMode,
    Vec<Event>,
)> {
    let mut buffered_events = Vec::new();

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

    let response = read_response_for_id(reader, 1, &mut buffered_events).await?;

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

    let initial_context = match read_response_for_id(reader, 2, &mut buffered_events).await {
        Ok(resp) => resp
            .result
            .and_then(|v| serde_json::from_value::<ContextState>(v).ok())
            .unwrap_or_default(),
        Err(_) => ContextState::default(),
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

    let initial_files = match read_response_for_id(reader, 3, &mut buffered_events).await {
        Ok(resp) => resp
            .result
            .and_then(|v| serde_json::from_value::<RepositoryListFilesResult>(v).ok())
            .map(|r| r.files)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    let mut initial_view_mode = ViewMode::Develop;
    let mut onboarding_state = OnboardingState::default();

    // 4. Query system status for onboarding check
    let status_req = Request {
        id: RequestId::Number(4),
        method: methods::SYSTEM_STATUS.to_string(),
        params: None,
    };
    let mut status_req_str = serde_json::to_string(&status_req)?;
    status_req_str.push('\n');
    writer.write_all(status_req_str.as_bytes()).await?;
    writer.flush().await?;

    if let Ok(resp) = read_response_for_id(reader, 4, &mut buffered_events).await {
        if let Some(val) = resp.result {
            let has_git = val.get("has_git").and_then(|v| v.as_bool()).unwrap_or(true);
            let has_config = val.get("has_config").and_then(|v| v.as_bool()).unwrap_or(false);
            let has_api_key = val.get("has_api_key").and_then(|v| v.as_bool()).unwrap_or(false);
            let config_path = val.get("config_path").and_then(|v| v.as_str()).map(|s| s.to_string());
            let credentials_path = val.get("credentials_path").and_then(|v| v.as_str()).map(|s| s.to_string());
            let default_cfg = val.get("default_config_path").and_then(|v| v.as_str()).unwrap_or("tauqe.toml").to_string();
            let default_creds = val.get("default_credentials_path").and_then(|v| v.as_str()).unwrap_or("~/.config/tauqe/credentials.toml").to_string();
            onboarding_state.has_git = has_git;
            onboarding_state.has_config = has_config;
            onboarding_state.has_api_key = has_api_key;
            onboarding_state.config_path = config_path;
            onboarding_state.credentials_path = credentials_path;
            onboarding_state.default_config_path = default_cfg;
            onboarding_state.default_credentials_path = default_creds;

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

    Ok((
        init_result,
        initial_context,
        initial_files,
        onboarding_state,
        initial_view_mode,
        buffered_events,
    ))
}
