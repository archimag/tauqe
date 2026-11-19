//! Application-owned stored-document operations and selection policy.
use crate::api::{Application, ApplicationEvent, ApplicationObserver};
use std::path::{Path, PathBuf};
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::plan::{
    find_duplicate_item_ids, storage::PlanStorage, Plan, PlanItem, PlanItemStatus, PlanSummary,
};
use tauqe_core::review::{storage::ReviewStorage, IssueStatus, ReviewIssue, ReviewSession};
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("Plan ID must be non-empty and must not contain path separators")]
    InvalidPlanId,
    #[error("Plan contains a duplicate or empty step ID: '{0}'")]
    InvalidPlanTree(String),
    #[error("review_id is required: review item updates are scoped to an explicit session")]
    MissingReviewId,
    #[error("Another operation is already running")]
    Busy,
    #[error("{0}")]
    Storage(String),
}
#[derive(Clone)]
pub struct Documents {
    application: Application,
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
        Documents {
            application: self.clone(),
            root: root.to_path_buf(),
        }
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
        self.repository_documents()
            .await
            .delete_plan(id, observer)
            .await
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
        self.repository_documents()
            .await
            .delete_review(id, observer)
            .await
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
}

impl Documents {
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
    async fn delete_plan(
        &self,
        id: &str,
        observer: &dyn ApplicationObserver,
    ) -> Result<bool, DocumentError> {
        let root = self.root.clone();
        let id = id.to_string();
        let deleted = read({
            let id = id.clone();
            move || PlanStorage::delete_plan(&root, &id)
        })
        .await?;
        if deleted {
            self.application
                .release_document_focus(&DiscussionTarget::Plan { id }, observer)
                .await;
            self.publish_plan_list(observer).await;
        }
        Ok(deleted)
    }
    async fn set_active_plan(
        &self,
        id: Option<String>,
        observer: &dyn ApplicationObserver,
    ) -> Result<(), DocumentError> {
        let root = self.root.clone();
        let stored = id.clone();
        read(move || PlanStorage::save_active_id(&root, stored.as_deref()))
            .await
            .map_err(DocumentError::Storage)?;
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
    pub async fn review_list(&self) -> Result<(Vec<ReviewSession>, Option<String>), String> {
        let reviews = self.reviews().await?;
        let active = self.active_review_id().await.ok().flatten();
        Ok((reviews, active))
    }
    pub async fn active_review_id(&self) -> Result<Option<String>, String> {
        let root = self.root.clone();
        read(move || ReviewStorage::load_active_id(&root)).await
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
    async fn delete_review(
        &self,
        id: &str,
        observer: &dyn ApplicationObserver,
    ) -> Result<bool, DocumentError> {
        let root = self.root.clone();
        let id = id.to_string();
        let deleted = read({
            let id = id.clone();
            move || ReviewStorage::delete_review(&root, &id)
        })
        .await?;
        if deleted {
            self.application
                .release_document_focus(&DiscussionTarget::Review { id }, observer)
                .await;
            self.publish_review_list(observer).await;
        }
        Ok(deleted)
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
            self.application
                .publish_review_snapshot(&self.root, &id, observer)
                .await;
        }
        Ok(item)
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
