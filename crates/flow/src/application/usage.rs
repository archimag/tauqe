//! Session accounting, window ownership, timing, and typed usage publication.

use super::context::ContextSession;
use super::usage_totals::UsageReceipt;
use crate::api::{next_operation_id, Application};
use crate::api::{UsageTotals, UsageTotalsSnapshot};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;
use tauqe_core::execution::usage::UsageSink;
use tauqe_core::providers::ModelUsageInfo;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::Instant;

type UsagePublisher = Arc<dyn Fn(&str, &UsageTotalsSnapshot) + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct UsageSession {
    ledger: Arc<StdMutex<UsageLedger>>,
    window_gate: Arc<Mutex<()>>,
    publication_gate: Arc<Mutex<()>>,
}

impl UsageSession {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            ledger: Arc::new(StdMutex::new(UsageLedger::new(
                crate::application::infrastructure::usage::load_initial_snapshot(root),
            ))),
            window_gate: Arc::new(Mutex::new(())),
            publication_gate: Arc::new(Mutex::new(())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, UsageLedger> {
        self.ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    #[allow(dead_code)] // Used by request-bound accounting once its routes are migrated.
    pub(crate) fn record_window(&self, window: u64, report: &ModelUsageInfo) {
        if !self.lock().observe(window, report) {
            tracing::warn!(window_id = window, "Discarding late usage report");
        }
    }

    pub(crate) async fn snapshot(&self) -> UsageTotalsSnapshot {
        self.lock().snapshot().clone()
    }

    pub(crate) async fn begin<F>(
        &self,
        context: &ContextSession,
        operation_id: &str,
        publish: F,
    ) -> UsageGuard
    where
        F: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        self.begin_with_tick(context, operation_id, LIVE_DURATION_TICK, Arc::new(publish))
            .await
    }

    async fn begin_with_tick(
        &self,
        context: &ContextSession,
        operation_id: &str,
        tick: Duration,
        publisher: UsagePublisher,
    ) -> UsageGuard {
        let ownership = Arc::clone(&self.window_gate).lock_owned().await;
        let root = context.repository_root().await;
        let publication = self.publication_gate.lock().await;
        let window = self.lock().begin();
        let started = Instant::now();
        let ticker = spawn_live_duration_ticker(
            self,
            window,
            operation_id,
            started,
            tick,
            Arc::clone(&publisher),
        );
        let guard = UsageGuard {
            usage: self.clone(),
            operation_id: operation_id.to_owned(),
            window,
            started,
            ticker,
            publisher,
            root,
            ownership: Some(ownership),
        };
        guard.publish();
        drop(publication);
        guard
    }
}

async fn persist_lifetime(root: PathBuf, lifetime: &UsageTotals, operations: u64) {
    crate::application::infrastructure::usage::persist_lifetime(root, lifetime, operations).await;
}

/// Identity of one usage accounting window.
type WindowId = u64;

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
/// is the guarantee the accounting layer itself provides: observations and
/// closure act only on the window whose identity they carry. The window lease
/// keeps a successor from opening until persistence and final publication end.
#[derive(Default)]
struct UsageLedger {
    snapshot: UsageTotalsSnapshot,
    window: Option<WindowId>,
    cost_complete: bool,
}

impl UsageLedger {
    /// Wraps the snapshot the daemon starts from.
    ///
    /// No window owns the in-flight scope before the first operation opens
    /// one, so a fresh ledger carries the persisted project totals alone.
    fn new(snapshot: UsageTotalsSnapshot) -> Self {
        Self {
            snapshot,
            window: None,
            cost_complete: true,
        }
    }

    /// Returns the authoritative snapshot.
    fn snapshot(&self) -> &UsageTotalsSnapshot {
        &self.snapshot
    }

    /// Returns the authoritative snapshot for the lifetime scopes to mutate.
    fn snapshot_mut(&mut self) -> &mut UsageTotalsSnapshot {
        &mut self.snapshot
    }

    /// Reports whether `window` still owns the in-flight scope.
    fn owns(&self, window: WindowId) -> bool {
        self.window == Some(window)
    }

    /// Opens only after the prior owner has finished its terminal publication.
    fn begin(&mut self) -> WindowId {
        assert!(self.window.is_none(), "accounting window still open");
        let window = next_window_id();
        self.window = Some(window);
        self.cost_complete = true;
        self.snapshot.cur = Some(UsageTotals::default());
        window
    }

    /// Adds one provider report to `window`'s in-flight totals.
    ///
    /// A window that no longer owns the scope reports nothing: its successor
    /// installed its own in-flight totals, and folding a stale report into them
    /// would attribute one operation's spend to another.
    fn observe(&mut self, window: WindowId, usage: &ModelUsageInfo) -> bool {
        if !self.owns(window) {
            return false;
        }
        self.cost_complete &= usage.cost.is_some();
        self.snapshot
            .cur
            .get_or_insert_with(UsageTotals::default)
            .accumulate(&UsageTotals::from_usage(usage));
        true
    }

    /// Closes `window` exactly once, folding its totals and returning a receipt.
    fn close(&mut self, window: WindowId, elapsed_ms: u64) -> Option<UsageReceipt> {
        if !self.owns(window) {
            return None;
        }
        self.window = None;
        let mut completed = self.snapshot.cur.take().unwrap_or_default();
        completed.duration_ms = completed.duration_ms.saturating_add(elapsed_ms);
        self.snapshot.session.accumulate(&completed);
        self.snapshot.operations = self.snapshot.operations.saturating_add(1);
        self.snapshot.lifetime.accumulate(&completed);
        self.snapshot.lifetime_operations = self.snapshot.lifetime_operations.saturating_add(1);
        self.snapshot.prev = completed.clone();
        Some(UsageReceipt {
            window_id: window,
            totals: completed,
            cost_complete: self.cost_complete,
            snapshot: self.snapshot.clone(),
        })
    }
}

/// Synchronous receiver for an admitted request; late reports stay isolated.
struct WindowUsageSink {
    usage: UsageSession,
    window: WindowId,
}

impl UsageSink for WindowUsageSink {
    fn record(&self, usage: &ModelUsageInfo) {
        if !self.usage.lock().observe(self.window, usage) {
            tracing::warn!(window_id = self.window, "Discarding late usage report");
        }
    }
}

/// Publish through the caller's synchronous transport-independent port.
async fn publish_snapshot(state: &Application, operation_id: &str, publish: &UsagePublisher) {
    let snapshot = state.usage_snapshot().await;
    publish(operation_id, &snapshot);
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
/// [`UsageGuard::flush`]. Dropping an unflushed legacy guard folds its accepted
/// totals in memory; it does not start asynchronous persistence. While the
/// window is open, its heartbeat republishes the snapshot with the live
/// in-flight duration, so a client renders a running clock for the current
/// operation instead of a dash.
///
/// The guard carries two identities. The operation identifier is the one the
/// caller already announces through its own lifecycle events, so every snapshot
/// this window publishes names the operation a client is rendering. The window
/// identifier stays internal: every operation the guard performs is checked
/// against it under the accounting lock, so a late request cannot charge a
/// successor's scope.
pub struct UsageGuard {
    usage: UsageSession,
    root: PathBuf,
    ownership: Option<OwnedMutexGuard<()>>,
    /// Identifier the window publishes under: the one the caller announces
    /// through its own lifecycle events.
    operation_id: String,
    /// Identity of the accounting window this guard owns.
    window: WindowId,
    started: Instant,
    /// Live duration heartbeat of this window, silenced on flush and on drop.
    ticker: tokio::task::JoinHandle<()>,
    publisher: UsagePublisher,
}

impl Drop for UsageGuard {
    fn drop(&mut self) {
        // The heartbeat belongs to the window: an abandoned guard silences its
        // own heartbeat, so a dropped window never keeps publishing a running
        // clock for an operation that is no longer executing.
        self.ticker.abort();
        if self.ownership.is_some() {
            // Abandoned legacy windows retain their accepted spend in memory.
            // Drop never starts asynchronous persistence.
            self.usage
                .lock()
                .close(self.window, self.started.elapsed().as_millis() as u64);
        }
    }
}

/// Spawns the live duration heartbeat of one accounting window.
///
/// The published duration is derived, never stored: the authoritative
/// `cur.duration_ms` stays zero until [`UsageGuard::flush`] folds in the final
/// measurement, so a heartbeat can never leak into the session totals. The
/// heartbeat holds the publication gate across its synchronous send and checks
/// the window identity under the ledger lock, which keeps it ordered against
/// the final publish: once its window closes it exits without producing a
/// stale "still running" snapshot. The cadence arrives as `tick`, so a test can
/// compress it without touching the production default.
fn spawn_live_duration_ticker(
    usage: &UsageSession,
    window: WindowId,
    operation_id: &str,
    started: Instant,
    tick: Duration,
    publisher: UsagePublisher,
) -> tokio::task::JoinHandle<()> {
    let usage = usage.clone();
    let operation_id = operation_id.to_string();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            let _publication = usage.publication_gate.lock().await;
            // The publication gate orders the heartbeat against completion;
            // the ledger lock is released before calling the publisher.
            let ledger = usage.lock();
            if !ledger.owns(window) {
                break;
            }
            let mut live = ledger.snapshot().clone();
            if let Some(cur) = live.cur.as_mut() {
                cur.duration_ms = started.elapsed().as_millis() as u64;
            }
            drop(ledger);
            publisher(&operation_id, &live);
        }
    })
}

impl Application {
    #[cfg(feature = "test-support")]
    pub async fn test_begin_usage<F>(&self, operation_id: &str, publish: F) -> UsageGuard
    where
        F: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        self.usage_session()
            .begin(self.context_session(), operation_id, publish)
            .await
    }

    #[cfg(feature = "test-support")]
    pub async fn test_reload_usage_lifetime<F>(&self, publish: F)
    where
        F: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        self.reload_usage_lifetime(publish).await;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn test_begin_usage_with_tick<F>(
        &self,
        operation_id: &str,
        tick: Duration,
        publish: F,
    ) -> UsageGuard
    where
        F: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        self.usage_session()
            .begin_with_tick(
                self.context_session(),
                operation_id,
                tick,
                Arc::new(publish),
            )
            .await
    }

    pub async fn usage_snapshot(&self) -> UsageTotalsSnapshot {
        self.usage_session().snapshot().await
    }

    /// Clears the persisted project lifetime totals.
    ///
    /// Only the lifetime scope is cleared. The session counters keep describing
    /// what this process has already spent, so a reset never makes the running
    /// session appear free. The cleared totals are written to disk immediately
    /// and the resulting snapshot is published before it is returned, so a
    /// client renders the same state the next restart will read.
    async fn reset_lifetime_usage<F>(&self, publish: F) -> UsageTotalsSnapshot
    where
        F: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        let publisher: UsagePublisher = Arc::new(publish);
        let snapshot = {
            let mut ledger = self.usage_session().lock();
            {
                let snapshot = ledger.snapshot_mut();
                snapshot.lifetime = UsageTotals::default();
                snapshot.lifetime_operations = 0;
            }
            ledger.snapshot().clone()
        };
        persist_lifetime(
            self.repository_root().await,
            &snapshot.lifetime,
            snapshot.lifetime_operations,
        )
        .await;
        let operation_id = format!("reset-{}", next_operation_id());
        publish_snapshot(self, &operation_id, &publisher).await;
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
    pub(crate) async fn reload_usage_lifetime<F>(&self, publish: F)
    where
        F: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        let publisher: UsagePublisher = Arc::new(publish);
        let loaded =
            crate::application::infrastructure::usage::load_lifetime(self.repository_root().await)
                .await;
        {
            let mut ledger = self.usage_session().lock();
            let snapshot = ledger.snapshot_mut();
            snapshot.lifetime = loaded.0;
            snapshot.lifetime_operations = loaded.1;
        }
        let operation_id = format!("rebind-{}", next_operation_id());
        publish_snapshot(self, &operation_id, &publisher).await;
    }
}

impl UsageGuard {
    #[allow(dead_code)] // Used by the shared accounting supervisor after route migration.
    pub(crate) fn window_id(&self) -> u64 {
        self.window
    }

    pub fn sink(&self) -> Arc<dyn UsageSink> {
        Arc::new(WindowUsageSink {
            usage: self.usage.clone(),
            window: self.window,
        })
    }

    /// Adds the usage reported by one provider call to the in-flight totals.
    ///
    /// A provider may answer with token counts only, a cost only, or nothing at
    /// all; the totals accumulate whatever was reported and stay additive
    /// across the several calls a multi-round operation makes. A late report
    /// for a closed window records and publishes nothing.
    pub async fn observe(&self, usage: &ModelUsageInfo) {
        let publication_gate = Arc::clone(&self.usage.publication_gate);
        let _publication = publication_gate.lock().await;
        let applied = {
            let mut ledger = self.usage.lock();
            ledger.observe(self.window, usage)
        };
        if applied {
            self.publish();
        } else {
            tracing::warn!(
                operation_id = self.operation_id.as_str(),
                "Usage window was already closed; the observation is discarded",
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
        if let Err(error) = self.finish().await {
            tracing::error!(%error, "Usage window completion failed");
        }
    }

    /// Close once, retaining the lease through persistence and publication.
    pub async fn finish(mut self) -> Result<UsageReceipt, String> {
        self.ticker.abort();
        let ownership = self
            .ownership
            .take()
            .ok_or_else(|| "usage window already finished".to_string())?;
        let usage = self.usage.clone();
        let root = self.root.clone();
        let window = self.window;
        let started = self.started;
        let publisher = Arc::clone(&self.publisher);
        let operation_id = self.operation_id.clone();
        tokio::spawn(async move {
            let publication_gate = Arc::clone(&usage.publication_gate);
            let _publication = publication_gate.lock().await;
            let receipt = usage
                .lock()
                .close(window, started.elapsed().as_millis() as u64)
                .ok_or_else(|| "usage window ownership was lost".to_string())?;
            // Persistence is best effort, but precedes the final publication.
            persist_lifetime(
                root,
                &receipt.snapshot.lifetime,
                receipt.snapshot.lifetime_operations,
            )
            .await;
            publisher(&operation_id, &receipt.snapshot);
            drop(ownership);
            Ok(receipt)
        })
        .await
        .map_err(|error| format!("usage completion task failed: {error}"))?
    }

    /// Publishes the current snapshot to the client.
    fn publish(&self) {
        let snapshot = self.usage.lock().snapshot().clone();
        (self.publisher)(&self.operation_id, &snapshot);
    }
}

impl Application {
    pub async fn reset_project_usage(
        &self,
        observer: Arc<dyn crate::api::ApplicationObserver>,
    ) -> Result<UsageTotalsSnapshot, crate::api::OperationInProgress> {
        if self.active_operation_id().is_some() {
            return Err(crate::api::OperationInProgress);
        }
        Ok(self
            .reset_lifetime_usage(move |id, snapshot| {
                observer.publish(
                    id,
                    crate::api::ApplicationEvent::UsageTotals(snapshot.clone()),
                );
            })
            .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tauqe_core::config::AppConfig;

    fn application(root: &Path) -> Application {
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(AppConfig::default(), root.to_path_buf(), history)
    }

    fn report(tokens: u32) -> ModelUsageInfo {
        ModelUsageInfo {
            prompt_tokens: tokens,
            total_tokens: tokens,
            cost: Some(0.25),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn headless_accounting_publishes_and_restores_only_project_lifetime() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let publications = Arc::new(StdMutex::new(Vec::new()));
        let observed = Arc::clone(&publications);
        let guard = app
            .test_begin_usage("headless-operation", move |id, snapshot| {
                observed
                    .lock()
                    .unwrap()
                    .push((id.to_string(), snapshot.clone()));
            })
            .await;
        guard.observe(&report(5)).await;
        guard.flush().await;
        {
            let events = publications.lock().unwrap();
            assert!(events.iter().all(|(id, _)| id == "headless-operation"));
            assert!(events.first().unwrap().1.cur.is_some());
            let final_snapshot = &events.last().unwrap().1;
            assert!(final_snapshot.cur.is_none());
            assert_eq!(final_snapshot.session.total_tokens, 5);
            assert_eq!(final_snapshot.lifetime.total_tokens, 5);
            assert_eq!(final_snapshot.operations, 1);
            assert_eq!(final_snapshot.lifetime_operations, 1);
        }
        let restarted = application(root.path());
        let restored = restarted.usage_snapshot().await;
        assert_eq!(restored.lifetime.total_tokens, 5);
        assert_eq!(restored.lifetime_operations, 1);
        assert!(restored.session.is_empty());
        assert_eq!(restored.operations, 0);
        assert!(restored.cur.is_none());
    }

    #[tokio::test]
    async fn cloned_applications_wait_for_completion_and_discard_late_reports() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let peer = app.clone();
        let publications = Arc::new(StdMutex::new(Vec::new()));
        let observed = Arc::clone(&publications);
        let stale = app
            .test_begin_usage_with_tick("stale", Duration::from_secs(60), move |id, snapshot| {
                observed
                    .lock()
                    .unwrap()
                    .push((id.to_string(), snapshot.clone()));
            })
            .await;
        stale.observe(&report(100)).await;
        let late = stale.sink();
        let waiting = tokio::spawn(async move {
            peer.test_begin_usage_with_tick("current", Duration::from_secs(60), |_, _| {})
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "a successor waits for the final publication"
        );
        let receipt = stale.finish().await.unwrap();
        assert_eq!(receipt.totals.total_tokens, 100);
        assert!(receipt.cost_complete);
        assert_eq!(receipt.snapshot.operations, 1);
        let successor = waiting.await.unwrap();
        let before = publications.lock().unwrap().len();
        late.record(&report(200));
        successor.observe(&report(5)).await;
        let second = successor.finish().await.unwrap();
        assert_ne!(receipt.window_id, second.window_id);
        assert_eq!(second.snapshot.session.total_tokens, 105);
        assert_eq!(second.snapshot.operations, 2);
        assert_eq!(publications.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn failed_persistence_still_publishes_the_terminal_in_memory_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let document = root.path().join(".tauqe/usage.json");
        std::fs::create_dir_all(&document).unwrap();
        let publications = Arc::new(StdMutex::new(Vec::new()));
        let observed = Arc::clone(&publications);
        let guard = app
            .test_begin_usage("storage-failure", move |_, snapshot| {
                observed.lock().unwrap().push(snapshot.clone());
            })
            .await;
        guard.observe(&report(5)).await;
        guard.flush().await;
        let events = publications.lock().unwrap();
        let snapshot = events.last().unwrap();
        assert!(snapshot.cur.is_none());
        assert_eq!(snapshot.session.total_tokens, 5);
        assert_eq!(snapshot.lifetime.total_tokens, 5);
        assert!(document.is_dir());
    }
}
