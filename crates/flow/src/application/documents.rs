//! Application-owned stored-document operations and selection policy.

mod execution;

use crate::api::{Application, ApplicationEvent, ApplicationObserver};
use std::path::{Path, PathBuf};
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::plan::{
    find_duplicate_item_ids, storage::PlanStorage, Plan, PlanItem, PlanItemStatus, PlanSummary,
};
use tauqe_core::review::{
    storage::{GroupCompletion, GroupStepStatusError, ReviewStorage},
    IssueStatus, ReviewGroup, ReviewIssue, ReviewSession,
};
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("Plan ID must be non-empty and must not contain path separators")]
    InvalidPlanId,
    #[error("Plan contains a duplicate or empty step ID: '{0}'")]
    InvalidPlanTree(String),
    #[error("review_id is required: review item updates are scoped to an explicit session")]
    MissingReviewId,
    #[error("review_id, group_id and step_id must be non-empty")]
    InvalidReviewGroupStepAddress,
    #[error("{0}")]
    ReviewGroupStepNotFound(String),
    #[error("{0}")]
    ReviewGroupStepRejected(String),
    #[error("Another operation is already running")]
    Busy,
    #[error("{0}")]
    Storage(String),
}
#[derive(Clone)]
pub struct Documents {
    root: PathBuf,
}
#[derive(Debug, thiserror::Error)]
pub enum PlanPreparationError {
    #[error("Missing plan ID and no active plan is set")]
    MissingId,
    #[error("Missing blocked step ID to replan")]
    MissingStep,
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    NotLeaf(String),
    #[error("{0}")]
    Immutable(String),
    #[error("{0}")]
    Storage(String),
}
pub struct PreparedPlanReplan {
    pub plan: Plan,
    pub step: PlanItem,
}
async fn read<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| format!("blocking task failed: {error}"))?
        .map_err(|error| format!("{error:#}"))
}
impl From<String> for DocumentError {
    fn from(error: String) -> Self {
        Self::Storage(error)
    }
}
impl Application {
    pub(crate) fn documents_at(&self, root: &Path) -> Documents {
        Documents::new(root)
    }
}
impl Application {
    async fn repository_documents(&self) -> Documents {
        self.documents_at(&self.repository_root().await)
    }

    pub async fn list_plans(&self) -> Result<(Vec<Plan>, Option<String>), String> {
        self.repository_documents().await.plan_list().await
    }

    pub async fn get_plan(&self, id: Option<String>) -> Result<Option<Plan>, String> {
        self.repository_documents().await.resolve_plan(id).await
    }

    pub async fn save_plan(
        &self,
        plan: Plan,
        observer: &dyn ApplicationObserver,
    ) -> Result<(), DocumentError> {
        let _operation = self
            .try_begin_operation()
            .map_err(|_| DocumentError::Busy)?;
        self.repository_documents()
            .await
            .save_plan(plan, observer)
            .await
    }

    pub async fn update_plan_item(
        &self,
        id: &str,
        item: &str,
        status: Option<PlanItemStatus>,
        observer: &dyn ApplicationObserver,
    ) -> Result<Option<Plan>, DocumentError> {
        let _operation = self
            .try_begin_operation()
            .map_err(|_| DocumentError::Busy)?;
        self.repository_documents()
            .await
            .update_plan_item(id, item, status, observer)
            .await
    }

    pub async fn clear_plan_horizon(
        &self,
        id: &str,
        observer: &dyn ApplicationObserver,
    ) -> Result<Option<Plan>, DocumentError> {
        let _operation = self
            .try_begin_operation()
            .map_err(|_| DocumentError::Busy)?;
        self.repository_documents()
            .await
            .clear_plan_horizon(id, observer)
            .await
    }

    pub async fn delete_plan(
        &self,
        id: &str,
        observer: &dyn ApplicationObserver,
    ) -> Result<bool, DocumentError> {
        let _operation = self
            .try_begin_operation()
            .map_err(|_| DocumentError::Busy)?;
        let documents = self.repository_documents().await;
        let deleted = documents.delete_plan(id).await?;
        if deleted {
            self.release_document_focus(&DiscussionTarget::Plan { id: id.into() }, observer)
                .await;
            documents.publish_plan_list(observer).await;
        }
        Ok(deleted)
    }

    pub async fn set_active_plan(
        &self,
        id: Option<String>,
        observer: &dyn ApplicationObserver,
    ) -> Result<(), DocumentError> {
        let _operation = self
            .try_begin_operation()
            .map_err(|_| DocumentError::Busy)?;
        self.repository_documents()
            .await
            .set_active_plan(id, observer)
            .await
    }

    pub async fn list_reviews(&self) -> Result<(Vec<ReviewSession>, Option<String>), String> {
        self.repository_documents().await.review_list().await
    }

    pub async fn get_review(&self, id: Option<String>) -> Result<Option<ReviewSession>, String> {
        self.repository_documents().await.resolve_review(id).await
    }

    pub async fn delete_review(
        &self,
        id: &str,
        observer: &dyn ApplicationObserver,
    ) -> Result<bool, DocumentError> {
        let _operation = self
            .try_begin_operation()
            .map_err(|_| DocumentError::Busy)?;
        let documents = self.repository_documents().await;
        let deleted = documents.delete_review(id).await?;
        if deleted {
            self.release_document_focus(&DiscussionTarget::Review { id: id.into() }, observer)
                .await;
            documents.publish_review_list(observer).await;
        }
        Ok(deleted)
    }

    pub async fn update_review_item(
        &self,
        id: Option<String>,
        item: u32,
        status: Option<IssueStatus>,
        note: Option<String>,
        observer: &dyn ApplicationObserver,
    ) -> Result<Option<ReviewIssue>, DocumentError> {
        let _operation = self
            .try_begin_operation()
            .map_err(|_| DocumentError::Busy)?;
        self.repository_documents()
            .await
            .update_review_item(id, item, status, note, observer)
            .await
    }

    pub async fn update_review_group_step(
        &self,
        review_id: &str,
        group_id: &str,
        step_id: &str,
        status: PlanItemStatus,
        observer: &dyn ApplicationObserver,
    ) -> Result<ReviewGroup, DocumentError> {
        let _operation = self
            .try_begin_operation()
            .map_err(|_| DocumentError::Busy)?;
        self.repository_documents()
            .await
            .update_review_group_step(review_id, group_id, step_id, status, observer)
            .await
    }
}

impl Documents {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    async fn publish_plan(&self, plan: &Plan, is_save: bool, observer: &dyn ApplicationObserver) {
        observer.publish(
            "",
            ApplicationEvent::PlanDocument {
                plan: plan.clone(),
                is_save,
            },
        );
        self.publish_plan_list(observer).await;
    }

    async fn publish_plan_list(&self, observer: &dyn ApplicationObserver) {
        if let Ok((plans, active_id)) = self.plan_list().await {
            observer.publish("", ApplicationEvent::PlanList { plans, active_id });
        }
    }

    async fn publish_review_list(&self, observer: &dyn ApplicationObserver) {
        if let Ok((reviews, active_id)) = self.review_list().await {
            observer.publish("", ApplicationEvent::ReviewList { reviews, active_id });
        }
    }

    pub(crate) async fn publish_review_snapshot(
        &self,
        id: &str,
        observer: &dyn ApplicationObserver,
    ) {
        if let Ok(Some(session)) = self.review(id).await {
            observer.publish("", ApplicationEvent::ReviewDocument(session));
        }
        self.publish_review_list(observer).await;
    }

    pub async fn plans(&self) -> Result<Vec<Plan>, String> {
        let root = self.root.clone();
        read(move || PlanStorage::load_all(&root)).await
    }
    pub async fn plan_summaries(&self) -> Result<Vec<PlanSummary>, String> {
        let root = self.root.clone();
        read(move || PlanStorage::list_plans(&root)).await
    }
    pub async fn active_plan_id(&self) -> Result<Option<String>, String> {
        let root = self.root.clone();
        read(move || PlanStorage::load_active_id(&root)).await
    }
    pub async fn plan(&self, id: &str) -> Result<Option<Plan>, String> {
        let root = self.root.clone();
        let id = id.to_string();
        read(move || PlanStorage::load_plan(&root, &id)).await
    }

    /// Records the marker without publishing or claiming another operation.
    pub(crate) async fn write_active_plan(&self, id: Option<String>) -> Result<(), String> {
        let root = self.root.clone();
        read(move || PlanStorage::save_active_id(&root, id.as_deref())).await
    }
    pub async fn plan_list(&self) -> Result<(Vec<Plan>, Option<String>), String> {
        let root = self.root.clone();
        read(move || {
            Ok((
                PlanStorage::load_all(&root)?,
                PlanStorage::load_active_id(&root).ok().flatten(),
            ))
        })
        .await
    }
    pub async fn resolve_plan(&self, id: Option<String>) -> Result<Option<Plan>, String> {
        let active = self.active_plan_id().await.unwrap_or(None);
        let id = id.filter(|id| !id.trim().is_empty()).or(active);
        let id = match id {
            Some(id) => Some(id),
            None => self
                .plan_summaries()
                .await
                .ok()
                .and_then(|plans| plans.into_iter().next().map(|plan| plan.id)),
        };
        match id {
            Some(id) => self.plan(&id).await,
            None => Ok(None),
        }
    }
    async fn save_plan(
        &self,
        mut plan: Plan,
        observer: &dyn ApplicationObserver,
    ) -> Result<(), DocumentError> {
        if plan.id.trim().is_empty() || plan.id.contains('/') || plan.id.contains('\\') {
            return Err(DocumentError::InvalidPlanId);
        }
        if let Some(id) = find_duplicate_item_ids(&plan.implementation.items) {
            return Err(DocumentError::InvalidPlanTree(id));
        }
        plan.implementation.normalize_leaf_files();
        let root = self.root.clone();
        let stored = plan.clone();
        read(move || PlanStorage::save_plan(&root, &stored))
            .await
            .map_err(DocumentError::Storage)?;
        self.publish_plan(&plan, true, observer).await;
        Ok(())
    }
    async fn update_plan_item(
        &self,
        id: &str,
        item: &str,
        status: Option<PlanItemStatus>,
        observer: &dyn ApplicationObserver,
    ) -> Result<Option<Plan>, DocumentError> {
        let root = self.root.clone();
        let id = id.to_string();
        let item = item.to_string();
        let plan = read(move || PlanStorage::update_item(&root, &id, &item, status, None, None))
            .await
            .map_err(DocumentError::Storage)?;
        if let Some(plan) = &plan {
            self.publish_plan(plan, false, observer).await;
        }
        Ok(plan)
    }
    async fn clear_plan_horizon(
        &self,
        id: &str,
        observer: &dyn ApplicationObserver,
    ) -> Result<Option<Plan>, DocumentError> {
        let root = self.root.clone();
        let id = id.to_string();
        let plan = read(move || PlanStorage::clear_horizon(&root, &id))
            .await
            .map_err(DocumentError::Storage)?;
        if let Some(plan) = &plan {
            self.publish_plan(plan, false, observer).await;
        }
        Ok(plan)
    }
    async fn delete_plan(&self, id: &str) -> Result<bool, DocumentError> {
        let root = self.root.clone();
        let id = id.to_string();
        read(move || PlanStorage::delete_plan(&root, &id))
            .await
            .map_err(DocumentError::Storage)
    }
    async fn set_active_plan(
        &self,
        id: Option<String>,
        observer: &dyn ApplicationObserver,
    ) -> Result<(), DocumentError> {
        self.write_active_plan(id.clone()).await?;
        if let Ok(plans) = self.plans().await {
            observer.publish(
                "",
                ApplicationEvent::PlanList {
                    plans,
                    active_id: id,
                },
            );
        }
        Ok(())
    }
    pub async fn reviews(&self) -> Result<Vec<ReviewSession>, String> {
        let root = self.root.clone();
        read(move || ReviewStorage::load_all(&root)).await
    }

    pub(crate) async fn save_review(&self, session: &ReviewSession) -> Result<(), String> {
        let root = self.root.clone();
        let session = session.clone();
        read(move || ReviewStorage::save_session(&root, &session)).await
    }

    pub(crate) async fn persist_review_regrouping(
        &self,
        regrouping: tauqe_core::review::analysis::PostAnalysisRegrouping,
    ) -> Result<Option<ReviewSession>, String> {
        let root = self.root.clone();
        read(move || regrouping.persist(&root)).await
    }

    pub(crate) async fn append_review_group_findings(
        &self,
        review_id: &str,
        group_id: &str,
        findings: Vec<ReviewIssue>,
    ) -> Result<Option<ReviewSession>, String> {
        let root = self.root.clone();
        let review_id = review_id.to_owned();
        let group_id = group_id.to_owned();
        read(move || ReviewStorage::append_group_findings(&root, &review_id, &group_id, findings))
            .await
    }
    pub async fn review_list(&self) -> Result<(Vec<ReviewSession>, Option<String>), String> {
        let reviews = self.reviews().await?;
        let active = self.active_review_id().await.ok().flatten();
        Ok((reviews, active))
    }
    pub async fn active_review_id(&self) -> Result<Option<String>, String> {
        let root = self.root.clone();
        read(move || ReviewStorage::load_active_id(&root)).await
    }

    pub(crate) async fn write_active_review(&self, id: &str) -> Result<(), String> {
        let root = self.root.clone();
        let id = id.to_owned();
        read(move || ReviewStorage::save_active_id(&root, Some(&id))).await
    }

    pub(crate) async fn close_review_group(
        &self,
        review_id: &str,
        group_id: &str,
    ) -> Result<GroupCompletion, String> {
        let root = self.root.clone();
        let review = review_id.to_owned();
        let group = group_id.to_owned();
        read(move || {
            ReviewStorage::close_group_implementation(
                &root,
                &review,
                &group,
                "Resolved by the completed implementation horizon of the review group",
            )
        })
        .await
        .map_err(|err| format!("Failed to persist review group '{group_id}' completion: {err}"))
    }

    pub(crate) async fn block_review_group(
        &self,
        review_id: &str,
        group_id: &str,
        reason: &str,
    ) -> Result<(), String> {
        let root = self.root.clone();
        let review = review_id.to_owned();
        let group = group_id.to_owned();
        let reason = reason.to_owned();
        read(move || ReviewStorage::set_group_blocked_reason(&root, &review, &group, Some(&reason)))
            .await
            .map_err(|err| format!("Failed to persist review group '{group_id}' blocker: {err}"))?
            .map(|_| ())
            .ok_or_else(|| format!("Review group '{group_id}' not found in session '{review_id}'"))
    }

    pub(crate) async fn complete_review_finding(
        &self,
        review_id: &str,
        item_id: u32,
        note: &str,
    ) -> Result<(), String> {
        self.write_review_finding_verdict(review_id, item_id, note, ReviewStorage::complete_finding)
            .await
    }

    pub(crate) async fn skip_review_finding(
        &self,
        review_id: &str,
        item_id: u32,
        reason: &str,
    ) -> Result<(), String> {
        self.write_review_finding_verdict(review_id, item_id, reason, ReviewStorage::skip_finding)
            .await
    }

    async fn write_review_finding_verdict(
        &self,
        review_id: &str,
        item_id: u32,
        note: &str,
        write: impl FnOnce(&Path, &str, u32, &str) -> anyhow::Result<Option<()>> + Send + 'static,
    ) -> Result<(), String> {
        let root = self.root.clone();
        let review = review_id.to_owned();
        let note = note.to_owned();
        read(move || write(&root, &review, item_id, &note))
            .await
            .map_err(|err| format!("Failed to persist review finding #{item_id} verdict: {err}"))?
            .ok_or_else(|| format!("Review finding #{item_id} not found in session '{review_id}'"))
    }
    pub async fn review(&self, id: &str) -> Result<Option<ReviewSession>, String> {
        let root = self.root.clone();
        let id = id.to_string();
        read(move || ReviewStorage::load_review(&root, &id)).await
    }
    pub async fn resolve_review(
        &self,
        id: Option<String>,
    ) -> Result<Option<ReviewSession>, String> {
        let active = self.active_review_id().await.unwrap_or(None);
        let id = id.filter(|id| !id.trim().is_empty()).or(active);
        let id = match id {
            Some(id) => Some(id),
            None => {
                let root = self.root.clone();
                read(move || ReviewStorage::list_reviews(&root))
                    .await
                    .ok()
                    .and_then(|reviews| reviews.into_iter().next().map(|review| review.id))
            }
        };
        match id {
            Some(id) => self.review(&id).await,
            None => Ok(None),
        }
    }
    async fn delete_review(&self, id: &str) -> Result<bool, DocumentError> {
        let root = self.root.clone();
        let id = id.to_string();
        read(move || ReviewStorage::delete_review(&root, &id))
            .await
            .map_err(DocumentError::Storage)
    }
    async fn update_review_item(
        &self,
        id: Option<String>,
        item_id: u32,
        status: Option<IssueStatus>,
        note: Option<String>,
        observer: &dyn ApplicationObserver,
    ) -> Result<Option<ReviewIssue>, DocumentError> {
        let id = id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or(DocumentError::MissingReviewId)?
            .to_string();
        let root = self.root.clone();
        let target = id.clone();
        let item = read(move || {
            ReviewStorage::update_session_item(&root, &target, item_id, |item| {
                if let Some(status) = status {
                    item.status = status;
                }
                if let Some(note) = note {
                    item.note = match note.trim() {
                        "" => None,
                        trimmed => Some(trimmed.to_string()),
                    };
                }
            })
        })
        .await
        .map_err(DocumentError::Storage)?;
        if item.is_some() {
            self.publish_review_snapshot(&id, observer).await;
        }
        Ok(item)
    }
    async fn update_review_group_step(
        &self,
        review_id: &str,
        group_id: &str,
        step_id: &str,
        status: PlanItemStatus,
        observer: &dyn ApplicationObserver,
    ) -> Result<ReviewGroup, DocumentError> {
        if [review_id, group_id, step_id]
            .iter()
            .any(|id| id.trim().is_empty())
        {
            return Err(DocumentError::InvalidReviewGroupStepAddress);
        }
        let root = self.root.clone();
        let review = review_id.to_owned();
        let group = group_id.to_owned();
        let step = step_id.to_owned();
        let saved = tokio::task::spawn_blocking(move || {
            ReviewStorage::update_group_step_status(&root, &review, &group, &step, status)
        })
        .await
        .map_err(|error| DocumentError::Storage(format!("blocking task failed: {error}")))?
        .map_err(|error| match error {
            GroupStepStatusError::MissingSession(_)
            | GroupStepStatusError::MissingGroup { .. }
            | GroupStepStatusError::MissingStep(_) => {
                DocumentError::ReviewGroupStepNotFound(error.to_string())
            }
            GroupStepStatusError::NotArchitectural(_)
            | GroupStepStatusError::InvalidTransition(_)
            | GroupStepStatusError::InvalidTree(_) => {
                DocumentError::ReviewGroupStepRejected(error.to_string())
            }
            GroupStepStatusError::Storage(_) => DocumentError::Storage(error.to_string()),
        })?;
        self.publish_review_snapshot(review_id, observer).await;
        Ok(saved)
    }
    async fn effective_plan_id(&self, id: String) -> Result<String, PlanPreparationError> {
        let id = if id.trim().is_empty() {
            self.active_plan_id()
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
        } else {
            id
        };
        if id.trim().is_empty() {
            Err(PlanPreparationError::MissingId)
        } else {
            Ok(id)
        }
    }
    pub(crate) async fn prepare_plan_refinement(
        &self,
        id: String,
    ) -> Result<Plan, PlanPreparationError> {
        let id = self.effective_plan_id(id).await?;
        self.plan(&id)
            .await
            .map_err(PlanPreparationError::Storage)?
            .ok_or_else(|| PlanPreparationError::NotFound(format!("Plan '{id}' not found")))
    }
    pub(crate) async fn prepare_plan_replan(
        &self,
        id: String,
        step_id: String,
    ) -> Result<PreparedPlanReplan, PlanPreparationError> {
        let id = self.effective_plan_id(id).await?;
        if step_id.trim().is_empty() {
            return Err(PlanPreparationError::MissingStep);
        };
        let plan = self.prepare_plan_refinement(id.clone()).await?;
        let step = plan
            .implementation
            .find_item(&step_id)
            .cloned()
            .ok_or_else(|| {
                PlanPreparationError::NotFound(format!("Step '{step_id}' not found in plan '{id}'"))
            })?;
        if !step.is_leaf() {
            return Err(PlanPreparationError::NotLeaf(format!(
                "Step '{}' ({}) is not a leaf and cannot be replanned",
                step.id, step.title
            )));
        }
        if step.status == PlanItemStatus::Done {
            return Err(PlanPreparationError::Immutable(format!(
                "Step '{}' is DONE and immutable; completed steps cannot be replanned",
                step.id
            )));
        }
        Ok(PreparedPlanReplan { plan, step })
    }
}

#[cfg(test)]
mod tests;
