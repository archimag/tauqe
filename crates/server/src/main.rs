#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tauqe_protocol::{events, Event, HistoryEntryAddedEvent, Request, RequestId, Response, ResponseError, UiHistoryItem};

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
    let config = tauqe_core::config::load_config_with_options(tauqe_core::config::ConfigLoadOptions {
        repo_root: None,
        explicit_config: explicit_config.as_deref(),
        explicit_credentials: explicit_credentials.as_deref(),
    });
    let repo_state = tauqe_core::git::get_repository_state(None);
    let repo_path = PathBuf::from(&repo_state.root);

    let (history_tx, mut history_rx) = tokio::sync::mpsc::unbounded_channel::<UiHistoryItem>();
    let (out, mut out_rx) = OutChannel::new();

    let state = AppState::new(config, repo_path, history_tx, out);

    // Forward UI history items to out channel as events
    let out_for_history = state.out.clone();
    tokio::spawn(async move {
        while let Some(item) = history_rx.recv().await {
            let ev = Event {
                method: events::HISTORY_ENTRY_ADDED.to_string(),
                params: serde_json::to_value(HistoryEntryAddedEvent { item }).ok(),
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

    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(err) => {
                let err_resp = Response {
                    id: RequestId::Number(0),
                    result: None,
                    error: Some(ResponseError {
                        code: "PARSE_ERROR".to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                };
                state.out.send_response(&err_resp);
                continue;
            }
        };

        let state_for_req = Arc::clone(&state);
        tokio::spawn(async move {
            let response = handle_request(request, &state_for_req).await;
            state_for_req.out.send_response(&response);
        });
    }

    Ok(())
}
