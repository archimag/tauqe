//! Exclusive operation registration and cooperative cancellation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tauqe_core::stop::{HardCancellation, StopSource, StopToken};

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
    stop: StopSource,
    abort_handle: Option<tokio::task::AbortHandle>,
    release_by_supervisor: bool,
    publication_holds: usize,
    guard_dropped: bool,
}

/// Keeps registration alive while a supervisor finishes context publication.
pub(crate) struct PublicationHold {
    registry: Arc<OperationRegistry>,
    id: u64,
}

impl PublicationHold {
    pub(crate) fn set_abort_handle(&self, handle: tokio::task::AbortHandle) {
        let mut slot = self.registry.lock();
        if let Some(op) = slot.as_mut().filter(|op| op.id == self.id) {
            op.abort_handle = Some(handle);
        }
    }

    /// A detached supervisor must not clear a later task's registration.
    pub(crate) fn clear_abort_handle(&self, task_id: tokio::task::Id) {
        let mut slot = self.registry.lock();
        if let Some(op) = slot.as_mut().filter(|op| op.id == self.id) {
            if op
                .abort_handle
                .as_ref()
                .is_some_and(|handle| handle.id() == task_id)
            {
                op.abort_handle = None;
            }
        }
    }
}

impl Drop for PublicationHold {
    fn drop(&mut self) {
        let mut slot = self.registry.lock();
        if let Some(op) = slot.as_mut().filter(|op| op.id == self.id) {
            op.publication_holds -= 1;
            if op.publication_holds == 0 && op.guard_dropped {
                *slot = None;
            }
        }
    }
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
    stop: StopToken,
}

impl OperationGuard {
    /// Retain registration through task recovery, even if the caller exits.
    pub(crate) fn retain_supervision(&self) -> Option<PublicationHold> {
        self.retain_publication(&self.registry)
    }

    // The scenario publication hookup follows the session-level binding.
    #[allow(dead_code)]
    pub(crate) fn retain_publication(
        &self,
        registry: &Arc<OperationRegistry>,
    ) -> Option<PublicationHold> {
        if !Arc::ptr_eq(&self.registry, registry) {
            return None;
        }
        let mut slot = registry.lock();
        let op = slot
            .as_mut()
            .filter(|op| op.id == self.id && !op.guard_dropped)?;
        op.publication_holds += 1;
        Some(PublicationHold {
            registry: registry.clone(),
            id: self.id,
        })
    }

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

    /// Identifier used by acknowledgements and events for this registered operation.
    pub(crate) fn publication_id(&self) -> String {
        format!("op-{}", self.id)
    }

    /// Reads the full stop state at scheduling boundaries.
    pub fn stop_token(&self) -> StopToken {
        self.stop.clone()
    }

    /// Observes only Hard on the same operation channel.
    pub fn hard_cancellation(&self) -> HardCancellation {
        self.stop.hard_cancellation()
    }

    /// Reserves release for the guard held by the interactive supervisor through
    /// recovery and terminal publication, independently of the worker handle.
    pub(crate) fn reserve_supervisor_release(&self) -> bool {
        let mut slot = self.registry.lock();
        let Some(op) = slot.as_mut().filter(|op| op.id == self.id) else {
            return false;
        };
        op.release_by_supervisor = true;
        true
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
            match slot.as_mut().filter(|op| op.id == self.id) {
                Some(op) if op.publication_holds == 0 => {
                    *slot = None;
                    true
                }
                Some(op) => {
                    op.guard_dropped = true;
                    false
                }
                None => false,
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
        let stop = StopSource::new();
        let token = stop.token();
        *slot = Some(ActiveOperation {
            id,
            stop,
            abort_handle: None,
            release_by_supervisor: false,
            publication_holds: 0,
            guard_dropped: false,
        });
        tracing::info!(operation_slot_id = id, "Registered active operation slot");
        Ok(OperationGuard {
            registry: Arc::clone(self),
            id,
            stop: token,
        })
    }

    pub(crate) fn current_id(&self) -> Option<u64> {
        self.lock().as_ref().map(|op| op.id)
    }

    pub(crate) fn hard_cancel(&self) -> Option<u64> {
        let slot = self.lock();
        let op = slot.as_ref()?;
        if !op.stop.token().hard_cancellation().is_cancelled() {
            tracing::info!(
                operation_slot_id = op.id,
                "Delivered hard-cancel signal to active operation"
            );
        }
        op.stop.request_hard();
        Some(op.id)
    }

    pub(crate) fn soft_stop(&self) {
        if let Some(op) = self.lock().as_ref() {
            op.stop.request_soft();
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
        if !slot
            .as_ref()
            .is_some_and(|op| op.id == id && !op.release_by_supervisor && op.publication_holds == 0)
        {
            return false;
        }
        *slot = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn hard_view_observes_prior_cancellation() {
        let registry = Arc::new(OperationRegistry::default());
        let guard = registry.begin().unwrap();
        assert_eq!(registry.hard_cancel(), Some(guard.id()));

        let cancellation = guard.hard_cancellation();
        assert!(cancellation.is_cancelled());
        timeout(Duration::from_secs(1), cancellation.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn soft_stop_does_not_cancel_hard_view() {
        let registry = Arc::new(OperationRegistry::default());
        let guard = registry.begin().unwrap();
        registry.soft_stop();

        let cancellation = guard.hard_cancellation();
        assert!(!cancellation.is_cancelled());
        assert!(timeout(Duration::from_millis(20), cancellation.cancelled())
            .await
            .is_err());

        assert_eq!(registry.hard_cancel(), Some(guard.id()));
        timeout(Duration::from_secs(1), cancellation.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn retained_supervision_clears_only_its_task_and_holds_registration() {
        let registry = Arc::new(OperationRegistry::default());
        let guard = registry.begin().unwrap();
        let id = guard.id();
        let hold = guard.retain_supervision().unwrap();
        let first = tokio::spawn(std::future::pending::<()>());
        let second = tokio::spawn(std::future::pending::<()>());
        hold.set_abort_handle(first.abort_handle());
        guard.set_abort_handle(second.abort_handle());
        hold.clear_abort_handle(first.id());
        assert!(registry.abort(id));
        assert!(second.await.unwrap_err().is_cancelled());
        assert!(!first.is_finished());
        first.abort();
        let _ = first.await;
        drop(guard);
        assert_eq!(registry.current_id(), Some(id));
        assert!(registry.begin().is_err());
        drop(hold);
        assert_eq!(registry.current_id(), None);
    }
}
