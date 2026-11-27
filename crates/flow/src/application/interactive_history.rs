//! Start the interactive journal turn from the context it actually observes.

use crate::api::{Application, HistoryError, OperationGuard};
use crate::application::history::HistorySession;
use crate::application::infrastructure::execution::ExecutionSession;
use std::sync::{Arc, Mutex};
use tauqe_core::context::ContextManager;
use tauqe_core::edits::EditOutcome;
use tauqe_core::execution::AcceptedRoundId;
use tauqe_core::history::HistorySnapshot;
use tauqe_core::history::TurnRoundRecord;
use tokio::sync::oneshot;

pub struct InteractiveHistoryStart {
    #[cfg(test)]
    pub turn_id: u64,
    pub history: HistorySnapshot,
    pub(crate) context: ContextManager,
    pub(crate) open: OpenInteractiveTurn,
}

/// Consumable ownership of one registered journal turn.
pub(crate) struct OpenInteractiveTurn {
    turn_id: u64,
    operation_id: u64,
    history: HistorySession,
    execution: ExecutionSession,
}

pub(crate) struct CompleteTurn {
    pub rounds: Vec<TurnRoundRecord>,
    pub source: Option<AcceptedRoundId>,
    pub response: String,
    pub edit: Option<EditOutcome>,
}

pub(crate) struct InterruptTurn {
    pub rounds: Vec<TurnRoundRecord>,
    pub reason: String,
    pub diagnostic: String,
    pub cancelled: bool,
    pub confirmed_edit: Option<EditOutcome>,
    pub confirmed_documents: bool,
}

impl OpenInteractiveTurn {
    fn check(&self, operation: &OperationGuard) -> Result<(), HistoryError> {
        if self.operation_id != operation.id() || !self.execution.owns(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        Ok(())
    }

    pub(crate) async fn complete(
        self,
        operation: &OperationGuard,
        input: CompleteTurn,
    ) -> Result<(), HistoryError> {
        self.check(operation)?;
        self.history.complete_open_turn(self.turn_id, input).await
    }

    pub(crate) async fn interrupt(
        self,
        operation: &OperationGuard,
        input: InterruptTurn,
    ) -> Result<(), HistoryError> {
        self.check(operation)?;
        self.history.interrupt_open_turn(self.turn_id, input).await
    }
}

impl HistorySession {
    pub(crate) async fn begin_compacted(
        &self,
        execution: &ExecutionSession,
        operation: Arc<OperationGuard>,
        prompt: String,
        context_snapshot: ContextManager,
    ) -> Result<InteractiveHistoryStart, HistoryError> {
        let (context, files, watermark) = self
            .prepare_bound(execution, &operation, context_snapshot)
            .await?;
        let (open, snapshot) = self
            .register_open_turn(execution.clone(), operation, prompt, files, watermark)
            .await?;
        Ok(InteractiveHistoryStart {
            #[cfg(test)]
            turn_id: open.turn_id,
            history: snapshot,
            context,
            open,
        })
    }
}

impl HistorySession {
    /// Once admitted, this task owns the write and its handle until delivery or
    /// interruption. Dropping the waiting future cannot abandon a written turn.
    async fn register_open_turn(
        &self,
        execution: ExecutionSession,
        operation: Arc<OperationGuard>,
        prompt: String,
        files: Vec<String>,
        watermark: Option<u64>,
    ) -> Result<(OpenInteractiveTurn, HistorySnapshot), HistoryError> {
        if !execution.owns(&operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        if operation.hard_cancellation().is_cancelled() {
            return Err(HistoryError::Storage(
                "History preparation was cancelled".into(),
            ));
        }
        let history = self.clone();
        let (sender, receiver) = oneshot::channel();
        let (accepted, acceptance) = oneshot::channel::<()>();
        tokio::spawn(async move {
            match history.start_turn(prompt, files, watermark).await {
                Ok((turn_id, snapshot)) => {
                    let open = OpenInteractiveTurn {
                        turn_id,
                        operation_id: operation.id(),
                        history,
                        execution,
                    };
                    if operation.hard_cancellation().is_cancelled() || sender.is_closed() {
                        if let Err(error) =
                            open.interrupt(&operation, abandoned_registration()).await
                        {
                            tracing::error!(turn_id, %error, "Could not interrupt abandoned history registration");
                        }
                        let _ = sender.send(Err(HistoryError::Storage(
                            "History preparation was cancelled".into(),
                        )));
                    } else {
                        // Sending into a oneshot is not acceptance: the waiter can
                        // disappear after send but before taking the value.
                        let slot = Arc::new(Mutex::new(Some(open)));
                        if sender.send(Ok((Arc::clone(&slot), snapshot))).is_err()
                            || acceptance.await.is_err()
                        {
                            let abandoned = slot
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .take();
                            if let Some(open) = abandoned {
                                if let Err(error) =
                                    open.interrupt(&operation, abandoned_registration()).await
                                {
                                    tracing::error!(turn_id, %error, "Could not interrupt abandoned history registration");
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                }
            }
        });
        let (slot, snapshot) = receiver
            .await
            .map_err(|_| HistoryError::Storage("History registration task stopped".into()))??;
        // No suspension between taking the handle and returning it to the
        // caller: dropping the waiter before this point leaves it in the slot.
        let open = slot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .ok_or_else(|| HistoryError::Storage("History registration handle was lost".into()))?;
        let _ = accepted.send(());
        Ok((open, snapshot))
    }
}

fn abandoned_registration() -> InterruptTurn {
    InterruptTurn {
        rounds: Vec::new(),
        reason: "Operation cancelled before history registration was delivered".into(),
        diagnostic: String::new(),
        cancelled: true,
        confirmed_edit: None,
        confirmed_documents: false,
    }
}

impl Application {
    #[cfg(test)]
    pub(crate) async fn prepare_interactive_history(
        &self,
        operation: Arc<OperationGuard>,
        prompt: String,
    ) -> Result<InteractiveHistoryStart, HistoryError> {
        if !self.execution_session().owns(&operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        let factory =
            crate::application::infrastructure::execution::ExecutionFactory::bind(self, &operation)
                .await
                .map_err(|error| HistoryError::Storage(error.to_string()))?;
        let scope = tauqe_core::execution::usage::UsageScope::new(Arc::new(TestUsage));
        scope.activate();
        let config = Arc::new(
            factory
                .resolve_model(
                    factory.selection().clone(),
                    tauqe_core::models::ModelRole::REPL,
                )
                .config,
        );
        let (_, started) = self
            .interactive_execution()
            .prepare(
                self.context_session(),
                &self.execution_session(),
                &operation,
                self.history_session(),
                config,
                tauqe_core::execution::usage::UsageBinding::Bound(scope),
                prompt,
                &tauqe_core::execution::EventSink::noop(),
            )
            .await
            .map_err(|error| HistoryError::Storage(error.to_string()))?;
        Ok(started)
    }
}

#[cfg(test)]
struct TestUsage;

#[cfg(test)]
impl tauqe_core::execution::usage::UsageSink for TestUsage {
    fn record(&self, _: &tauqe_core::providers::ModelUsageInfo) {}
}

impl HistorySession {
    async fn prepare_bound(
        &self,
        execution: &ExecutionSession,
        operation: &Arc<OperationGuard>,
        context: ContextManager,
    ) -> Result<(ContextManager, Vec<String>, Option<u64>), HistoryError> {
        if !execution.owns(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        if operation.hard_cancellation().is_cancelled() {
            return Err(HistoryError::Storage(
                "History preparation was cancelled".into(),
            ));
        }
        // Release the context lock before file reads and before taking the
        // history mutex. Read files and arrival metadata from the same copy.
        let (files, watermark) =
            tauqe_core::execution::cache::interactive_context_projection(context.clone())
                .await
                .map_err(|error| {
                    tauqe_core::blocking::log_join_error(
                        &error,
                        "capture interactive turn context",
                    );
                    HistoryError::Offload(error.to_string())
                })?;
        if !execution.owns(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        if operation.hard_cancellation().is_cancelled() {
            return Err(HistoryError::Storage(
                "History preparation was cancelled".into(),
            ));
        }
        Ok((context, files, watermark))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::AppConfig;
    use tauqe_core::context::ContextAccess;
    use tauqe_core::history::entry::HistoryEntry;
    use tauqe_core::history::HistoryMutation;

    fn application(root: &std::path::Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(AppConfig::default(), root.to_path_buf(), sender)
    }

    #[tokio::test]
    async fn lost_waiter_interrupts_the_turn_written_during_registration() {
        let root = tempfile::tempdir().unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), sender);
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let pause = app.history_session().pause_registration().await;
        let waiting = tokio::spawn({
            let app = app.clone();
            let operation = operation.clone();
            async move {
                app.prepare_interactive_history(operation, "Interrupted registration".into())
                    .await
            }
        });
        pause.written.notified().await;
        waiting.abort();
        assert!(matches!(waiting.await, Err(error) if error.is_cancelled()));
        pause.resume.notify_one();
        let mutations = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let first = events.recv().await.unwrap();
            let second = events.recv().await.unwrap();
            (first, second)
        })
        .await
        .unwrap();
        let HistoryMutation::Appended { item: written, .. } = mutations.0 else {
            panic!("registration must publish the prompt");
        };
        let HistoryMutation::RolledBack { item: rolled_back } = mutations.1 else {
            panic!("lost waiter must roll back the prompt");
        };
        assert_eq!(written.id, rolled_back.id);
        assert!(
            events.try_recv().is_err(),
            "the turn must be interrupted once"
        );
        assert!(app
            .history_session()
            .clone_for_turn()
            .await
            .unwrap()
            .snapshot()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn delivered_registration_is_completed_only_by_its_recipient() {
        let root = tempfile::tempdir().unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), sender);
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let pause = app.history_session().pause_registration().await;
        let waiting = tokio::spawn({
            let app = app.clone();
            let operation = operation.clone();
            async move {
                app.prepare_interactive_history(operation, "Delivered registration".into())
                    .await
            }
        });
        pause.written.notified().await;
        app.soft_stop_active_operation();
        assert!(!operation.hard_cancellation().is_cancelled());
        pause.resume.notify_one();
        let started = waiting.await.unwrap().unwrap();
        assert!(started.history.is_empty());
        started
            .open
            .complete(
                &operation,
                CompleteTurn {
                    rounds: Vec::new(),
                    source: None,
                    response: "Completed".into(),
                    edit: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            events.try_recv(),
            Ok(HistoryMutation::Appended { .. })
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(HistoryMutation::Appended { .. })
        ));
        assert!(
            events.try_recv().is_err(),
            "the turn must not be interrupted again"
        );
        let snapshot = app
            .history_session()
            .clone_for_turn()
            .await
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(snapshot.entries().len(), 2);
        assert!(
            matches!(&snapshot.entries()[1], HistoryEntry::Response { message, .. } if message == "Completed")
        );
    }

    #[tokio::test]
    async fn failed_registration_does_not_leave_an_open_turn() {
        let root = tempfile::tempdir().unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), sender);
        std::fs::create_dir_all(root.path().join(".tauqe/history.jsonl")).unwrap();
        let operation = Arc::new(app.try_begin_operation().unwrap());
        assert!(matches!(
            app.prepare_interactive_history(operation, "Unwritable".into())
                .await,
            Err(HistoryError::Storage(_))
        ));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn start_records_readable_paths_and_the_full_context_arrival_watermark() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("kept.rs"), "source").unwrap();
        std::fs::write(root.path().join("missing.rs"), "source").unwrap();
        let app = application(root.path());
        app.record_history_turn("Previous request".into(), Vec::new())
            .await
            .unwrap();
        app.test_mutate_manual_context(|context| {
            context
                .add_file(
                    "kept.rs",
                    ContextAccess::ReadOnly,
                    tauqe_core::context::ContextLayer::User,
                )
                .unwrap();
            context
                .scenario_mut(|scenario| {
                    scenario.add_auto_file("missing.rs", ContextAccess::ReadOnly)
                })
                .unwrap();
        })
        .await
        .unwrap();
        let watermark = app
            .context_state()
            .await
            .items
            .iter()
            .map(|item| item.insertion_seq)
            .max();
        std::fs::remove_file(root.path().join("missing.rs")).unwrap();
        let identity = app.session_uuid().await;
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let started = app
            .prepare_interactive_history(operation, "Current request".into())
            .await
            .unwrap();
        let before = started.history;
        assert_eq!(before.entries().len(), 1);
        assert!(
            matches!(&before.entries()[0], HistoryEntry::Turn { prompt, .. } if prompt == "Previous request")
        );
        let after = app
            .history_session()
            .clone_for_turn()
            .await
            .unwrap()
            .snapshot()
            .unwrap();
        match after.entries().last().unwrap() {
            HistoryEntry::Turn {
                id,
                prompt,
                context,
                context_watermark,
                ..
            } => {
                assert_eq!(*id, Some(started.turn_id));
                assert_eq!(prompt, "Current request");
                assert_eq!(context, &vec!["kept.rs".to_string()]);
                assert_eq!(*context_watermark, watermark);
            }
            _ => panic!("the new journal entry must be the current turn"),
        }
        assert_eq!(identity, app.session_uuid().await);
    }

    #[tokio::test]
    async fn empty_context_records_no_arrival_watermark() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let started = app
            .prepare_interactive_history(operation, "Empty context".into())
            .await
            .unwrap();
        assert!(started.history.is_empty());
        let snapshot = app
            .history_session()
            .clone_for_turn()
            .await
            .unwrap()
            .snapshot()
            .unwrap();
        assert!(
            matches!(&snapshot.entries()[0], HistoryEntry::Turn { context, context_watermark: None, .. } if context.is_empty())
        );
    }

    #[tokio::test]
    async fn unwritable_journal_refuses_to_start_the_turn() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        std::fs::create_dir_all(root.path().join(".tauqe/history.jsonl")).unwrap();
        let operation = Arc::new(app.try_begin_operation().unwrap());
        assert!(matches!(
            app.prepare_interactive_history(operation.clone(), "Request".into())
                .await,
            Err(HistoryError::Storage(message)) if message.contains("history.jsonl")
        ));
        assert!(app.try_begin_operation().is_err());
    }

    #[tokio::test]
    async fn foreign_operation_is_rejected_before_the_journal_changes() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        app.record_history_turn("Existing".into(), Vec::new())
            .await
            .unwrap();
        let other = Application::default();
        let foreign = Arc::new(other.try_begin_operation().unwrap());
        let _own = app.try_begin_operation().unwrap();
        assert!(matches!(
            app.prepare_interactive_history(foreign, "Must not be recorded".into())
                .await,
            Err(HistoryError::OperationNotOwned)
        ));
        let snapshot = app
            .history_session()
            .clone_for_turn()
            .await
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(snapshot.entries().len(), 1);
        assert!(
            matches!(&snapshot.entries()[0], HistoryEntry::Turn { prompt, .. } if prompt == "Existing")
        );
    }

    #[tokio::test]
    async fn preparation_compacts_before_recording_the_new_task() {
        use tauqe_core::models::ProviderKind;
        use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};

        let root = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.history.budget_tokens = 450;
        config.history.tail_turns = 1;
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(config, root.path().to_path_buf(), sender);
        for _ in 0..3 {
            app.history_session()
                .record_history_exchange("Old task ".repeat(100), vec![], "Done".into(), None)
                .await
                .unwrap();
        }
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"summary":"Old tasks completed."}"#,
        )]));
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let started = app
            .prepare_interactive_history(operation, "New task".into())
            .await
            .unwrap();
        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]
            .messages
            .iter()
            .any(|message| message.content.contains("Old task")));
        assert!(requests[0]
            .messages
            .iter()
            .all(|message| !message.content.contains("New task")));
        assert!(started.history.entries().iter().all(
            |entry| !matches!(entry, HistoryEntry::Turn { prompt, .. } if prompt == "New task")
        ));
        let journal = app
            .history_session()
            .clone_for_turn()
            .await
            .unwrap()
            .snapshot()
            .unwrap();
        assert!(
            matches!(journal.entries().last(), Some(HistoryEntry::Turn { id: Some(id), prompt, .. }) if *id == started.turn_id && prompt == "New task")
        );
        assert_eq!(journal.entries().iter().filter(|entry| matches!(entry, HistoryEntry::Turn { prompt, .. } if prompt == "New task")).count(), 1);
    }
}
