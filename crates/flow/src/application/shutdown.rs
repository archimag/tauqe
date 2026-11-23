//! Cancellation watchdog and bounded operation shutdown policy.

use crate::api::Application;
use std::time::Duration;

/// Poll cadence while the last-resort hard-cancel abort waits for an in-flight
/// git transaction to close.
const HARD_CANCEL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Maximum time the last-resort hard-cancel abort is deferred while a git
/// transaction stays open before the stalled workflow task is force-aborted
/// anyway. A transaction open this long means the turn is wedged; core startup
/// crash recovery unwinds any residue on the next launch.
const HARD_CANCEL_TRANSACTION_DEFER_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

impl Application {
    /// Signal cancellation immediately, then allow cooperative unwind before
    /// starting the bounded last-resort abort. Resolve caller-scoped budgets
    /// before spawning so they apply unchanged inside the watchdog task.
    fn hard_cancel_active_operation(&self, unwind_grace: Duration, exit_deadline: Duration) {
        let Some(op_id) = self.cancel_active_operation() else {
            return;
        };
        let application = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(unwind_grace).await;
            application
                .abort_stalled_operation_with_deadline(op_id, exit_deadline)
                .await;
        });
    }

    /// Force-aborts `op_id` under a deadline the caller has already resolved.
    ///
    /// A watchdog spawned by `Application::hard_cancel_active_operation` cannot
    /// read a task-scoped deadline: the value does not cross `tokio::spawn` and
    /// would silently fall back to the production default. The spawner
    /// therefore resolves it in the scope that actually carries the override
    /// and hands it over here.
    pub async fn abort_stalled_operation_with_deadline(
        &self,
        op_id: u64,
        exit_deadline: std::time::Duration,
    ) {
        let deadline = tokio::time::Instant::now() + HARD_CANCEL_TRANSACTION_DEFER_LIMIT;
        while self.transaction_is_active() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(HARD_CANCEL_POLL_INTERVAL).await;
        }

        if !self.abort_operation(op_id) {
            return;
        }
        tracing::warn!(
            operation_slot_id = op_id,
            "Hard-cancel grace period elapsed; aborting stalled workflow task",
        );

        if !self.confirm_operation_exit(op_id, exit_deadline).await {
            self.release_stale_registration(op_id);
        }
    }

    /// Force-aborts the registered operation at the very end of the shutdown
    /// drain.
    ///
    /// The long-running work of this daemon does not live in the request
    /// handler: `model/ask`, `plan/execute`, `plan/executePipeline`, and
    /// `review/execute` spawn their turn or batch and return, so the handler
    /// task set is typically empty while the operation slot still holds the task
    /// that really runs. The slot is the only place that task is tracked —
    /// through the abort handle a running turn installs — so a stalled drain
    /// must reach through it instead of relying on the cooperative cancel
    /// signal alone, which such a task may ignore indefinitely.
    ///
    /// Dropping the runtime at process exit is not a substitute: it abandons a
    /// half-finished checkpoint or step commit instead of unwinding it. The
    /// abort is therefore deferred, but never dropped, while a git transaction
    /// is still open, and `budget` bounds that deferral because this runs inside
    /// the daemon's own shutdown window. The slot is then polled, still within
    /// the same budget, until the aborted task has really exited, so the caller
    /// never returns while the task is mid-unwind.
    pub async fn force_abort_active_operation(&self, budget: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + budget;

        while self.transaction_is_active() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(HARD_CANCEL_POLL_INTERVAL).await;
        }

        let Some(op_id) = self.active_operation_id() else {
            return;
        };
        if !self.abort_operation(op_id) {
            tracing::warn!(operation_slot_id = op_id,
                "Shutdown drain expired with an operation still registered but no abort handle installed");
            return;
        }

        tracing::warn!(
            operation_slot_id = op_id,
            "Shutdown drain expired; force-aborting the registered operation",
        );

        // The abort lands at the task's next suspension point. Waiting for the
        // slot to clear keeps the runtime alive until that task has dropped, so
        // an open transaction is unwound instead of abandoned by process exit.
        while self.active_operation_id().is_some() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(HARD_CANCEL_POLL_INTERVAL).await;
        }
    }

    /// Confirms, within the shared task-exit deadline, that the operation
    /// `op_id` released its registration.
    ///
    /// The slot is the only evidence an external watchdog can observe: the
    /// session lock is released only when the aborted task really stops, so a
    /// registration that is gone — cleared by the task's own guard drop or
    /// replaced by a newer operation — is the confirmation that this operation
    /// no longer owns any state.
    async fn confirm_operation_exit(&self, op_id: u64, exit_deadline: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + exit_deadline;
        loop {
            let registered = self.active_operation_id() == Some(op_id);
            if !registered {
                return true;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                tracing::warn!(
                    operation_slot_id = op_id,
                    deadline_ms = exit_deadline.as_millis() as u64,
                    "task-exit confirmation expired; the operation is still registered",
                );
                return false;
            }
            tokio::time::sleep(HARD_CANCEL_POLL_INTERVAL.min(deadline - now)).await;
        }
    }

    /// Releases an orphaned registration after the exit deadline, unless an
    /// interactive supervisor owns release through terminal publication.
    ///
    /// The registry checks both identity and release ownership under its lock,
    /// so neither a newer registration nor a supervisor's slot is cleared.
    fn release_stale_registration(&self, op_id: u64) {
        let released = self.release_operation_registration(op_id);
        if released {
            tracing::warn!(
                operation_slot_id = op_id,
                "Released the operation registration of an orphaned task",
            );
        }
    }
}

pub enum CancellationMode {
    Hard,
    Soft,
}

impl Application {
    pub fn cancel(&self, mode: CancellationMode, exit_deadline: Duration) {
        match mode {
            CancellationMode::Hard => self.hard_cancel_active_operation(
                super::task_exit::OPERATION_UNWIND_GRACE,
                exit_deadline,
            ),
            CancellationMode::Soft => self.soft_stop_active_operation(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    #[tokio::test]
    async fn watchdog_cancels_immediately_and_reaps_the_registered_task() {
        let app = Application::default();
        let guard = Arc::new(app.try_begin_operation().unwrap());
        let cancel = guard.cancel_receiver();
        let owned = Arc::clone(&guard);
        let task = tokio::spawn(async move {
            let _guard = owned;
            std::future::pending::<()>().await;
        });
        guard.set_abort_handle(task.abort_handle());
        drop(guard);
        app.hard_cancel_active_operation(Duration::ZERO, Duration::from_secs(1));
        assert!(*cancel.borrow());
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(app.active_operation_id(), None);
    }

    #[tokio::test]
    async fn abort_waits_for_an_open_transaction() {
        let app = Application::default();
        let guard = Arc::new(app.try_begin_operation().unwrap());
        let id = guard.id();
        let owned = Arc::clone(&guard);
        let task = tokio::spawn(async move {
            let _guard = owned;
            std::future::pending::<()>().await;
        });
        guard.set_abort_handle(task.abort_handle());
        drop(guard);
        let transaction = app.test_transaction_activity();
        transaction.store(true, Ordering::SeqCst);
        let watchdog_app = app.clone();
        let watchdog = tokio::spawn(async move {
            watchdog_app
                .abort_stalled_operation_with_deadline(id, Duration::from_secs(1))
                .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        assert_eq!(app.active_operation_id(), Some(id));
        transaction.store(false, Ordering::SeqCst);
        watchdog.await.unwrap();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(app.active_operation_id(), None);
    }

    #[tokio::test]
    async fn expired_exit_confirmation_releases_only_the_orphan_registration() {
        let app = Application::default();
        let guard = app.try_begin_operation().unwrap();
        // Keep the registration alive after the task exits to model an orphan
        // that has not dropped its operation resources by the exit deadline.
        let task = tokio::spawn(std::future::pending::<()>());
        guard.set_abort_handle(task.abort_handle());
        app.abort_stalled_operation_with_deadline(guard.id(), Duration::from_millis(10))
            .await;
        assert!(task.await.unwrap_err().is_cancelled());
        let next = app.try_begin_operation().unwrap();
        drop(guard);
        assert_eq!(app.active_operation_id(), Some(next.id()));
    }
}
