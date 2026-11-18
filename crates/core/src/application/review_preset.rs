//! Preset review preparation and grouped audit lifecycle.
use crate::api::{Application, GroupAuditObserver, OperationGuard, ResolvedReviewSynthesis};
use crate::config::ReviewPreset;
use crate::models::{ModelRef, ModelRole, ModelSelection};
use crate::providers::{LlmProvider, ModelUsageInfo};
use crate::review::uuid::generate_uuid_v7;
use crate::review::ReviewStorage;
use crate::review::{PostAnalysisMode, ReviewGroup, ReviewLaunchConfig, ReviewSession};
use crate::workflow::batch::BatchStop;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct ResolvedPresetReview {
    name: String,
    preset: ReviewPreset,
    pub model: ModelRef,
    groups: Vec<ReviewGroup>,
    mode: PostAnalysisMode,
}
#[derive(Debug, thiserror::Error)]
pub enum PresetReviewPreparationError {
    #[error("Review preset '{0}' not found")]
    NotFound(String),
    #[error("Review preset '{0}' declares no groups")]
    EmptyGroups(String),
    #[error("preset review requires an active operation for this session")]
    OperationNotOwned,
}
impl Application {
    pub async fn resolve_review_preset(
        &self,
        name: &str,
        selection: Option<&ModelSelection>,
    ) -> Result<ResolvedPresetReview, PresetReviewPreparationError> {
        let cfg = self.configuration().await.config;
        let preset = cfg
            .review
            .presets
            .iter()
            .find(|p| p.name.trim() == name)
            .cloned()
            .ok_or_else(|| PresetReviewPreparationError::NotFound(name.into()))?;
        let model = cfg.resolve_model(
            &selection
                .cloned()
                .unwrap_or(ModelSelection::Role(ModelRole::Reviewer)),
        );
        let groups = preset.to_launch_groups();
        if groups.is_empty() {
            return Err(PresetReviewPreparationError::EmptyGroups(name.into()));
        }
        let mode = preset.post_analysis;
        Ok(ResolvedPresetReview {
            name: name.into(),
            preset,
            model,
            groups,
            mode,
        })
    }
    pub fn prepare_preset_review(
        &self,
        operation: &OperationGuard,
        resolved: &ResolvedPresetReview,
        prompt: Option<String>,
    ) -> Result<ReviewSession, PresetReviewPreparationError> {
        if !self.owns_operation(operation) {
            return Err(PresetReviewPreparationError::OperationNotOwned);
        }
        resolved.materialize(prompt)
    }
}
impl ResolvedPresetReview {
    fn materialize(
        &self,
        user_prompt: Option<String>,
    ) -> Result<ReviewSession, PresetReviewPreparationError> {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let review_id = generate_uuid_v7();

        // The declared audit paths are the launch snapshot's targets: literal
        // paths and patterns both, exactly as configured.
        let mut target_files: Vec<String> = self
            .groups
            .iter()
            .flat_map(|group| group.paths.iter())
            .cloned()
            .collect();
        target_files.sort();
        target_files.dedup();
        // The launch is named by configuration: the preset's title is the
        // session title, never a streamed summary.
        let title = self
            .preset
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.preset.name.trim().to_string());

        let session = ReviewSession {
            // The UUIDv7 minted before the batch carries a millisecond
            // timestamp plus random bits, so two preset launches never share
            // a session file, and the same string keys the provider session.
            id: review_id,
            title,
            created_at,
            model: self.model.clone(),
            description: user_prompt.clone(),
            user_prompt: user_prompt.clone(),
            target_files,
            // Findings are appended by the group supervisor, so the snapshot
            // stores the empty list the batch starts from.
            items: Vec::new(),
            raw_markdown: String::new(),
            groups: self.groups.clone(),
            post_analysis: None,
            // Immutable launch snapshot: a later edit of the project
            // configuration never rewrites a stored session.
            launch_config: Some(ReviewLaunchConfig {
                preset: Some(self.name.clone()),
                mode: self.mode,
                groups: self.groups.clone(),
            }),
        };

        Ok(session)
    }
}

#[async_trait::async_trait]
pub trait PresetReviewObserver: GroupAuditObserver {
    async fn reviewer(
        &self,
        resolved: &ResolvedReviewSynthesis,
    ) -> Result<Arc<dyn LlmProvider>, String>;
    async fn post_analysis(
        &self,
        session: &ReviewSession,
        operation: &OperationGuard,
    ) -> Result<ReviewSession, String>;
}
#[derive(Debug, thiserror::Error)]
pub enum PresetReviewExecutionError {
    #[error("Review was cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}
pub struct PresetReviewResult {
    pub session: ReviewSession,
    pub usage: ModelUsageInfo,
    pub current_cost: f64,
    pub session_total_cost: f64,
}
async fn offload<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("blocking task failed: {e}"))?
        .map_err(|e| format!("{e:#}"))
}
impl Application {
    pub async fn execute_preset_review(
        &self,
        operation: Arc<OperationGuard>,
        root: &Path,
        session: ReviewSession,
        observer: Arc<dyn PresetReviewObserver>,
        deadline: Duration,
    ) -> Result<PresetReviewResult, PresetReviewExecutionError> {
        use PresetReviewExecutionError::{Cancelled, Failed};
        if !self.owns_operation(&operation) {
            return Err(Failed(
                "preset review requires an active operation for this session".into(),
            ));
        }
        if *operation.cancel_receiver().borrow() {
            return Err(Cancelled);
        }
        let save_root = root.to_path_buf();
        let to_save = session.clone();
        offload(move || ReviewStorage::save_session(&save_root, &to_save))
            .await
            .map_err(Failed)?;
        tracing::info!(
            review_id = session.id.as_str(),
            groups = session.groups.len(),
            "preset review session persisted"
        );
        let resolved = self
            .resolve_review_synthesis(Some(&ModelSelection::Specific(session.model.clone())))
            .await;
        let provider = observer.reviewer(&resolved).await.map_err(Failed)?;
        let report = crate::api::run_group_audits(
            self,
            root,
            &session,
            operation.clone(),
            provider,
            observer.clone(),
            deadline,
        )
        .await
        .map_err(Failed)?;
        match &report.stop {
            BatchStop::Cancelled => return Err(Cancelled),
            BatchStop::Failed { step_id } => {
                return Err(Failed(format!(
                    "Review group batch stopped on failed group '{step_id}'"
                )))
            }
            BatchStop::Blocked { step_id, reason } => {
                return Err(Failed(format!(
                    "Review group batch stopped on blocked group '{step_id}': {reason}"
                )))
            }
            BatchStop::Completed | BatchStop::Stopped => {}
        }
        let load_root = root.to_path_buf();
        let id = session.id.clone();
        let mut stored = offload(move || ReviewStorage::load_review(&load_root, &id))
            .await
            .map_err(Failed)?
            .ok_or_else(|| {
                Failed(format!(
                    "Review session '{}' disappeared after the group batch",
                    session.id
                ))
            })?;
        let mode = session
            .launch_config
            .as_ref()
            .map(|c| c.mode)
            .unwrap_or(PostAnalysisMode::None);
        if mode == PostAnalysisMode::Automatic && report.is_success() {
            stored = observer
                .post_analysis(&stored, &operation)
                .await
                .map_err(Failed)?;
        }
        if let Err(error) = self
            .record_history_review(
                session.model.to_string(),
                session.user_prompt.clone(),
                stored.description.clone(),
                stored.target_files.clone(),
                stored.items.len(),
            )
            .await
        {
            tracing::warn!(error = %error, "Failed to record the review in interaction history");
        }
        let usage = ModelUsageInfo {
            prompt_tokens: report.usage.prompt_tokens,
            completion_tokens: report.usage.completion_tokens,
            total_tokens: report.usage.total_tokens,
            reasoning_tokens: None,
            cached_tokens: None,
            cost: report.usage.cost,
        };
        Ok(PresetReviewResult {
            session: stored,
            current_cost: report.usage.cost.unwrap_or(0.0),
            session_total_cost: self.usage_snapshot().await.session.cost,
            usage,
        })
    }
}
