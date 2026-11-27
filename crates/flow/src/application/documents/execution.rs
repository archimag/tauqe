//! Durable writes requested by plan and review execution.

use super::Documents;
use tauqe_core::blocking::{OperationHandle, StartedOperations};
use tauqe_core::models::ArchitectTier;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::plan::{Plan, PlanImplementation, PlanItem, PlanItemStatus};
use tauqe_core::review::{
    storage::{InitialGroupHorizon, InitialGroupHorizonError, ReviewStorage},
    IssueStatus, ReviewGroup, ReviewItem,
};
use tauqe_core::workflow::implementation::StepFailure;

impl Documents {
    /// Admit the atomic replacement into a retained completion owner. Losing
    /// the async waiter cannot discard or repeat an admitted write.
    pub(crate) fn start_group_horizon_replacement(
        &self,
        operations: &StartedOperations,
        address: (String, String),
        expected: (ReviewGroup, Vec<ReviewItem>),
        items: Vec<PlanItem>,
        tier: ArchitectTier,
    ) -> Result<
        OperationHandle<
            Result<ReviewGroup, tauqe_core::review::storage::GroupHorizonReplacementError>,
        >,
        &'static str,
    > {
        let root = self.root.clone();
        #[cfg(test)]
        let gates = crate::workflows::review::refinement::test_gates::for_root(&root);
        operations.start(
            move || {
                #[cfg(test)]
                if let Some(gates) = gates {
                    // The worker has already been admitted. Keep the actual write
                    // inside the retained operation while the test holds it.
                    gates.in_write.entered.notify_one();
                    let permit = loop {
                        if let Ok(permit) = gates.in_write.release.try_acquire() {
                            break permit;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    };
                    permit.forget();
                }
                ReviewStorage::replace_uncompleted_group_subplan(
                    &root,
                    &address.0,
                    &address.1,
                    &expected.0,
                    &expected.1,
                    items,
                    tier,
                )
            },
            None,
        )
    }

    /// The review workflow consumes this adapter when initial synthesis is wired in.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn write_initial_group_horizon(
        &self,
        review: &str,
        group: &str,
        items: Vec<PlanItem>,
        tier: ArchitectTier,
    ) -> Result<InitialGroupHorizon, InitialGroupHorizonError> {
        let root = self.root.clone();
        let review = review.to_owned();
        let group = group.to_owned();
        tauqe_core::blocking::run_blocking_async(move || {
            ReviewStorage::write_initial_group_horizon(&root, &review, &group, items, tier)
        })
        .await
        .map_err(|error| {
            InitialGroupHorizonError::Storage(anyhow::anyhow!(
                "Initial group horizon worker failed: {error}"
            ))
        })?
    }
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
