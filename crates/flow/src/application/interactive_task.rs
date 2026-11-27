//! Serialized interactive task execution and crash recovery.

#[cfg(test)]
use crate::api::Application;
use crate::api::OperationGuard;
use crate::application::context::ContextSession;
use crate::application::history::HistorySession;
use crate::application::infrastructure::execution::ExecutionSession;
use crate::application::interactive_history::{InteractiveHistoryStart, OpenInteractiveTurn};

use crate::workflows::repl::{documents::DocumentChanges, ReplResult};
use std::future::Future;
use std::sync::Arc;
use tauqe_core::config::AppConfig;
use tauqe_core::context::ContextManager;
use tauqe_core::execution::usage::UsageBinding;
use tauqe_core::execution::AcceptedRoundId;
use tauqe_core::execution::ExecutionError as SystemError;
use tauqe_core::history::TurnRoundRecord;
use tauqe_core::workflow::policy::owned::Settlement;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[cfg(test)]
type DocumentWriteGate = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);

#[derive(Clone, Default)]
pub(crate) struct InteractiveExecution {
    lock: Arc<Mutex<()>>,
    #[cfg(test)]
    pub(crate) document_write_gate: Arc<Mutex<Option<DocumentWriteGate>>>,
    #[cfg(test)]
    pub(crate) report_gate: Arc<Mutex<Option<DocumentWriteGate>>>,
    #[cfg(test)]
    pub(crate) continuation_gate: Arc<Mutex<Option<DocumentWriteGate>>>,
    #[cfg(test)]
    pub(crate) terminal_gate: Arc<Mutex<Option<DocumentWriteGate>>>,
}

#[cfg(test)]
impl InteractiveExecution {
    pub(crate) async fn pause_at(gate: &Arc<Mutex<Option<DocumentWriteGate>>>) {
        if let Some((entered, resume)) = gate.lock().await.take() {
            entered.notify_one();
            resume.notified().await;
        }
    }
}

pub struct InteractiveTaskOutcome {
    pub(crate) result: Result<ReplResult, SystemError>,
    pub context: ContextManager,
}

#[derive(Debug, thiserror::Error)]
pub enum InteractiveTaskError {
    #[error("Interactive execution requires an active operation for this session")]
    OperationNotOwned,
    #[error("{0}")]
    TaskFailed(String),
    #[error("{0}")]
    Interrupted(#[source] tokio::task::JoinError),
}

pub(crate) struct InteractiveTaskReport {
    pub(crate) preparation: OpenInteractiveTurn,
    pub(crate) task: Result<InteractiveTaskOutcome, InteractiveTaskError>,
    pub(crate) rounds: Vec<TurnRoundRecord>,
    pub(crate) source: Option<AcceptedRoundId>,
    pub(crate) settlement: Settlement,
    pub(crate) diagnostic: Option<String>,
    pub(crate) documents: DocumentChanges,
}

pub(crate) struct InteractiveTaskResult {
    pub(crate) task: Result<InteractiveTaskOutcome, InteractiveTaskError>,
    pub(crate) settlement: Settlement,
    pub(crate) diagnostic: Option<String>,
}

pub struct InteractiveTask {
    operation: Arc<OperationGuard>,
    task: tokio::task::JoinHandle<InteractiveTaskResult>,
    preparation: OpenInteractiveTurn,
    read_rounds: Box<dyn FnOnce() -> Vec<TurnRoundRecord> + Send>,
    _serial: OwnedMutexGuard<()>,
}

#[cfg(test)]
impl Application {
    pub(crate) async fn start_test_task<F, R>(
        &self,
        operation: Arc<OperationGuard>,
        read_rounds: Box<dyn FnOnce() -> Vec<TurnRoundRecord> + Send>,
        run: F,
    ) -> Result<InteractiveTask, InteractiveTaskError>
    where
        F: FnOnce(ContextManager) -> R + Send + 'static,
        R: Future<Output = InteractiveTaskOutcome> + Send + 'static,
    {
        use crate::application::infrastructure::execution::ExecutionFactory;
        use tauqe_core::execution::usage::{UsageBinding, UsageScope, UsageSink};
        struct Noop;
        impl UsageSink for Noop {
            fn record(&self, _: &tauqe_core::providers::ModelUsageInfo) {}
        }
        // The test uses the production admission, registration and launch path.
        let factory = ExecutionFactory::bind(self, &operation)
            .await
            .map_err(|_| InteractiveTaskError::OperationNotOwned)?;
        let scope = UsageScope::new(Arc::new(Noop));
        scope.activate();
        let config = Arc::new(
            factory
                .resolve_model(
                    factory.selection().clone(),
                    tauqe_core::models::ModelRole::REPL,
                )
                .config,
        );
        let (serial, started) = self
            .interactive_execution()
            .prepare(
                self.context_session(),
                &self.execution_session(),
                &operation,
                self.history_session(),
                config,
                UsageBinding::Bound(scope),
                "Test interactive task".into(),
                &tauqe_core::execution::EventSink::noop(),
            )
            .await?;
        Ok(self.interactive_execution().start_prepared(
            operation,
            serial,
            started.context,
            started.open,
            read_rounds,
            move |context, operation| async move {
                let task = tokio::spawn(run(context));
                operation.set_abort_handle(task.abort_handle());
                let result = task.await.map_err(InteractiveTaskError::Interrupted);
                operation.clear_abort_handle();
                InteractiveTaskResult {
                    task: result,
                    settlement: Settlement::NoChanges { cancelled: false },
                    diagnostic: None,
                }
            },
        ))
    }
}

impl InteractiveExecution {
    /// Own the serial lease and the interruptible Scribe stage. Journal registration
    /// is not abortable: its protected writer must deliver or interrupt its handle.
    #[allow(clippy::too_many_arguments)] // Call-scoped inputs stay explicit rather than forming another binding.
    pub(crate) async fn prepare(
        &self,
        context: &ContextSession,
        execution: &ExecutionSession,
        operation: &Arc<OperationGuard>,
        history: &HistorySession,
        config: Arc<AppConfig>,
        usage: UsageBinding,
        prompt: String,
        events: &tauqe_core::execution::EventSink,
    ) -> Result<(OwnedMutexGuard<()>, InteractiveHistoryStart), InteractiveTaskError> {
        let serial = self.acquire(execution, operation).await?;
        let cancel = operation.hard_cancellation();
        let sink = events.clone();
        let compact_history = history.clone();
        let compact_execution = execution.clone();
        let compact_operation = operation.clone();
        let mut compaction = tokio::spawn(async move {
            compact_history
                .compact_bound_history(
                    &compact_execution,
                    &compact_operation,
                    &config,
                    usage,
                    None,
                    &sink,
                )
                .await
        });
        operation.set_abort_handle(compaction.abort_handle());
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                compaction.abort();
                compaction.await
            }
            result = &mut compaction => result,
        };
        operation.clear_abort_handle();
        let result = result.map_err(|error| {
            InteractiveTaskError::TaskFailed(format!(
                "Could not prepare interactive turn history: {error}"
            ))
        })?;
        result.map_err(|error| {
            InteractiveTaskError::TaskFailed(format!(
                "Could not start interactive turn history: {error}"
            ))
        })?;
        if cancel.is_cancelled() {
            return Err(InteractiveTaskError::TaskFailed(
                "Interactive preparation was cancelled".into(),
            ));
        }
        let snapshot = context.context_for_turn().await;
        // Once registration begins, wait for the protected writer even if cancellation
        // arrives. The caller receives the open handle and settles it normally.
        let started = history
            .begin_compacted(execution, operation.clone(), prompt, snapshot)
            .await
            .map_err(|error| {
                InteractiveTaskError::TaskFailed(format!(
                    "Could not start interactive turn history: {error}"
                ))
            })?;
        Ok((serial, started))
    }

    /// Admission to the serialized turn precedes both journal preparation and computation.
    pub(crate) async fn acquire(
        &self,
        execution: &ExecutionSession,
        operation: &OperationGuard,
    ) -> Result<OwnedMutexGuard<()>, InteractiveTaskError> {
        if !execution.owns(operation) || !operation.reserve_supervisor_release() {
            return Err(InteractiveTaskError::OperationNotOwned);
        }
        let cancel = operation.hard_cancellation();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(InteractiveTaskError::TaskFailed("Interactive preparation was cancelled".into())),
            guard = Arc::clone(&self.lock).lock_owned() => {
                if cancel.is_cancelled() {
                    Err(InteractiveTaskError::TaskFailed("Interactive preparation was cancelled".into()))
                } else {
                    Ok(guard)
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // Every argument is an owned prerequisite of the running turn.
    pub(crate) fn start_prepared<F, R>(
        &self,
        operation: Arc<OperationGuard>,
        serial: OwnedMutexGuard<()>,
        context: ContextManager,
        preparation: OpenInteractiveTurn,
        read_rounds: Box<dyn FnOnce() -> Vec<TurnRoundRecord> + Send>,
        run: F,
    ) -> InteractiveTask
    where
        F: FnOnce(ContextManager, Arc<OperationGuard>) -> R + Send + 'static,
        R: Future<Output = InteractiveTaskResult> + Send + 'static,
    {
        let owner = operation.clone();
        // This coordinator is never the abort target: it retains the policy
        // result after either child task has stopped.
        let task = tokio::spawn(async move { run(context, owner).await });
        InteractiveTask {
            operation,
            task,
            preparation,
            read_rounds,
            _serial: serial,
        }
    }
}

impl InteractiveTask {
    /// The accounting supervisor owns the join even if its external waiter leaves.
    pub(crate) async fn join(self) -> InteractiveTaskReport {
        self.drain().await
    }

    async fn drain(self) -> InteractiveTaskReport {
        let result = self.task.await;
        self.operation.clear_abort_handle();
        let InteractiveTaskResult {
            task,
            settlement,
            diagnostic,
        } = result.unwrap_or_else(|error| {
            let diagnostic = format!("Interactive coordinator failed: {error}");
            InteractiveTaskResult {
                task: Err(InteractiveTaskError::TaskFailed(diagnostic.clone())),
                settlement: Settlement::RolledBack(
                    tauqe_core::edits::transaction::RollbackOutcome::Incomplete {
                        policy_error: None,
                        backup_error: diagnostic.clone(),
                    },
                ),
                diagnostic: Some(diagnostic),
            }
        });
        let rounds = (self.read_rounds)();
        let source = task
            .as_ref()
            .ok()
            .and_then(|outcome| outcome.result.as_ref().ok())
            .and_then(ReplResult::source);
        let documents = task
            .as_ref()
            .ok()
            .and_then(|outcome| outcome.result.as_ref().ok())
            .and_then(ReplResult::changes)
            .cloned()
            .unwrap_or_default();
        InteractiveTaskReport {
            preparation: self.preparation,
            task,
            rounds,
            source,
            settlement,
            diagnostic,
            documents,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::infrastructure::execution::ExecutionFactory;
    use crate::application::interactive_history::InterruptTurn;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauqe_core::context::ContextAccess;
    use tauqe_core::execution::usage::{UsageScope, UsageSink};
    use tauqe_core::history::entry::HistoryEntry;
    use tauqe_core::models::ProviderKind;
    use tauqe_core::providers::{LlmProvider, LlmRequest, ProviderResult, StreamEvent};
    use tokio::sync::{mpsc, Notify};

    struct NoopUsage;
    impl UsageSink for NoopUsage {
        fn record(&self, _: &tauqe_core::providers::ModelUsageInfo) {}
    }

    struct WaitingScribe {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        resume: Notify,
        exited: AtomicBool,
    }

    #[async_trait::async_trait]
    impl LlmProvider for WaitingScribe {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            _: tauqe_core::stop::HardCancellation,
        ) -> ProviderResult {
            struct Exit<'a>(&'a AtomicBool);
            impl Drop for Exit<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _exit = Exit(&self.exited);
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            self.resume.notified().await;
            let _ = tx
                .send(StreamEvent::TextDelta(
                    r#"{"summary":"Earlier tasks completed."}"#.into(),
                ))
                .await;
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }
    }

    async fn compacting_app(root: &std::path::Path) -> Application {
        let (sender, _) = mpsc::unbounded_channel();
        let mut config = tauqe_core::config::AppConfig::default();
        config.history.budget_tokens = 60;
        config.history.tail_turns = 1;
        let app = Application::for_repository(config, root.to_path_buf(), sender);
        for index in 0..4 {
            app.history_session()
                .record_history_exchange(
                    format!("Task {index}"),
                    Vec::new(),
                    format!("Done {index}"),
                    None,
                )
                .await
                .unwrap();
        }
        app
    }

    fn waiting_scribe() -> (Arc<WaitingScribe>, tokio::sync::oneshot::Receiver<()>) {
        let (entered, ready) = tokio::sync::oneshot::channel();
        (
            Arc::new(WaitingScribe {
                entered: std::sync::Mutex::new(Some(entered)),
                resume: Notify::new(),
                exited: AtomicBool::new(false),
            }),
            ready,
        )
    }

    async fn prepare_with_scribe(
        app: &Application,
        operation: &Arc<OperationGuard>,
    ) -> Result<(OwnedMutexGuard<()>, InteractiveHistoryStart), InteractiveTaskError> {
        let factory = ExecutionFactory::bind(app, operation)
            .await
            .map_err(|error| InteractiveTaskError::TaskFailed(error.to_string()))?;
        let config = Arc::new(
            factory
                .resolve_model(
                    factory.selection().clone(),
                    tauqe_core::models::ModelRole::REPL,
                )
                .config,
        );
        let scope = UsageScope::new(Arc::new(NoopUsage));
        scope.activate();
        app.interactive_execution()
            .prepare(
                app.context_session(),
                &app.execution_session(),
                operation,
                app.history_session(),
                config,
                UsageBinding::Bound(scope),
                "New task".into(),
                &tauqe_core::execution::EventSink::noop(),
            )
            .await
    }

    #[tokio::test]
    async fn hard_before_preparation_never_starts_scribe_or_registers_history() {
        let root = tempfile::tempdir().unwrap();
        let app = compacting_app(root.path()).await;
        let (provider, _entered) = waiting_scribe();
        app.test_set_provider_for(ProviderKind::OpenRouter, provider)
            .await;
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let before = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(app.cancel_active_operation(), Some(operation.id()));
        assert!(matches!(
            prepare_with_scribe(&app, &operation).await,
            Err(InteractiveTaskError::TaskFailed(message)) if message.contains("cancelled")
        ));
        assert_eq!(
            app.history_page(Some(100), None).await.unwrap().items,
            before.items
        );
        assert!(!app.abort_operation(operation.id()));
    }

    #[tokio::test]
    async fn cancellation_during_scribe_leaves_no_new_turn_or_abort_handle() {
        let root = tempfile::tempdir().unwrap();
        let app = compacting_app(root.path()).await;
        let (provider, entered) = waiting_scribe();
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let before = app.history_page(Some(100), None).await.unwrap();
        let waiting = tokio::spawn({
            let app = app.clone();
            let operation = operation.clone();
            async move { prepare_with_scribe(&app, &operation).await }
        });
        entered.await.unwrap();
        assert!(app.try_begin_operation().is_err());
        assert_eq!(app.cancel_active_operation(), Some(operation.id()));
        assert!(waiting.await.unwrap().is_err());
        assert!(provider.exited.load(Ordering::SeqCst));
        assert!(!app.abort_operation(operation.id()));
        assert_eq!(
            app.history_page(Some(100), None).await.unwrap().items,
            before.items
        );
        assert_eq!(app.active_operation_id(), Some(operation.id()));
        assert!(app.try_begin_operation().is_err());
    }

    #[tokio::test]
    async fn context_is_captured_after_scribe_finishes_and_matches_the_journal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("late.rs"), "source").unwrap();
        let app = compacting_app(root.path()).await;
        let (provider, entered) = waiting_scribe();
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let waiting = tokio::spawn({
            let app = app.clone();
            let operation = operation.clone();
            async move { prepare_with_scribe(&app, &operation).await }
        });
        entered.await.unwrap();
        app.test_mutate_context(|context| {
            context.add_auto_file("late.rs", ContextAccess::ReadOnly)
        })
        .await
        .unwrap()
        .unwrap();
        let watermark = app.context_state().await.items[0].insertion_seq;
        app.soft_stop_active_operation();
        assert!(!operation.hard_cancellation().is_cancelled());
        provider.resume.notify_one();
        let (_serial, started) = waiting.await.unwrap().unwrap();
        assert!(started.context.contains("late.rs").unwrap());
        let journal = app
            .history_session()
            .clone_for_turn()
            .await
            .unwrap()
            .snapshot()
            .unwrap();
        assert!(matches!(journal.entries().last(),
            Some(HistoryEntry::Turn { context, context_watermark, prompt, .. })
                if context == &["late.rs".to_string()]
                    && *context_watermark == Some(watermark)
                    && prompt == "New task"));
        started
            .open
            .interrupt(
                &operation,
                InterruptTurn {
                    rounds: Vec::new(),
                    reason: "Test complete".into(),
                    diagnostic: String::new(),
                    cancelled: false,
                    confirmed_edit: None,
                    confirmed_documents: false,
                },
            )
            .await
            .unwrap();
    }

    async fn start<F, R>(
        app: &Application,
        operation: Arc<OperationGuard>,
        run: F,
    ) -> Result<InteractiveTask, InteractiveTaskError>
    where
        F: FnOnce(ContextManager) -> R + Send + 'static,
        R: Future<Output = InteractiveTaskOutcome> + Send + 'static,
    {
        app.start_test_task(operation, Box::new(Vec::new), run)
            .await
    }

    async fn close(
        report: InteractiveTaskReport,
        operation: &OperationGuard,
    ) -> Result<InteractiveTaskOutcome, InteractiveTaskError> {
        report
            .preparation
            .interrupt(
                operation,
                InterruptTurn {
                    rounds: report.rounds,
                    reason: "Test task settled".into(),
                    diagnostic: String::new(),
                    cancelled: false,
                    confirmed_edit: match &report.settlement {
                        Settlement::Finalized { edit, .. } => Some(edit.clone()),
                        _ => None,
                    },
                    confirmed_documents: false,
                },
            )
            .await
            .unwrap();
        report.task
    }

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
        let (sink, mut events) = tauqe_core::execution::EventSink::channel();
        let task = start(
            &app,
            Arc::clone(&operation),
            move |mut context| async move {
                context
                    .add_auto_file("discovered.rs", ContextAccess::ReadOnly)
                    .unwrap();
                sink.emit_text_delta("Reply");
                InteractiveTaskOutcome {
                    result: Err(SystemError::Provider("offline".into())),
                    context,
                }
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(events.recv().await.unwrap(), tauqe_core::providers::StreamEvent::TextDelta(text) if text == "Reply")
        );
        let outcome = close(task.join().await, &operation).await.unwrap();
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
        let starting = start(&app, Arc::clone(&operation), move |context| async move {
            observed.store(true, Ordering::SeqCst);
            InteractiveTaskOutcome {
                result: Err(SystemError::Cancelled),
                context,
            }
        });
        tokio::pin!(starting);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut starting)
                .await
                .is_err()
        );
        assert!(!polled.load(Ordering::SeqCst));
        app.test_mutate_context(|context| {
            context.add_auto_file("discovered.rs", ContextAccess::ReadOnly)
        })
        .await
        .unwrap()
        .unwrap();
        drop(lock);
        let outcome = close(starting.await.unwrap().join().await, &operation)
            .await
            .unwrap();
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
        let task = start(&app, Arc::clone(&operation), panicking_task)
            .await
            .unwrap();
        assert!(
            matches!(close(task.join().await, &operation).await, Err(InteractiveTaskError::Interrupted(cause)) if cause.is_panic() && cause.to_string().contains("simulated interactive panic"))
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
        let task = start(&app, Arc::clone(&operation), move |_context| async move {
            started.send(()).unwrap();
            std::future::pending::<InteractiveTaskOutcome>().await
        })
        .await
        .unwrap();
        ready.await.unwrap();
        assert_eq!(app.cancel_active_operation(), Some(operation.id()));
        assert!(app.abort_operation(operation.id()));
        assert!(matches!(
            close(task.join().await, &operation).await,
            Err(InteractiveTaskError::Interrupted(cause)) if cause.is_cancelled()
        ));
        assert!(operation.hard_cancellation().is_cancelled());
        assert!(!app.abort_operation(operation.id()));
        assert!(app.try_begin_operation().is_err());
    }

    #[tokio::test]
    async fn watchdog_cannot_release_registration_while_interactive_supervisor_is_publishing() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let id = operation.id();
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = start(&app, Arc::clone(&operation), move |_context| async move {
            started.send(()).unwrap();
            std::future::pending::<InteractiveTaskOutcome>().await
        })
        .await
        .unwrap();
        ready.await.unwrap();

        let deadline = std::time::Duration::from_millis(10);
        assert_eq!(app.cancel_active_operation(), Some(id));
        let watchdog_app = app.clone();
        let watchdog = tokio::spawn(async move {
            watchdog_app
                .abort_stalled_operation_with_deadline(id, deadline)
                .await;
        });
        assert!(matches!(
            close(task.join().await, &operation).await,
            Err(InteractiveTaskError::Interrupted(cause)) if cause.is_cancelled()
        ));
        assert!(!app.abort_operation(id));
        assert!(app.try_begin_operation().is_err());

        // Keep the supervisor's guard beyond join and the watchdog deadline,
        // as when recovery has finished but terminal publication is still pending.
        watchdog.await.unwrap();
        assert_eq!(app.active_operation_id(), Some(id));
        assert!(app.try_begin_operation().is_err());
        drop(operation);

        let next = app.try_begin_operation().unwrap();
        let next_task = tokio::spawn(std::future::pending::<()>());
        next.set_abort_handle(next_task.abort_handle());
        app.abort_stalled_operation_with_deadline(id, deadline)
            .await;
        assert_eq!(app.active_operation_id(), Some(next.id()));
        assert!(!next.hard_cancellation().is_cancelled());
        assert!(app.abort_operation(next.id()));
        assert!(next_task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn a_foreign_operation_never_starts_the_execution_factory() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let other = Application::default();
        let foreign = Arc::new(other.try_begin_operation().unwrap());
        let polled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        let task = start(&app, foreign, move |context| async move {
            observed.store(true, Ordering::SeqCst);
            InteractiveTaskOutcome {
                result: Err(SystemError::Cancelled),
                context,
            }
        })
        .await;
        assert!(matches!(task, Err(InteractiveTaskError::OperationNotOwned)));
        assert!(!polled.load(Ordering::SeqCst));
    }
}
