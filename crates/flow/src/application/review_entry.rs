//! Public review entry points; admission and settlement belong to the workflow.

use crate::api::{
    AnalyzeReviewError, Application, ApplicationObserver, ReviewPreview, ReviewPreviewError,
    ReviewStartError, ReviewStartInput,
};
use crate::workflows::review::analysis::AnalysisResources;
use crate::workflows::review::audit::AuditResources;
use std::{sync::Arc, time::Duration};
use tauqe_core::{models::ModelSelection, review::ReviewSession};

impl Application {
    pub(crate) fn audit_resources(&self) -> AuditResources {
        AuditResources {
            context: self.context_session().clone(),
            execution: self.execution_session(),
            history: self.history_session().clone(),
        }
    }

    pub async fn start_review(
        &self,
        input: ReviewStartInput,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<String, ReviewStartError> {
        self.audit_resources()
            .start(self.configuration_session(), input, observer, deadline)
            .await
    }

    pub async fn preview_review(
        &self,
        preset: Option<&str>,
        paths: &[String],
    ) -> Result<ReviewPreview, ReviewPreviewError> {
        self.audit_resources()
            .preview(self.configuration_session(), preset, paths)
            .await
    }

    pub async fn analyze_review(
        &self,
        id: String,
        model: Option<ModelSelection>,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<ReviewSession, AnalyzeReviewError> {
        let execution = self.execution_session();
        AnalysisResources {
            context: self.context_session(),
            execution: &execution,
            history: self.history_session(),
        }
        .analyze(self.configuration_session(), id, model, observer, deadline)
        .await
    }

    #[cfg(feature = "test-support")]
    #[allow(clippy::too_many_arguments)]
    pub async fn test_run_review_analysis(
        &self,
        operation: &crate::api::OperationGuard,
        root: &std::path::Path,
        session: &ReviewSession,
        model: Option<&ModelSelection>,
        observer: Arc<dyn ApplicationObserver>,
        is_batch: bool,
        deadline: Duration,
    ) -> Result<ReviewSession, AnalyzeReviewError> {
        let execution = self.execution_session();
        AnalysisResources {
            context: self.context_session(),
            execution: &execution,
            history: self.history_session(),
        }
        .test_run_review_analysis(
            self.configuration_session(),
            operation,
            root,
            session,
            model,
            observer,
            is_batch,
            deadline,
        )
        .await
    }

    #[cfg(feature = "test-support")]
    pub async fn test_run_group_audits(
        &self,
        root: &std::path::Path,
        session: &ReviewSession,
        operation: Arc<crate::api::OperationGuard>,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<tauqe_core::workflow::batch::BatchReport, String> {
        self.audit_resources()
            .test_run_group_audits(
                self.configuration_session(),
                root,
                session,
                operation,
                observer,
                deadline,
            )
            .await
    }
}
