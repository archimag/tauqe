//! Single-audit execution and completion behind transport publication ports.

use super::synthesis_completion::ReviewSynthesisPersistenceError;
use super::synthesis_request::{PreparedReviewSynthesisRequest, ReviewSynthesisRequestError};
use super::synthesis_response::{ReviewSynthesisCompletionError, ReviewSynthesisStream};
use super::AuditResources;
use crate::api::{OperationGuard, UsageTotalsSnapshot};
use crate::application::documents::Documents;
use crate::application::review_observer::ReviewObserver;
use crate::application::{structured_task, StructuredExecution};

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::parser::single_review_contract;
use tauqe_core::review::stream::ReviewStreamUpdate;
use tauqe_core::review::ReviewSession;

#[async_trait::async_trait]
pub trait ReviewSynthesisObserver: ReviewObserver {
    /// Runs after the review is durable and before its history entry.
    async fn publish_saved(
        &self,
        session: &ReviewSession,
        list: Option<(Vec<ReviewSession>, Option<String>)>,
    );
}

pub struct ReviewSynthesisExecutionConfig {
    /// Preserve the caller's existing RPC operation identity.
    pub operation_id: String,
    pub exit_deadline: Duration,
}

pub struct ReviewSynthesisResult {
    pub session: ReviewSession,
    pub usage: UsageTotalsSnapshot,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewSynthesisExecutionError {
    #[error(transparent)]
    Task(#[from] ReviewSynthesisRequestError),
    #[error(transparent)]
    Completion(#[from] ReviewSynthesisCompletionError),
    #[error(transparent)]
    Persistence(#[from] ReviewSynthesisPersistenceError),
}
impl ReviewSynthesisExecutionError {
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Completion(ReviewSynthesisCompletionError::Cancelled)
                | Self::Persistence(ReviewSynthesisPersistenceError::Cancelled)
        )
    }
}

impl AuditResources {
    pub(crate) async fn execute_review_synthesis(
        &self,
        operation: &OperationGuard,
        root: &Path,
        prepared: PreparedReviewSynthesisRequest,
        execution: &StructuredExecution,
        observer: Arc<dyn ReviewSynthesisObserver>,
        config: ReviewSynthesisExecutionConfig,
    ) -> Result<ReviewSynthesisResult, ReviewSynthesisExecutionError> {
        if !self.execution.owns(operation) {
            return Err(ReviewSynthesisRequestError.into());
        }
        let publisher = Arc::clone(&observer);
        let usage = self
            .usage
            .begin(&self.context, &config.operation_id, move |id, snapshot| {
                publisher.publish_usage_totals(id, snapshot);
            })
            .await;
        let cancel = operation.cancel_receiver();
        let (metadata, messages) = prepared.into_parts();
        let result = structured_task::run_with_session(
            &self.execution,
            operation,
            execution,
            single_review_contract(),
            messages,
            &config.operation_id,
            config.exit_deadline,
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
        let snapshot = self.usage.snapshot().await;
        observer.publish_usage(&snapshot);
        let cancelled = *cancel.borrow();
        let session = ReviewSynthesisStream::finish(result, metadata, cancelled)?;
        self.persist_review_synthesis(operation, root, &session, || async {
            let list = Documents::new(root).review_list().await.ok();
            observer.publish_saved(&session, list).await;
        })
        .await?;
        Ok(ReviewSynthesisResult {
            session,
            usage: snapshot,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use crate::application::PlanExecutionEnvironment;
    use crate::workflows::architect::ArchitectScope;
    use std::sync::Mutex;
    use tauqe_core::config::{AppConfig, ModelsConfig};
    use tauqe_core::context::ContextAccess;
    use tauqe_core::context::ContextFileContent;
    use tauqe_core::models::{ModelEntry, ModelRef, ModelRole, ModelSelection, ProviderKind};
    use tauqe_core::providers::{mock::MockResponse, MockLlmProvider, ModelUsageInfo, StreamEvent};
    use tauqe_core::review::ReviewStorage;

    use tokio::sync::watch;

    struct Observer {
        app: Application,
        root: std::path::PathBuf,
        updates: Mutex<Vec<&'static str>>,
        totals: Mutex<Vec<(String, UsageTotalsSnapshot)>>,
        final_usage: Mutex<Option<UsageTotalsSnapshot>>,
        cancel_on_usage: bool,
    }
    #[async_trait::async_trait]
    impl ReviewObserver for Observer {
        async fn pace(&self, event: &StreamEvent, _: &mut watch::Receiver<bool>) {
            if matches!(event, StreamEvent::Usage(_)) && self.cancel_on_usage {
                self.app.cancel_active_operation();
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
        fn publish_usage_totals(&self, id: &str, snapshot: &UsageTotalsSnapshot) {
            self.totals
                .lock()
                .unwrap()
                .push((id.into(), snapshot.clone()));
        }
        fn publish_usage(&self, snapshot: &UsageTotalsSnapshot) {
            assert!(ReviewStorage::load_all(&self.root).unwrap().is_empty());
            self.updates.lock().unwrap().push("usage");
            *self.final_usage.lock().unwrap() = Some(snapshot.clone());
        }
    }
    #[async_trait::async_trait]
    impl ReviewSynthesisObserver for Observer {
        async fn publish_saved(
            &self,
            session: &ReviewSession,
            _list: Option<(Vec<ReviewSession>, Option<String>)>,
        ) {
            assert_eq!(
                ReviewStorage::load_review(&self.root, &session.id)
                    .unwrap()
                    .unwrap(),
                *session
            );
            assert_eq!(
                self.app.history_page(None, None).await.unwrap().total_count,
                0
            );
            self.updates.lock().unwrap().push("saved");
        }
    }
    fn application(root: &Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(
            AppConfig {
                models: ModelsConfig {
                    reviewer: Some(vec![ModelEntry::new(
                        ModelRef::openrouter("test/reviewer"),
                        None,
                    )]),
                    ..Default::default()
                },
                ..Default::default()
            },
            root.to_path_buf(),
            sender,
        )
    }
    fn observer(app: &Application, root: &Path, cancel_on_usage: bool) -> Arc<Observer> {
        Arc::new(Observer {
            app: app.clone(),
            root: root.into(),
            updates: Mutex::new(Vec::new()),
            totals: Mutex::new(Vec::new()),
            final_usage: Mutex::new(None),
            cancel_on_usage,
        })
    }
    fn config() -> ReviewSynthesisExecutionConfig {
        ReviewSynthesisExecutionConfig {
            operation_id: "rpc-single-review".into(),
            exit_deadline: Duration::from_secs(1),
        }
    }
    async fn prepare(
        app: &Application,
        operation: &OperationGuard,
    ) -> PreparedReviewSynthesisRequest {
        let resolved = app.resolve_review_synthesis(None).await;
        app.audit_resources()
            .prepare_review_synthesis_request(
                operation,
                &resolved,
                &[ContextFileContent {
                    path: "audit.rs".into(),
                    content: "source".into(),
                    access: ContextAccess::ReadOnly,
                    insertion_seq: 0,
                }],
                Some("Audit boundaries".into()),
            )
            .unwrap()
    }
    fn response() -> MockResponse {
        MockResponse::text(
            serde_json::json!({"title": "Audit", "summary": "One boundary", "findings": []})
                .to_string(),
        )
        .with_reasoning("Inspect boundaries")
        .with_usage(ModelUsageInfo {
            total_tokens: 17,
            cost: Some(0.25),
            ..Default::default()
        })
    }

    async fn execution(app: &Application, operation: &OperationGuard) -> StructuredExecution {
        PlanExecutionEnvironment::new(app.configuration().await, operation)
            .for_reviewer_structured(
                &app.execution_session(),
                operation,
                &ModelSelection::Role(ModelRole::Reviewer),
                &ArchitectScope::Review("audit".into()),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn headless_execution_keeps_accounting_publication_and_history_order() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation).await;
        let observer = observer(&app, root.path(), false);
        let provider = Arc::new(MockLlmProvider::scripted([response()]));
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let execution = execution(&app, &operation).await;
        let result = app
            .audit_resources()
            .execute_review_synthesis(
                &operation,
                root.path(),
                prepared,
                &execution,
                observer.clone(),
                config(),
            )
            .await
            .unwrap();
        assert_eq!(result.session.description.as_deref(), Some("One boundary"));
        let request = provider.last_request().unwrap();
        assert_eq!(request.session_id.as_deref(), Some("tauqe-review-audit"));
        assert!(
            matches!(request.response_format, Some(tauqe_core::providers::ResponseFormat::JsonSchema { json_schema }) if json_schema.strict == Some(true))
        );
        assert_eq!(result.usage.operations, 1);
        assert_eq!(result.usage.prev.total_tokens, 17);
        assert_eq!(result.usage.session.cost, 0.25);
        assert!(result.usage.cur.is_none());
        assert_eq!(app.history_page(None, None).await.unwrap().total_count, 1);
        let updates = observer.updates.lock().unwrap();
        assert!(updates.contains(&"reasoning"));
        assert!(updates.contains(&"content"));
        assert!(
            updates.iter().position(|label| *label == "usage").unwrap()
                < updates.iter().position(|label| *label == "saved").unwrap()
        );
        let totals = observer.totals.lock().unwrap();
        assert!(totals.iter().all(|(id, _)| id == "rpc-single-review"));
        assert_eq!(&totals.last().unwrap().1, &result.usage);
        assert_eq!(app.active_operation_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn cancellation_during_pacing_is_charged_without_saving_a_review() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation).await;
        let observer = observer(&app, root.path(), true);
        app.test_set_provider_for(
            ProviderKind::OpenRouter,
            Arc::new(MockLlmProvider::scripted([response()])),
        )
        .await;
        let execution = execution(&app, &operation).await;
        let error = app
            .audit_resources()
            .execute_review_synthesis(
                &operation,
                root.path(),
                prepared,
                &execution,
                observer.clone(),
                config(),
            )
            .await
            .err()
            .unwrap();
        assert!(error.is_cancelled());
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
        assert!(ReviewStorage::load_all(root.path()).unwrap().is_empty());
        assert!(!root.path().join(".tauqe/history.jsonl").exists());
        assert!(!observer.updates.lock().unwrap().contains(&"saved"));
    }

    #[tokio::test]
    async fn provider_and_empty_output_failures_flush_usage_without_saving_history() {
        for script in [
            MockResponse::error("provider failed"),
            MockResponse::text(""),
        ] {
            let root = tempfile::tempdir().unwrap();
            let app = application(root.path());
            let operation = app.try_begin_operation().unwrap();
            let prepared = prepare(&app, &operation).await;
            let observer = observer(&app, root.path(), false);
            app.test_set_provider_for(
                ProviderKind::OpenRouter,
                Arc::new(MockLlmProvider::scripted([script])),
            )
            .await;
            let execution = execution(&app, &operation).await;
            let error = app
                .audit_resources()
                .execute_review_synthesis(
                    &operation,
                    root.path(),
                    prepared,
                    &execution,
                    observer.clone(),
                    config(),
                )
                .await
                .err()
                .unwrap();
            assert!(!error.is_cancelled());
            assert!(matches!(
                error,
                ReviewSynthesisExecutionError::Completion(_)
            ));
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
            assert!(ReviewStorage::load_all(root.path()).unwrap().is_empty());
            assert!(!root.path().join(".tauqe/history.jsonl").exists());
        }
    }

    #[tokio::test]
    async fn foreign_operation_is_refused_before_accounting_or_provider_execution() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path());
        let operation = app.try_begin_operation().unwrap();
        let prepared = prepare(&app, &operation).await;
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        let observer = observer(&app, root.path(), false);
        let provider = Arc::new(MockLlmProvider::scripted([response()]));
        app.test_set_provider_for(ProviderKind::OpenRouter, provider.clone())
            .await;
        let execution = execution(&app, &operation).await;
        let result = app
            .audit_resources()
            .execute_review_synthesis(
                &foreign,
                root.path(),
                prepared,
                &execution,
                observer.clone(),
                config(),
            )
            .await;
        assert!(matches!(
            result,
            Err(ReviewSynthesisExecutionError::Task(_))
        ));
        assert!(provider.requests().is_empty());
        assert!(observer.totals.lock().unwrap().is_empty());
        assert!(app.usage_snapshot().await.cur.is_none());
    }
}
