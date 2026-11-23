//! Interpret and settle the result of one review member.

use super::ReviewBatchExecutor;
use crate::workflows::step::completion::{StepEditCompletion, StepEditDisposition};
use tauqe_core::edits::VerificationVerdict;
use tauqe_core::workflow::batch::BatchStepOutcome;
use tauqe_core::workflow::implementation::ExecutionScope;

#[derive(Debug)]
pub(crate) struct ReviewLeafReceipt {
    pub commit: Option<String>,
    pub modified_paths: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReviewStepStop {
    #[error("operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Blocked(String),
    #[error("{0}")]
    Exhausted(String),
    #[error("infrastructure error: {0}")]
    Failed(String),
    #[error("{0}")]
    Preparation(String),
    #[error("infrastructure error: {0}")]
    Storage(String),
}

#[derive(Debug)]
pub(crate) enum ReviewSequenceStop {
    Completed,
    Interrupted(ReviewStepStop),
}

/// Receipts survive a partial run even when it cannot publish a completed edit.
#[derive(Debug)]
pub(crate) struct ReviewSequenceResult {
    pub executed_steps: Vec<ReviewLeafReceipt>,
    pub stop: ReviewSequenceStop,
}

impl ReviewSequenceResult {
    pub(crate) fn edit_completion(&self) -> StepEditCompletion {
        let mut completion = StepEditCompletion {
            disposition: StepEditDisposition::Completed,
            outcome: Default::default(),
        };
        match &self.stop {
            ReviewSequenceStop::Completed => {
                completion.outcome.applied = true;
                completion.outcome.changed_files = self
                    .executed_steps
                    .iter()
                    .flat_map(|step| step.modified_paths.clone())
                    .collect();
                completion.outcome.commit_hash = self
                    .executed_steps
                    .iter()
                    .rev()
                    .find_map(|step| step.commit.clone());
                completion.outcome.verification = Some(VerificationVerdict::Passed);
            }
            ReviewSequenceStop::Interrupted(
                ReviewStepStop::Blocked(_) | ReviewStepStop::Exhausted(_),
            ) => {}
            ReviewSequenceStop::Interrupted(ReviewStepStop::Cancelled) => {
                completion.disposition = StepEditDisposition::Cancelled;
            }
            ReviewSequenceStop::Interrupted(error) => {
                completion.disposition = StepEditDisposition::Failed;
                completion.outcome.error = Some(error.to_string());
            }
        }
        completion
    }
}

impl ReviewBatchExecutor<'_> {
    pub(super) async fn settle_run(
        &mut self,
        scope: &ExecutionScope,
        report: ReviewSequenceResult,
    ) -> Result<BatchStepOutcome, String> {
        match report.stop {
            ReviewSequenceStop::Completed => self.fix_member(scope).await,
            ReviewSequenceStop::Interrupted(ReviewStepStop::Cancelled) => {
                Ok(BatchStepOutcome::cancelled())
            }
            // A local obstacle does not stop unrelated queue members.
            ReviewSequenceStop::Interrupted(
                ReviewStepStop::Blocked(reason) | ReviewStepStop::Exhausted(reason),
            ) => self.skip_member(scope, &reason, false).await,
            ReviewSequenceStop::Interrupted(error) => {
                self.skip_member(scope, &error.to_string(), true).await
            }
        }
    }
}
