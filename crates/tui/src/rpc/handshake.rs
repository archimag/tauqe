use anyhow::Context;
use tauqe_protocol::{
    methods, ContextState, Event, InitializeParams, InitializeResult, Message,
    RepositoryListFilesResult, Request, RequestId, Response, SystemStatusResult, PROTOCOL_VERSION,
};
use tokio::io::{BufReader, Lines};
use tokio::process::ChildStdout;

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
        if let Some(response) = classify_handshake_line(&line, expected_id, buffered_events)? {
            return Ok(response);
        }
    }
}

/// Rejects a server that negotiated a protocol version other than this
/// client's.
///
/// The client declares [`PROTOCOL_VERSION`] in its initialization request and
/// the server answers with the version it will speak. A mismatch means the two
/// binaries were built from different wire contracts, so every later request
/// and event could be decoded differently on either side. The connection fails
/// here instead of running a protocol neither peer agreed on.
fn ensure_protocol_compatibility(server: &str) -> anyhow::Result<()> {
    if PROTOCOL_VERSION == server {
        return Ok(());
    }
    anyhow::bail!(
        "Protocol version mismatch: this client speaks '{PROTOCOL_VERSION}' but the server negotiated '{server}'. Rebuild the client and the server from the same revision."
    )
}

/// Classifies one line received while the handshake awaits a single response.
///
/// The handshake is strictly sequential, so exactly one request is in flight
/// and a response carrying any other identifier can only be a desynchronized
/// stream. Events are buffered for the caller to dispatch once the handshake
/// completes, a server-initiated request is ignored because the daemon never
/// calls the client, and a line that is not a valid wire message fails the
/// handshake instead of being dropped silently.
///
/// Returns the awaited response, or `None` when the line carried no answer.
fn classify_handshake_line(
    line: &str,
    expected_id: u64,
    buffered_events: &mut Vec<Event>,
) -> anyhow::Result<Option<Response>> {
    match serde_json::from_str::<Message>(line) {
        Ok(Message::Response(response)) => {
            if response.id == RequestId::Number(expected_id) {
                Ok(Some(response))
            } else {
                anyhow::bail!(
                    "Server answered request {:?} while the handshake awaited request {expected_id}",
                    response.id
                )
            }
        }
        Ok(Message::Event(event)) => {
            buffered_events.push(event);
            Ok(None)
        }
        Ok(Message::Request(_)) => Ok(None),
        Err(err) => anyhow::bail!("Server sent a malformed message during initialization: {err}"),
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

    ensure_protocol_compatibility(&handshake.protocol_version)?;

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
    conn.write_line(&serde_json::to_string(&status_req)?)
        .await?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn response_line(id: u64) -> String {
        serde_json::to_string(&Message::Response(Response::ok(
            RequestId::Number(id),
            serde_json::json!({ "ok": true }),
        )))
        .expect("a response serializes")
    }

    #[test]
    fn matching_protocol_version_is_accepted() {
        assert!(ensure_protocol_compatibility(PROTOCOL_VERSION).is_ok());
    }

    /// A mismatch must name both versions, so the developer can tell which
    /// binary is stale instead of reading a generic connection failure.
    #[test]
    fn mismatched_protocol_version_names_both_versions() {
        let error = ensure_protocol_compatibility("server-v2")
            .expect_err("a version mismatch must fail the handshake");
        let rendered = error.to_string();
        assert!(rendered.contains(PROTOCOL_VERSION));
        assert!(rendered.contains("server-v2"));
    }

    #[test]
    fn awaited_response_is_returned_and_events_are_buffered() {
        let mut buffered = Vec::new();

        let event_line = serde_json::to_string(&Message::Event(Event {
            method: "model/delta".to_string(),
            params: None,
        }))
        .expect("an event serializes");
        let event_outcome = classify_handshake_line(&event_line, 7, &mut buffered)
            .expect("an event is a valid handshake line");
        assert!(event_outcome.is_none());
        assert_eq!(buffered.len(), 1);

        let response = classify_handshake_line(&response_line(7), 7, &mut buffered)
            .expect("the awaited response is a valid handshake line")
            .expect("the awaited response is returned");
        assert_eq!(response.id, RequestId::Number(7));
    }

    #[test]
    fn malformed_line_fails_instead_of_being_dropped() {
        let mut buffered = Vec::new();
        let error = classify_handshake_line("not a wire message", 1, &mut buffered)
            .expect_err("a malformed line must fail the handshake");
        assert!(error.to_string().contains("malformed"));
        assert!(buffered.is_empty());
    }

    #[test]
    fn response_for_another_request_fails_instead_of_being_dropped() {
        let mut buffered = Vec::new();
        let error = classify_handshake_line(&response_line(99), 1, &mut buffered)
            .expect_err("a foreign response must fail the handshake");
        assert!(error.to_string().contains("99"));
    }

    #[test]
    fn server_initiated_request_is_ignored() {
        let mut buffered = Vec::new();
        let request_line = serde_json::to_string(&Message::Request(Request {
            id: RequestId::Number(5),
            method: "client/ping".to_string(),
            params: None,
        }))
        .expect("a request serializes");
        let outcome = classify_handshake_line(&request_line, 1, &mut buffered)
            .expect("a server request is valid handshake input");
        assert!(outcome.is_none());
        assert!(buffered.is_empty());
    }
}
