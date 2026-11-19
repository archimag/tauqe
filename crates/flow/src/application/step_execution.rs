//! Supervision and context adoption of implementation step tasks.

use crate::api::{Application, OperationGuard};

use crate::application::step_dispatch::StepLoopReport;
use std::future::Future;
use tauqe_core::context::ContextManager;
use tauqe_core::git::checkpoint::RecoveryOutcome;

impl Application {
    /// Register the task that actually executes the step, settle recovery after
    /// a forced abort or panic, and adopt context before the event relay closes.
    pub(crate) async fn run_step_task(
        &self,
        operation: &OperationGuard,
        operation_id: &str,
        run: impl Future<Output = (StepLoopReport, ContextManager)> + Send + 'static,
    ) -> StepLoopReport {
        if !self.owns_operation(operation) {
            return StepLoopReport::Failed(
                "Step execution requires an active operation for this session".to_string(),
            );
        }
        let cancel = operation.cancel_receiver();
        let task = tokio::spawn(run);
        operation.set_abort_handle(task.abort_handle());
        let joined = task.await;
        operation.clear_abort_handle();
        let (report, context) = match joined {
            Ok(joined) => joined,
            Err(error) => {
                self.unwind_abandoned_transaction(operation_id).await;
                let cancelled = *cancel.borrow();
                if !cancelled {
                    tracing::warn!(operation_id, error = %error, "step run task terminated unexpectedly");
                }
                let report = if cancelled {
                    StepLoopReport::Cancelled
                } else {
                    StepLoopReport::Failed(format!(
                        "The step run task terminated unexpectedly: {error}"
                    ))
                };
                (report, self.context_for_turn().await)
            }
        };
        if let Err(error) = self.merge_turn_context(context).await {
            tracing::warn!(error = %error, "Failed to merge the step context");
        }
        report
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
    pub(crate) async fn unwind_abandoned_transaction(&self, op_id: &str) {
        let repo_root = self.repository_root().await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tauqe_core::context::ContextAccess;
    use tauqe_core::control::outcome::{LoopResult, ResponseEnvelope};

    fn application(root: &std::path::Path) -> Application {
        for file in ["kept.rs", "discovered.rs"] {
            std::fs::write(root.join(file), "source").unwrap();
        }
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(
            tauqe_core::config::AppConfig::default(),
            root.to_path_buf(),
            sender,
        )
    }

    #[tokio::test]
    async fn normal_completion_merges_context_and_clears_the_task_abort_handle() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        app.test_mutate_context(|context| context.add_file("kept.rs", ContextAccess::ReadOnly))
            .await
            .unwrap()
            .unwrap();
        let mut context = app.context_for_turn().await;
        let operation = app.try_begin_operation().unwrap();
        let report = app
            .run_step_task(&operation, "step-normal", async move {
                context
                    .add_auto_file("discovered.rs", ContextAccess::ReadOnly)
                    .unwrap();
                (
                    StepLoopReport::Turn(LoopResult::Diverged(ResponseEnvelope::default())),
                    context,
                )
            })
            .await;
        assert!(matches!(
            report,
            StepLoopReport::Turn(LoopResult::Diverged(_))
        ));
        assert_eq!(app.context_state().await.items.len(), 2);
        assert_eq!(app.active_operation_id(), Some(operation.id()));
        assert!(!app.abort_operation(operation.id()));
        assert!(app.try_begin_operation().is_err());
        drop(operation);
        assert!(app.try_begin_operation().is_ok());
    }

    async fn panic_after_local_change(
        mut context: ContextManager,
    ) -> (StepLoopReport, ContextManager) {
        context
            .add_auto_file("discovered.rs", ContextAccess::ReadOnly)
            .unwrap();
        panic!("simulated step panic");
    }

    #[tokio::test]
    async fn panic_without_cancel_is_a_failure_and_keeps_shared_context() {
        // Recovery has no Git transaction to unwind in this directory. The
        // supervisor still reports the panic and discards unfinished local context.
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        app.test_mutate_context(|context| context.add_file("kept.rs", ContextAccess::ReadOnly))
            .await
            .unwrap()
            .unwrap();
        let operation = app.try_begin_operation().unwrap();
        let report = app
            .run_step_task(
                &operation,
                "step-panic",
                panic_after_local_change(app.context_for_turn().await),
            )
            .await;
        assert!(
            matches!(report, StepLoopReport::Failed(reason) if reason.contains("The step run task terminated unexpectedly:") && reason.contains("simulated step panic"))
        );
        let state = app.context_state().await;
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].path, "kept.rs");
        assert!(!app.abort_operation(operation.id()));
    }

    #[tokio::test]
    async fn forced_abort_uses_the_operation_cancel_signal_and_preserves_shared_context() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        app.test_mutate_context(|context| context.add_file("kept.rs", ContextAccess::ReadOnly))
            .await
            .unwrap()
            .unwrap();
        let mut context = app.context_for_turn().await;
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let (started, ready) = tokio::sync::oneshot::channel();
        let runner = app.clone();
        let task_operation = Arc::clone(&operation);
        let supervisor = tokio::spawn(async move {
            runner
                .run_step_task(&task_operation, "step-abort", async move {
                    context
                        .add_auto_file("discovered.rs", ContextAccess::ReadOnly)
                        .unwrap();
                    started.send(()).unwrap();
                    std::future::pending::<(StepLoopReport, ContextManager)>().await
                })
                .await
        });
        ready.await.unwrap();
        assert_eq!(app.cancel_active_operation(), Some(operation.id()));
        assert!(app.abort_operation(operation.id()));
        assert!(matches!(
            supervisor.await.unwrap(),
            StepLoopReport::Cancelled
        ));
        let state = app.context_state().await;
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].path, "kept.rs");
        assert_eq!(app.active_operation_id(), Some(operation.id()));
        assert!(!app.abort_operation(operation.id()));
    }

    #[tokio::test]
    async fn a_foreign_operation_never_starts_the_supplied_task() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        let _own = app.try_begin_operation().unwrap();
        let polled = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&polled);
        let context = app.context_for_turn().await;
        let report = app
            .run_step_task(&foreign, "foreign-step", async move {
                marker.store(true, Ordering::SeqCst);
                (StepLoopReport::Cancelled, context)
            })
            .await;
        assert!(
            matches!(report, StepLoopReport::Failed(reason) if reason == "Step execution requires an active operation for this session")
        );
        assert!(!polled.load(Ordering::SeqCst));
        assert!(!other.abort_operation(foreign.id()));
    }
}
