use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{watch, Mutex};
use tauqe_core::config::AppConfig;
use tauqe_core::context::ContextManager;
use tauqe_core::history::HistoryManager;
use tauqe_protocol::{
    events, DiscussionFocus, DiscussionFocusChangedEvent, Event, ModelSelection, UiHistoryItem,
};

use crate::transport::OutChannel;

pub struct ActiveOperation {
    pub id: u64,
    pub cancel_tx: watch::Sender<bool>,
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
}

impl OperationGuard {
    /// Returns a receiver that observes this operation's cancellation signal.
    pub fn cancel_receiver(&self) -> watch::Receiver<bool> {
        self.cancel_rx.clone()
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
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let mut slot = self.state.lock_active_operation();
        if slot.as_ref().map(|op| op.id) == Some(self.id) {
            *slot = None;
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
    pub context: tokio::sync::RwLock<ContextManager>,
    pub history: Mutex<HistoryManager>,
    pub session_lock: Mutex<()>,
    /// Single registration slot for the currently running operation.
    ///
    /// A synchronous mutex is used deliberately: an [`OperationGuard`] must
    /// clear the slot from its `Drop`, and `std::sync::Mutex` can be locked
    /// without an executor context. The lock is only ever held for brief,
    /// non-awaiting critical sections.
    pub active_cancel: std::sync::Mutex<Option<ActiveOperation>>,
    pub total_cost: Mutex<f64>,
    /// Active discussion focus anchor shared by REPL discussion turns.
    ///
    /// While a focus is set, every plain discussion prompt is enriched with the
    /// current state of the focused plan or review session, turning one-shot
    /// comments into a continuous multi-turn dialogue.
    pub discussion_focus: Mutex<DiscussionFocus>,
    pub history_sender: tokio::sync::mpsc::UnboundedSender<UiHistoryItem>,
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
        history_sender: tokio::sync::mpsc::UnboundedSender<UiHistoryItem>,
        out: OutChannel,
    ) -> Arc<Self> {
        // Crash recovery: check and recover any dangling checkpoint or step commits from an interrupted workflow
        if let Ok(Some(recovery_msg)) =
            tauqe_core::git::checkpoint::recover_interrupted_workflow(&repo_path)
        {
            eprintln!("[tauqe-recovery] {}", recovery_msg);
        }

        // Review legacy migration runs exactly once at startup, so listing or
        // loading reviews never writes to disk and stays free of read side
        // effects. Failures are logged by the core.
        tauqe_core::review::ReviewStorage::migrate_legacy_if_needed(&repo_path);

        let default_selection = config.default_selection();
        let history_sender_init = history_sender.clone();
        let mut initial_history_manager = HistoryManager::new(repo_path.clone());
        initial_history_manager.set_listener(move |item| {
            let _ = history_sender_init.send(item);
        });

        Arc::new(Self {
            config: Mutex::new(ConfigAndSelection {
                config,
                selection: default_selection,
            }),
            context: tokio::sync::RwLock::new(ContextManager::new(repo_path)),
            history: Mutex::new(initial_history_manager),
            session_lock: Mutex::new(()),
            active_cancel: std::sync::Mutex::new(None),
            total_cost: Mutex::new(0.0),
            discussion_focus: Mutex::new(DiscussionFocus::None),
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
            return Err(());
        }
        let id = next_operation_id();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        *slot = Some(ActiveOperation {
            id,
            cancel_tx,
            abort_handle: None,
        });
        drop(slot);
        Ok(OperationGuard {
            state: Arc::clone(self),
            id,
            cancel_rx,
        })
    }

    /// Reports whether an operation is currently registered.
    pub fn is_operation_active(&self) -> bool {
        self.lock_active_operation().is_some()
    }

    /// Removes and returns the registered operation so its cancel signal can be
    /// delivered while the slot is free for a later operation to register.
    pub fn take_active_operation(&self) -> Option<ActiveOperation> {
        self.lock_active_operation().take()
    }

    /// Delivers a cancellation signal to the active operation without clearing
    /// its registration, so callers can still observe when the operation
    /// finishes and unwinds its transaction.
    pub fn cancel_active_operation(&self) {
        let slot = self.lock_active_operation();
        if let Some(op) = slot.as_ref() {
            let _ = op.cancel_tx.send(true);
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
