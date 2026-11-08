//! Bounded confirmation that a spawned task stopped running.
//!
//! Aborting a Tokio task does not end it where the abort is raised: the abort
//! lands at the task's next suspension point, so a task inside a non-yielding
//! stretch keeps running until that stretch ends. Treating an abort as
//! synchronous death therefore fails in both directions at once. A waiter that
//! relies on the task handle parks forever on a task that never yields, while a
//! caller that returns immediately releases the state the still-running task is
//! about to touch.
//!
//! This module owns the one discipline every abort site of the daemon shares:
//!
//! - A task the daemon terminates is confirmed, within [`TASK_EXIT_DEADLINE`],
//!   to have stopped running.
//! - A confirmation that expires is recorded once as a structured diagnostic
//!   naming the operation and the task, and the handle is released
//!   deliberately. The abort stays delivered, so an orphaned task ends at its
//!   next suspension point, and no second kill is attempted.
//! - A task the daemon awaits for its own result is awaited under the same
//!   deadline, so a normal completion is bounded by the same budget as a
//!   termination.
//!
//! The deadline dominates the provider's own documented completion-path bound:
//! an otherwise delivered stream performs exactly one more step before it
//! reports its usage, the best-effort generation-stats fetch bounded by
//! `GENERATION_STATS_TIMEOUT`, so a healthy completed stream is never
//! misreported as an orphan.
//!
//! Rationale (Why): the operation slot is the daemon's only admission gate. A
//! registration that is never released refuses every later request with
//! `OPERATION_IN_PROGRESS` until the process exits, while a slot released
//! before its task has really stopped admits a new operation against state the
//! old task still owns.

use std::time::Duration;

use tokio::task::{JoinError, JoinHandle};

/// Longest the daemon waits for a task it has terminated to stop running.
///
/// An otherwise delivered stream performs exactly one step after its last
/// event — the best-effort generation-stats fetch, bounded by
/// `GENERATION_STATS_TIMEOUT` of five seconds — so this budget covers that
/// completion path several times over and can never misreport a healthy
/// completed stream as an orphan. It equals the cancellation convergence grace
/// the turn machinery grants a workflow before force-aborting it, so the daemon
/// answers one question — how long may a task take to stop? — with one number.
pub(crate) const TASK_EXIT_DEADLINE: Duration = Duration::from_secs(30);

// Task-scoped override of `TASK_EXIT_DEADLINE`.
//
// An expired confirmation costs the production deadline by design, so a
// deterministic test shortens it; scoping the value to the task tree that
// installed it keeps a concurrently running test on the production deadline.
#[cfg(test)]
tokio::task_local! {
    static TASK_EXIT_DEADLINE_OVERRIDE: Duration;
}

/// Resolves the task-exit deadline in force for this task.
#[cfg(test)]
pub(crate) fn task_exit_deadline() -> Duration {
    TASK_EXIT_DEADLINE_OVERRIDE
        .try_with(|value| *value)
        .unwrap_or(TASK_EXIT_DEADLINE)
}

/// Resolves the task-exit deadline in force for this task.
///
/// Production always waits out [`TASK_EXIT_DEADLINE`]; only a test may scope a
/// shorter value to the task tree that drives the confirmation.
#[cfg(not(test))]
pub(crate) fn task_exit_deadline() -> Duration {
    TASK_EXIT_DEADLINE
}

/// Runs `work` under a task-scoped override of the task-exit deadline.
#[cfg(test)]
pub(crate) async fn with_task_exit_deadline<F>(deadline: Duration, work: F) -> F::Output
where
    F: std::future::Future,
{
    TASK_EXIT_DEADLINE_OVERRIDE.scope(deadline, work).await
}

/// Confirmation that a task the daemon terminated stopped running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskExit {
    /// The task ended within [`TASK_EXIT_DEADLINE`].
    Confirmed,
    /// The deadline expired while the task was still running.
    ///
    /// The abort stays delivered, so the task ends at its next suspension
    /// point; its handle was released deliberately and no second kill is
    /// attempted.
    Orphaned,
}

/// Outcome of awaiting a spawned task under [`TASK_EXIT_DEADLINE`].
#[derive(Debug)]
pub(crate) enum Awaited<T> {
    /// The task ended within the deadline; its join outcome travels unchanged.
    Joined(Result<T, JoinError>),
    /// The deadline expired while the task was still running.
    Orphaned,
}

/// Aborts `handle` and confirms that its task stopped running.
///
/// `operation` names the operation the task belongs to and `task` names the
/// task, so the expiry diagnostic a wedged task produces identifies the
/// abandoned work without a second lookup. The confirmation never parks past
/// [`TASK_EXIT_DEADLINE`] and never raises a second abort: an orphaned task is
/// expected to end at its next suspension point.
pub(crate) async fn abort_and_reap<T>(
    operation: &str,
    task: &str,
    handle: JoinHandle<T>,
) -> TaskExit {
    abort_and_reap_with_deadline(operation, task, task_exit_deadline(), handle).await
}

/// Aborts `handle` and confirms its exit under an explicitly resolved deadline.
///
/// [`abort_and_reap`] resolves the deadline in the task that awaits it, which is
/// the calling task for every direct call site. A watchdog that reaps a task it
/// did not spawn runs in a task of its own, and the task-scoped deadline seam
/// does not cross `tokio::spawn`: such a spawner resolves
/// [`task_exit_deadline`] before the spawn and hands the value over here, so the
/// confirmation applies the very deadline the spawner observed.
pub(crate) async fn abort_and_reap_with_deadline<T>(
    operation: &str,
    task: &str,
    deadline: Duration,
    mut handle: JoinHandle<T>,
) -> TaskExit {
    handle.abort();
    match confirm_exit_with_deadline(operation, task, deadline, &mut handle).await {
        Some(_) => TaskExit::Confirmed,
        None => TaskExit::Orphaned,
    }
}

/// Awaits `handle` under the task-exit deadline without terminating it.
///
/// This is the completion shape of the same discipline: a task awaited for its
/// own result is waited for no longer than a terminated one, so a task that
/// stops yielding cannot park its caller indefinitely.
pub(crate) async fn await_bounded<T>(
    operation: &str,
    task: &str,
    mut handle: JoinHandle<T>,
) -> Awaited<T> {
    match confirm_exit_with_deadline(operation, task, task_exit_deadline(), &mut handle).await {
        Some(outcome) => Awaited::Joined(outcome),
        None => Awaited::Orphaned,
    }
}

/// Races one task against an explicit deadline and records an expired
/// confirmation.
///
/// `None` means the deadline expired first. The handle stays with the caller,
/// so releasing it remains visible at the call site instead of happening inside
/// the race.
async fn confirm_exit_with_deadline<T>(
    operation: &str,
    task: &str,
    deadline: Duration,
    handle: &mut JoinHandle<T>,
) -> Option<Result<T, JoinError>> {
    match tokio::time::timeout(deadline, handle).await {
        Ok(outcome) => Some(outcome),
        Err(_) => {
            record_orphan(operation, task, deadline);
            None
        }
    }
}

/// Records one expired task-exit confirmation.
///
/// The record names the operation, the task, and the budget it exceeded, so a
/// journal entry identifies the abandoned work without a second lookup.
fn record_orphan(operation: &str, task: &str, deadline: Duration) {
    tracing::warn!(
        operation_id = operation,
        task,
        deadline_ms = deadline.as_millis() as u64,
        "task-exit confirmation expired; releasing the handle of a still-running task",
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::*;

    /// Flips a flag when the owning task future is dropped, so a test can prove
    /// that an abandoned task really stopped running instead of lingering as a
    /// detached zombie.
    struct ExitMarker(Arc<AtomicBool>);

    impl Drop for ExitMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Budget after which a deliberate non-yielding stretch ends on its own.
    ///
    /// The stretch exists to prove what an abort cannot reach, so it spins
    /// synchronously instead of parking on an abort landing point. The budget is
    /// the second line of defence behind the test's release guard: the runtime
    /// the stretch belongs to waits for its pinned worker when it shuts down, so
    /// an unbounded spin would turn one failed assertion into a hung suite.
    const PINNED_STRETCH_BUDGET: Duration = Duration::from_secs(5);

    /// Releases a pinned stretch on drop, so a failing assertion never leaves a
    /// task spinning after the test has returned.
    struct StretchRelease(Arc<AtomicBool>);

    impl Drop for StretchRelease {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A task parked at a suspension point observes its abort at once, so the
    /// confirmation completes long before the deadline expires.
    #[tokio::test]
    async fn aborting_a_parked_task_confirms_its_exit() {
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        // Let the task reach its suspension point, so the abort has a poll
        // boundary to land on instead of racing the very first poll.
        tokio::task::yield_now().await;

        let outcome = abort_and_reap("op-test", "parked-task", handle).await;

        assert_eq!(
            outcome,
            TaskExit::Confirmed,
            "a parked task must confirm its exit once it is aborted"
        );
    }

    /// A task inside a non-yielding stretch cannot observe its abort at all, so
    /// the confirmation expires while the task provably still runs. The abort
    /// stays delivered: once the stretch ends the task dies without a second
    /// kill.
    #[test]
    fn a_pinned_task_is_orphaned_at_the_deadline_and_dies_once_it_yields() {
        // The pinned task owns a runtime of its own: the fixture's whole point
        // is a stretch that never yields back to its scheduler, so it must not
        // be able to pin the scheduler the confirmation runs on.
        let pinned_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the pinned runtime starts");

        let released = Arc::new(AtomicBool::new(false));
        // The pinned stretch spins on this flag, and its runtime waits for the
        // pinned worker when it shuts down, so the flag is released on drop.
        let _release_guard = StretchRelease(Arc::clone(&released));
        let exited = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();

        let handle = {
            let released = Arc::clone(&released);
            let marker = ExitMarker(Arc::clone(&exited));
            pinned_runtime.spawn(async move {
                let _marker = marker;
                let _ = entered_tx.send(());
                // A non-yielding stretch: the task reaches no suspension point,
                // so the abort cannot land while it spins. Its budget ends the
                // spin even when the release guard never fires.
                let deadline = std::time::Instant::now() + PINNED_STRETCH_BUDGET;
                while !released.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    std::thread::yield_now();
                }
            })
        };

        let confirmation_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the confirmation runtime starts");

        let outcome = confirmation_runtime.block_on(async move {
            entered_rx
                .await
                .expect("the pinned task must enter its non-yielding stretch");
            // The confirmation runs as a task of its own runtime, which is also
            // what makes the compressed deadline of the test seam reachable: a
            // task-scoped override belongs to a task.
            tokio::spawn(with_task_exit_deadline(
                Duration::from_millis(50),
                abort_and_reap("op-test", "pinned-task", handle),
            ))
            .await
            .expect("the confirmation task joins")
        });

        assert_eq!(
            outcome,
            TaskExit::Orphaned,
            "a task that never yields cannot confirm its exit before the deadline"
        );
        assert!(
            !exited.load(Ordering::SeqCst),
            "the abort cannot land while the task never reaches a suspension point"
        );

        released.store(true, Ordering::SeqCst);
        let stopped = confirmation_runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !exited.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
        });
        assert!(
            stopped.is_ok(),
            "the abandoned task must end once its non-yielding stretch ends"
        );
    }

    /// The completion shape of the same primitive returns the task's own
    /// result, so a caller learns what its task produced instead of only that
    /// it stopped.
    #[tokio::test]
    async fn a_bounded_await_returns_a_completed_task_result() {
        let handle = tokio::spawn(async { 7u32 });

        let outcome = await_bounded("op-test", "quick-task", handle).await;

        match outcome {
            Awaited::Joined(Ok(value)) => assert_eq!(value, 7),
            other => panic!("expected the task output, got {other:?}"),
        }
    }
}
