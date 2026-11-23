//! Supervision and context adoption of implementation step tasks.

use crate::api::OperationGuard;

use super::{StepAttemptReport, StepLoopReport, StepResources};
use crate::workflows::{
    round::RoundError,
    turn::{DiskWorker, TurnSettlement},
};
use std::future::Future;
use std::path::PathBuf;
use tauqe_core::context::ContextManager;
use tauqe_core::execution::{EventSink, ExecutionError};
use tauqe_core::git::checkpoint::RecoveryOutcome;
use tauqe_core::providers::StreamEvent;

impl StepResources {
    /// The cancellable task drives rounds; only its supervisor owns the disk join.
    pub(super) async fn supervise_step_attempt(
        &self,
        operation: &OperationGuard,
        operation_id: &str,
        root: PathBuf,
        worker: DiskWorker,
        events: &EventSink,
        run: impl Future<Output = (TurnSettlement, ContextManager)> + Send + 'static,
    ) -> StepLoopReport {
        let joined = join_step_task(operation, run).await;
        let disk = worker.finish().await;
        let edit = disk.finalized_edit();
        let recovery_error = if edit.is_none() { disk.notice() } else { None };
        if disk.worker_failed() {
            Self::unwind_abandoned_transaction(root, operation_id).await;
        }
        let (outcome, mut context, task_cancelled) = match joined {
            Ok((settlement, context)) => (settlement.outcome, Some(context), settlement.cancelled),
            Err(error) => {
                let reason = format!("The step run task terminated unexpectedly: {error}");
                if !*operation.cancel_receiver().borrow() {
                    tracing::warn!(operation_id, %error, "step run task terminated unexpectedly");
                }
                (
                    Err(ExecutionError::Other(reason).into()),
                    None,
                    error.is_cancelled(),
                )
            }
        };
        let cancelled = task_cancelled
            || *operation.cancel_receiver().borrow()
            || matches!(
                &outcome,
                Err(RoundError::Execution(ExecutionError::Cancelled))
            );
        let publish_context = edit
            .as_ref()
            .is_some_and(|edit| edit.applied && !edit.changed_files.is_empty());
        if publish_context {
            let mut updated = match context.take() {
                Some(context) => context,
                None => self.context.context_for_turn().await,
            };
            tauqe_core::blocking::run_blocking(|| updated.clear_transient());
            context = Some(updated);
        }
        if let Some(context) = context {
            match self.context.merge_turn_context(context).await {
                Ok(state) if publish_context => events.emit(StreamEvent::ContextChanged(state)),
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "Failed to merge the step context"),
            }
        }
        if let Some(error) = &recovery_error {
            events.emit_text_delta(format!("\n\n{error}\n\n"));
        }
        StepLoopReport::Turn(Box::new(StepAttemptReport {
            outcome,
            edit,
            recovery_error,
            cancelled,
        }))
    }

    /// Unwinds the Git transaction of a turn that could not settle it itself.
    ///
    /// A turn that is force-aborted or panics drops its control loop mid-flight, so
    /// the rollback the edit loop owns never runs and the checkpoint and step
    /// commits it captured stay in the repository. The checkpoint recovery pass
    /// resolves exactly that residue, so the working tree returns to its pre-turn
    /// revision at the moment the turn ends instead of waiting for the next daemon
    /// start.
    ///
    /// The pass is best effort. A recovery that refuses because it cannot prove the
    /// transaction abandoned reports its reason and leaves the residue to the
    /// startup pass, which runs with no turn in flight; the turn's own ending is
    /// never replaced by a recovery failure.
    async fn unwind_abandoned_transaction(repo_root: PathBuf, op_id: &str) {
        let recovered = tauqe_core::blocking::run_blocking_async(move || {
            tauqe_core::git::checkpoint::recover_interrupted_workflow(&repo_root)
        })
        .await;
        match recovered {
            Ok(Ok(RecoveryOutcome::NothingToDo)) => {}
            Ok(Ok(outcome)) => {
                tracing::warn!(
                    operation_id = %op_id,
                    "Unwound the transaction of an interrupted turn: {}",
                    outcome,
                );
            }
            Ok(Err(error)) => {
                tracing::error!(
                    operation_id = %op_id,
                    "Failed to unwind the transaction of an interrupted turn: {:#}",
                    error,
                );
            }
            Err(error) => {
                tracing::error!(
                    operation_id = %op_id,
                    "Failed to schedule the transaction unwind of an interrupted turn: blocking task failed: {error}",
                );
            }
        }
    }
}

async fn join_step_task<T: Send + 'static>(
    operation: &OperationGuard,
    run: impl Future<Output = T> + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let task = tokio::spawn(run);
    operation.set_abort_handle(task.abort_handle());
    let joined = task.await;
    operation.clear_abort_handle();
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;

    #[tokio::test]
    async fn forced_abort_clears_the_round_handle_without_releasing_the_operation() {
        let app = Application::default();
        let operation = std::sync::Arc::new(app.try_begin_operation().unwrap());
        let (started, ready) = tokio::sync::oneshot::channel();
        let task_operation = operation.clone();
        let supervisor = tokio::spawn(async move {
            join_step_task(&task_operation, async move {
                started.send(()).unwrap();
                std::future::pending::<()>().await
            })
            .await
        });
        ready.await.unwrap();
        assert_eq!(app.cancel_active_operation(), Some(operation.id()));
        assert!(app.abort_operation(operation.id()));
        assert!(supervisor.await.unwrap().unwrap_err().is_cancelled());
        assert_eq!(app.active_operation_id(), Some(operation.id()));
        assert!(!app.abort_operation(operation.id()));
    }
}
