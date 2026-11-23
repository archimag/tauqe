//! Durable writes requested by plan and review execution.

use super::Documents;
use tauqe_core::models::ArchitectTier;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::plan::{Plan, PlanImplementation, PlanItem, PlanItemStatus};
use tauqe_core::review::{storage::ReviewStorage, IssueStatus};
use tauqe_core::workflow::implementation::StepFailure;

impl Documents {
    pub(crate) async fn write_new_plan_horizon(
        &self,
        mut draft: Plan,
        items: Vec<PlanItem>,
    ) -> Result<Plan, String> {
        let root = self.root.clone();
        super::read(move || {
            draft.implementation.items = PlanStorage::merge_subplan(&[], items)?;
            PlanStorage::save_plan(&root, &draft)?;
            Ok(draft)
        })
        .await
    }

    pub(crate) async fn write_synthesized_horizon(
        &self,
        plan: String,
        items: Vec<PlanItem>,
        tier: ArchitectTier,
        last_replanned_step: Option<String>,
    ) -> Result<Plan, String> {
        let root = self.root.clone();
        super::read(move || {
            PlanStorage::replace_uncompleted_horizon(&root, &plan, items, tier, last_replanned_step)
        })
        .await
    }

    pub(crate) async fn write_plan_leaf(
        &self,
        plan: &str,
        step: &str,
        status: PlanItemStatus,
        blocker: Option<&str>,
        commit: Option<&str>,
    ) -> Result<PlanImplementation, StepFailure> {
        let root = self.root.clone();
        let (plan, step, blocker, commit) = (
            plan.to_owned(),
            step.to_owned(),
            blocker.map(str::to_owned),
            commit.map(str::to_owned),
        );
        tauqe_core::blocking::run_blocking_async(move || {
            PlanStorage::update_item(
                &root,
                &plan,
                &step,
                Some(status),
                blocker.as_deref(),
                commit.as_deref(),
            )
            .map_err(|error| step_write_failure(&step, error))?
            .map(|plan| plan.implementation)
            .ok_or_else(|| {
                StepFailure::infrastructure(format!(
                    "Plan '{}' does not store step '{}'",
                    plan, step
                ))
            })
        })
        .await
        .map_err(leaf_worker_failure)?
    }

    pub(crate) async fn write_group_leaf(
        &self,
        review: &str,
        group: &str,
        step: &str,
        status: PlanItemStatus,
        blocker: Option<&str>,
        commit: Option<&str>,
    ) -> Result<PlanImplementation, StepFailure> {
        let root = self.root.clone();
        let (review, group, step, blocker, commit) = (
            review.to_owned(),
            group.to_owned(),
            step.to_owned(),
            blocker.map(str::to_owned),
            commit.map(str::to_owned),
        );
        tauqe_core::blocking::run_blocking_async(move || {
            ReviewStorage::transition_group_leaf(
                &root,
                &review,
                &group,
                &step,
                status,
                blocker.as_deref(),
                commit.as_deref(),
            )
            .map_err(|error| step_write_failure(&step, error))
        })
        .await
        .map_err(leaf_worker_failure)?
    }

    pub(crate) async fn write_finding_status(
        &self,
        review: &str,
        item: u32,
        status: IssueStatus,
    ) -> Result<(), StepFailure> {
        let root = self.root.clone();
        let review = review.to_owned();
        tauqe_core::blocking::run_blocking_async(move || {
            finding_write(
                &review,
                item,
                ReviewStorage::update_session_item_status(&root, &review, item, status),
            )
        })
        .await
        .map_err(leaf_worker_failure)?
    }

    pub(crate) async fn write_finding_commit(
        &self,
        review: &str,
        item: u32,
        hash: &str,
    ) -> Result<(), StepFailure> {
        let root = self.root.clone();
        let review = review.to_owned();
        let hash = hash.to_owned();
        tauqe_core::blocking::run_blocking_async(move || {
            finding_write(
                &review,
                item,
                ReviewStorage::update_session_item(&root, &review, item, |issue| {
                    issue.commit = Some(hash);
                }),
            )
        })
        .await
        .map_err(leaf_worker_failure)?
    }

    pub(crate) async fn replace_plan_horizon(
        &self,
        plan: &str,
        items: Vec<PlanItem>,
        tier: ArchitectTier,
        last_replanned_step: Option<String>,
    ) -> Result<(), StepFailure> {
        let root = self.root.clone();
        let plan = plan.to_owned();
        tauqe_core::blocking::run_blocking_async(move || {
            PlanStorage::replace_uncompleted_horizon(&root, &plan, items, tier, last_replanned_step)
                .map(drop)
                .map_err(|error| StepFailure::infrastructure(error.to_string()))
        })
        .await
        .map_err(|reason| {
            StepFailure::infrastructure(format!(
                "The implementation horizon could not be persisted: blocking task failed: {reason}"
            ))
        })?
    }
}

fn leaf_worker_failure(reason: impl std::fmt::Display) -> StepFailure {
    StepFailure::infrastructure(format!(
        "The implementation leaf could not be persisted: blocking task failed: {reason}"
    ))
}

fn step_write_failure(step: &str, error: impl std::fmt::Display) -> StepFailure {
    StepFailure::infrastructure(format!(
        "Failed to persist the status of step '{}': {}",
        step, error
    ))
}

fn finding_write<T>(
    review: &str,
    item: u32,
    result: anyhow::Result<Option<T>>,
) -> Result<(), StepFailure> {
    result
        .map_err(|error| {
            StepFailure::infrastructure(format!(
                "Failed to persist the status of review finding #{}: {}",
                item, error
            ))
        })?
        .map(drop)
        .ok_or_else(|| {
            StepFailure::infrastructure(format!(
                "Review session '{}' does not store finding #{}",
                review, item
            ))
        })
}
