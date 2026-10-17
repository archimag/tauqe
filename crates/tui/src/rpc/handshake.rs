use anyhow::Context;
use tokio::io::{BufReader, Lines};
use tokio::process::ChildStdout;
use tauqe_protocol::{
    methods, ContextState, Event, InitializeParams, InitializeResult, Message,
    RepositoryListFilesResult, Request, RequestId, Response, SystemStatusResult, PROTOCOL_VERSION,
};

use super::transport::RpcConnection;
use crate::app::{OnboardingState, ViewMode};

/// Everything the client needs after the initialize handshake: the negotiated
/// protocol result plus the initial context, file list, onboarding state, and any
/// events buffered while the handshake responses were awaited.
pub struct InitState {
    pub handshake: InitializeResult,
    pub context: ContextState,
    pub files: Vec<String>,
    pub onboarding: OnboardingState,
    pub view_mode: ViewMode,
    pub events: Vec<Event>,
}

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
    conn: &mut RpcConnection,
    reader: &mut Lines<BufReader<ChildStdout>>,
) -> anyhow::Result<InitState> {
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
    conn.write_line(&serde_json::to_string(&init_req)?).await?;

    let response = read_response_for_id(reader, 1, &mut buffered_events).await?;

    let handshake: InitializeResult = match response.result {
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
    conn.write_line(&serde_json::to_string(&ctx_req)?).await?;

    let context = match read_response_for_id(reader, 2, &mut buffered_events).await {
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
    conn.write_line(&serde_json::to_string(&list_req)?).await?;

    let files = match read_response_for_id(reader, 3, &mut buffered_events).await {
        Ok(resp) => resp
            .result
            .and_then(|v| serde_json::from_value::<RepositoryListFilesResult>(v).ok())
            .map(|r| r.files)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    // 4. Query system status for the onboarding check
    let mut view_mode = ViewMode::Repl;
    let mut onboarding = OnboardingState::default();

    let status_req = Request {
        id: RequestId::Number(4),
        method: methods::SYSTEM_STATUS.to_string(),
        params: None,
    };
    conn.write_line(&serde_json::to_string(&status_req)?).await?;

    if let Ok(resp) = read_response_for_id(reader, 4, &mut buffered_events).await {
        if let Some(val) = resp.result {
            if let Ok(status) = serde_json::from_value::<SystemStatusResult>(val) {
                onboarding.apply_status(&status);
                if !status.has_git || !status.has_config || !status.has_api_key {
                    view_mode = ViewMode::Onboarding;
                    onboarding.step = onboarding.next_step();
                }
            }
        }
    }

    Ok(InitState {
        handshake,
        context,
        files,
        onboarding,
        view_mode,
        events: buffered_events,
    })
}
