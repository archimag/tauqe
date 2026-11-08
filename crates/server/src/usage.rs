//! Session-wide usage accounting for every operation kind.
//!
//! Token, cost, and duration totals previously lived wherever an operation
//! happened to need them: the session cost was a bare `f64` on the state, and
//! every handler derived its own view of the tokens it had spent. That made the
//! interface unable to answer a simple question — *what has this session cost
//! so far, and what is the current operation spending right now?* — without
//! replaying per-operation events.
//!
//! This module owns the single authoritative [`UsageLedger`] behind
//! [`AppState::usage`]. An operation opens a [`UsageGuard`], reports the usage
//! each provider call returns through [`UsageGuard::observe`], and closes the
//! window with [`UsageGuard::flush`], which folds the operation into the
//! session totals together with the duration the harness measured itself.
//! While the window is open, a per-second heartbeat republishes the snapshot
//! with the live in-flight duration, so a client renders a running clock for
//! the current operation instead of a dash. Every transition publishes a
//! `model/usageTotals` event under the operation identifier its caller already
//! announces through `model/started` and `model/finished`, so a client renders
//! one consistent snapshot attributed to the operation it renders instead of
//! correlating a second identifier of its own.
//!
//! Every window carries an identity that lives beside the snapshot under the
//! same mutex, so the two can never disagree. The accounting layer itself
//! therefore decides who may act on the in-flight scope: the heartbeat
//! publishes only while the recorded identity is verifiably its own, and a
//! flush folds only the window it still owns. Stopping the heartbeat task is a
//! best-effort optimization on top of that guarantee, never its substitute: an
//! abort lands only at the task's next suspension point, so a superseded task
//! may still reach the lock once, and it must find itself dispossessed there.
//!
//! The project lifetime scope outlives the process. It accumulates every
//! completed operation and is persisted in `<repo_root>/.tauqe/usage.json`,
//! a directory Git ignores. The document is read once when the state is
//! created and rewritten after every flush through the atomic-write policy of
//! the core engine, so a crash publishes either the previous complete document
//! or the new complete one, never a truncated one. Reading and writing run on
//! the blocking pool: the daemon runtime must never stall on the file system.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use tauqe_protocol::model::{
    ModelUsageInfo, ModelUsageTotalsEvent, UsageTotals, UsageTotalsSnapshot,
};
use tauqe_protocol::{events, Event};

use crate::state::{next_operation_id, AppState};

/// Directory, relative to the repository root, holding the project telemetry.
const TELEMETRY_DIR: &str = ".tauqe";

/// File inside [`TELEMETRY_DIR`] carrying the project lifetime totals.
const TELEMETRY_FILE: &str = "usage.json";

/// Resolves the telemetry file of a repository.
fn telemetry_path(repo_root: &Path) -> PathBuf {
    repo_root.join(TELEMETRY_DIR).join(TELEMETRY_FILE)
}

/// Loads the persisted project lifetime totals of a repository.
///
/// A missing file, an unreadable file, or a document written by an
/// incompatible version is a normal state, not an error: the totals start from
/// zero and the next flush rewrites the document. Only the lifetime scope is
/// persisted, because the session and per-operation scopes restart with the
/// daemon and restoring them would resurrect stale counters.
pub(crate) fn load_initial_snapshot(repo_root: &Path) -> UsageTotalsSnapshot {
    let path = telemetry_path(repo_root);
    // `AppState::new` runs inside the daemon runtime, so the synchronous read
    // is handed to the blocking-aware helper instead of occupying a worker.
    let loaded = tauqe_core::blocking::run_blocking(move || read_document(&path));
    match loaded {
        Some((lifetime, lifetime_operations)) => UsageTotalsSnapshot {
            lifetime,
            lifetime_operations,
            ..UsageTotalsSnapshot::default()
        },
        None => UsageTotalsSnapshot::default(),
    }
}

/// Reads the telemetry document, returning its lifetime totals.
///
/// Returns `None` when no document exists yet, when it cannot be read, or when
/// it is not valid JSON; every one of those cases is reported and degraded to
/// zero totals rather than failing the daemon startup.
fn read_document(path: &Path) -> Option<(UsageTotals, u64)> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to read the project telemetry",
            );
            return None;
        }
    };
    let document: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(document) => document,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Discarding unreadable project telemetry",
            );
            return None;
        }
    };
    let lifetime = document
        .get("lifetime")
        .and_then(|value| serde_json::from_value::<UsageTotals>(value.clone()).ok())
        .unwrap_or_default();
    let lifetime_operations = document
        .get("lifetime_operations")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Some((lifetime, lifetime_operations))
}

/// Writes the telemetry document atomically under `<repo_root>/.tauqe`.
///
/// The directory is created on demand, so a repository that has never recorded
/// a turn still gains its telemetry file on the first flush. The payload is
/// handed to the core atomic-write policy, which stages, syncs, and renames it,
/// so a concurrent reader never observes a partial document.
fn write_document(path: &Path, lifetime: &UsageTotals, operations: u64) -> Result<(), String> {
    let lifetime = serde_json::to_value(lifetime)
        .map_err(|err| format!("Failed to encode the project telemetry: {}", err))?;
    let document = serde_json::json!({
        "lifetime": lifetime,
        "lifetime_operations": operations,
    });
    let payload = serde_json::to_vec_pretty(&document)
        .map_err(|err| format!("Failed to encode the project telemetry: {}", err))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create {}: {}", parent.display(), err))?;
    }
    tauqe_tools::atomic_write::write_atomic(path, &payload).map_err(|err| format!("{:#}", err))
}

/// Persists the project lifetime totals off the async worker pool.
async fn persist_lifetime(state: &Arc<AppState>, lifetime: &UsageTotals, operations: u64) {
    let path = telemetry_path(state.repo_root().await.as_path());
    let lifetime = lifetime.clone();
    let written = tauqe_core::blocking::run_blocking_async(move || {
        write_document(&path, &lifetime, operations)
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "Failed to persist the project telemetry");
        }
        Err(err) => {
            tracing::warn!(error = %err, "Project telemetry write task failed");
        }
    }
}

/// Identity of one usage accounting window.
pub(crate) type WindowId = u64;

/// Monotonic source of window identities.
static WINDOW_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Mints the identity of the next accounting window.
fn next_window_id() -> WindowId {
    WINDOW_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Authoritative accounting state of the daemon.
///
/// The snapshot and the identity of the window that owns its in-flight scope
/// live under one mutex, so identity and window can never disagree. Ownership
/// is the guarantee the accounting layer itself provides: the heartbeat, an
/// observation, and a flush act on the in-flight scope only while the recorded
/// identity is their own, so a window a successor already replaced can neither
/// publish nor fold anything that belongs to that successor, whatever order the
/// two tasks reach the lock in. The guarantee therefore never depends on the
/// operation slot or on a task abort having taken effect.
pub struct UsageLedger {
    snapshot: UsageTotalsSnapshot,
    window: Option<WindowId>,
}

impl UsageLedger {
    /// Wraps the snapshot the daemon starts from.
    ///
    /// No window owns the in-flight scope before the first operation opens
    /// one, so a fresh ledger carries the persisted project totals alone.
    pub(crate) fn new(snapshot: UsageTotalsSnapshot) -> Self {
        Self {
            snapshot,
            window: None,
        }
    }

    /// Returns the authoritative snapshot.
    pub(crate) fn snapshot(&self) -> &UsageTotalsSnapshot {
        &self.snapshot
    }

    /// Returns the authoritative snapshot for the lifetime scopes to mutate.
    pub(crate) fn snapshot_mut(&mut self) -> &mut UsageTotalsSnapshot {
        &mut self.snapshot
    }

    /// Reports whether `window` still owns the in-flight scope.
    pub(crate) fn owns(&self, window: WindowId) -> bool {
        self.window == Some(window)
    }

    /// Opens a fresh window, discarding an abandoned in-flight scope.
    ///
    /// A window left behind by an operation that failed before flushing is
    /// discarded here, and the identity minted for its successor is what makes
    /// the abandoned heartbeat find itself dispossessed instead of publishing
    /// the successor's state under its own identifier and clock.
    fn begin(&mut self) -> WindowId {
        let window = next_window_id();
        self.window = Some(window);
        self.snapshot.cur = Some(UsageTotals::default());
        window
    }

    /// Adds one provider report to `window`'s in-flight totals.
    ///
    /// A window that no longer owns the scope reports nothing: its successor
    /// installed its own in-flight totals, and folding a stale report into them
    /// would attribute one operation's spend to another.
    fn observe(&mut self, window: WindowId, usage: &UsageTotals) -> bool {
        if !self.owns(window) {
            return false;
        }
        self.snapshot
            .cur
            .get_or_insert_with(UsageTotals::default)
            .accumulate(usage);
        true
    }

    /// Closes `window`, returning the totals it observed.
    ///
    /// Only a window that still owns the scope is closed. Every other window is
    /// absent for its guard, so the successor's in-flight totals are never
    /// taken by a flush that finished after its own window was discarded.
    fn close(&mut self, window: WindowId, elapsed_ms: u64) -> Option<UsageTotals> {
        if !self.owns(window) {
            return None;
        }
        self.window = None;
        let mut completed = self.snapshot.cur.take().unwrap_or_default();
        completed.duration_ms = completed.duration_ms.saturating_add(elapsed_ms);
        Some(completed)
    }
}

/// Encodes one snapshot publication for the wire.
fn snapshot_event_params(
    operation_id: &str,
    snapshot: &UsageTotalsSnapshot,
) -> Option<serde_json::Value> {
    serde_json::to_value(ModelUsageTotalsEvent {
        operation_id: operation_id.to_string(),
        snapshot: snapshot.clone(),
    })
    .ok()
}

/// Publishes the current snapshot to the client.
async fn publish_snapshot(state: &AppState, operation_id: &str) {
    let snapshot = state.usage_snapshot().await;
    state.out.send_event(&Event {
        method: events::MODEL_USAGE_TOTALS.to_string(),
        params: snapshot_event_params(operation_id, &snapshot),
    });
}

/// Cadence of the live in-flight duration heartbeat.
///
/// While an accounting window is open, its heartbeat republishes the snapshot
/// once per tick with the duration measured so far. The daemon owns the clock,
/// so a client only mirrors the published value instead of running a timer of
/// its own, mirroring the reconnect countdown contract.
const LIVE_DURATION_TICK: Duration = Duration::from_secs(1);

/// Open accounting window for one operation.
///
/// The window starts when the operation begins and closes on
/// [`UsageGuard::flush`]. Dropping it without flushing abandons the window; the
/// next [`AppState::begin_usage`] discards the abandoned totals, so a failed
/// operation can never leave a permanently in-flight window behind. While the
/// window is open, its heartbeat republishes the snapshot with the live
/// in-flight duration, so a client renders a running clock for the current
/// operation instead of a dash.
///
/// The guard carries two identities. The operation identifier is the one the
/// caller already announces through its own lifecycle events, so every snapshot
/// this window publishes names the operation a client is rendering. The window
/// identifier stays internal: every operation the guard performs is checked
/// against it under the accounting lock, so the guard can never publish or fold
/// a scope a successor already replaced.
pub struct UsageGuard {
    state: Arc<AppState>,
    /// Identifier the window publishes under: the one the caller announces
    /// through its own lifecycle events.
    operation_id: String,
    /// Identity of the accounting window this guard owns.
    window: WindowId,
    started: Instant,
    /// Live duration heartbeat of this window, silenced on flush and on drop.
    ticker: tokio::task::JoinHandle<()>,
}

impl Drop for UsageGuard {
    fn drop(&mut self) {
        // The heartbeat belongs to the window: an abandoned guard silences its
        // own heartbeat, so a dropped window never keeps publishing a running
        // clock for an operation that is no longer executing.
        self.ticker.abort();
    }
}

/// Spawns the live duration heartbeat of one accounting window.
///
/// The published duration is derived, never stored: the authoritative
/// `cur.duration_ms` stays zero until [`UsageGuard::flush`] folds in the final
/// measurement, so a heartbeat can never leak into the session totals. The
/// heartbeat holds the accounting lock across its synchronous send and checks
/// the window identity under that very lock, which keeps it strictly ordered
/// against both the flush and the successor's `begin`: the moment the identity
/// it was spawned with stops owning the in-flight scope it exits without
/// publishing, so neither a closed window nor a replaced one can produce a
/// stale "still running" snapshot. The cadence arrives as `tick`, so a test can
/// compress it without touching the production default.
fn spawn_live_duration_ticker(
    state: &Arc<AppState>,
    window: WindowId,
    operation_id: &str,
    started: Instant,
    tick: Duration,
) -> tokio::task::JoinHandle<()> {
    let state = Arc::clone(state);
    let operation_id = operation_id.to_string();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            // The lock is held across the send because the send is
            // synchronous: the heartbeat stays strictly ordered against
            // flush without ever awaiting on a locked window. Ownership is
            // read under the same lock, so a heartbeat whose window a
            // successor already replaced observes that it no longer owns the
            // in-flight scope and exits without publishing anything — least of
            // all the successor's state under its own identifier and clock.
            let ledger = state.usage.lock().await;
            if !ledger.owns(window) {
                break;
            }
            let mut live = ledger.snapshot().clone();
            if let Some(cur) = live.cur.as_mut() {
                cur.duration_ms = started.elapsed().as_millis() as u64;
            }
            state.out.send_event(&Event {
                method: events::MODEL_USAGE_TOTALS.to_string(),
                params: snapshot_event_params(&operation_id, &live),
            });
        }
    })
}

impl AppState {
    /// Opens a usage accounting window for the operation named by `operation_id`.
    ///
    /// The identifier is the one the caller already announces through its own
    /// lifecycle events, so the in-flight totals, the running-clock heartbeat,
    /// and the final flush of this window name the operation a client is
    /// rendering instead of a second identity it would have to correlate.
    ///
    /// Ownership stays a separate, internal identity minted under the lock that
    /// installs the window, so a window left behind by an operation that failed
    /// before flushing is discarded here and its abandoned heartbeat can never
    /// mistake the successor's totals for its own.
    pub async fn begin_usage(self: &Arc<Self>, operation_id: &str) -> UsageGuard {
        self.begin_usage_with_tick(operation_id, LIVE_DURATION_TICK)
            .await
    }

    /// Opens a usage accounting window whose heartbeat runs on `tick`.
    ///
    /// [`AppState::begin_usage`] passes the production cadence, while a test
    /// passes a compressed one, so no assertion has to wait a whole production
    /// tick. The seam is private to this module and never changes the cadence
    /// the daemon publishes with.
    async fn begin_usage_with_tick(
        self: &Arc<Self>,
        operation_id: &str,
        tick: Duration,
    ) -> UsageGuard {
        let window = {
            let mut ledger = self.usage.lock().await;
            ledger.begin()
        };
        let started = Instant::now();
        let ticker = spawn_live_duration_ticker(self, window, operation_id, started, tick);
        let guard = UsageGuard {
            state: Arc::clone(self),
            operation_id: operation_id.to_string(),
            window,
            started,
            ticker,
        };
        guard.publish().await;
        guard
    }

    /// Clears the persisted project lifetime totals.
    ///
    /// Only the lifetime scope is cleared. The session counters keep describing
    /// what this process has already spent, so a reset never makes the running
    /// session appear free. The cleared totals are written to disk immediately
    /// and the resulting snapshot is published before it is returned, so a
    /// client renders the same state the next restart will read.
    pub async fn reset_lifetime_usage(self: &Arc<Self>) -> UsageTotalsSnapshot {
        let snapshot = {
            let mut ledger = self.usage.lock().await;
            {
                let snapshot = ledger.snapshot_mut();
                snapshot.lifetime = UsageTotals::default();
                snapshot.lifetime_operations = 0;
            }
            ledger.snapshot().clone()
        };
        persist_lifetime(self, &snapshot.lifetime, snapshot.lifetime_operations).await;
        let operation_id = format!("reset-{}", next_operation_id());
        publish_snapshot(self, &operation_id).await;
        snapshot
    }

    /// Adopts the project lifetime totals stored in the current repository.
    ///
    /// Rebinding to a repository must adopt the telemetry stored there.
    /// Keeping the previous repository's lifetime would present its counters
    /// as belonging to the new project, and the next flush would overwrite the
    /// new telemetry with those stale totals. Only the lifetime scope is
    /// replaced: the session totals describe this process and keep their
    /// values.
    pub async fn reload_usage_lifetime(self: &Arc<Self>) {
        let path = telemetry_path(self.repo_root().await.as_path());
        // The document is read from disk, so the read goes to the blocking
        // pool instead of stalling an async worker thread on the file system.
        let loaded =
            match tauqe_core::blocking::run_blocking_async(move || read_document(&path)).await {
                Ok(Some((lifetime, lifetime_operations))) => (lifetime, lifetime_operations),
                Ok(None) => (UsageTotals::default(), 0),
                Err(err) => {
                    tracing::warn!(error = %err, "Project telemetry read task failed");
                    (UsageTotals::default(), 0)
                }
            };
        {
            let mut ledger = self.usage.lock().await;
            let snapshot = ledger.snapshot_mut();
            snapshot.lifetime = loaded.0;
            snapshot.lifetime_operations = loaded.1;
        }
        let operation_id = format!("rebind-{}", next_operation_id());
        publish_snapshot(self, &operation_id).await;
    }
}

impl UsageGuard {
    /// Adds the usage reported by one provider call to the in-flight totals.
    ///
    /// A provider may answer with token counts only, a cost only, or nothing at
    /// all; the totals accumulate whatever was reported and stay additive
    /// across the several calls a multi-round operation makes. A window that no
    /// longer owns the in-flight scope records and publishes nothing, because
    /// the totals installed under the lock belong to its successor.
    pub async fn observe(&self, usage: &ModelUsageInfo) {
        let applied = {
            let mut ledger = self.state.usage.lock().await;
            ledger.observe(self.window, &UsageTotals::from_usage(usage))
        };
        if applied {
            self.publish().await;
        } else {
            tracing::warn!(
                operation_id = self.operation_id.as_str(),
                "Usage window was already replaced; the observation is discarded",
            );
        }
    }

    /// Closes the window, folding the operation into both totals and persisting
    /// the project total.
    ///
    ///
    /// The duration is measured by the harness rather than reported by the
    /// provider, so it reflects how long the operation actually held the
    /// developer's turn. The project lifetime is accumulated under the same
    /// lock as the session totals, so the two scopes can never disagree about
    /// the operation that just finished. The heartbeat is silenced first and
    /// the window it was spawned with is closed under the same lock, so the
    /// final publish below is the last snapshot this operation can ever
    /// produce.
    ///
    /// Only the window this guard owns is folded and cleared: a guard whose
    /// window a successor already discarded treats its own scope as absent, so
    /// it can neither take the successor's in-flight totals nor publish them
    /// under an identifier they do not belong to.
    pub async fn flush(self) {
        self.ticker.abort();
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let folded = {
            let mut ledger = self.state.usage.lock().await;
            match ledger.close(self.window, elapsed_ms) {
                Some(completed) => {
                    let snapshot = ledger.snapshot_mut();
                    snapshot.session.accumulate(&completed);
                    snapshot.operations = snapshot.operations.saturating_add(1);
                    snapshot.lifetime.accumulate(&completed);
                    let operations = snapshot.lifetime_operations.saturating_add(1);
                    snapshot.lifetime_operations = operations;
                    snapshot.prev = completed;
                    Some((snapshot.lifetime.clone(), operations))
                }
                None => None,
            }
        };
        let Some((lifetime, lifetime_operations)) = folded else {
            tracing::warn!(
                operation_id = self.operation_id.as_str(),
                "Usage window was already replaced; the flush folds nothing",
            );
            return;
        };
        // Persist before publishing: the announced snapshot then always
        // matches what a restarted daemon would load.
        persist_lifetime(&self.state, &lifetime, lifetime_operations).await;
        self.publish().await;
    }

    /// Publishes the current snapshot to the client.
    async fn publish(&self) {
        publish_snapshot(&self.state, &self.operation_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
    use crate::transport::OutChannel;
    use std::path::PathBuf;
    use tauqe_core::config::AppConfig;

    fn usage(prompt: u32, completion: u32) -> ModelUsageInfo {
        ModelUsageInfo {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cost: Some(0.25),
            ..ModelUsageInfo::default()
        }
    }

    fn test_state() -> (
        Arc<AppState>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        PathBuf,
    ) {
        let root = scratch_root("usage");
        let (out, rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
        (state, rx, root)
    }

    /// Builds a state bound to an existing directory, so two states can share
    /// one telemetry file the way two daemon runs share a repository.
    fn state_at(root: &Path) -> Arc<AppState> {
        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(
            AppConfig::default(),
            root.to_path_buf(),
            history_sender,
            out,
        )
    }

    fn read_stored_lifetime(root: &Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(telemetry_path(root)).expect("telemetry file exists");
        serde_json::from_str(&raw).expect("telemetry file is valid JSON")
    }

    /// Decodes a `model/usageTotals` event line from the test transport,
    /// returning `None` for every other event.
    fn decode_usage_event(raw: &str) -> Option<ModelUsageTotalsEvent> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        if value.get("method").and_then(|method| method.as_str())
            != Some(events::MODEL_USAGE_TOTALS)
        {
            return None;
        }
        serde_json::from_value(value.get("params")?.clone()).ok()
    }

    /// Every flushed operation folds into the session totals, and the previous
    /// operation keeps its own totals so the footer can show both.
    #[tokio::test]
    async fn flushes_operations_into_the_session_totals() {
        let (state, mut rx, root) = test_state();

        let first = state.begin_usage("op-first").await;
        first.observe(&usage(100, 40)).await;
        first.flush().await;

        let second = state.begin_usage("op-second").await;
        second.observe(&usage(10, 5)).await;
        second.observe(&usage(2, 3)).await;
        second.flush().await;

        let snapshot = state.usage_snapshot().await;
        assert_eq!(snapshot.operations, 2);
        assert_eq!(snapshot.session.prompt_tokens, 112);
        assert_eq!(snapshot.session.completion_tokens, 48);
        assert_eq!(snapshot.session.total_tokens, 160);
        assert!((snapshot.session.cost - 0.75).abs() < f64::EPSILON);
        assert_eq!(snapshot.prev.total_tokens, 20);
        assert!(snapshot.cur.is_none(), "a flushed window leaves no residue");

        // Each begin, observe, and flush publishes the snapshot: 3 + 4. The
        // live-duration heartbeat may add its own publications on a slow
        // runner, so the count is restricted to the transition events: their
        // in-flight window still carries the zero stored duration, while a
        // heartbeat always reports a whole tick or more.
        let mut published = 0;
        while let Ok(raw) = rx.try_recv() {
            if let Some(event) = decode_usage_event(&raw) {
                let is_heartbeat = event
                    .snapshot
                    .cur
                    .as_ref()
                    .is_some_and(|cur| cur.duration_ms > 0);
                if !is_heartbeat {
                    published += 1;
                }
            }
        }
        assert_eq!(published, 7, "every transition publishes the snapshot");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The in-flight operation stays visible before it is flushed.
    #[tokio::test]
    async fn keeps_the_in_flight_operation_visible_until_flush() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage("op-visible").await;
        guard.observe(&usage(3, 2)).await;

        let in_flight = state.usage_snapshot().await;
        assert_eq!(in_flight.session.total_tokens, 0);
        assert_eq!(in_flight.operation_totals().total_tokens, 5);

        guard.flush().await;
        let flushed = state.usage_snapshot().await;
        assert!(flushed.cur.is_none());
        assert_eq!(flushed.session.total_tokens, 5);
        assert_eq!(flushed.prev.total_tokens, 5);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// While the window is open, the heartbeat republishes the snapshot once
    /// per tick with the live in-flight duration; flush silences it and folds
    /// the full measured duration into the session and lifetime totals.
    ///
    /// The heartbeat runs on the real clock, so the test opens the window on a
    /// compressed tick and polls the transport until at least one live
    /// publication arrives, instead of relying on a paused tokio clock, which
    /// would require the `test-util` feature the workspace does not enable.
    #[tokio::test]
    async fn live_duration_heartbeat_publishes_and_is_silenced_by_flush() {
        // The window runs on a compressed tick: the first heartbeat arrives in
        // tens of milliseconds instead of a whole production second.
        const TEST_TICK: Duration = Duration::from_millis(50);
        let (state, mut rx, root) = test_state();

        let guard = state.begin_usage_with_tick("op-heartbeat", TEST_TICK).await;
        guard.observe(&usage(3, 2)).await;

        // Poll the transport until a heartbeat publishes the live duration.
        // Each publication re-reports the observed tokens with a growing
        // live duration, while the stored window keeps its zero duration
        // until flush.
        let mut max_live_ms = 0u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while max_live_ms == 0 && std::time::Instant::now() < deadline {
            while let Ok(raw) = rx.try_recv() {
                let Some(event) = decode_usage_event(&raw) else {
                    continue;
                };
                let Some(cur) = event.snapshot.cur.as_ref() else {
                    continue;
                };
                if cur.duration_ms > 0 {
                    assert_eq!(
                        cur.total_tokens, 5,
                        "the heartbeat re-reports the observed tokens"
                    );
                    max_live_ms = max_live_ms.max(cur.duration_ms);
                }
            }
            if max_live_ms == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        assert!(
            max_live_ms > 0,
            "the heartbeat must publish a live in-flight duration"
        );

        // The stored window was never touched by the heartbeat: flush folds
        // the full measured duration, never the heartbeat's derived one on
        // top of it.
        guard.flush().await;
        let snapshot = state.usage_snapshot().await;
        assert!(snapshot.cur.is_none());
        assert_eq!(snapshot.prev.total_tokens, 5);
        assert!(
            snapshot.prev.duration_ms >= max_live_ms,
            "flush records the whole window duration"
        );
        assert_eq!(snapshot.session.duration_ms, snapshot.prev.duration_ms);
        assert_eq!(snapshot.lifetime.duration_ms, snapshot.prev.duration_ms);

        // Drain everything published so far, then let several ticks pass:
        // the queue must stay empty because the heartbeat was silenced with
        // the window and no stale "still running" snapshot can arrive after
        // the final publish.
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(TEST_TICK + Duration::from_millis(200)).await;
        assert!(
            rx.try_recv().is_err(),
            "no heartbeat may outlive the flushed window"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A completed operation is folded into the project lifetime and written to
    /// `.tauqe/usage.json`, so a restarted daemon continues the same totals
    /// while its session scope starts from zero.
    #[tokio::test]
    async fn flush_persists_the_project_lifetime() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage("op-persist").await;
        guard.observe(&usage(100, 40)).await;
        guard.flush().await;

        let stored = read_stored_lifetime(&root);
        assert_eq!(stored["lifetime"]["total_tokens"].as_u64(), Some(140));
        assert_eq!(stored["lifetime_operations"].as_u64(), Some(1));

        let restarted = state_at(&root);
        let snapshot = restarted.usage_snapshot().await;
        assert_eq!(snapshot.lifetime.total_tokens, 140);
        assert_eq!(snapshot.lifetime_operations, 1);
        assert!(
            snapshot.session.is_empty(),
            "the session scope restarts with the process"
        );
        assert_eq!(snapshot.operations, 0);

        drop(restarted);
    }

    /// A window a successor replaced must never publish again: the abandoned
    /// heartbeat may still reach the accounting lock after its own abort was
    /// requested, and the identity it carries is what makes it observe that the
    /// in-flight totals now belong to another operation.
    ///
    /// Without that identity the heartbeat would publish the successor's window
    /// under its own operation id and its own clock, so a client would render a
    /// running operation that already ended.
    #[tokio::test]
    async fn an_abandoned_window_never_publishes_under_its_successor() {
        // The windows run on a compressed tick, so the abandoned heartbeat
        // really reaches the lock while the successor owns it.
        const TEST_TICK: Duration = Duration::from_millis(20);
        let (state, mut rx, root) = test_state();

        let abandoned = state.begin_usage_with_tick("op-abandoned", TEST_TICK).await;
        let abandoned_id = abandoned.operation_id.clone();
        abandoned.observe(&usage(100, 40)).await;

        // The successor installs its own window while the abandoned guard is
        // still alive: the abandoned totals are discarded with that window.
        let successor = state.begin_usage_with_tick("op-successor", TEST_TICK).await;
        let successor_id = successor.operation_id.clone();
        successor.observe(&usage(3, 2)).await;
        while rx.try_recv().is_ok() {}

        // The successor heartbeat proves itself while the abandoned heartbeat
        // runs out its remaining ticks: every publication of that stretch must
        // name the successor, and none may carry the abandoned identifier.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        let mut successor_live = false;
        let mut leaked = 0usize;
        while !successor_live && std::time::Instant::now() < deadline {
            while let Ok(raw) = rx.try_recv() {
                let Some(event) = decode_usage_event(&raw) else {
                    continue;
                };
                if event.operation_id == abandoned_id {
                    leaked += 1;
                    continue;
                }
                if event.operation_id == successor_id
                    && event
                        .snapshot
                        .cur
                        .as_ref()
                        .is_some_and(|cur| cur.duration_ms > 0)
                {
                    successor_live = true;
                }
            }
            if !successor_live {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            successor_live,
            "the successor heartbeat must still publish its live duration"
        );
        assert_eq!(
            leaked, 0,
            "an abandoned window must never publish the successor's state"
        );

        // The successor's flush folds exactly the totals its own window
        // observed; the abandoned observation was discarded with its window.
        successor.flush().await;
        let snapshot = state.usage_snapshot().await;
        assert_eq!(snapshot.operations, 1, "only the successor flushed");
        assert_eq!(snapshot.session.prompt_tokens, 3);
        assert_eq!(snapshot.session.completion_tokens, 2);
        assert_eq!(snapshot.session.total_tokens, 5);
        assert_eq!(snapshot.prev.total_tokens, 5);
        assert!(
            snapshot.cur.is_none(),
            "the successor's flush closes its own window"
        );

        drop(abandoned);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A flush closes only the window it still owns: a superseded guard can
    /// neither take the successor's in-flight totals nor publish them under an
    /// identifier they do not belong to.
    #[tokio::test]
    async fn a_superseded_flush_folds_only_its_own_window() {
        // The production cadence is irrelevant here, so the heartbeat is parked
        // far beyond the test to keep the published events deterministic.
        const PARKED_TICK: Duration = Duration::from_secs(3600);
        let (state, mut rx, root) = test_state();

        let abandoned = state
            .begin_usage_with_tick("op-superseded", PARKED_TICK)
            .await;
        abandoned.observe(&usage(100, 40)).await;
        let successor = state.begin_usage_with_tick("op-current", PARKED_TICK).await;
        successor.observe(&usage(3, 2)).await;
        while rx.try_recv().is_ok() {}

        abandoned.flush().await;
        let in_flight = state.usage_snapshot().await;
        assert!(
            in_flight.cur.is_some(),
            "a superseded flush must leave the successor's window installed"
        );
        assert_eq!(
            in_flight.operation_totals().total_tokens,
            5,
            "the successor's in-flight totals stay its own"
        );
        assert_eq!(in_flight.operations, 0, "a superseded flush counts nothing");
        assert!(
            rx.try_recv().is_err(),
            "a superseded flush publishes nothing"
        );

        successor.flush().await;
        let flushed = state.usage_snapshot().await;
        assert!(flushed.cur.is_none());
        assert_eq!(flushed.operations, 1);
        assert_eq!(flushed.session.total_tokens, 5);
        assert_eq!(flushed.prev.total_tokens, 5);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Resetting clears the project lifetime in memory and on disk, while the
    /// session counters keep describing what this process already spent.
    #[tokio::test]
    async fn reset_zeroes_the_project_lifetime_and_keeps_the_session() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage("op-reset").await;
        guard.observe(&usage(10, 5)).await;
        guard.flush().await;

        let snapshot = state.reset_lifetime_usage().await;
        assert!(snapshot.lifetime.is_empty());
        assert_eq!(snapshot.lifetime_operations, 0);
        assert_eq!(snapshot.session.total_tokens, 15);
        assert_eq!(snapshot.operations, 1);

        let stored = read_stored_lifetime(&root);
        assert_eq!(stored["lifetime"]["total_tokens"].as_u64(), Some(0));
        assert_eq!(stored["lifetime_operations"].as_u64(), Some(0));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A corrupt telemetry document never blocks startup: the totals start from
    /// zero and the next flush rewrites the document.
    #[test]
    fn unreadable_telemetry_falls_back_to_empty_totals() {
        let root = scratch_root("usage-corrupt");
        let _ = std::fs::create_dir_all(root.join(TELEMETRY_DIR));
        std::fs::write(telemetry_path(&root), "{ not telemetry").unwrap();

        let snapshot = load_initial_snapshot(&root);
        assert!(snapshot.lifetime.is_empty());
        assert_eq!(snapshot.lifetime_operations, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rebinding to another repository adopts the lifetime totals persisted
    /// there, while the session totals keep describing what this process has
    /// already spent.
    #[tokio::test]
    async fn rebinding_adopts_the_telemetry_of_the_new_repository() {
        let (state, _rx, first_root) = test_state();
        let second_root = scratch_root("usage-rebind");

        let guard = state.begin_usage("op-first").await;
        guard.observe(&usage(10, 5)).await;
        guard.flush().await;
        assert_eq!(state.usage_snapshot().await.lifetime.total_tokens, 15);

        // The second repository already carries its own, much smaller,
        // telemetry document.
        let second = state_at(&second_root);
        let stored = second.begin_usage("op-second").await;
        stored.observe(&usage(7, 3)).await;
        stored.flush().await;
        drop(second);

        // Rebinding re-anchors the context first and then reloads the
        // telemetry of the newly bound repository.
        let rebind_root = second_root.clone();
        state
            .mutate_context(move |ctx| ctx.set_repo_root(rebind_root))
            .await
            .expect("context mutation runs");
        state.reload_usage_lifetime().await;

        let snapshot = state.usage_snapshot().await;
        assert_eq!(snapshot.lifetime.total_tokens, 10);
        assert_eq!(snapshot.lifetime_operations, 1);
        assert_eq!(
            snapshot.session.total_tokens, 15,
            "the session scope belongs to the process, not to the repository"
        );

        let _ = std::fs::remove_dir_all(&first_root);
        let _ = std::fs::remove_dir_all(&second_root);
    }
}
