//! Review post-analysis orchestration behind a transport-owned event port.

use crate::api::{
    Application, OperationGuard, PreparedReviewPostAnalysis, ReviewAnalysisCompletionError,
    ReviewAnalysisPersistenceError, ReviewAnalysisPreparationError, ReviewAnalysisResponse,
    ReviewAnalysisUpdate,
};
use crate::providers::LlmProvider;
use crate::review::ReviewSession;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub use crate::application::review_observer::ReviewObserver as ReviewAnalysisObserver;

#[derive(Debug, thiserror::Error)]
pub enum ReviewAnalysisExecutionError {
    #[error(transparent)]
    Preparation(#[from] ReviewAnalysisPreparationError),
    #[error(transparent)]
    Completion(#[from] ReviewAnalysisCompletionError),
    #[error(transparent)]
    Persistence(#[from] ReviewAnalysisPersistenceError),
}

impl Application {
    /// Started notices precede this call; terminal review publication follows it.
    pub async fn execute_review_post_analysis(
        &self,
        operation: &OperationGuard,
        root: &Path,
        prepared: &PreparedReviewPostAnalysis,
        provider: Arc<dyn LlmProvider>,
        observer: Arc<dyn ReviewAnalysisObserver>,
        exit_deadline: Duration,
    ) -> Result<ReviewSession, ReviewAnalysisExecutionError> {
        if !self.owns_operation(operation) {
            return Err(ReviewAnalysisPreparationError::OperationNotOwned.into());
        }
        let operation_id = format!("op-{}", operation.id());
        let publisher = Arc::clone(&observer);
        let usage = self
            .begin_usage(&operation_id, move |id, snapshot| {
                publisher.publish_usage_totals(id, snapshot);
            })
            .await;
        let mut cancel = operation.cancel_receiver();
        let mut task = self.start_review_post_analysis(operation, prepared, provider)?;
        let mut response = ReviewAnalysisResponse::default();
        while let Some(event) = task.next_event().await {
            observer.pace(&event, &mut cancel).await;
            match response.observe(event) {
                ReviewAnalysisUpdate::Usage(report) => usage.observe(&report).await,
                ReviewAnalysisUpdate::Finished => break,
                ReviewAnalysisUpdate::Ignored => {}
                update => observer.publish(update),
            }
        }
        task.clear_abort_handle();
        usage.flush().await;
        observer.publish_usage(&self.usage_snapshot().await);
        let cancelled = *cancel.borrow();
        let regrouping = match response.finish(&prepared.session, cancelled) {
            Ok(regrouping) => regrouping,
            Err(error) => {
                if error.requires_abort() {
                    let _ = task.abort_and_reap(&operation_id, exit_deadline).await;
                }
                return Err(error.into());
            }
        };
        self.persist_review_post_analysis(operation, root, prepared, regrouping)
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::UsageTotalsSnapshot;
    use crate::config::{AppConfig, ModelsConfig};
    use crate::models::{ModelEntry, ModelRef};
    use crate::providers::{mock::MockResponse, MockLlmProvider, ModelUsageInfo, StreamEvent};
    use crate::review::ReviewStorage;
    use std::sync::Mutex;
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
    impl ReviewAnalysisObserver for Observer {
        async fn pace(&self, event: &StreamEvent, _: &mut watch::Receiver<bool>) {
            if matches!(event, StreamEvent::Usage(_)) {
                self.updates.lock().unwrap().push("pace_usage");
                if let Some(app) = &self.cancel_on_usage {
                    app.cancel_active_operation();
                }
            }
        }
        fn publish(&self, update: ReviewAnalysisUpdate) {
            let label = match update {
                ReviewAnalysisUpdate::Reasoning(_) => "reasoning",
                ReviewAnalysisUpdate::Content(_) => "content",
                ReviewAnalysisUpdate::Reset => "reset",
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
        app.prepare_review_post_analysis(operation, root, &session, None)
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
        let stored = app
            .execute_review_post_analysis(
                &operation,
                root.path(),
                &prepared,
                provider,
                observer.clone(),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(stored.description.as_deref(), Some("One boundary"));
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
        let result = app
            .execute_review_post_analysis(
                &operation,
                root.path(),
                &prepared,
                Arc::new(MockLlmProvider::scripted([response()])),
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
        let result = app
            .execute_review_post_analysis(
                &foreign,
                root.path(),
                &prepared,
                provider.clone(),
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
            .execute_review_post_analysis(
                &operation,
                root.path(),
                &prepared,
                provider,
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
