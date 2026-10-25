#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::path::PathBuf;
use std::sync::Arc;

use tauqe_core::history::HistoryMutation;
use tauqe_protocol::{
    errors, events, methods, Event, HistoryCompactedEvent, HistoryEntryAddedEvent,
    HistoryEntryRolledBackEvent, Request, RequestId, Response, CLIENT_SHUTDOWN_WAIT,
    SHUTDOWN_DRAIN_BUDGET,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

mod handlers;
mod logging;
mod state;
mod transport;
mod usage;

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

    logging::install_panic_hook();
    // The returned guard owns the non-blocking appender buffer; binding it to a
    // live variable keeps it until `main` returns so no record is lost.
    // A failure here must not be silent: without a subscriber the daemon would
    // run normally yet drop every tracing event for its whole lifetime. stderr
    // is the only diagnostic channel still available at this point.
    let _log_guard = match logging::init_logging(&repo_path) {
        Ok(guard) => Some(guard),
        Err(err) => {
            eprintln!("[WARN] Failed to initialise file logging: {err}");
            None
        }
    };
    let log_path = logging::log_file_path(&repo_path);
    tracing::info!(
        pid = std::process::id(),
        repo_root = %repo_path.display(),
        explicit_config = %explicit_config
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<auto>".to_string()),
        explicit_credentials = %explicit_credentials
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<auto>".to_string()),
        log_path = %log_path.display(),
        "Server daemon started",
    );
    let config =
        tauqe_core::config::load_config_with_options(tauqe_core::config::ConfigLoadOptions {
            repo_root: Some(&repo_path),
            explicit_config: explicit_config.as_deref(),
            explicit_credentials: explicit_credentials.as_deref(),
        });

    let (history_tx, mut history_rx) = tokio::sync::mpsc::unbounded_channel::<HistoryMutation>();
    let (out, mut out_rx) = OutChannel::new();

    let state = AppState::new(config, repo_path, history_tx, out);

    // Forward every history mutation to the client as an event: an append
    // becomes a `history/entryAdded` record, while a compaction becomes a
    // structural `history/compacted` record carrying the summary and the
    // preserved tail. Reporting compaction as its own event lets the client
    // rebuild its session view from the authoritative result instead of
    // reconstructing it from append events alone.
    let out_for_history = state.out.clone();
    let state_for_history = Arc::clone(&state);
    tokio::spawn(async move {
        while let Some(mutation) = history_rx.recv().await {
            // The token estimate reads the persisted journal from disk, so the
            // probe goes through the owned-guard blocking-pool helper instead
            // of stalling the async worker thread on a synchronous read.
            let estimated_tokens = state_for_history
                .mutate_history(|h| h.estimated_tokens().ok())
                .await
                .ok()
                .flatten();
            let ev = match mutation {
                HistoryMutation::Appended { item } => Event {
                    method: events::HISTORY_ENTRY_ADDED.to_string(),
                    params: serde_json::to_value(HistoryEntryAddedEvent {
                        item,
                        estimated_tokens,
                    })
                    .ok(),
                },
                HistoryMutation::Compacted { summary, tail_ids } => {
                    // The compacted session is the summary followed by the
                    // preserved tail, so its entry count derives from the tail
                    // size and needs no additional disk read.
                    let total_count = tail_ids.len().saturating_add(1);
                    Event {
                        method: events::HISTORY_COMPACTED.to_string(),
                        params: serde_json::to_value(HistoryCompactedEvent {
                            summary,
                            tail_ids,
                            total_count,
                            estimated_tokens: estimated_tokens.unwrap_or(0),
                        })
                        .ok(),
                    }
                }
                // A rollback is neither an append nor a deletion: the record
                // leaves the model context but stays in the chronicle with the
                // discarded prompt and the interruption reason, so the client
                // updates exactly that entry instead of guessing.
                HistoryMutation::RolledBack { item } => Event {
                    method: events::HISTORY_ENTRY_ROLLED_BACK.to_string(),
                    params: serde_json::to_value(HistoryEntryRolledBackEvent { item }).ok(),
                },
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

    // The signal watch is created before the read loop starts, so a signal
    // delivered between two requests is never lost. It races the stdin reader
    // and resolves to the same controlled exit instead of killing the process
    // in the middle of a transaction.
    let shutdown_watch = shutdown_signal();
    tokio::pin!(shutdown_watch);

    // Requests whose handlers block on long-running work (LLM turns, the
    // deterministic pre-flight verification) are offloaded so the read loop
    // keeps draining stdin and can observe cancels and EOF. Every other request
    // is handled inline, which serializes state mutations in the exact order
    // the client sent them and keeps responses ordered.
    let mut offloaded = tokio::task::JoinSet::new();

    let shutdown_reason = loop {
        let line = tokio::select! {
            line = reader.next_line() => match line {
                Ok(Some(line)) => line,
                Ok(None) => break ShutdownReason::StdinEof,
                Err(err) => break ShutdownReason::StdinFailed(err.to_string()),
            },
            reason = &mut shutdown_watch => break reason,
        };
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
                tracing::warn!(
                    error = %err,
                    code = errors::PARSE_ERROR,
                    "Rejected unparsable RPC request",
                );
                let err_resp = Response::err(RequestId::Null, errors::PARSE_ERROR, err.to_string());
                state.out.send_response(&err_resp);
                continue;
            }
        };

        // A request whose correlation id is explicitly null cannot be answered
        // unambiguously. Rejecting it before dispatch keeps a malformed client
        // from silently triggering a turn it can never correlate.
        if request.id == RequestId::Null {
            tracing::warn!(
                method = %request.method,
                code = errors::INVALID_REQUEST,
                "Rejected RPC request with a null id",
            );
            let err_resp = Response::err(
                RequestId::Null,
                errors::INVALID_REQUEST,
                "Request id must not be null",
            );
            state.out.send_response(&err_resp);
            continue;
        }

        let request_id = request.id.clone();
        let method = request.method.clone();
        tracing::info!(
            request_id = ?request_id,
            method = %method,
            params = %request_param_keys(&request.params),
            "RPC request received",
        );

        if is_long_running(&request.method) {
            let state_for_req = Arc::clone(&state);
            offloaded.spawn(async move {
                let started = std::time::Instant::now();
                let response = handle_request(request, &state_for_req).await;
                log_request_outcome(&request_id, &method, started.elapsed(), &response);
                state_for_req.out.send_response(&response);
            });
        } else {
            let started = std::time::Instant::now();
            let response = handle_request(request, &state).await;
            log_request_outcome(&request_id, &method, started.elapsed(), &response);
            state.out.send_response(&response);
        }
    };

    shutdown_reason.record();

    // The client closed the pipe or a signal asked the daemon to stop: cancel
    // the active operation and drain in-flight work so the process never exits
    // mid-transaction.
    drain_in_flight(&state, &mut offloaded).await;

    Ok(())
}

/// Why the daemon's request loop ended.
///
/// Every cause funnels into the same controlled exit: cancel the active
/// operation, drain in-flight work, and return. The cause is carried as typed
/// data, never inferred from surrounding text, so the journal entry and the
/// shutdown path can never disagree.
#[derive(Debug)]
enum ShutdownReason {
    /// The client closed its end of the stdin pipe.
    StdinEof,
    /// Reading stdin failed, which is a defect rather than a controlled end.
    StdinFailed(String),
    /// The operating system delivered a termination signal.
    Signal(ShutdownSignal),
}

impl ShutdownReason {
    /// Stable journal name of the cause.
    ///
    /// Each cause names itself, so `.tauqe/server.log` answers why the daemon
    /// left on its own instead of showing an unexplained disappearance.
    fn label(&self) -> &'static str {
        match self {
            Self::StdinEof => "stdin-eof",
            Self::StdinFailed(_) => "stdin-read-error",
            Self::Signal(signal) => signal.label(),
        }
    }

    /// Records the cause in the journal before the drain begins.
    ///
    /// An expected end of the request stream is ordinary information, while a
    /// failed read stays a warning carrying the underlying error.
    fn record(&self) {
        let reason = self.label();
        match self {
            Self::StdinFailed(error) => tracing::warn!(
                reason = reason,
                error = %error,
                "beginning graceful shutdown",
            ),
            _ => tracing::info!(reason = reason, "beginning graceful shutdown"),
        }
    }
}

/// A termination signal the daemon converts into its own shutdown path.
///
/// `SIGINT` is deliberately absent: the `Ctrl+C` chord belongs to the keyboard
/// contour of the TUI (`crates/tui/Conventions.md` §5.5) and must never be
/// handled through a process signal handler.
#[derive(Debug)]
enum ShutdownSignal {
    /// `SIGHUP`: the controlling terminal disappeared, usually because the TUI
    /// window was closed.
    Hangup,
    /// `SIGTERM`: an operator or a supervisor asked the daemon to stop.
    Terminate,
}

impl ShutdownSignal {
    /// Signal name as it appears in the journal.
    fn label(&self) -> &'static str {
        match self {
            Self::Hangup => "SIGHUP",
            Self::Terminate => "SIGTERM",
        }
    }

    /// Platform signal this cause listens for.
    #[cfg(unix)]
    fn kind(&self) -> tokio::signal::unix::SignalKind {
        use tokio::signal::unix::SignalKind;

        match self {
            Self::Hangup => SignalKind::hangup(),
            Self::Terminate => SignalKind::terminate(),
        }
    }
}

/// Resolves when the operating system asks the daemon to terminate.
///
/// Neither signal may end the process on the spot, because an abrupt death
/// skips the cancellation of the active turn and the rollback of its Git
/// transaction. Both are therefore reported to the caller, which routes them
/// into the same cancel, drain, and exit path as stdin EOF. The future is
/// awaited alongside the stdin reader and never completes on its own.
#[cfg(unix)]
async fn shutdown_signal() -> ShutdownReason {
    use tokio::signal::unix::signal;

    match (
        signal(ShutdownSignal::Hangup.kind()),
        signal(ShutdownSignal::Terminate.kind()),
    ) {
        (Ok(mut hangup), Ok(mut terminate)) => {
            tokio::select! {
                _ = hangup.recv() => ShutdownReason::Signal(ShutdownSignal::Hangup),
                _ = terminate.recv() => ShutdownReason::Signal(ShutdownSignal::Terminate),
            }
        }
        (hangup, terminate) => {
            // A handler that cannot be installed leaves the daemon on its
            // pre-signal behaviour: it still stops on stdin EOF.
            tracing::warn!(
                hangup_error = ?hangup.as_ref().err(),
                terminate_error = ?terminate.as_ref().err(),
                "Failed to install shutdown signal handlers; relying on stdin EOF",
            );
            std::future::pending().await
        }
    }
}

/// On platforms without Unix signals stdin EOF remains the only controlled
/// shutdown channel, and this future never completes.
#[cfg(not(unix))]
async fn shutdown_signal() -> ShutdownReason {
    std::future::pending().await
}

/// Poll cadence while waiting for in-flight work to unwind during shutdown.
const SHUTDOWN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Window the final force-abort may spend waiting for an open Git transaction to
/// close.
///
/// It is exactly the headroom the shared lifecycle contract grants the daemon on
/// top of [`SHUTDOWN_DRAIN_BUDGET`]: the client keeps waiting for that long after
/// the daemon's own deadline, so the abort can be deferred over an open
/// transaction without the client killing the process inside the very cleanup
/// the deferral protects.
const FORCE_ABORT_BUDGET: std::time::Duration =
    CLIENT_SHUTDOWN_WAIT.saturating_sub(SHUTDOWN_DRAIN_BUDGET);

/// Methods whose handlers block on long-running work — an LLM turn, a
/// repository lifecycle request that reloads configuration and rebuilds the
/// history manager from disk, a Git history rewrite, or the deterministic
/// pre-flight toolchain verification — and must therefore be offloaded from the
/// read loop. Offloading keeps the loop free to accept cancels and to observe
/// stdin EOF. Every other method is short and runs inline to preserve request
/// ordering.
fn is_long_running(method: &str) -> bool {
    matches!(
        method,
        methods::CLIENT_INITIALIZE
            | methods::REPOSITORY_INIT
            | methods::MODEL_ASK
            | methods::PLAN_REFINE
            | methods::PLAN_REPLAN
            | methods::PLAN_EXECUTE
            | methods::PLAN_EXECUTE_PIPELINE
            | methods::REVIEW_START
            | methods::REVIEW_EXECUTE
            | methods::REVIEW_REFINE_ITEM
            | methods::GIT_SQUASH_GENERATE_MESSAGE
            | methods::GIT_SQUASH_APPLY
            | methods::LOCALE_GENERATE
    )
}

/// Cancels the active operation and waits for in-flight work to finish so the
/// process never exits mid-transaction.
///
/// A running turn holds the single operation slot for its whole duration and
/// rolls its Git transaction back before releasing it, so an empty slot is a
/// reliable "work finished" signal. Offloaded long-running handlers are drained
/// as well. The deadline is the shared `SHUTDOWN_DRAIN_BUDGET` from the protocol
/// contract, so a wedged provider cannot block shutdown forever while the client
/// that spawned the daemon waits for that same budget.
///
/// Two different things are still running at that point, and they need two
/// different aborts. The join set holds the request handlers, while the work
/// those handlers started — a batch supervisor or a model turn — was spawned by
/// them and is tracked only through the operation slot's abort handle. When the
/// deadline expires both are torn down: the handlers are aborted directly, and
/// the registered operation is force-aborted through the slot, deferring over an
/// open Git transaction for exactly the headroom the client grants on top of the
/// drain budget. Startup crash recovery covers whatever residue remains.
async fn drain_in_flight(state: &Arc<AppState>, offloaded: &mut tokio::task::JoinSet<()>) {
    drain_in_flight_with_budget(state, offloaded, SHUTDOWN_DRAIN_BUDGET, FORCE_ABORT_BUDGET).await
}

/// Implements [`drain_in_flight`] with explicit deadlines, so a deterministic
/// test can exercise the timeout path without spending the real budget.
async fn drain_in_flight_with_budget(
    state: &Arc<AppState>,
    offloaded: &mut tokio::task::JoinSet<()>,
    drain_budget: std::time::Duration,
    abort_budget: std::time::Duration,
) {
    tracing::info!("Draining in-flight work before shutdown");
    let deadline = tokio::time::Instant::now() + drain_budget;
    loop {
        // Re-signal on every pass so an operation that registers while we wait
        // is cancelled too.
        state.cancel_active_operation();

        if !state.is_operation_active() && offloaded.is_empty() {
            tracing::info!("In-flight work drained; shutdown complete");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                timeout_ms = drain_budget.as_millis() as u64,
                "Shutdown drain timed out; aborting remaining work",
            );
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

    let remaining = offloaded.len();
    offloaded.abort_all();
    tracing::warn!(
        remaining,
        "Force-aborting in-flight handler tasks at shutdown"
    );
    while offloaded.join_next().await.is_some() {}

    // The handler task is not where the work lives: a long-running request
    // spawns its turn or batch and returns, so the join set is usually empty
    // while the operation slot still holds the task that really runs. Reaching
    // through the slot is the only way to abort it, and dropping the runtime
    // instead would strand a Git transaction mid-write.
    state.force_abort_active_operation(abort_budget).await;
}

/// Renders a compact, secret-free summary of RPC parameters for the journal.
///
/// Only the top-level key set is recorded — never the values — so prompt
/// bodies, file contents, and credentials can never leak into the log.
fn request_param_keys(params: &Option<serde_json::Value>) -> String {
    match params {
        Some(serde_json::Value::Object(map)) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            keys.join(",")
        }
        Some(_) => "<non-object>".to_string(),
        None => "<none>".to_string(),
    }
}

/// Records the terminal outcome of an RPC request together with its duration.
///
/// Only the status is logged — an `ok` marker or the error code — never the
/// response payload, so results cannot leak into the journal.
fn log_request_outcome(
    request_id: &RequestId,
    method: &str,
    elapsed: std::time::Duration,
    response: &Response,
) {
    let duration_ms = elapsed.as_millis() as u64;
    match &response.error {
        Some(err) => tracing::warn!(
            request_id = ?request_id,
            method = %method,
            duration_ms,
            error_code = %err.code,
            "RPC request failed",
        ),
        None => tracing::info!(
            request_id = ?request_id,
            method = %method,
            duration_ms,
            "RPC request completed",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Repository lifecycle requests reload configuration, re-anchor the
    /// context, and rebuild the history manager from disk, while
    /// `git/squashApply` rewrites history and records the result behind an
    /// `fsync` barrier. All of them must be offloaded, otherwise the stdin
    /// reader loop stalls and can observe neither cancels nor EOF while they
    /// run.
    #[test]
    fn repository_lifecycle_requests_are_offloaded() {
        for method in [
            methods::CLIENT_INITIALIZE,
            methods::REPOSITORY_INIT,
            methods::GIT_SQUASH_APPLY,
        ] {
            assert!(is_long_running(method), "'{method}' must be offloaded");
        }

        // Short requests stay inline so their responses keep the client's
        // request order.
        assert!(!is_long_running(methods::CONFIG_GET));
        assert!(!is_long_running(methods::HISTORY_GET));
    }

    /// Every termination cause carries its own stable journal name, so a look
    /// at `.tauqe/server.log` answers why the daemon left on its own instead of
    /// showing an unexplained disappearance.
    #[test]
    fn shutdown_causes_are_named_distinctly() {
        let causes = [
            ShutdownReason::StdinEof,
            ShutdownReason::StdinFailed("broken pipe".to_string()),
            ShutdownReason::Signal(ShutdownSignal::Hangup),
            ShutdownReason::Signal(ShutdownSignal::Terminate),
        ];

        let mut labels: Vec<&str> = Vec::new();
        for cause in &causes {
            let label = cause.label();
            assert!(!label.is_empty(), "a cause without a name cannot be logged");
            assert!(
                !labels.contains(&label),
                "'{label}' names more than one cause"
            );
            labels.push(label);
        }
    }

    /// `SIGHUP` and `SIGTERM` are the only signals the daemon converts into its
    /// own shutdown path. `SIGINT` must stay untouched: the `Ctrl+C` chord
    /// belongs to the keyboard contour of the TUI.
    #[cfg(unix)]
    #[test]
    fn only_hangup_and_terminate_reach_the_shutdown_path() {
        use tokio::signal::unix::SignalKind;

        assert_eq!(ShutdownSignal::Hangup.kind(), SignalKind::hangup());
        assert_eq!(ShutdownSignal::Terminate.kind(), SignalKind::terminate());
        assert_ne!(ShutdownSignal::Hangup.kind(), SignalKind::terminate());
        assert_ne!(ShutdownSignal::Hangup.kind(), SignalKind::interrupt());
        assert_ne!(ShutdownSignal::Terminate.kind(), SignalKind::interrupt());
    }

    /// Without a delivered signal the watch never completes, on every platform:
    /// a self-completing future would abort the daemon spuriously, and a
    /// platform without Unix signals must poll it without panicking.
    #[tokio::test]
    async fn shutdown_watch_never_completes_on_its_own() {
        let outcome =
            tokio::time::timeout(std::time::Duration::from_millis(20), shutdown_signal()).await;
        assert!(
            outcome.is_err(),
            "the shutdown watch fired without a signal"
        );
    }

    /// Monotonic suffix keeping the scratch repositories of this module apart.
    static SCRATCH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Builds a daemon state bound to its own scratch repository.
    fn test_state(label: &str) -> (Arc<AppState>, PathBuf) {
        let unique = SCRATCH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "tauqe-drain-{}-{}-{}",
            std::process::id(),
            label,
            unique
        ));
        let _ = std::fs::create_dir_all(&root);
        let (out, _out_rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            AppState::new(
                tauqe_core::config::AppConfig::default(),
                root.clone(),
                history_sender,
                out,
            ),
            root,
        )
    }

    /// Flips a flag when the owning task future is dropped, so the test can prove
    /// that a force-aborted operation really stopped running instead of lingering
    /// detached.
    struct ExitMarker(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for ExitMarker {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A long-running request returns immediately after spawning its batch or
    /// turn, so the drain's join set is empty while the operation slot still
    /// holds the task that really runs. Expiring the drain deadline must abort
    /// that task through the slot, not merely re-signal it: a cooperative cancel
    /// can be ignored, and leaving the task running drops the runtime
    /// mid-transaction on process exit.
    #[tokio::test]
    async fn drain_deadline_force_aborts_the_registered_operation() {
        let (state, root) = test_state("force-abort");

        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Arc::new(state.try_begin_operation().expect("the slot is free"));
        let task = {
            let guard = Arc::clone(&guard);
            let marker = ExitMarker(Arc::clone(&exited));
            tokio::spawn(async move {
                let _guard = guard;
                let _marker = marker;
                // An operation that ignores the cooperative cancel signal, as a
                // wedged provider round or a wedged toolchain step does.
                std::future::pending::<()>().await
            })
        };
        guard.set_abort_handle(task.abort_handle());
        drop(guard);

        // The handler task set is empty exactly as it is in production, where a
        // long-running request returns right after spawning its batch or turn.
        let mut offloaded = tokio::task::JoinSet::new();
        drain_in_flight_with_budget(
            &state,
            &mut offloaded,
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(500),
        )
        .await;

        assert!(
            exited.load(std::sync::atomic::Ordering::SeqCst),
            "the stalled operation must be force-aborted, not merely signalled"
        );
        assert!(
            !state.is_operation_active(),
            "the aborted operation must release its registration"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
