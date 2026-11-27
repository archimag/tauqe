//! Review preview and audit lifecycle.

mod groups;
mod preset;
mod preview;
mod synthesis_completion;
mod synthesis_execution;
mod synthesis_preparation;
mod synthesis_request;
mod synthesis_response;

pub use preset::PresetReviewPreparationError;
pub use preview::{ReviewPreview, ReviewPreviewError, ReviewPreviewGroup};
pub use synthesis_preparation::ReviewSynthesisPreparationError;

use super::analysis::AnalysisResources;
#[cfg(feature = "test-support")]
use crate::api::OperationGuard;
use crate::api::{ApplicationEvent, ApplicationObserver, UsageTotalsSnapshot};
use crate::application::configuration::Configuration;
use crate::application::context::ContextSession;
use crate::application::history::HistorySession;
use crate::application::review_observer::ReviewObserver;
use crate::application::{ExecutionSession, PlanExecutionEnvironment};
use crate::workflows::architect::ArchitectScope;
use groups::{run_group_audits, GroupAuditObserver};
use preset::{PresetReviewExecutionError, ResolvedPresetReview};
#[cfg(feature = "test-support")]
use std::path::Path;
use std::{sync::Arc, time::Duration};
use synthesis_execution::{ReviewSynthesisExecutionConfig, ReviewSynthesisObserver};
use synthesis_preparation::ResolvedReviewSynthesis;
use tauqe_core::config::{AppConfig, ReviewPreset};
use tauqe_core::models::{ModelRef, ModelRole, ModelSelection};
use tauqe_core::providers::{ModelUsageInfo, StreamEvent};
use tauqe_core::review::{stream::ReviewStreamUpdate, ReviewGroup, ReviewSession};
use tauqe_core::stop::HardCancellation;

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

pub(super) struct ReviewPublication {
    pub(super) observer: Arc<dyn ApplicationObserver>,
    pub(super) id: String,
}
impl ReviewPublication {
    pub(super) fn review(&self, event: ReviewEvent) {
        self.observer
            .publish(&self.id, ApplicationEvent::ReviewRun(event));
    }
}
#[async_trait::async_trait]
impl ReviewObserver for ReviewPublication {
    async fn pace(&self, event: &StreamEvent, cancel: &HardCancellation) {
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

pub(crate) struct AuditResources {
    pub(crate) context: ContextSession,
    pub(crate) execution: ExecutionSession,
    pub(crate) history: HistorySession,
}

fn preset_name(name: Option<&str>) -> Option<&str> {
    name.map(str::trim).filter(|name| !name.is_empty())
}

fn find_preset<'a>(config: &'a AppConfig, name: &str) -> Option<&'a ReviewPreset> {
    config
        .review
        .presets
        .iter()
        .find(|preset| preset.name.trim() == name)
}

impl AuditResources {
    pub(crate) async fn start(
        self,
        configuration: &Configuration,
        input: ReviewStartInput,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<String, ReviewStartError> {
        let operation = Arc::new(
            self.execution
                .operations
                .begin()
                .map_err(|_| ReviewStartError::Busy)?,
        );
        let environment = PlanExecutionEnvironment::new(configuration.snapshot().await, &operation);
        if let Some(name) = preset_name(input.preset.as_deref()) {
            let resolved =
                ResolvedPresetReview::resolve(environment.config(), name, input.model.as_ref())?;
            let root = self.context.repository_root().await;
            let session = resolved.materialize(input.prompt);
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
                let result = self
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
            .unwrap_or(ModelSelection::Role(ModelRole::Reviewer));
        let resolved =
            ResolvedReviewSynthesis::resolve(environment.config().clone(), Some(&selection));
        let window = self
            .prepare_review_synthesis_window(&resolved, &input.path_patterns)
            .await?;
        let prepared = self
            .prepare_review_synthesis_request(&operation, &resolved, &window.files, input.prompt)
            .map_err(|_| ReviewStartError::Busy)?;
        let execution = environment
            .for_reviewer_structured(
                &self.execution,
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
            let result = self
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

    pub(super) fn analysis(&self) -> AnalysisResources<'_> {
        AnalysisResources {
            context: &self.context,
            execution: &self.execution,
            history: &self.history,
        }
    }
}

#[cfg(feature = "test-support")]
impl AuditResources {
    pub(crate) async fn test_run_group_audits(
        &self,
        configuration: &Configuration,
        root: &Path,
        session: &ReviewSession,
        operation: Arc<OperationGuard>,
        observer: Arc<dyn ApplicationObserver>,
        deadline: Duration,
    ) -> Result<tauqe_core::workflow::batch::BatchReport, String> {
        let environment = PlanExecutionEnvironment::new(configuration.snapshot().await, &operation);
        let selection = ModelSelection::Specific(session.model.clone());
        let publication = Arc::new(ReviewPublication {
            observer,
            id: operation.publication_id(),
        });
        run_group_audits(
            self,
            (&environment, &selection),
            root,
            session,
            operation,
            publication,
            deadline,
        )
        .await
        .map(|(report, _)| report)
    }
}
