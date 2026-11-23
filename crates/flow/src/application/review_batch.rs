//! Facade entry for review execution.

use crate::api::{Application, ApplicationObserver, ReviewBatchStarted, ReviewLaunchError};
#[cfg(feature = "test-support")]
use crate::api::{OperationGuard, ReviewBatchOutcome};
#[cfg(feature = "test-support")]
use crate::workflows::review::ReviewBatch;
use crate::workflows::review::ReviewResources;
use std::sync::Arc;
use tauqe_core::models::ModelSelection;
use tauqe_core::review::ReviewExecuteTarget;

impl Application {
    fn review_resources(&self) -> ReviewResources {
        ReviewResources {
            step: self.step_resources(),
            dialogue: self.dialogue_session().clone(),
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
        self.review_resources()
            .run_batch(
                ReviewBatch {
                    owner_id,
                    members: &tasks,
                    deferred,
                },
                self.configuration().await,
                model,
                &operation,
                observer,
            )
            .await
    }
}
