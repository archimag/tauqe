//! Serialized interactive task execution and crash recovery.

use crate::api::{Application, OperationGuard};

use std::future::Future;
use std::sync::Arc;
use tauqe_core::context::ContextManager;
use tauqe_core::control::outcome::{LoopResult, SystemError};
use tauqe_core::control::repl::ReplArtifact;
use tokio::sync::Mutex;

#[derive(Default)]
pub(crate) struct InteractiveExecution {
    lock: Mutex<()>,
}

pub struct InteractiveTaskOutcome {
    pub result: Result<LoopResult<ReplArtifact>, SystemError>,
    pub context: ContextManager,
}

#[derive(Debug, thiserror::Error)]
pub enum InteractiveTaskError {
    #[error("Interactive execution requires an active operation for this session")]
    OperationNotOwned,
    #[error("{0}")]
    TaskFailed(String),
}

pub struct InteractiveTask {
    application: Application,
    operation: Arc<OperationGuard>,
    task: tokio::task::JoinHandle<InteractiveTaskOutcome>,
}

impl Application {
    pub(crate) fn start_interactive_task<F, R>(
        &self,
        operation: Arc<OperationGuard>,
        run: F,
    ) -> Result<InteractiveTask, InteractiveTaskError>
    where
        F: FnOnce(ContextManager) -> R + Send + 'static,
        R: Future<Output = InteractiveTaskOutcome> + Send + 'static,
    {
        if !self.owns_operation(&operation) {
            return Err(InteractiveTaskError::OperationNotOwned);
        }
        let application = self.clone();
        let task = tokio::spawn(async move {
            let _guard = application.interactive_execution().lock.lock().await;
            let context = application.context_for_turn().await;
            run(context).await
        });
        operation.set_abort_handle(task.abort_handle());
        Ok(InteractiveTask {
            application: self.clone(),
            operation,
            task,
        })
    }
}

impl InteractiveTask {
    /// Keep stream consumption outside this await. A control error is a normal
    /// task outcome; only an aborted or panicked task triggers recovery.
    pub async fn join(
        self,
        operation_id: &str,
    ) -> Result<InteractiveTaskOutcome, InteractiveTaskError> {
        let joined = self.task.await;
        self.operation.clear_abort_handle();
        match joined {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.application
                    .unwind_abandoned_transaction(operation_id)
                    .await;
                Err(InteractiveTaskError::TaskFailed(error.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauqe_core::context::ContextAccess;

    fn application(root: &std::path::Path) -> Application {
        std::fs::write(root.join("discovered.rs"), "source").unwrap();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(
            tauqe_core::config::AppConfig::default(),
            root.to_path_buf(),
            history,
        )
    }

    #[tokio::test]
    async fn control_errors_are_normal_task_outcomes_and_streams_can_be_consumed_before_join() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let (sink, mut events) = tauqe_core::control::context::EventSink::channel();
        let task = app
            .start_interactive_task(Arc::clone(&operation), move |mut context| async move {
                context
                    .add_auto_file("discovered.rs", ContextAccess::ReadOnly)
                    .unwrap();
                sink.emit_text_delta("Reply");
                InteractiveTaskOutcome {
                    result: Err(SystemError::Provider("offline".into())),
                    context,
                }
            })
            .unwrap();
        assert!(
            matches!(events.recv().await.unwrap(), tauqe_core::providers::StreamEvent::TextDelta(text) if text == "Reply")
        );
        let outcome = task.join("interactive-control-error").await.unwrap();
        assert!(
            matches!(outcome.result, Err(SystemError::Provider(reason)) if reason == "offline")
        );
        assert!(outcome.context.contains("discovered.rs").unwrap());
        assert!(
            app.context_state().await.items.is_empty(),
            "joining does not adopt unfinished interactive context"
        );
        assert!(!app.abort_operation(operation.id()));
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn context_is_captured_after_the_private_execution_lock_is_acquired() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let lock = app.interactive_execution().lock.lock().await;
        let polled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        let task = app
            .start_interactive_task(Arc::clone(&operation), move |context| async move {
                observed.store(true, Ordering::SeqCst);
                InteractiveTaskOutcome {
                    result: Err(SystemError::Cancelled),
                    context,
                }
            })
            .unwrap();
        tokio::task::yield_now().await;
        assert!(!polled.load(Ordering::SeqCst));
        app.test_mutate_context(|context| {
            context.add_auto_file("discovered.rs", ContextAccess::ReadOnly)
        })
        .await
        .unwrap()
        .unwrap();
        drop(lock);
        let outcome = task.join("interactive-lock").await.unwrap();
        assert!(polled.load(Ordering::SeqCst));
        assert!(outcome.context.contains("discovered.rs").unwrap());
    }

    async fn panicking_task(_context: ContextManager) -> InteractiveTaskOutcome {
        panic!("simulated interactive panic");
    }

    #[tokio::test]
    async fn panic_is_reported_after_recovery_and_abort_handle_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let task = app
            .start_interactive_task(Arc::clone(&operation), panicking_task)
            .unwrap();
        assert!(
            matches!(task.join("interactive-panic").await, Err(InteractiveTaskError::TaskFailed(reason)) if reason.contains("simulated interactive panic"))
        );
        assert!(!app.abort_operation(operation.id()));
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn a_forced_abort_remains_a_task_failure_for_the_transport_to_classify() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = app
            .start_interactive_task(Arc::clone(&operation), move |_context| async move {
                started.send(()).unwrap();
                std::future::pending::<InteractiveTaskOutcome>().await
            })
            .unwrap();
        ready.await.unwrap();
        assert_eq!(app.cancel_active_operation(), Some(operation.id()));
        assert!(app.abort_operation(operation.id()));
        assert!(matches!(
            task.join("interactive-abort").await,
            Err(InteractiveTaskError::TaskFailed(_))
        ));
        assert!(*operation.cancel_receiver().borrow());
        assert!(!app.abort_operation(operation.id()));
        assert!(app.try_begin_operation().is_err());
    }

    #[tokio::test]
    async fn a_foreign_operation_never_starts_the_execution_factory() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let other = Application::default();
        let foreign = Arc::new(other.try_begin_operation().unwrap());
        let polled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        let task = app.start_interactive_task(foreign, move |context| async move {
            observed.store(true, Ordering::SeqCst);
            InteractiveTaskOutcome {
                result: Err(SystemError::Cancelled),
                context,
            }
        });
        assert!(matches!(task, Err(InteractiveTaskError::OperationNotOwned)));
        assert!(!polled.load(Ordering::SeqCst));
    }
}
