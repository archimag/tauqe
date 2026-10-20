//! Session-wide usage accounting for every operation kind.
//!
//! Token, cost, and duration totals previously lived wherever an operation
//! happened to need them: the session cost was a bare `f64` on the state, and
//! every handler derived its own view of the tokens it had spent. That made the
//! interface unable to answer a simple question — *what has this session cost
//! so far, and what is the current operation spending right now?* — without
//! replaying per-operation events.
//!
//! This module owns the single authoritative [`UsageTotalsSnapshot`] behind
//! [`AppState::usage`]. An operation opens a [`UsageGuard`], reports the usage
//! each provider call returns through [`UsageGuard::observe`], and closes the
//! window with [`UsageGuard::flush`], which folds the operation into the
//! session totals together with the duration the harness measured itself.
//! Every transition publishes a `model/usageTotals` event, so a client renders
//! one consistent snapshot instead of stitching counters together.
//!
//! The project lifetime scope outlives the process. It accumulates every
//! completed operation and is persisted in `<repo_root>/.tauqe/usage.json`,
//! a directory Git ignores. The document is read once when the state is
//! created and rewritten after every flush through the atomic-write policy of
//! the core engine, so a crash publishes either the previous complete document
//! or the new complete one, never a truncated one. Reading and writing run on
//! the blocking pool: the daemon runtime must never stall on the file system.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

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
    tauqe_core::atomic_write::write_atomic(path, &payload)
        .map_err(|err| format!("{:#}", err))
}

/// Persists the project lifetime totals off the async worker pool.
async fn persist_lifetime(state: &Arc<AppState>, lifetime: &UsageTotals, operations: u64) {
    let path = telemetry_path(state.repo_root().await.as_path());
    let lifetime = lifetime.clone();
    let written =
        tauqe_core::blocking::run_blocking_async(move || write_document(&path, &lifetime, operations))
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

/// Publishes the current snapshot to the client.
async fn publish_snapshot(state: &AppState, operation_id: &str) {
    let snapshot = state.usage.lock().await.clone();
    state.out.send_event(&Event {
        method: events::MODEL_USAGE_TOTALS.to_string(),
        params: serde_json::to_value(ModelUsageTotalsEvent {
            operation_id: operation_id.to_string(),
            snapshot,
        })
        .ok(),
    });
}

/// Open accounting window for one operation.
///
/// The window starts when the operation begins and closes on
/// [`UsageGuard::flush`]. Dropping it without flushing abandons the window; the
/// next [`AppState::begin_usage`] discards the abandoned totals, so a failed
/// operation can never leave a permanently in-flight window behind.
pub struct UsageGuard {
    state: Arc<AppState>,
    operation_id: String,
    started: Instant,
}

impl AppState {
    /// Opens a usage accounting window for one operation.
    ///
    /// A window left behind by an operation that failed before flushing is
    /// discarded here, so one abandoned turn never inflates the in-flight
    /// totals of the next one.
    pub async fn begin_usage(self: &Arc<Self>) -> UsageGuard {
        let operation_id = format!("op-{}", next_operation_id());
        {
            let mut snapshot = self.usage.lock().await;
            snapshot.cur = Some(UsageTotals::default());
        }
        let guard = UsageGuard {
            state: Arc::clone(self),
            operation_id,
            started: Instant::now(),
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
            let mut guard = self.usage.lock().await;
            guard.lifetime = UsageTotals::default();
            guard.lifetime_operations = 0;
            guard.clone()
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
        let loaded = match tauqe_core::blocking::run_blocking_async(move || read_document(&path))
            .await
        {
            Ok(Some((lifetime, lifetime_operations))) => (lifetime, lifetime_operations),
            Ok(None) => (UsageTotals::default(), 0),
            Err(err) => {
                tracing::warn!(error = %err, "Project telemetry read task failed");
                (UsageTotals::default(), 0)
            }
        };
        {
            let mut snapshot = self.usage.lock().await;
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
    /// across the several calls a multi-round operation makes.
    pub async fn observe(&self, usage: &ModelUsageInfo) {
        {
            let mut snapshot = self.state.usage.lock().await;
            snapshot
                .cur
                .get_or_insert_with(UsageTotals::default)
                .accumulate(&UsageTotals::from_usage(usage));
        }
        self.publish().await;
    }

    /// Closes the window, folding the operation into both totals and persisting
    /// the project total.
    ///
    /// The duration is measured by the harness rather than reported by the
    /// provider, so it reflects how long the operation actually held the
    /// developer's turn. The project lifetime is accumulated under the same
    /// lock as the session totals, so the two scopes can never disagree about
    /// the operation that just finished.
    pub async fn flush(self) {
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let (lifetime, lifetime_operations) = {
            let mut snapshot = self.state.usage.lock().await;
            let mut completed = snapshot.cur.take().unwrap_or_default();
            completed.duration_ms = completed.duration_ms.saturating_add(elapsed_ms);
            snapshot.session.accumulate(&completed);
            snapshot.prev = completed.clone();
            snapshot.operations = snapshot.operations.saturating_add(1);
            snapshot.lifetime.accumulate(&completed);
            snapshot.lifetime_operations = snapshot.lifetime_operations.saturating_add(1);
            (snapshot.lifetime.clone(), snapshot.lifetime_operations)
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
        let root = std::env::temp_dir().join(format!("tauqe-usage-{}", next_operation_id()));
        let _ = std::fs::create_dir_all(&root);
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
        AppState::new(AppConfig::default(), root.to_path_buf(), history_sender, out)
    }

    fn read_stored_lifetime(root: &Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(telemetry_path(root)).expect("telemetry file exists");
        serde_json::from_str(&raw).expect("telemetry file is valid JSON")
    }

    /// Every flushed operation folds into the session totals, and the previous
    /// operation keeps its own totals so the footer can show both.
    #[tokio::test]
    async fn flushes_operations_into_the_session_totals() {
        let (state, mut rx, root) = test_state();

        let first = state.begin_usage().await;
        first.observe(&usage(100, 40)).await;
        first.flush().await;

        let second = state.begin_usage().await;
        second.observe(&usage(10, 5)).await;
        second.observe(&usage(2, 3)).await;
        second.flush().await;

        let snapshot = state.usage.lock().await.clone();
        assert_eq!(snapshot.operations, 2);
        assert_eq!(snapshot.session.prompt_tokens, 112);
        assert_eq!(snapshot.session.completion_tokens, 48);
        assert_eq!(snapshot.session.total_tokens, 160);
        assert!((snapshot.session.cost - 0.75).abs() < f64::EPSILON);
        assert_eq!(snapshot.prev.total_tokens, 20);
        assert!(snapshot.cur.is_none(), "a flushed window leaves no residue");

        // Each begin, observe, and flush publishes the snapshot: 3 + 4.
        let mut published = 0;
        while rx.try_recv().is_ok() {
            published += 1;
        }
        assert_eq!(published, 7, "every transition publishes the snapshot");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The in-flight operation stays visible before it is flushed.
    #[tokio::test]
    async fn keeps_the_in_flight_operation_visible_until_flush() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage().await;
        guard.observe(&usage(3, 2)).await;

        let in_flight = state.usage.lock().await.clone();
        assert_eq!(in_flight.session.total_tokens, 0);
        assert_eq!(in_flight.operation_totals().total_tokens, 5);

        guard.flush().await;
        let flushed = state.usage.lock().await.clone();
        assert!(flushed.cur.is_none());
        assert_eq!(flushed.session.total_tokens, 5);
        assert_eq!(flushed.prev.total_tokens, 5);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A completed operation is folded into the project lifetime and written to
    /// `.tauqe/usage.json`, so a restarted daemon continues the same totals
    /// while its session scope starts from zero.
    #[tokio::test]
    async fn flush_persists_the_project_lifetime() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage().await;
        guard.observe(&usage(100, 40)).await;
        guard.flush().await;

        let stored = read_stored_lifetime(&root);
        assert_eq!(stored["lifetime"]["total_tokens"].as_u64(), Some(140));
        assert_eq!(stored["lifetime_operations"].as_u64(), Some(1));

        let restarted = state_at(&root);
        let snapshot = restarted.usage.lock().await.clone();
        assert_eq!(snapshot.lifetime.total_tokens, 140);
        assert_eq!(snapshot.lifetime_operations, 1);
        assert!(
            snapshot.session.is_empty(),
            "the session scope restarts with the process"
        );
        assert_eq!(snapshot.operations, 0);

        drop(restarted);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Resetting clears the project lifetime in memory and on disk, while the
    /// session counters keep describing what this process already spent.
    #[tokio::test]
    async fn reset_zeroes_the_project_lifetime_and_keeps_the_session() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage().await;
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
        let root = std::env::temp_dir().join(format!("tauqe-usage-corrupt-{}", next_operation_id()));
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
        let second_root =
            std::env::temp_dir().join(format!("tauqe-usage-rebind-{}", next_operation_id()));
        let _ = std::fs::create_dir_all(&second_root);

        let guard = state.begin_usage().await;
        guard.observe(&usage(10, 5)).await;
        guard.flush().await;
        assert_eq!(state.usage.lock().await.lifetime.total_tokens, 15);

        // The second repository already carries its own, much smaller,
        // telemetry document.
        let second = state_at(&second_root);
        let stored = second.begin_usage().await;
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

        let snapshot = state.usage.lock().await.clone();
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
