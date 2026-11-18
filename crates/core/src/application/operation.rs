//! Exclusive operation registration and cooperative cancellation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

static OP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Process-wide identity shared by operations and auxiliary usage windows.
pub fn next_operation_id() -> u64 {
    OP_COUNTER.fetch_add(1, Ordering::SeqCst)
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("an operation is already in progress")]
pub struct OperationInProgress;

#[derive(Default)]
pub(crate) struct OperationRegistry {
    slot: Mutex<Option<ActiveOperation>>,
}

struct ActiveOperation {
    id: u64,
    cancel_tx: watch::Sender<bool>,
    /// Soft-stop signal observed by a running batch supervisor between members.
    soft_stop_tx: watch::Sender<bool>,
    abort_handle: Option<tokio::task::AbortHandle>,
}

/// RAII guard owning a registered active operation.
///
/// Registration is atomic through [`crate::api::Application::try_begin_operation`], which
/// refuses to start a second operation while one is registered. The guard
/// clears the shared slot on drop only when the slot still holds this
/// operation's id, so a superseded or stale guard can never wipe a newer
/// operation's registration or a cancellation delivered in between.
pub struct OperationGuard {
    registry: Arc<OperationRegistry>,
    id: u64,
    cancel_rx: watch::Receiver<bool>,
    soft_stop_rx: watch::Receiver<bool>,
}

impl OperationGuard {
    pub(crate) fn belongs_to(&self, registry: &Arc<OperationRegistry>) -> bool {
        Arc::ptr_eq(&self.registry, registry) && registry.current_id() == Some(self.id)
    }

    /// Returns the identifier of the registered operation.
    ///
    /// Every lifecycle event a supervisor publishes — a replanning phase, a
    /// step failure, and the terminal batch completion — carries this
    /// identifier, so a client can attribute each of them to the operation its
    /// request started instead of correlating an unregistered counter.
    pub fn id(&self) -> u64 {
        self.id
    }

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
        let mut slot = self.registry.lock();
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
        let mut slot = self.registry.lock();
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
            let mut slot = self.registry.lock();
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

impl OperationRegistry {
    fn lock(&self) -> MutexGuard<'_, Option<ActiveOperation>> {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn begin(self: &Arc<Self>) -> Result<OperationGuard, OperationInProgress> {
        let mut slot = self.lock();
        if slot.is_some() {
            return Err(OperationInProgress);
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
        tracing::info!(operation_slot_id = id, "Registered active operation slot");
        Ok(OperationGuard {
            registry: Arc::clone(self),
            id,
            cancel_rx,
            soft_stop_rx,
        })
    }

    pub(crate) fn current_id(&self) -> Option<u64> {
        self.lock().as_ref().map(|op| op.id)
    }

    pub(crate) fn hard_cancel(&self) -> Option<u64> {
        let slot = self.lock();
        let op = slot.as_ref()?;
        if !op.cancel_tx.send_replace(true) {
            tracing::info!(
                operation_slot_id = op.id,
                "Delivered hard-cancel signal to active operation"
            );
        }
        Some(op.id)
    }

    pub(crate) fn soft_stop(&self) {
        if let Some(op) = self.lock().as_ref() {
            let _ = op.soft_stop_tx.send(true);
            tracing::info!(
                operation_slot_id = op.id,
                "Requested soft stop of active operation"
            );
        }
    }

    pub(crate) fn abort(&self, id: u64) -> bool {
        let slot = self.lock();
        let Some(op) = slot.as_ref().filter(|op| op.id == id) else {
            return false;
        };
        let Some(handle) = &op.abort_handle else {
            return false;
        };
        handle.abort();
        true
    }

    pub(crate) fn release(&self, id: u64) -> bool {
        let mut slot = self.lock();
        if slot.as_ref().map(|op| op.id) != Some(id) {
            return false;
        }
        *slot = None;
        true
    }
}
