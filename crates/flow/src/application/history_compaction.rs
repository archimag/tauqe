//! Operation-bound journal compaction without holding the history lock during Scribe I/O.

use super::history::HistorySession;
use crate::api::{HistoryError, OperationGuard};
use crate::application::infrastructure::execution::ExecutionSession;
use tauqe_core::config::AppConfig;
use tauqe_core::execution::usage::UsageBinding;
use tauqe_core::execution::EventSink;
use tauqe_core::history::CompactionOutcome;
use tauqe_core::providers::ChatMessage;

impl HistorySession {
    /// The caller's operation keeps the history copy and adoption exclusive
    /// with destructive journal mutations, without locking over provider I/O.
    pub(crate) async fn compact_bound_history(
        &self,
        resources: &ExecutionSession,
        operation: &OperationGuard,
        config: &AppConfig,
        usage: UsageBinding,
        active_turn: Option<u64>,
        events: &EventSink,
    ) -> Result<Option<tauqe_core::history::HistorySnapshot>, HistoryError> {
        if !resources.owns(operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        let cancel = operation.hard_cancellation();
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let history = self.clone_for_turn().await?;
        let result = history
            .compact_if_needed(config.history.tail_turns, active_turn, |request| async {
                if cancel.is_cancelled() || !resources.owns(operation) {
                    return Err(HistoryError::Storage(
                        "History compaction was cancelled".into(),
                    ));
                }
                let execution = resources
                    .scribe(config, operation, usage)
                    .await
                    .map_err(|error| HistoryError::Storage(error.to_string()))?;
                execution
                    .run(
                        &tauqe_core::prompt::history_compaction_contract(),
                        vec![ChatMessage::user(request)],
                        events,
                        &cancel,
                    )
                    .await
                    .map_err(|error| HistoryError::Storage(error.to_string()))?
                    .ok_or_else(|| {
                        HistoryError::Storage("Scribe produced no recoverable summary".into())
                    })
            })
            .await
            .map_err(|error| HistoryError::Storage(error.to_string()))??;
        if cancel.is_cancelled() || !resources.owns(operation) {
            return Ok(None);
        }
        match result {
            CompactionOutcome::Compacted(candidate) => self
                .accept_compaction(candidate)
                .await
                .and_then(|snapshot| match active_turn {
                    Some(turn_id) => history
                        .snapshot_before(turn_id)
                        .map_err(|error| HistoryError::Storage(error.to_string()))
                        .map(Some),
                    None => Ok(Some(snapshot)),
                }),
            CompactionOutcome::NotNeeded | CompactionOutcome::BudgetUnreachable => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use crate::application::infrastructure::execution::ExecutionFactory;
    use std::sync::Arc;
    use tauqe_core::config::AppConfig;
    use tauqe_core::execution::usage::{UsageBinding, UsageScope, UsageSink};
    use tauqe_core::history::HistoryMutation;
    use tauqe_core::history::{
        HistoryEntry, HistoryFileChange, HistoryFileOperation, TurnRoundRecord,
    };
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::providers::{LlmProvider, LlmRequest, StreamEvent};
    use tokio::sync::mpsc;

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
            _: tauqe_core::stop::HardCancellation,
        ) -> tauqe_core::providers::ProviderResult {
            for event in &self.0 {
                tx.send(event.clone()).await.unwrap();
            }
            Ok(())
        }
    }

    struct Recorder;
    impl UsageSink for Recorder {
        fn record(&self, _: &tauqe_core::providers::ModelUsageInfo) {}
    }

    async fn prepare(
        app: &Application,
        operation: Arc<OperationGuard>,
    ) -> Result<crate::application::interactive_history::InteractiveHistoryStart, HistoryError>
    {
        if !app.execution_session().owns(&operation) {
            return Err(HistoryError::OperationNotOwned);
        }
        let factory = ExecutionFactory::bind(app, &operation)
            .await
            .map_err(|error| HistoryError::Storage(error.to_string()))?;
        let scope = UsageScope::new(Arc::new(Recorder));
        scope.activate();
        let config = Arc::new(
            factory
                .resolve_model(
                    factory.selection().clone(),
                    tauqe_core::models::ModelRole::REPL,
                )
                .config,
        );
        let (_, started) = app
            .interactive_execution()
            .prepare(
                app.context_session(),
                &app.execution_session(),
                &operation,
                app.history_session(),
                config,
                UsageBinding::Bound(scope),
                "Current task".into(),
                &EventSink::noop(),
            )
            .await
            .map_err(|error| HistoryError::Storage(error.to_string()))?;
        Ok(started)
    }

    async fn populated(
        root: std::path::PathBuf,
    ) -> (
        Application,
        AppConfig,
        mpsc::UnboundedReceiver<HistoryMutation>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut initial = AppConfig::default();
        initial.history.budget_tokens = 60;
        initial.history.tail_turns = 1;
        let app = Application::for_repository(initial.clone(), root, sender);
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
        (app, initial, receiver)
    }

    #[tokio::test]
    async fn compaction_adopts_the_watermark_and_forwards_the_structural_event() {
        let root = tempfile::tempdir().unwrap();
        let (app, _config, mut events) = populated(root.path().to_path_buf()).await;
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let before = app.history_page(Some(100), None).await.unwrap();
        let maximum_id = before.items.iter().map(|item| item.id).max().unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"summary":"Earlier tasks completed."}"#,
        )]));
        app.test_set_provider_for(tauqe_core::models::ProviderKind::OpenRouter, provider)
            .await;
        let started = prepare(&app, operation).await.unwrap();
        let turn_id = started.turn_id;
        assert!(matches!(
            started.history.entries().first(),
            Some(HistoryEntry::Summary { .. })
        ));
        let compacted = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(compacted.items.len(), 4);
        assert_eq!(compacted.items.last().unwrap().id, turn_id);
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
        let (app, _config, _events) = populated(root.path().to_path_buf()).await;
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let before = app.history_page(Some(100), None).await.unwrap();
        assert!(app
            .prepare_interactive_history(operation.clone(), "Current task".into())
            .await
            .is_err());
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::error(
            "stream failed",
        )]));
        app.test_set_provider_for(tauqe_core::models::ProviderKind::OpenRouter, provider)
            .await;
        assert!(app
            .prepare_interactive_history(operation, "Current task".into())
            .await
            .is_err());
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
            let (app, _config, _events) = populated(root.path().to_path_buf()).await;
            let operation = Arc::new(app.try_begin_operation().unwrap());
            let before = app.history_page(Some(100), None).await.unwrap();
            app.test_set_provider_for(
                tauqe_core::models::ProviderKind::OpenRouter,
                Arc::new(ScriptedStream(events)),
            )
            .await;
            assert!(app
                .prepare_interactive_history(operation, "Current task".into())
                .await
                .is_err());
            let after = app.history_page(Some(100), None).await.unwrap();
            assert_eq!(after.items, before.items);
            assert_eq!(after.estimated_tokens, before.estimated_tokens);
        }
    }

    #[tokio::test]
    async fn reset_summary_preserves_the_retained_exchange() {
        let root = tempfile::tempdir().unwrap();
        let (app, _config, _events) = populated(root.path().to_path_buf()).await;
        let operation = Arc::new(app.try_begin_operation().unwrap());
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
        let started = prepare(&app, operation).await.unwrap();
        assert_eq!(started.history.entries().len(), 3);
        let after = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(after.items.len(), 4);
        assert_eq!(after.items[0].text, "Retained.");
        assert_eq!(&after.items[1..3], &before.items[6..]);
        assert_eq!(after.items[3].text, "Current task");
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
        let mut initial = AppConfig::default();
        initial.history.budget_tokens = 140;
        initial.history.tail_turns = 1;
        let app = Application::for_repository(initial, root.path().to_path_buf(), sender);
        let operation = Arc::new(app.try_begin_operation().unwrap());
        app.test_set_provider_for(
            tauqe_core::models::ProviderKind::OpenRouter,
            Arc::new(MockLlmProvider::scripted([MockResponse::text(
                r#"{"summary":"Step one completed and verified."}"#,
            )])),
        )
        .await;
        let started = prepare(&app, operation).await.unwrap();
        let turn_id = started.turn_id;
        assert_eq!(started.history.entries().len(), 3);
        let after = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(after.items.len(), 4);
        assert_eq!(after.items[0].text, "Step one completed and verified.");
        assert_eq!(after.items[1].text, "Implement step two");
        assert_eq!(after.items[1].rounds.len(), 1);
        assert_eq!(after.items[1].rounds[0].feedback, "verification passed");
        assert_eq!(after.items[2].commit_hash.as_deref(), Some("abc1234"));
        assert_eq!(after.items[3].id, turn_id);
    }

    #[tokio::test]
    async fn a_journal_without_a_compactable_head_never_resolves_a_provider() {
        let root = tempfile::tempdir().unwrap();
        let (app, mut config, _events) = populated(root.path().to_path_buf()).await;
        config.history.tail_turns = 4;
        let change = app.begin_configuration_change().unwrap();
        change
            .replace_configuration(
                config,
                crate::application::configuration::ReloadSelection::Preserve,
            )
            .await;
        drop(change);
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let started = prepare(&app, operation).await.unwrap();
        assert_eq!(started.history.entries().len(), 8);
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 9);
    }

    #[tokio::test]
    async fn compaction_uses_the_launch_snapshot_after_configuration_changes() {
        let root = tempfile::tempdir().unwrap();
        let (app, mut initial, _events) = populated(root.path().to_path_buf()).await;
        initial.models.scribe = Some(tauqe_core::models::ModelRef::openrouter("scribe/a"));
        initial.request.max_input_tokens = 10_000;
        let operation = app.try_begin_operation().unwrap();
        let mut changed = initial.clone();
        changed.models.scribe = Some(tauqe_core::models::ModelRef::openrouter("scribe/b"));
        changed.history.tail_turns = 4;
        changed.request.max_input_tokens = 1;
        app.replace_configuration(
            changed,
            crate::application::configuration::ReloadSelection::Preserve,
        )
        .await;
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"summary":"Earlier tasks completed."}"#,
        )]));
        app.test_set_provider_for(
            tauqe_core::models::ProviderKind::OpenRouter,
            provider.clone(),
        )
        .await;
        let before = app.history_page(Some(100), None).await.unwrap();
        let result = app
            .history_session()
            .compact_bound_history(
                &app.execution_session(),
                &operation,
                &initial,
                UsageBinding::Bound({
                    let scope = UsageScope::new(Arc::new(Recorder));
                    scope.activate();
                    scope
                }),
                None,
                &EventSink::noop(),
            )
            .await
            .unwrap();
        assert!(result.is_some());
        let request = provider.last_request().unwrap();
        assert_eq!(request.model, "scribe/a");
        assert_eq!(request.reasoning_effort, None);
        assert_eq!(request.session_id, None);
        let after = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(after.items.len(), 3);
        assert_eq!(&after.items[1..], &before.items[6..]);
    }

    #[tokio::test]
    async fn snapshot_input_limit_refuses_scribe_before_dispatch_after_reload() {
        let root = tempfile::tempdir().unwrap();
        let (app, mut initial, _events) = populated(root.path().to_path_buf()).await;
        initial.request.max_input_tokens = 1;
        let operation = app.try_begin_operation().unwrap();
        let mut changed = initial.clone();
        changed.request.max_input_tokens = 10_000;
        changed.history.tail_turns = 4;
        app.replace_configuration(
            changed,
            crate::application::configuration::ReloadSelection::Preserve,
        )
        .await;
        let before = app.history_page(Some(100), None).await.unwrap();
        let provider = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(
            tauqe_core::models::ProviderKind::OpenRouter,
            provider.clone(),
        )
        .await;
        let scope = UsageScope::new(Arc::new(Recorder));
        scope.activate();
        assert!(app
            .history_session()
            .compact_bound_history(
                &app.execution_session(),
                &operation,
                &initial,
                UsageBinding::Bound(scope),
                None,
                &EventSink::noop(),
            )
            .await
            .is_err());
        assert!(provider.requests().is_empty());
        assert_eq!(
            app.history_page(Some(100), None).await.unwrap().items,
            before.items
        );
    }

    #[tokio::test]
    async fn foreign_and_revoked_operations_never_resolve_scribe_or_change_history() {
        let root = tempfile::tempdir().unwrap();
        let (app, config, _events) = populated(root.path().to_path_buf()).await;
        let provider = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(
            tauqe_core::models::ProviderKind::OpenRouter,
            provider.clone(),
        )
        .await;
        let before = app.history_page(Some(100), None).await.unwrap();
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        let stale = app.try_begin_operation().unwrap();
        assert!(app.release_operation_registration(stale.id()));
        let replacement = app.try_begin_operation().unwrap();
        for operation in [&foreign, &stale] {
            let scope = UsageScope::new(Arc::new(Recorder));
            scope.activate();
            assert!(matches!(
                app.history_session()
                    .compact_bound_history(
                        &app.execution_session(),
                        operation,
                        &config,
                        UsageBinding::Bound(scope),
                        None,
                        &EventSink::noop(),
                    )
                    .await,
                Err(HistoryError::OperationNotOwned)
            ));
        }
        assert!(provider.requests().is_empty());
        assert_eq!(
            app.history_page(Some(100), None).await.unwrap().items,
            before.items
        );
        assert_eq!(app.active_operation_id(), Some(replacement.id()));
    }

    #[tokio::test]
    async fn compaction_refuses_an_operation_from_another_session() {
        let root = tempfile::tempdir().unwrap();
        let (app, _config, _events) = populated(root.path().to_path_buf()).await;
        let other = Application::default();
        let operation = Arc::new(other.try_begin_operation().unwrap());
        assert!(matches!(
            prepare(&app, operation).await,
            Err(HistoryError::OperationNotOwned)
        ));
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 8);
    }

    #[tokio::test]
    async fn scribe_admission_failure_does_not_rewrite_history() {
        let root = tempfile::tempdir().unwrap();
        let (app, mut config, _events) = populated(root.path().to_path_buf()).await;
        config.request.max_input_tokens = 1;
        let change = app.begin_configuration_change().unwrap();
        change
            .replace_configuration(
                config,
                crate::application::configuration::ReloadSelection::Preserve,
            )
            .await;
        drop(change);
        let operation = Arc::new(app.try_begin_operation().unwrap());
        let before = app.history_page(Some(100), None).await.unwrap();
        let provider = Arc::new(MockLlmProvider::new());
        app.test_set_provider_for(
            tauqe_core::models::ProviderKind::OpenRouter,
            provider.clone(),
        )
        .await;
        assert!(app
            .prepare_interactive_history(operation, "Current task".into())
            .await
            .is_err());
        assert!(provider.requests().is_empty());
        let after = app.history_page(Some(100), None).await.unwrap();
        assert_eq!(after.items, before.items);
        assert_eq!(after.estimated_tokens, before.estimated_tokens);
    }
}
