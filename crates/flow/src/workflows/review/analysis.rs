//! Classify and regroup the findings of an existing review.

mod completion;
mod execution;
mod load;
mod preparation;
mod response;

pub use completion::ReviewAnalysisPersistenceError;
pub use execution::ReviewAnalysisExecutionError;
pub use load::ReviewAnalysisLoadError;
pub use preparation::ReviewAnalysisPreparationError;
pub use response::ReviewAnalysisCompletionError;

use super::audit::{ReviewEvent, ReviewPublication};
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::usage_totals::UsageReceipt;
use crate::application::{
    configuration::Configuration, context::ContextSession, documents::Documents,
    history::HistorySession, ExecutionSession, PlanExecutionEnvironment,
};
use crate::workflows::architect::ArchitectScope;
use std::{path::Path, sync::Arc, time::Duration};
use tauqe_core::models::{ModelRole, ModelSelection};
use tauqe_core::review::ReviewSession;
use tauqe_core::workflow::turn::{ReviewWorkPhase, TurnWorkContext, TurnWorkQueue};

#[derive(Debug, thiserror::Error)]
pub enum AnalyzeReviewError {
    #[error("Another operation is already running")]
    Busy,
    #[error(transparent)]
    Load(#[from] ReviewAnalysisLoadError),
    #[error("{0}")]
    Provider(String),
    #[error(transparent)]
    Execution(#[from] ReviewAnalysisExecutionError),
}

pub(crate) struct AnalysisResources<'a> {
    pub(crate) context: &'a ContextSession,
    pub(crate) execution: &'a ExecutionSession,
    pub(crate) history: &'a HistorySession,
}

impl AnalysisResources<'_> {
    pub(crate) async fn analyze(
        &self,
        configuration: &Configuration,
        id: String,
        model: Option<ModelSelection>,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<ReviewSession, AnalyzeReviewError> {
        let operation = self
            .execution
            .operations
            .begin()
            .map_err(|_| AnalyzeReviewError::Busy)?;
        let root = self.context.repository_root().await;
        let session = self
            .load_review_for_post_analysis(&operation, &root, &id)
            .await?;
        let publication = Arc::new(ReviewPublication {
            observer,
            id: operation.publication_id(),
        });
        let environment = PlanExecutionEnvironment::new(configuration.snapshot().await, &operation);
        self.run_review_analysis(
            &operation,
            &environment,
            &root,
            &session,
            model.as_ref(),
            publication,
            false,
            deadline,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_review_analysis(
        &self,
        operation: &OperationGuard,
        environment: &PlanExecutionEnvironment,
        root: &Path,
        session: &ReviewSession,
        selection: Option<&ModelSelection>,
        publication: Arc<ReviewPublication>,
        is_batch: bool,
        deadline: Duration,
    ) -> Result<ReviewSession, AnalyzeReviewError> {
        self.run_review_analysis_with_receipt(
            operation,
            environment,
            root,
            session,
            selection,
            publication,
            is_batch,
            deadline,
        )
        .await
        .map(|(session, _)| session)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_review_analysis_with_receipt(
        &self,
        operation: &OperationGuard,
        environment: &PlanExecutionEnvironment,
        root: &Path,
        session: &ReviewSession,
        selection: Option<&ModelSelection>,
        publication: Arc<ReviewPublication>,
        is_batch: bool,
        deadline: Duration,
    ) -> Result<(ReviewSession, UsageReceipt), AnalyzeReviewError> {
        let prepared = self
            .prepare_review_post_analysis(operation, environment, root, session, selection)
            .await
            .map_err(ReviewAnalysisExecutionError::from)?;
        let model = prepared.tier.model.clone();
        let execution = environment
            .for_review_post_analysis(
                self.execution,
                operation,
                &prepared.tier,
                &ArchitectScope::Review(session.id.clone()),
            )
            .await
            .map_err(|error| AnalyzeReviewError::Provider(error.to_string()))?;
        publication.observer.publish(
            &publication.id,
            ApplicationEvent::Started {
                model: model.clone(),
                role: Some(ModelRole::Architect),
                alias: None,
                effort: prepared.tier.effort,
                plan_context: None,
                is_batch,
                work_context: Some(TurnWorkContext::ReviewPhase {
                    review_id: session.id.clone(),
                    review_title: session.title.clone(),
                    phase: ReviewWorkPhase::PostAnalysis,
                    queue: TurnWorkQueue::default(),
                }),
            },
        );
        publication.review(ReviewEvent::AnalyzeStarted {
            model,
            findings_count: session.items.len(),
        });
        let (stored, receipt) = self
            .execute_review_post_analysis(
                operation,
                root,
                &prepared,
                &execution,
                publication.clone(),
                deadline,
            )
            .await?;
        Documents::new(root)
            .publish_review_snapshot(&session.id, publication.observer.as_ref())
            .await;
        publication.review(ReviewEvent::AnalyzeFinished(stored.clone()));
        Ok((stored, receipt))
    }

    #[cfg(feature = "test-support")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn test_run_review_analysis(
        &self,
        configuration: &Configuration,
        operation: &OperationGuard,
        root: &Path,
        session: &ReviewSession,
        model: Option<&ModelSelection>,
        observer: Arc<dyn ApplicationObserver>,
        is_batch: bool,
        deadline: Duration,
    ) -> Result<ReviewSession, AnalyzeReviewError> {
        let publication = Arc::new(ReviewPublication {
            observer,
            id: operation.publication_id(),
        });
        let environment = PlanExecutionEnvironment::new(configuration.snapshot().await, operation);
        self.run_review_analysis(
            operation,
            &environment,
            root,
            session,
            model,
            publication,
            is_batch,
            deadline,
        )
        .await
    }
}
