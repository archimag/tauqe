use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tauqe_core::config::AppConfig;
use tauqe_core::context::ContextManager;
use tauqe_core::history::{HistoryManager, HistoryMutation};
use tauqe_protocol::{
    events, DiscussionFocus, DiscussionFocusChangedEvent, Event, ModelSelection,
    UsageTotalsSnapshot,
};
use tokio::sync::{watch, Mutex};

use crate::transport::OutChannel;

/// Poll cadence while the last-resort hard-cancel abort waits for an in-flight
/// git transaction to close.
const HARD_CANCEL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Maximum time the last-resort hard-cancel abort is deferred while a git
/// transaction stays open before the stalled workflow task is force-aborted
/// anyway. A transaction open this long means the turn is wedged; core startup
/// crash recovery unwinds any residue on the next launch.
const HARD_CANCEL_TRANSACTION_DEFER_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

pub struct ActiveOperation {
    pub id: u64,
    pub cancel_tx: watch::Sender<bool>,
    /// Soft-stop signal observed by a running batch supervisor between members.
    pub soft_stop_tx: watch::Sender<bool>,
    pub abort_handle: Option<tokio::task::AbortHandle>,
}

/// RAII guard owning a registered active operation.
///
/// Registration is atomic through [`AppState::try_begin_operation`], which
/// refuses to start a second operation while one is registered. The guard
/// clears the shared slot on drop only when the slot still holds this
/// operation's id, so a superseded or stale guard can never wipe a newer
/// operation's registration or a cancellation delivered in between.
pub struct OperationGuard {
    state: Arc<AppState>,
    id: u64,
    cancel_rx: watch::Receiver<bool>,
    soft_stop_rx: watch::Receiver<bool>,
}

impl OperationGuard {
    /// Returns a receiver that observes this operation's cancellation signal.
    pub fn cancel_receiver(&self) -> watch::Receiver<bool> {
        self.cancel_rx.clone()
    }

    /// Returns a receiver that observes this operation's soft-stop signal.
    ///
    /// A batch supervisor checks it between members: the current member finishes
    /// its bounded turn, compiler verification, and Git commit, and only then
    /// does the queue stop before launching the next one.
    pub fn soft_stop_receiver(&self) -> watch::Receiver<bool> {
        self.soft_stop_rx.clone()
    }

    /// Installs the abort handle of the task currently driving this operation.
    pub fn set_abort_handle(&self, handle: tokio::task::AbortHandle) {
        let mut slot = self.state.lock_active_operation();
        if let Some(op) = slot.as_mut() {
            if op.id == self.id {
                op.abort_handle = Some(handle);
            }
        }
    }

    /// Clears the abort handle installed by the task that just finished.
    ///
    /// A multi-round operation reuses one slot across several streamed provider
    /// calls, and each call installs its own handle. Leaving a finished task's
    /// handle behind lets a later hard-cancel target an already completed task
    /// while the operation is in fact still working between rounds, so the slot
    /// is emptied as soon as the streamed task resolves.
    pub fn clear_abort_handle(&self) {
        let mut slot = self.state.lock_active_operation();
        if let Some(op) = slot.as_mut() {
            if op.id == self.id {
                op.abort_handle = None;
            }
        }
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let released = {
            let mut slot = self.state.lock_active_operation();
            if slot.as_ref().map(|op| op.id) == Some(self.id) {
                *slot = None;
                true
            } else {
                false
            }
        };
        if released {
            tracing::info!(
                operation_slot_id = self.id,
                "Released active operation slot"
            );
        }
    }
}

/// Model configuration and the active model selection guarded by a single mutex.
///
/// Keeping both values behind one lock makes it impossible to acquire them in
/// conflicting orders: a `model/ask` racing a `config/set` previously deadlocked
/// on two separate guards. Callers clone the values they need and drop the guard
/// before any unrelated `.await` instead of holding it across subsystems.
pub struct ConfigAndSelection {
    pub config: AppConfig,
    pub selection: ModelSelection,
}

pub struct AppState {
    pub config: Mutex<ConfigAndSelection>,
    /// Shared context manager behind a read/write lock.
    ///
    /// The lock lives in an `Arc` so a mutation can move its owned write guard
    /// into the blocking pool and keep the whole read-modify-write sequence
    /// exclusive, instead of cloning the manager and overwriting it afterwards.
    pub context: Arc<tokio::sync::RwLock<ContextManager>>,
    /// Shared interaction history behind a mutex.
    ///
    /// The lock lives in an `Arc` so a mutation can move its owned guard into
    /// the blocking pool and keep the whole read-modify-write sequence
    /// exclusive, instead of cloning the manager and overwriting it afterwards.
    pub history: Arc<tokio::sync::Mutex<HistoryManager>>,
    pub session_lock: Mutex<()>,
    /// Single registration slot for the currently running operation.
    ///
    /// A synchronous mutex is used deliberately: an [`OperationGuard`] must
    /// clear the slot from its `Drop`, and `std::sync::Mutex` can be locked
    /// without an executor context. The lock is only ever held for brief,
    /// non-awaiting critical sections.
    pub active_cancel: std::sync::Mutex<Option<ActiveOperation>>,
    pub total_cost: Mutex<f64>,
    /// Authoritative token, cost, and duration totals for the session.
    ///
    /// Every operation kind — an interactive turn, a plan batch, a review run,
    /// a locale synthesis — reports through [`crate::usage::UsageGuard`], so the
    /// interface renders one consistent snapshot instead of re-deriving totals
    /// from per-handler counters. The project lifetime scope is loaded from
    /// `<repo_root>/.tauqe/usage.json` at construction, so the counters survive
    /// a daemon restart.
    pub usage: Mutex<UsageTotalsSnapshot>,
    /// Active discussion focus anchor shared by REPL discussion turns.
    ///
    /// While a focus is set, every plain discussion prompt is enriched with the
    /// current state of the focused plan or review session, turning one-shot
    /// comments into a continuous multi-turn dialogue.
    pub discussion_focus: Mutex<DiscussionFocus>,
    /// Identifier of the current REPL dialogue session.
    ///
    /// Every interactive turn binds its provider requests to this identifier so
    /// the backend keeps its prompt-prefix cache warm across the rounds of one
    /// dialogue. Clearing the history rotates the identifier, so the discarded
    /// transcript can never warm the cache of the fresh dialogue.
    pub session_uuid: Mutex<String>,
    pub history_sender: tokio::sync::mpsc::UnboundedSender<HistoryMutation>,
    pub out: OutChannel,
    /// Shared flag set while a git-backed apply policy holds an open
    /// transaction. Turn supervisors consult it before force-aborting a
    /// workflow task so a half-finished checkpoint or step commit is never
    /// abandoned mid-transaction.
    pub git_transaction_active: Arc<AtomicBool>,
    /// Test-only seam letting deterministic unit tests inject a scripted
    /// provider instead of building a real network provider from config.
    #[cfg(test)]
    pub provider_override: Mutex<Option<Arc<dyn tauqe_core::providers::LlmProvider>>>,
    /// Test-only seam pinning the active turn marker so deterministic tests can
    /// exercise marker-bound protocol output without generating a fresh marker.
    #[cfg(test)]
    pub marker_override: Mutex<Option<String>>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        repo_path: PathBuf,
        history_sender: tokio::sync::mpsc::UnboundedSender<HistoryMutation>,
        out: OutChannel,
    ) -> Arc<Self> {
        // Crash recovery: check and recover any dangling checkpoint or step commits from an interrupted workflow
        if let Ok(Some(recovery_msg)) =
            tauqe_core::git::checkpoint::recover_interrupted_workflow(&repo_path)
        {
            tracing::warn!(
                repo_root = %repo_path.display(),
                "Recovered interrupted workflow: {}",
                recovery_msg,
            );
        }

        // Review legacy migration runs exactly once at startup, so listing or
        // loading reviews never writes to disk and stays free of read side
        // effects. Failures are logged by the core.
        tauqe_core::review::ReviewStorage::migrate_legacy_if_needed(&repo_path);

        let default_selection = config.default_selection();
        // The project lifetime totals outlive the process, so they are read
        // from `.tauqe/usage.json` before the repository root moves into the
        // context manager.
        let initial_usage = crate::usage::load_initial_snapshot(&repo_path);
        let history_sender_init = history_sender.clone();
        let mut initial_history_manager = HistoryManager::new(repo_path.clone());
        // Forward every history mutation — appends and structural compactions —
        // so the client rebuilds its session view from the authoritative
        // result instead of inferring it from append events alone.
        initial_history_manager.set_listener(move |mutation| {
            let _ = history_sender_init.send(mutation);
        });

        Arc::new(Self {
            config: Mutex::new(ConfigAndSelection {
                config,
                selection: default_selection,
            }),
            context: Arc::new(tokio::sync::RwLock::new(ContextManager::new(repo_path))),
            history: Arc::new(Mutex::new(initial_history_manager)),
            session_lock: Mutex::new(()),
            active_cancel: std::sync::Mutex::new(None),
            total_cost: Mutex::new(0.0),
            usage: Mutex::new(initial_usage),
            discussion_focus: Mutex::new(DiscussionFocus::None),
            session_uuid: Mutex::new(tauqe_core::review::uuid::generate_uuid_v7()),
            history_sender,
            out,
            git_transaction_active: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            provider_override: Mutex::new(None),
            #[cfg(test)]
            marker_override: Mutex::new(None),
        })
    }

    /// Locks the active-operation slot, recovering from a poisoned mutex instead
    /// of panicking so an unexpected panic elsewhere cannot wedge the daemon.
    fn lock_active_operation(&self) -> std::sync::MutexGuard<'_, Option<ActiveOperation>> {
        self.active_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Atomically registers a new active operation.
    ///
    /// Returns `Err(())` when another operation is already registered. On
    /// success it returns an [`OperationGuard`] that owns the registration and
    /// clears the slot on drop.
    pub fn try_begin_operation(self: &Arc<Self>) -> Result<OperationGuard, ()> {
        let mut slot = self.lock_active_operation();
        if slot.is_some() {
            drop(slot);
            tracing::warn!(
                code = tauqe_protocol::errors::OPERATION_IN_PROGRESS,
                "Refused to register a new operation: a slot is already active",
            );
            return Err(());
        }
        let id = next_operation_id();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (soft_stop_tx, soft_stop_rx) = watch::channel(false);
        *slot = Some(ActiveOperation {
            id,
            cancel_tx,
            soft_stop_tx,
            abort_handle: None,
        });
        drop(slot);
        tracing::info!(operation_slot_id = id, "Registered active operation slot");
        Ok(OperationGuard {
            state: Arc::clone(self),
            id,
            cancel_rx,
            soft_stop_rx,
        })
    }

    /// Reports whether an operation is currently registered.
    pub fn is_operation_active(&self) -> bool {
        self.lock_active_operation().is_some()
    }

    /// Delivers the hard-cancel signal to the active operation and schedules the
    /// last-resort force-abort watchdog.
    ///
    /// The operation slot is intentionally left registered: the cancelled
    /// workflow keeps mutating state (rolling back its git transaction,
    /// releasing the session lock) until it observes the cancel signal and drops
    /// its [`OperationGuard`]. Clearing the slot immediately would let a client
    /// start `git/undo`, `config/set`, or a new turn against a half-unwound turn
    /// and would let the batch supervisor abort a later step.
    pub fn hard_cancel_active_operation(self: &Arc<Self>) {
        let Some(op_id) = self.signal_hard_cancel() else {
            return;
        };
        let state = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(crate::handlers::model::CANCEL_UNWIND_GRACE).await;
            state.abort_stalled_operation(op_id).await;
        });
    }

    /// Force-aborts the operation identified by `op_id` after the hard-cancel
    /// grace period has elapsed, once no git transaction is open.
    ///
    /// Cooperative cancellation is the primary path; this is the last resort for
    /// a workflow that ignored the cancel signal. The abort is deferred — never
    /// dropped — while a git transaction is still open, so a half-finished
    /// checkpoint or step commit is never abandoned mid-transaction, and the
    /// wait is bounded so a wedged turn cannot leak its task for the daemon's
    /// lifetime. The handle is read fresh from the slot, so the abort targets
    /// the currently running inner task and a superseded watchdog can never
    /// abort a newer operation.
    pub async fn abort_stalled_operation(&self, op_id: u64) {
        let deadline = tokio::time::Instant::now() + HARD_CANCEL_TRANSACTION_DEFER_LIMIT;
        while self.git_transaction_active.load(Ordering::SeqCst)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(HARD_CANCEL_POLL_INTERVAL).await;
        }

        let handle = {
            let slot = self.lock_active_operation();
            match slot.as_ref() {
                Some(op) if op.id == op_id => op.abort_handle.clone(),
                _ => None,
            }
        };
        if let Some(handle) = handle {
            tracing::warn!(
                operation_slot_id = op_id,
                "Hard-cancel grace period elapsed; aborting stalled workflow task",
            );
            handle.abort();
        }
    }

    /// Delivers a cancellation signal to the active operation without clearing
    /// its registration, so callers can still observe when the operation
    /// finishes and unwinds its transaction.
    ///
    /// The signal is raised at most once per operation: the shutdown drain
    /// re-signals on every poll, and logging each repeat would flood the
    /// journal.
    pub fn cancel_active_operation(&self) {
        let _ = self.signal_hard_cancel();
    }

    /// Raises the hard-cancel signal on the registered operation and returns its
    /// id, or `None` when no operation is registered. Shared by the shutdown
    /// drain and the interactive hard-cancel path.
    fn signal_hard_cancel(&self) -> Option<u64> {
        let slot = self.lock_active_operation();
        let op = slot.as_ref()?;
        if !op.cancel_tx.send_replace(true) {
            tracing::info!(
                operation_slot_id = op.id,
                "Delivered hard-cancel signal to active operation",
            );
        }
        Some(op.id)
    }

    /// Requests a graceful stop of the active operation without clearing its
    /// registration.
    ///
    /// A batch supervisor observes this signal between members: the current
    /// member finishes its bounded turn, compiler verification, and Git commit,
    /// and only then does the queue stop before launching the next member.
    /// Unlike [`AppState::cancel_active_operation`], the request is advisory and
    /// never interrupts work that is already running.
    pub fn soft_stop_active_operation(&self) {
        let slot = self.lock_active_operation();
        if let Some(op) = slot.as_ref() {
            let _ = op.soft_stop_tx.send(true);
            tracing::info!(
                operation_slot_id = op.id,
                "Requested soft stop of active operation",
            );
        }
    }

    /// Returns the authoritative repository root shared by every handler.
    ///
    /// Git, context, plan, and review handlers must all resolve the repository
    /// from this single source instead of re-deriving it from the process CWD,
    /// which can drift from the initialized repository.
    pub async fn repo_root(&self) -> PathBuf {
        self.context.read().await.repo_root().to_path_buf()
    }

    /// Applies a mutation to the shared context atomically.
    ///
    /// The exclusive write guard is acquired before the blocking closure runs
    /// and is moved into the blocking pool together with it, so the entire
    /// read-modify-write sequence stays serialized. Cloning the manager under a
    /// short read guard and overwriting the slot afterwards would let a
    /// concurrent mutation be silently lost and would reset the monotonic
    /// context revision.
    pub async fn mutate_context<T, F>(self: &Arc<Self>, mutate: F) -> Result<T, String>
    where
        F: FnOnce(&mut ContextManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        let guard = Arc::clone(&self.context).write_owned().await;
        tokio::task::spawn_blocking(move || {
            let mut guard = guard;
            mutate(&mut guard)
        })
        .await
        .map_err(|err| format!("blocking task failed: {}", err))
    }

    /// Applies a mutation to the shared interaction history atomically.
    ///
    /// History records are persisted with an `fsync` barrier, so the mutation
    /// runs on the blocking pool. The exclusive guard is acquired first and
    /// moved into the blocking task together with the closure, so the whole
    /// read-modify-write sequence stays serialized: cloning the manager under a
    /// short guard and overwriting the slot afterwards would drop concurrent
    /// appends and compactions.
    pub async fn mutate_history<T, F>(self: &Arc<Self>, mutate: F) -> Result<T, String>
    where
        F: FnOnce(&mut HistoryManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        let guard = Arc::clone(&self.history).lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let mut guard = guard;
            mutate(&mut guard)
        })
        .await
        .map_err(|err| format!("blocking task failed: {}", err))
    }

    /// Returns the current discussion focus anchor.
    pub async fn discussion_focus(&self) -> DiscussionFocus {
        self.discussion_focus.lock().await.clone()
    }

    /// Replaces the discussion focus anchor, broadcasting a change event only
    /// when the anchor actually changes, and returns the resulting focus.
    pub async fn set_discussion_focus(&self, focus: DiscussionFocus) -> DiscussionFocus {
        let changed = {
            let mut guard = self.discussion_focus.lock().await;
            if *guard == focus {
                false
            } else {
                *guard = focus.clone();
                true
            }
        };
        if changed {
            self.out.send_event(&Event {
                method: events::DISCUSSION_FOCUS_CHANGED.to_string(),
                params: serde_json::to_value(DiscussionFocusChangedEvent {
                    focus: focus.clone(),
                })
                .ok(),
            });
        }
        focus
    }

    /// Clears the discussion focus anchor, returning the resulting focus.
    pub async fn clear_discussion_focus(&self) -> DiscussionFocus {
        self.set_discussion_focus(DiscussionFocus::None).await
    }

    /// Returns the identifier of the current REPL dialogue session.
    pub async fn session_uuid(&self) -> String {
        self.session_uuid.lock().await.clone()
    }

    /// Starts a fresh REPL dialogue session, returning its identifier.
    ///
    /// The identifier changes, so a provider-side prompt-prefix cache built
    /// from the discarded transcript can never be reused by the new dialogue.
    /// UUIDv7 is reused from the review subsystem so the workspace keeps a
    /// single identifier generator.
    pub async fn reset_session_uuid(&self) -> String {
        let fresh = tauqe_core::review::uuid::generate_uuid_v7();
        let previous = {
            let mut guard = self.session_uuid.lock().await;
            std::mem::replace(&mut *guard, fresh.clone())
        };
        tracing::info!(
            previous_session = %previous,
            session_id = %fresh,
            "Rotated the dialogue session identifier",
        );
        fresh
    }

    /// Returns the turn marker for the current turn, or `None` when markers are
    /// disabled.
    ///
    /// The marker binds structural protocol tags to a single turn so literal
    /// source tags are never mistaken for protocol commands. Production turns
    /// always generate a fresh marker; deterministic tests may pin it through
    /// the `marker_override` seam or disable it by injecting a scripted
    /// provider.
    pub async fn next_turn_marker(&self) -> Option<String> {
        #[cfg(test)]
        {
            if let Some(marker) = self.marker_override.lock().await.clone() {
                return Some(marker);
            }
            if self.provider_override.lock().await.is_some() {
                return None;
            }
        }
        Some(tauqe_core::edits::protocol::xml::generate_turn_marker())
    }
}

static OP_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn next_operation_id() -> u64 {
    OP_COUNTER.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::ContextAccess;

    /// Concurrent context mutations must all survive: a clone-then-overwrite
    /// sequence would drop the mutations that finished earlier and reset the
    /// monotonic revision.
    #[tokio::test]
    async fn mutate_context_serializes_concurrent_mutations() {
        let root =
            std::env::temp_dir().join(format!("tauqe-state-context-{}", next_operation_id()));
        let _ = std::fs::create_dir_all(&root);

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let mut tasks = Vec::new();
        for index in 0..8 {
            let path = format!("file{}.rs", index);
            std::fs::write(root.join(&path), "fn stub() {}\n").unwrap();
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                state
                    .mutate_context(move |ctx| {
                        ctx.add_auto_file(&path, ContextAccess::ReadOnly)
                            .map(|_| ())
                    })
                    .await
            }));
        }

        for task in tasks {
            let outcome = task.await.expect("mutation task joins");
            let applied = outcome.expect("mutation runs on the blocking pool");
            assert!(
                applied.is_ok(),
                "context mutation failed: {:?}",
                applied.err()
            );
        }

        let snapshot = state.context.read().await.get_state();
        assert_eq!(
            snapshot.items.len(),
            8,
            "every concurrent mutation must survive"
        );
        assert!(
            snapshot.revision > 1,
            "the context revision must stay monotonic"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Concurrent history mutations must all survive: a clone-then-overwrite
    /// sequence would drop the records that finished earlier.
    #[tokio::test]
    async fn mutate_history_serializes_concurrent_mutations() {
        let root =
            std::env::temp_dir().join(format!("tauqe-state-history-{}", next_operation_id()));
        let _ = std::fs::create_dir_all(&root);

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let mut tasks = Vec::new();
        for index in 0..8 {
            let prompt = format!("Task {}", index);
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                state
                    .mutate_history(move |hm| hm.record_turn(prompt, Vec::new()).map(|_| ()))
                    .await
            }));
        }

        for task in tasks {
            let outcome = task.await.expect("mutation task joins");
            let applied = outcome.expect("mutation runs on the blocking pool");
            assert!(
                applied.is_ok(),
                "history mutation failed: {:?}",
                applied.err()
            );
        }

        let entries = state
            .history
            .lock()
            .await
            .get_entries()
            .expect("history reads back");
        assert_eq!(entries.len(), 8, "every concurrent mutation must survive");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A finished streaming round must leave no abort handle behind, or a later
    /// hard-cancel would abort a completed task instead of the work in flight.
    #[tokio::test]
    async fn clear_abort_handle_drops_the_installed_handle() {
        let root = std::env::temp_dir().join(format!("tauqe-state-abort-{}", next_operation_id()));
        let _ = std::fs::create_dir_all(&root);

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let guard = state.try_begin_operation().expect("slot is free");
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        guard.set_abort_handle(task.abort_handle());

        let installed = {
            let slot = state.lock_active_operation();
            slot.as_ref()
                .and_then(|op| op.abort_handle.as_ref())
                .is_some()
        };
        assert!(installed, "the handle must be installed while streaming");

        guard.clear_abort_handle();
        let cleared = {
            let slot = state.lock_active_operation();
            slot.as_ref()
                .and_then(|op| op.abort_handle.as_ref())
                .is_none()
        };
        assert!(cleared, "the handle must be cleared once the task finished");

        task.abort();
        let _ = task.await;
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }
}
