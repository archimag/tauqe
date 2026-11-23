//! Review post-analysis orchestration behind a transport-owned event port.

use super::{
    preparation::PreparedReviewPostAnalysis, response::ReviewAnalysisResponse, AnalysisResources,
    ReviewAnalysisCompletionError, ReviewAnalysisPersistenceError, ReviewAnalysisPreparationError,
};
use crate::api::OperationGuard;
use crate::application::review_observer::ReviewObserver;
use crate::application::{structured_task, StructuredExecution};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::analysis::post_analysis_contract;
use tauqe_core::review::stream::ReviewStreamUpdate;
use tauqe_core::review::ReviewSession;

#[derive(Debug, thiserror::Error)]
pub enum ReviewAnalysisExecutionError {
    #[error(transparent)]
    Preparation(#[from] ReviewAnalysisPreparationError),
    #[error(transparent)]
    Completion(#[from] ReviewAnalysisCompletionError),
    #[error(transparent)]
    Persistence(#[from] ReviewAnalysisPersistenceError),
}

impl AnalysisResources<'_> {
    /// Started notices precede this call; terminal review publication follows it.
    pub(super) async fn execute_review_post_analysis(
        &self,
        operation: &OperationGuard,
        root: &Path,
        prepared: &PreparedReviewPostAnalysis,
        execution: &StructuredExecution,
        observer: Arc<dyn ReviewObserver>,
        exit_deadline: Duration,
    ) -> Result<ReviewSession, ReviewAnalysisExecutionError> {
        if !self.execution.owns(operation) {
            return Err(ReviewAnalysisPreparationError::OperationNotOwned.into());
        }
        let operation_id = operation.publication_id();
        let publisher = Arc::clone(&observer);
        let usage = self
            .usage
            .begin(self.context, &operation_id, move |id, snapshot| {
                publisher.publish_usage_totals(id, snapshot);
            })
            .await;
        let cancel = operation.cancel_receiver();
        let result = structured_task::run_with_session(
            self.execution,
            operation,
            execution,
            post_analysis_contract(),
            prepared.messages(),
            &operation_id,
            exit_deadline,
            |event| {
                let usage = &usage;
                let observer = &observer;
                let mut pacing_cancel = cancel.clone();
                async move {
                    observer.pace(&event, &mut pacing_cancel).await;
                    match event {
                        StreamEvent::Usage(report) => usage.observe(&report).await,
                        StreamEvent::TextDelta(delta) => {
                            observer.publish(ReviewStreamUpdate::Content(delta))
                        }
                        StreamEvent::ReasoningDelta(delta) => {
                            observer.publish(ReviewStreamUpdate::Reasoning(delta))
                        }
                        StreamEvent::ResetStream => observer.publish(ReviewStreamUpdate::Reset),
                        _ => {}
                    }
                }
            },
        )
        .await;
        usage.flush().await;
        observer.publish_usage(&self.usage.snapshot().await);
        let cancelled = *cancel.borrow();
        let regrouping = ReviewAnalysisResponse::finish(result, &prepared.session, cancelled)?;
        self.persist_review_post_analysis(operation, root, prepared, regrouping)
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Application, UsageTotalsSnapshot};
    use crate::application::PlanExecutionEnvironment;
    use crate::workflows::architect::ArchitectScope;
    use std::sync::Mutex;
    use tauqe_core::config::{AppConfig, ModelsConfig};
    use tauqe_core::models::{ModelEntry, ModelRef, ProviderKind};
    use tauqe_core::providers::{mock::MockResponse, MockLlmProvider, ModelUsageInfo, StreamEvent};
    use tauqe_core::review::ReviewStorage;
    use tokio::sync::watch;

    #[derive(Default)]
    struct Observer {
        updates: Mutex<Vec<&'static str>>,
        totals: Mutex<Vec<UsageTotalsSnapshot>>,
        final_usage: Mutex<Option<UsageTotalsSnapshot>>,
        cancel_on_usage: Option<Application>,
        root: std::path::PathBuf,
    }
    #[async_trait::async_trait]
    impl ReviewObserver for Observer {
        async fn pace(&self, event: &StreamEvent, _: &mut watch::Receiver<bool>) {
            if matches!(event, StreamEvent::Usage(_)) {
                self.updates.lock().unwrap().push("pace_usage");
                if let Some(app) = &self.cancel_on_usage {
                    app.cancel_active_operation();
                }
            }
        }
        fn publish(&self, update: ReviewStreamUpdate) {
            let label = match update {
                ReviewStreamUpdate::Reasoning(_) => "reasoning",
                ReviewStreamUpdate::Content(_) => "content",
                ReviewStreamUpdate::Reset => "reset",
                _ => panic!("accounting and terminal updates stay inside core"),
            };
            self.updates.lock().unwrap().push(label);
        }
        fn publish_usage_totals(&self, _: &str, snapshot: &UsageTotalsSnapshot) {
            self.totals.lock().unwrap().push(snapshot.clone());
        }
        fn publish_usage(&self, snapshot: &UsageTotalsSnapshot) {
            // The completed usage publication still precedes any regrouping write.
            let stored = ReviewStorage::load_review(&self.root, "execution-review")
                .unwrap()
                .unwrap();
            assert!(stored.description.is_none());
            self.updates.lock().unwrap().push("final_usage");
            *self.final_usage.lock().unwrap() = Some(snapshot.clone());
        }
    }

    fn application(root: &Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        let config = AppConfig {
            models: ModelsConfig {
                architect: Some(vec![ModelEntry::new(
                    ModelRef::openrouter("test/architect"),
                    None,
                )]),
                ..Default::default()
            },
            ..Default::default()
        };
        Application::for_repository(config, root.to_path_buf(), sender)
    }
    async fn prepare(
        app: &Application,
        operation: &OperationGuard,
        root: &Path,
    ) -> PreparedReviewPostAnalysis {
        let session = ReviewSession {
            id: "execution-review".into(),
            items: vec![serde_json::from_value(serde_json::json!({
                "id": 1, "uuid": "018f9c1e-6b3a-7c1e-8b3a-000000000001",
                "title": "Boundary", "type": "architecture", "body": "Keep this finding"
            }))
            .unwrap()],
            ..Default::default()
        };
        ReviewStorage::save_session(root, &session).unwrap();
        let environment = PlanExecutionEnvironment::new(app.configuration().await, operation);
        app.audit_resources()
            .analysis()
            .prepare_review_post_analysis(operation, &environment, root, &session, None)
            .await
            .unwrap()
    }
    async fn execution(
        app: &Application,
        operation: &OperationGuard,
        prepared: &PreparedReviewPostAnalysis,
    ) -> StructuredExecution {
        PlanExecutionEnvironment::new(app.configuration().await, operation)
            .for_review_post_analysis(
                &app.execution_session(),
                operation,
                &prepared.tier,
                &ArchitectScope::Review(prepared.session.id.clone()),
            )
            .await
            .unwrap()
    }
    fn response() -> MockResponse {
        MockResponse::text(serde_json::json!({"summary": "One boundary", "triage": [], "independent_issues": [], "architectural_groups": []}).to_string())
            .with_reasoning("Review the boundaries")
            .with_usage(ModelUsageInfo { total_tokens: 17, cost: Some(0.25), ..Default::default() })
    }

    #[tokio::test]
    async fn headless_execution_accounts_before_persistence_and_records_history() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation, root.path()).await;
        let observer = Arc::new(Observer {
            root: root.path().into(),
            ..Default::default()
        });
        let provider = Arc::new(MockLlmProvider::scripted([response()]));
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let execution = execution(&app, &operation, &prepared).await;
        let stored = app
            .audit_resources()
            .analysis()
            .execute_review_post_analysis(
                &operation,
                root.path(),
                &prepared,
                &execution,
                observer.clone(),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(stored.description.as_deref(), Some("One boundary"));
        let request = provider.last_request().unwrap();
        assert_eq!(
            request.session_id.as_deref(),
            Some("tauqe-review-execution-review")
        );
        assert_eq!(request.messages[0].role, "system");
        assert_eq!(request.messages[1], prepared.messages()[0]);
        let snapshot = observer.final_usage.lock().unwrap().clone().unwrap();
        assert_eq!(snapshot.operations, 1);
        assert_eq!(snapshot.prev.total_tokens, 17);
        assert_eq!(snapshot.session.cost, 0.25);
        assert!(snapshot.cur.is_none());
        assert!(root.path().join(".tauqe/history.jsonl").is_file());
        let updates = observer.updates.lock().unwrap();
        assert!(updates.contains(&"reasoning"));
        assert!(updates.contains(&"content"));
        assert_eq!(updates.last(), Some(&"final_usage"));
        assert_eq!(observer.totals.lock().unwrap().last(), Some(&snapshot));
    }

    #[tokio::test]
    async fn cancellation_raised_by_pacing_is_charged_without_regrouping_or_history() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation, root.path()).await;
        let observer = Arc::new(Observer {
            root: root.path().into(),
            cancel_on_usage: Some(app.clone()),
            ..Default::default()
        });
        app.test_set_provider_for(
            ProviderKind::OpenRouter,
            Arc::new(MockLlmProvider::scripted([response()])),
        )
        .await;
        let execution = execution(&app, &operation, &prepared).await;
        let result = app
            .audit_resources()
            .analysis()
            .execute_review_post_analysis(
                &operation,
                root.path(),
                &prepared,
                &execution,
                observer.clone(),
                Duration::from_secs(1),
            )
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisExecutionError::Completion(
                ReviewAnalysisCompletionError::Cancelled(_)
            ))
        ));
        assert_eq!(
            observer
                .final_usage
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .prev
                .total_tokens,
            17
        );
        assert_eq!(app.usage_snapshot().await.operations, 1);
        assert_eq!(
            ReviewStorage::load_review(root.path(), &prepared.session.id)
                .unwrap()
                .unwrap(),
            prepared.session
        );
        assert!(!root.path().join(".tauqe/history.jsonl").exists());
    }

    #[tokio::test]
    async fn provider_failure_flushes_accounting_and_foreign_operations_never_start_it() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation, root.path()).await;
        let observer = Arc::new(Observer {
            root: root.path().into(),
            ..Default::default()
        });
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::error(
            "provider failed",
        )]));
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let execution = execution(&app, &operation, &prepared).await;
        let result = app
            .audit_resources()
            .analysis()
            .execute_review_post_analysis(
                &foreign,
                root.path(),
                &prepared,
                &execution,
                observer.clone(),
                Duration::from_secs(1),
            )
            .await;
        assert!(matches!(
            result,
            Err(ReviewAnalysisExecutionError::Preparation(
                ReviewAnalysisPreparationError::OperationNotOwned
            ))
        ));
        assert!(provider.requests().is_empty());
        assert!(observer.totals.lock().unwrap().is_empty());
        let result = app
            .audit_resources()
            .analysis()
            .execute_review_post_analysis(
                &operation,
                root.path(),
                &prepared,
                &execution,
                observer.clone(),
                Duration::from_secs(1),
            )
            .await;
        assert!(
            matches!(result, Err(ReviewAnalysisExecutionError::Completion(ReviewAnalysisCompletionError::Provider(message))) if message == "provider failed")
        );
        assert_eq!(
            observer
                .final_usage
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .operations,
            1
        );
        assert!(!root.path().join(".tauqe/history.jsonl").exists());
    }
}
