//! Facade entry for review execution.

use crate::api::{Application, ApplicationObserver, ReviewBatchStarted, ReviewLaunchError};
#[cfg(feature = "test-support")]
use crate::api::{OperationGuard, ReviewBatchOutcome};
use crate::workflows::review::refinement::{GroupRefineError, GroupRefineInput, GroupRefineResult};
#[cfg(feature = "test-support")]
use crate::workflows::review::ReviewBatch;
use crate::workflows::review::ReviewResources;
use std::sync::Arc;
use tauqe_core::models::ModelSelection;
use tauqe_core::review::ReviewExecuteTarget;

impl Application {
    pub async fn refine_review_group(
        &self,
        input: GroupRefineInput,
        observer: Arc<dyn ApplicationObserver>,
        exit_deadline: std::time::Duration,
    ) -> Result<GroupRefineResult, GroupRefineError> {
        self.review_resources()
            .refine_group(self.configuration_session(), input, observer, exit_deadline)
            .await
    }

    fn review_resources(&self) -> ReviewResources {
        ReviewResources {
            step: self.step_resources(),
        }
    }

    pub async fn execute_review(
        &self,
        target: ReviewExecuteTarget,
        model: Option<ModelSelection>,
        observer: Arc<dyn ApplicationObserver>,
    ) -> Result<ReviewBatchStarted, ReviewLaunchError> {
        self.review_resources()
            .launch_batch(
                self.configuration_session(),
                self.focus_session(),
                target,
                model,
                observer,
            )
            .await
    }

    #[cfg(feature = "test-support")]
    pub async fn test_run_review_batch(
        &self,
        owner_id: &str,
        tasks: Vec<String>,
        deferred: usize,
        model: Option<ModelSelection>,
        operation: Arc<OperationGuard>,
        observer: Arc<dyn ApplicationObserver>,
    ) -> ReviewBatchOutcome {
        let mut members = Vec::with_capacity(tasks.len());
        for id in &tasks {
            let Some(member) = crate::workflows::review::ReviewMember::from_batch_id(id) else {
                return ReviewBatchOutcome::Failed {
                    completed: 0,
                    member: crate::workflows::review::FailedMember::Unresolved {
                        member_id: id.clone(),
                    },
                    reason: format!("Invalid review step id: {id}"),
                    telemetry: crate::workflows::review::BatchTelemetry::default(),
                };
            };
            members.push(member);
        }
        self.review_resources()
            .run_batch(
                ReviewBatch::from_batch_ids(owner_id, &members, deferred),
                self.configuration().await,
                model,
                &operation,
                observer,
            )
            .await
    }
}
