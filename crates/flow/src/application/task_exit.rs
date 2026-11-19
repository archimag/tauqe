//! Bounded task-exit confirmation with an explicit caller-owned budget.

use std::time::Duration;
use tokio::task::{JoinError, JoinHandle};

/// Confirmation that a task the caller terminated stopped running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskExit {
    /// The task ended within the caller deadline.
    Confirmed,
    /// The deadline expired while the task was still running.
    ///
    /// The abort stays delivered, so the task ends at its next suspension
    /// point; its handle was released deliberately and no second kill is
    /// attempted.
    Orphaned,
}

/// Outcome of awaiting a spawned task under the caller deadline.
#[derive(Debug)]
pub enum Awaited<T> {
    /// The task ended within the deadline; its join outcome travels unchanged.
    Joined(Result<T, JoinError>),
    /// The deadline expired while the task was still running.
    Orphaned,
}

/// Abort once and confirm termination within the supplied deadline.
pub async fn abort_and_reap_with_deadline<T>(
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

/// Await normal completion within the supplied deadline.
pub async fn await_with_deadline<T>(
    operation: &str,
    task: &str,
    deadline: Duration,
    mut handle: JoinHandle<T>,
) -> Awaited<T> {
    match confirm_exit_with_deadline(operation, task, deadline, &mut handle).await {
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

pub const OPERATION_UNWIND_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
pub const TASK_EXIT_DEADLINE: std::time::Duration = OPERATION_UNWIND_GRACE;
