//! Technical confirmation of spawned task completion.

use std::time::Duration;
use tokio::task::{JoinError, JoinHandle};

/// Result of a task join, independent of the scenario that launched it.
#[derive(Debug)]
pub enum TaskOutcome<T> {
    Completed(T),
    Failed(String),
    Cancelled,
    Orphaned,
}

fn classify<T>(result: Result<T, JoinError>) -> TaskOutcome<T> {
    match result {
        Ok(value) => TaskOutcome::Completed(value),
        Err(error) if error.is_cancelled() => TaskOutcome::Cancelled,
        Err(error) => TaskOutcome::Failed(error.to_string()),
    }
}

/// Wait for a task whose lifetime is governed by its owning scenario.
pub async fn join<T>(handle: JoinHandle<T>) -> TaskOutcome<T> {
    classify(handle.await)
}

/// Confirm termination within a caller-owned budget; never claim an expired join succeeded.
pub async fn wait<T>(
    operation: &str,
    task: &str,
    deadline: Duration,
    handle: &mut JoinHandle<T>,
) -> TaskOutcome<T> {
    match tokio::time::timeout(deadline, handle).await {
        Ok(result) => classify(result),
        Err(_) => {
            tracing::warn!(
                operation_id = operation,
                task,
                deadline_ms = deadline.as_millis() as u64,
                "task-exit confirmation expired; releasing the handle of a still-running task"
            );
            TaskOutcome::Orphaned
        }
    }
}

/// Deliver an abort and confirm the task stopped within the same caller budget.
pub async fn stop<T>(
    operation: &str,
    task: &str,
    deadline: Duration,
    handle: &mut JoinHandle<T>,
) -> TaskOutcome<T> {
    handle.abort();
    wait(operation, task, deadline, handle).await
}

/// Confirmation that a task the caller terminated stopped running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskExit {
    Confirmed,
    Orphaned,
}

#[derive(Debug)]
pub enum Awaited<T> {
    Joined(Result<T, JoinError>),
    Orphaned,
}

/// Compatibility surface for callers that need the original join error.
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

pub async fn await_with_deadline<T>(
    operation: &str,
    task: &str,
    deadline: Duration,
    mut handle: JoinHandle<T>,
) -> Awaited<T> {
    match confirm_exit_with_deadline(operation, task, deadline, &mut handle).await {
        Some(result) => Awaited::Joined(result),
        None => Awaited::Orphaned,
    }
}

async fn confirm_exit_with_deadline<T>(
    operation: &str,
    task: &str,
    deadline: Duration,
    handle: &mut JoinHandle<T>,
) -> Option<Result<T, JoinError>> {
    match tokio::time::timeout(deadline, handle).await {
        Ok(result) => Some(result),
        Err(_) => {
            tracing::warn!(
                operation_id = operation,
                task,
                deadline_ms = deadline.as_millis() as u64,
                "task-exit confirmation expired; releasing the handle of a still-running task"
            );
            None
        }
    }
}
