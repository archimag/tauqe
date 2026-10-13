#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tauqe_protocol::{
    errors, events, methods, Event, HistoryEntryAddedEvent, Request, RequestId, Response,
    UiHistoryItem,
};

mod handlers;
mod state;
mod transport;

use handlers::handle_request;
use state::AppState;
use transport::OutChannel;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tauqe_core::init();

    let explicit_config = tauqe_core::config::cli_arg_value_from(std::env::args(), "--config");
    let explicit_credentials =
        tauqe_core::config::cli_arg_value_from(std::env::args(), "--credentials");
    let repo_state = tauqe_core::git::get_repository_state(None);
    let repo_path = PathBuf::from(&repo_state.root);
    let config = tauqe_core::config::load_config_with_options(tauqe_core::config::ConfigLoadOptions {
        repo_root: Some(&repo_path),
        explicit_config: explicit_config.as_deref(),
        explicit_credentials: explicit_credentials.as_deref(),
    });

    let (history_tx, mut history_rx) = tokio::sync::mpsc::unbounded_channel::<UiHistoryItem>();
    let (out, mut out_rx) = OutChannel::new();

    let state = AppState::new(config, repo_path, history_tx, out);

    // Forward UI history items to out channel as events
    let out_for_history = state.out.clone();
    let state_for_history = Arc::clone(&state);
    tokio::spawn(async move {
        while let Some(item) = history_rx.recv().await {
            let estimated_tokens = {
                let h = state_for_history.history.lock().await;
                h.estimated_tokens().ok()
            };
            let ev = Event {
                method: events::HISTORY_ENTRY_ADDED.to_string(),
                params: serde_json::to_value(HistoryEntryAddedEvent {
                    item,
                    estimated_tokens,
                })
                .ok(),
            };
            out_for_history.send_event(&ev);
        }
    });

    // Single dedicated stdout writer task guaranteeing atomic line output without interleaving
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();

    // Requests whose handlers block on long-running work (LLM turns, the
    // deterministic pre-flight verification) are offloaded so the read loop
    // keeps draining stdin and can observe cancels and EOF. Every other request
    // is handled inline, which serializes state mutations in the exact order
    // the client sent them and keeps responses ordered.
    let mut offloaded = tokio::task::JoinSet::new();

    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(err) => {
                // A request that fails to parse has no correlatable id: answer
                // with the explicit null id so it can never collide with a real
                // request's id.
                let err_resp =
                    Response::err(RequestId::Null, errors::PARSE_ERROR, err.to_string());
                state.out.send_response(&err_resp);
                continue;
            }
        };

        if is_long_running(&request.method) {
            let state_for_req = Arc::clone(&state);
            offloaded.spawn(async move {
                let response = handle_request(request, &state_for_req).await;
                state_for_req.out.send_response(&response);
            });
        } else {
            let response = handle_request(request, &state).await;
            state.out.send_response(&response);
        }
    }

    // stdin reached EOF (or failed): cancel the active operation and drain
    // in-flight work so the process never exits mid-transaction.
    drain_in_flight(&state, &mut offloaded).await;

    Ok(())
}

/// How long graceful shutdown waits for in-flight work before giving up.
const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Poll cadence while waiting for in-flight work to unwind during shutdown.
const SHUTDOWN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Methods whose handlers block on long-running work — an LLM turn or the
/// deterministic pre-flight toolchain verification — and must therefore be
/// offloaded from the read loop. Offloading keeps the loop free to accept
/// cancels and to observe stdin EOF. Every other method is short and runs
/// inline to preserve request ordering.
fn is_long_running(method: &str) -> bool {
    matches!(
        method,
        methods::PLAN_REFINE
            | methods::PLAN_REPLAN
            | methods::PLAN_EXECUTE
            | methods::REVIEW_EXECUTE
            | methods::GIT_SQUASH_GENERATE_MESSAGE
    )
}

/// Cancels the active operation and waits for in-flight work to finish so the
/// process never exits mid-transaction.
///
/// A running turn holds the single operation slot for its whole duration and
/// rolls its Git transaction back before releasing it, so an empty slot is a
/// reliable "work finished" signal. Offloaded long-running handlers are drained
/// as well. A bounded budget keeps a wedged provider from blocking shutdown
/// forever; anything still running when it expires is aborted, and startup
/// crash recovery covers any residue.
async fn drain_in_flight(state: &Arc<AppState>, offloaded: &mut tokio::task::JoinSet<()>) {
    let deadline = tokio::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
    loop {
        // Re-signal on every pass so an operation that registers while we wait
        // is cancelled too.
        state.cancel_active_operation();

        if !state.is_operation_active() && offloaded.is_empty() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }

        if offloaded.is_empty() {
            tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
        } else {
            tokio::select! {
                _ = offloaded.join_next() => {}
                _ = tokio::time::sleep(SHUTDOWN_POLL_INTERVAL) => {}
            }
        }
    }

    offloaded.abort_all();
    while offloaded.join_next().await.is_some() {}
}
