//! Review launch and post-analysis scenarios.
use crate::api::{
    Application, ApplicationEvent, ApplicationObserver, GroupAuditObserver, OperationGuard,
    PresetReviewPreparationError, ReviewAnalysisExecutionError, ReviewAnalysisLoadError,
    ReviewObserver, ReviewSynthesisExecutionConfig, ReviewSynthesisObserver,
    ReviewSynthesisPreparationError, UsageTotalsSnapshot,
};
use crate::application::infrastructure::execution::PlanExecutionEnvironment;
use crate::application::plan_horizon::ArchitectScope;
use std::{path::Path, sync::Arc, time::Duration};
use tauqe_core::models::{ModelRef, ModelRole, ModelSelection};
use tauqe_core::providers::{ModelUsageInfo, StreamEvent};
use tauqe_core::review::{stream::ReviewStreamUpdate, ReviewGroup, ReviewSession};
use tauqe_core::workflow::turn::{ReviewWorkPhase, TurnWorkContext, TurnWorkQueue};

pub struct ReviewStartInput {
    pub preset: Option<String>,
    pub model: Option<ModelSelection>,
    pub path_patterns: Vec<String>,
    pub prompt: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewStartError {
    #[error("Another operation is already running")]
    Busy,
    #[error("{0}")]
    Provider(String),
    #[error(transparent)]
    Window(#[from] ReviewSynthesisPreparationError),
    #[error(transparent)]
    Preset(#[from] PresetReviewPreparationError),
}

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

pub enum ReviewEvent {
    Started {
        model: ModelRef,
        files_count: usize,
        estimated_tokens: usize,
        omitted_files: usize,
    },
    Finished {
        session: ReviewSession,
        usage: ModelUsageInfo,
        session_total_cost: f64,
        current_cost: f64,
    },
    Cancelled,
    Error(String),
    Reasoning(String),
    Content(String),
    Reset,
    AnalyzeStarted {
        model: ModelRef,
        findings_count: usize,
    },
    AnalyzeFinished(ReviewSession),
    GroupStarted {
        group: ReviewGroup,
        files_count: usize,
        estimated_tokens: usize,
    },
    GroupFinished {
        group_id: String,
        findings_count: usize,
        error: Option<String>,
    },
}

pub(crate) struct ReviewPublication {
    observer: Arc<dyn ApplicationObserver>,
    id: String,
}
impl ReviewPublication {
    fn review(&self, event: ReviewEvent) {
        self.observer
            .publish(&self.id, ApplicationEvent::ReviewRun(event));
    }
}
#[async_trait::async_trait]
impl ReviewObserver for ReviewPublication {
    async fn pace(&self, event: &StreamEvent, cancel: &mut tokio::sync::watch::Receiver<bool>) {
        self.observer.ready(event, cancel).await;
    }
    fn publish(&self, event: ReviewStreamUpdate) {
        let event = match event {
            ReviewStreamUpdate::Reasoning(delta) => ReviewEvent::Reasoning(delta),
            ReviewStreamUpdate::Content(delta) => ReviewEvent::Content(delta),
            ReviewStreamUpdate::Reset => ReviewEvent::Reset,
            _ => return,
        };
        self.review(event);
    }
    fn publish_usage_totals(&self, id: &str, snapshot: &UsageTotalsSnapshot) {
        self.observer
            .publish(id, ApplicationEvent::UsageTotals(snapshot.clone()));
    }
    fn publish_usage(&self, snapshot: &UsageTotalsSnapshot) {
        self.observer
            .publish(&self.id, ApplicationEvent::Usage(snapshot.clone()));
    }
}
#[async_trait::async_trait]
impl ReviewSynthesisObserver for ReviewPublication {
    async fn publish_saved(
        &self,
        session: &ReviewSession,
        list: Option<(Vec<ReviewSession>, Option<String>)>,
    ) {
        self.observer
            .publish(&self.id, ApplicationEvent::ReviewDocument(session.clone()));
        if let Some((reviews, active_id)) = list {
            self.observer.publish(
                &self.id,
                ApplicationEvent::ReviewList { reviews, active_id },
            );
        }
    }
}
#[async_trait::async_trait]
impl GroupAuditObserver for ReviewPublication {
    fn group_started(&self, group: &ReviewGroup, files_count: usize, estimated_tokens: usize) {
        self.review(ReviewEvent::GroupStarted {
            group: group.clone(),
            files_count,
            estimated_tokens,
        });
    }
    fn group_finished(&self, group: &ReviewGroup, findings_count: usize, error: Option<String>) {
        self.review(ReviewEvent::GroupFinished {
            group_id: group.id.clone(),
            findings_count,
            error,
        });
    }
    async fn publish_review(
        &self,
        session: &ReviewSession,
        list: Option<(Vec<ReviewSession>, Option<String>)>,
    ) {
        self.publish_saved(session, list).await;
    }
}

impl Application {
    pub async fn start_review(
        &self,
        input: ReviewStartInput,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<String, ReviewStartError> {
        let operation = Arc::new(
            self.try_begin_operation()
                .map_err(|_| ReviewStartError::Busy)?,
        );
        let application = self.clone();
        if let Some(name) = input
            .preset
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let environment = PlanExecutionEnvironment::new(self.configuration().await, &operation);
            let resolved = self
                .resolve_review_preset(&environment, name, input.model.as_ref())
                .await?;
            let root = self.repository_root().await;
            let session = self.prepare_preset_review(&operation, &resolved, input.prompt)?;
            let selection = resolved.selection;
            let id = operation.publication_id();
            let publication = Arc::new(ReviewPublication {
                observer,
                id: id.clone(),
            });
            publication.review(ReviewEvent::Started {
                model: session.model.clone(),
                files_count: 0,
                estimated_tokens: 0,
                omitted_files: 0,
            });
            tokio::spawn(async move {
                use crate::api::PresetReviewExecutionError;
                let result = application
                    .execute_preset_review(
                        (&environment, &selection),
                        operation.clone(),
                        &root,
                        session,
                        publication.clone(),
                        deadline,
                    )
                    .await;
                publication.review(match result {
                    Ok(result) => ReviewEvent::Finished {
                        session: result.session,
                        usage: result.usage,
                        session_total_cost: result.session_total_cost,
                        current_cost: result.current_cost,
                    },
                    Err(PresetReviewExecutionError::Cancelled) => ReviewEvent::Cancelled,
                    Err(error) => ReviewEvent::Error(error.to_string()),
                });
                drop(operation);
            });
            return Ok(id);
        }
        let selection = input
            .model
            .clone()
            .unwrap_or(ModelSelection::Role(ModelRole::Reviewer));
        let environment = PlanExecutionEnvironment::new(self.configuration().await, &operation);
        let resolved = Self::resolve_review_synthesis_from_config(
            environment.config().clone(),
            Some(&selection),
        );
        let window = self
            .prepare_review_synthesis_window(&resolved, &input.path_patterns)
            .await?;
        let prepared = self
            .prepare_review_synthesis_request(&operation, &resolved, &window.files, input.prompt)
            .map_err(|_| ReviewStartError::Busy)?;
        let execution = environment
            .for_reviewer_structured(
                self,
                &operation,
                &selection,
                &ArchitectScope::Review(prepared.id().to_string()),
            )
            .await
            .map_err(|error| ReviewStartError::Provider(error.to_string()))?;
        let id = format!("op-{}", crate::api::next_operation_id());
        let publication = Arc::new(ReviewPublication {
            observer,
            id: id.clone(),
        });
        publication.review(ReviewEvent::Started {
            model: resolved.model,
            files_count: window.files.len(),
            estimated_tokens: window.estimated_tokens,
            omitted_files: window.omitted_by_budget,
        });
        tokio::spawn(async move {
            let result = application
                .execute_review_synthesis(
                    &operation,
                    &window.root,
                    prepared,
                    &execution,
                    publication.clone(),
                    ReviewSynthesisExecutionConfig {
                        operation_id: publication.id.clone(),
                        exit_deadline: deadline,
                    },
                )
                .await;
            publication.review(match result {
                Ok(result) => ReviewEvent::Finished {
                    session: result.session,
                    usage: result.usage.prev.to_usage_info(),
                    session_total_cost: result.usage.session.cost,
                    current_cost: result.usage.prev.cost,
                },
                Err(error) if error.is_cancelled() => ReviewEvent::Cancelled,
                Err(error) => ReviewEvent::Error(error.to_string()),
            });
        });
        Ok(id)
    }

    pub async fn analyze_review(
        &self,
        id: String,
        model: Option<ModelSelection>,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<ReviewSession, AnalyzeReviewError> {
        let operation = self
            .try_begin_operation()
            .map_err(|_| AnalyzeReviewError::Busy)?;
        let root = self.repository_root().await;
        let session = self
            .load_review_for_post_analysis(&operation, &root, &id)
            .await?;
        let publication = Arc::new(ReviewPublication {
            observer,
            id: operation.publication_id(),
        });
        let environment = PlanExecutionEnvironment::new(self.configuration().await, &operation);
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
    pub(crate) async fn run_review_analysis(
        &self,
        operation: &OperationGuard,
        environment: &PlanExecutionEnvironment,
        root: &Path,
        session: &ReviewSession,
        model: Option<&ModelSelection>,
        publication: Arc<ReviewPublication>,
        is_batch: bool,
        deadline: Duration,
    ) -> Result<ReviewSession, AnalyzeReviewError> {
        let prepared = self
            .prepare_review_post_analysis(operation, environment, root, session, model)
            .await
            .map_err(ReviewAnalysisExecutionError::from)?;
        let model = prepared.tier.model.clone();
        let execution = environment
            .for_review_post_analysis(
                self,
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
        let stored = self
            .execute_review_post_analysis(
                operation,
                root,
                &prepared,
                &execution,
                publication.clone(),
                deadline,
            )
            .await?;
        self.publish_review_snapshot(root, &session.id, publication.observer.as_ref())
            .await;
        publication.review(ReviewEvent::AnalyzeFinished(stored.clone()));
        Ok(stored)
    }

    #[cfg(feature = "test-support")]
    #[allow(clippy::too_many_arguments)]
    pub async fn test_run_review_analysis(
        &self,
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
        let environment = PlanExecutionEnvironment::new(self.configuration().await, operation);
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

#[cfg(feature = "test-support")]
impl Application {
    pub async fn test_run_group_audits(
        &self,
        root: &Path,
        session: &ReviewSession,
        operation: Arc<OperationGuard>,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<tauqe_core::workflow::batch::BatchReport, String> {
        let environment = PlanExecutionEnvironment::new(self.configuration().await, &operation);
        let selection = ModelSelection::Specific(session.model.clone());
        let publication = Arc::new(ReviewPublication {
            observer,
            id: operation.publication_id(),
        });
        crate::api::run_group_audits(
            self,
            (&environment, &selection),
            root,
            session,
            operation,
            publication,
            deadline,
        )
        .await
    }
}
