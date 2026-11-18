//! Session accounting, window ownership, timing, and typed usage publication.

use crate::api::{next_operation_id, Application};
use crate::api::{UsageTotals, UsageTotalsSnapshot};
use crate::providers::ModelUsageInfo;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

type UsagePublisher = Arc<dyn Fn(&str, &UsageTotalsSnapshot) + Send + Sync>;

#[derive(Default)]
pub(crate) struct UsageSession {
    ledger: Mutex<UsageLedger>,
}

impl UsageSession {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            ledger: Mutex::new(UsageLedger::new(
                crate::infrastructure::usage::load_initial_snapshot(root),
            )),
        }
    }
}

async fn persist_lifetime(state: &Application, lifetime: &UsageTotals, operations: u64) {
    crate::infrastructure::usage::persist_lifetime(
        state.repository_root().await,
        lifetime,
        operations,
    )
    .await;
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
/// is the guarantee the accounting layer itself provides: the heartbeat, an
/// observation, and a flush act on the in-flight scope only while the recorded
/// identity is their own, so a window a successor already replaced can neither
/// publish nor fold anything that belongs to that successor, whatever order the
/// two tasks reach the lock in. The guarantee therefore never depends on the
/// operation slot or on a task abort having taken effect.
#[derive(Default)]
struct UsageLedger {
    snapshot: UsageTotalsSnapshot,
    window: Option<WindowId>,
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
/// [`UsageGuard::flush`]. Dropping it without flushing abandons the window; the
/// next [`Application::begin_usage`] discards the abandoned totals, so a failed
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
    state: Application,
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
    state: &Application,
    window: WindowId,
    operation_id: &str,
    started: Instant,
    tick: Duration,
    publisher: UsagePublisher,
) -> tokio::task::JoinHandle<()> {
    let state = state.clone();
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
            let ledger = state.usage_session().ledger.lock().await;
            if !ledger.owns(window) {
                break;
            }
            let mut live = ledger.snapshot().clone();
            if let Some(cur) = live.cur.as_mut() {
                cur.duration_ms = started.elapsed().as_millis() as u64;
            }
            publisher(&operation_id, &live);
        }
    })
}

impl Application {
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
    pub async fn begin_usage<F>(&self, operation_id: &str, publish: F) -> UsageGuard
    where
        F: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        self.begin_usage_with_tick(operation_id, LIVE_DURATION_TICK, Arc::new(publish))
            .await
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
        self.begin_usage_with_tick(operation_id, tick, Arc::new(publish))
            .await
    }

    pub async fn usage_snapshot(&self) -> UsageTotalsSnapshot {
        self.usage_session().ledger.lock().await.snapshot().clone()
    }

    /// Opens a usage accounting window whose heartbeat runs on `tick`.
    ///
    /// [`Application::begin_usage`] passes the production cadence, while a test
    /// passes a compressed one, so no assertion has to wait a whole production
    /// tick. The seam is private to this module and never changes the cadence
    /// the daemon publishes with.
    async fn begin_usage_with_tick(
        &self,
        operation_id: &str,
        tick: Duration,
        publisher: UsagePublisher,
    ) -> UsageGuard {
        let window = {
            let mut ledger = self.usage_session().ledger.lock().await;
            ledger.begin()
        };
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
            state: self.clone(),
            operation_id: operation_id.to_string(),
            window,
            started,
            ticker,
            publisher,
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
    pub async fn reset_lifetime_usage<F>(&self, publish: F) -> UsageTotalsSnapshot
    where
        F: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        let publisher: UsagePublisher = Arc::new(publish);
        let snapshot = {
            let mut ledger = self.usage_session().ledger.lock().await;
            {
                let snapshot = ledger.snapshot_mut();
                snapshot.lifetime = UsageTotals::default();
                snapshot.lifetime_operations = 0;
            }
            ledger.snapshot().clone()
        };
        persist_lifetime(self, &snapshot.lifetime, snapshot.lifetime_operations).await;
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
    pub async fn reload_usage_lifetime<F>(&self, publish: F)
    where
        F: Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static,
    {
        let publisher: UsagePublisher = Arc::new(publish);
        let loaded =
            crate::infrastructure::usage::load_lifetime(self.repository_root().await).await;
        {
            let mut ledger = self.usage_session().ledger.lock().await;
            let snapshot = ledger.snapshot_mut();
            snapshot.lifetime = loaded.0;
            snapshot.lifetime_operations = loaded.1;
        }
        let operation_id = format!("rebind-{}", next_operation_id());
        publish_snapshot(self, &operation_id, &publisher).await;
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
            let mut ledger = self.state.usage_session().ledger.lock().await;
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
            let mut ledger = self.state.usage_session().ledger.lock().await;
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
        // Attempt persistence before publication. A failed write remains best
        // effort and does not suppress the terminal in-memory snapshot.
        persist_lifetime(&self.state, &lifetime, lifetime_operations).await;
        self.publish().await;
    }

    /// Publishes the current snapshot to the client.
    async fn publish(&self) {
        publish_snapshot(&self.state, &self.operation_id, &self.publisher).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use std::sync::Mutex as StdMutex;

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
            .begin_usage("headless-operation", move |id, snapshot| {
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
    async fn cloned_applications_share_window_ownership_and_discard_stale_observations() {
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
        let successor = peer
            .test_begin_usage_with_tick("current", Duration::from_secs(60), |_, _| {})
            .await;
        successor.observe(&report(5)).await;
        publications.lock().unwrap().clear();
        stale.observe(&report(200)).await;
        stale.flush().await;
        assert!(publications.lock().unwrap().is_empty());
        assert_eq!(
            app.usage_snapshot().await.operation_totals().total_tokens,
            5
        );
        successor.flush().await;
        assert_eq!(app.usage_snapshot().await.session.total_tokens, 5);
        assert_eq!(app.usage_snapshot().await.operations, 1);
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
            .begin_usage("storage-failure", move |_, snapshot| {
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
