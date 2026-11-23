//! Best-effort bounded journal compaction owned by the application.

use super::history::HistorySession;
use crate::api::{Application, HistoryError, OperationGuard};
use crate::application::infrastructure::execution::{CompactionEnvironment, ExecutionSession};
use tauqe_core::blocking::run_blocking_async;
use tauqe_core::execution::{CancellationToken, EventSink};
use tauqe_core::providers::ChatMessage;

impl Application {
    pub(crate) async fn compact_history(
        &self,
        operation: &OperationGuard,
        environment: &CompactionEnvironment,
    ) -> Result<(), HistoryError> {
        self.history_session()
            .compact_history(&self.execution_session(), operation, environment)
            .await
    }
}

impl HistorySession {
    /// The caller's operation keeps the history copy and adoption exclusive
    /// with destructive journal mutations, without locking over provider I/O.
    pub(crate) async fn compact_history(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        environment: &CompactionEnvironment,
    ) -> Result<(), HistoryError> {
        if !resources.owns(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        let cancel = operation.cancel_receiver();
        if *cancel.borrow() {
            return Ok(());
        }
        let policy = environment.policy();
        let history = self.clone_for_turn().await?;
        let budget = policy.budget_tokens;
        if !history.needs_compaction(budget) {
            return Ok(());
        }
        let before = history.estimated_tokens();
        let tail_turns = policy.tail_turns;
        let (history, partition) = run_blocking_async(move || {
            let partition = history.prepare_compaction(tail_turns);
            (history, partition)
        })
        .await
        .map_err(|error| HistoryError::Offload(error.to_string()))?;
        let Some((head, tail)) =
            partition.map_err(|error| HistoryError::Storage(error.to_string()))?
        else {
            return Ok(());
        };
        if *cancel.borrow() || !resources.owns(operation) {
            return Ok(());
        }
        let execution = match environment.scribe(resources, operation).await {
            Ok(execution) => execution,
            Err(error) => {
                tracing::warn!(error = %error, "session journal stayed over budget: the Scribe provider is unavailable");
                return Ok(());
            }
        };
        let result = execution
            .run(
                &tauqe_core::prompt::history_compaction_contract(),
                vec![ChatMessage::user(
                    tauqe_core::history::build_compaction_request(&head),
                )],
                &EventSink::noop(),
                &CancellationToken::from_receiver(cancel.clone()),
            )
            .await;
        let summary = match result {
            Ok(Some(summary)) => summary,
            Ok(None) => {
                tracing::warn!("History compaction produced no recoverable summary; skipping compaction to preserve history");
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(error = %error, "Failed to summarize history during compaction");
                return Ok(());
            }
        };
        if *cancel.borrow() || !resources.owns(operation) {
            return Ok(());
        }
        let operations = resources.operations.clone();
        let operation_id = operation.id();
        let history = run_blocking_async(move || {
            if *cancel.borrow() || operations.current_id() != Some(operation_id) {
                return Ok(None);
            }
            let mut history = history;
            history
                .apply_compaction(summary, tail)
                .map_err(|error| HistoryError::Storage(error.to_string()))?;
            Ok::<_, HistoryError>(Some(history))
        })
        .await
        .map_err(|error| HistoryError::Offload(error.to_string()))??;
        let Some(history) = history else {
            return Ok(());
        };
        let after = history.estimated_tokens();
        if after != before {
            tracing::info!(
                before_tokens = before,
                after_tokens = after,
                "compacted the session journal before the turn"
            );
        }
        self.adopt(history).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::infrastructure::execution::PlanExecutionEnvironment;
    use std::sync::Arc;
    use tauqe_core::config::AppConfig;
    use tauqe_core::history::HistoryMutation;
    use tauqe_core::history::{
        HistoryEntry, HistoryFileChange, HistoryFileOperation, TurnRoundRecord,
    };
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::providers::{LlmProvider, LlmRequest, StreamEvent};
    use tokio::sync::{mpsc, watch};

    struct ScriptedStream(Vec<StreamEvent>);

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedStream {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            tauqe_core::models::ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            _: watch::Receiver<bool>,
        ) -> tauqe_core::providers::ProviderResult {
            for event in &self.0 {
                tx.send(event.clone()).await.unwrap();
            }
            Ok(())
        }
    }

    fn environment(config: AppConfig, operation: &OperationGuard) -> CompactionEnvironment {
        PlanExecutionEnvironment::new(
            crate::application::configuration::ConfigurationSnapshot {
                selection: config.default_selection(),
                config,
            },
            operation,
        )
        .compaction_environment()
    }

    async fn populated(
        root: std::path::PathBuf,
    ) -> (
        Application,
        AppConfig,
        mpsc::UnboundedReceiver<HistoryMutation>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root, sender);
        for index in 1..=4 {
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
        let mut config = AppConfig::default();
        config.history.budget_tokens = app.history_token_estimate().await / 2;
        config.history.tail_turns = 1;
        (app, config, receiver)
    }

    #[tokio::test]
    async fn compaction_adopts_the_watermark_and_forwards_the_structural_event() {
        let root = tempfile::tempdir().unwrap();
        let (app, config, mut events) = populated(root.path().to_path_buf()).await;
        let operation = app.try_begin_operation().unwrap();
        let environment = environment(config, &operation);
        let before = app.history_page(Some(100), None).await.unwrap();
        let maximum_id = before.items.iter().map(|item| item.id).max().unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"summary":"Earlier tasks completed."}"#,
        )]));
        app.test_set_provider_for(tauqe_core::models::ProviderKind::OpenRouter, provider)
            .await;
        app.compact_history(&operation, &environment).await.unwrap();
        let compacted = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(compacted.items.len(), 3);
        assert!(compacted.estimated_tokens < before.estimated_tokens);
        let mut structural = 0;
        while let Ok(event) = events.try_recv() {
            if matches!(event, HistoryMutation::Compacted { .. }) {
                structural += 1;
            }
        }
        assert_eq!(structural, 1);
        app.record_history_turn("Next task".into(), Vec::new())
            .await
            .unwrap();
        let after = app.history_page(Some(100), None).await.unwrap();
        assert!(after.items.last().unwrap().id > maximum_id);
    }

    #[tokio::test]
    async fn provider_resolution_or_stream_failure_keeps_the_journal_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let (app, config, _events) = populated(root.path().to_path_buf()).await;
        let operation = app.try_begin_operation().unwrap();
        let environment = environment(config, &operation);
        let before = app.history_page(Some(100), None).await.unwrap();
        app.compact_history(&operation, &environment).await.unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::error(
            "stream failed",
        )]));
        app.test_set_provider_for(tauqe_core::models::ProviderKind::OpenRouter, provider)
            .await;
        app.compact_history(&operation, &environment).await.unwrap();
        let after = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(after.items, before.items);
        assert_eq!(after.estimated_tokens, before.estimated_tokens);
    }

    #[tokio::test]
    async fn empty_incomplete_and_cancelled_summaries_leave_history_unchanged() {
        for events in [
            vec![
                StreamEvent::TextDelta(r#"{"summary":"  "}"#.into()),
                StreamEvent::Done,
            ],
            vec![StreamEvent::TextDelta(r#"{"summary":"valid"}"#.into())],
            vec![
                StreamEvent::TextDelta(r#"{"summary":"valid"}"#.into()),
                StreamEvent::Cancelled,
                StreamEvent::Done,
            ],
        ] {
            let root = tempfile::tempdir().unwrap();
            let (app, config, _events) = populated(root.path().to_path_buf()).await;
            let operation = app.try_begin_operation().unwrap();
            let environment = environment(config, &operation);
            let before = app.history_page(Some(100), None).await.unwrap();
            app.test_set_provider_for(
                tauqe_core::models::ProviderKind::OpenRouter,
                Arc::new(ScriptedStream(events)),
            )
            .await;
            app.compact_history(&operation, &environment).await.unwrap();
            let after = app.history_page(Some(100), None).await.unwrap();
            assert_eq!(after.items, before.items);
            assert_eq!(after.estimated_tokens, before.estimated_tokens);
        }
    }

    #[tokio::test]
    async fn reset_summary_preserves_the_retained_exchange() {
        let root = tempfile::tempdir().unwrap();
        let (app, config, _events) = populated(root.path().to_path_buf()).await;
        let operation = app.try_begin_operation().unwrap();
        let environment = environment(config, &operation);
        let before = app.history_page(Some(100), None).await.unwrap();
        app.test_set_provider_for(
            tauqe_core::models::ProviderKind::OpenRouter,
            Arc::new(ScriptedStream(vec![
                StreamEvent::TextDelta(r#"{"summary":"discarded"}"#.into()),
                StreamEvent::ResetStream,
                StreamEvent::TextDelta(r#"{"summary":"Retained."}"#.into()),
                StreamEvent::Done,
            ])),
        )
        .await;
        app.compact_history(&operation, &environment).await.unwrap();
        let after = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(after.items.len(), 3);
        assert_eq!(after.items[0].text, "Retained.");
        assert_eq!(&after.items[1..], &before.items[6..]);
    }

    #[tokio::test]
    async fn budgeted_compaction_keeps_a_step_card_with_its_rounds() {
        let root = tempfile::tempdir().unwrap();
        let mut entries = Vec::new();
        for (id, prompt) in [(1, "Implement step one"), (3, "Implement step two")] {
            entries.push(HistoryEntry::Turn {
                id: Some(id),
                prompt: prompt.into(),
                context: vec!["src/lib.rs".into()],
                context_watermark: Some(1),
                rounds: vec![TurnRoundRecord {
                    index: 1,
                    message: "Inspected the module.".into(),
                    feedback: "verification passed".into(),
                }],
            });
            entries.push(HistoryEntry::Response {
                id: Some(id + 1),
                message: format!("{prompt} closed."),
                commit: Some("abc1234".into()),
                summary: Some(prompt.into()),
                files: vec![HistoryFileChange {
                    path: "src/lib.rs".into(),
                    op: Some(HistoryFileOperation::Replace),
                    to: None,
                }],
            });
        }
        std::fs::create_dir_all(root.path().join(".tauqe")).unwrap();
        let lines = entries
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            root.path().join(".tauqe/history.jsonl"),
            format!("{lines}\n"),
        )
        .unwrap();
        let (sender, _receiver) = mpsc::unbounded_channel();
        let app =
            Application::for_repository(AppConfig::default(), root.path().to_path_buf(), sender);
        let before = app.history_token_estimate().await;
        let mut config = AppConfig::default();
        config.history.budget_tokens = before - before / 4;
        config.history.tail_turns = 1;
        let operation = app.try_begin_operation().unwrap();
        let environment = environment(config, &operation);
        app.test_set_provider_for(
            tauqe_core::models::ProviderKind::OpenRouter,
            Arc::new(MockLlmProvider::scripted([MockResponse::text(
                r#"{"summary":"Step one completed and verified."}"#,
            )])),
        )
        .await;
        app.compact_history(&operation, &environment).await.unwrap();
        let after = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(after.items.len(), 3);
        assert_eq!(after.items[0].text, "Step one completed and verified.");
        assert_eq!(after.items[1].text, "Implement step two");
        assert_eq!(after.items[1].rounds.len(), 1);
        assert_eq!(after.items[1].rounds[0].feedback, "verification passed");
        assert_eq!(after.items[2].commit_hash.as_deref(), Some("abc1234"));
        assert!(after.estimated_tokens <= environment.policy().budget_tokens);
    }

    #[tokio::test]
    async fn an_in_budget_journal_never_resolves_a_provider() {
        let root = tempfile::tempdir().unwrap();
        let (app, mut config, _events) = populated(root.path().to_path_buf()).await;
        config.history.budget_tokens = u64::MAX;
        let operation = app.try_begin_operation().unwrap();
        let environment = environment(config, &operation);
        app.compact_history(&operation, &environment).await.unwrap();
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 8);
    }

    #[tokio::test]
    async fn compaction_refuses_an_operation_from_another_session() {
        let root = tempfile::tempdir().unwrap();
        let (app, config, _events) = populated(root.path().to_path_buf()).await;
        let other = Application::default();
        let operation = other.try_begin_operation().unwrap();
        let environment = environment(config, &operation);
        assert!(matches!(
            app.compact_history(&operation, &environment).await,
            Err(HistoryError::OperationNotOwned)
        ));
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 8);
    }
}
