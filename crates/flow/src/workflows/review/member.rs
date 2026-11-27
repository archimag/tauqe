//! Interpret and settle the result of one review member.

use super::ReviewBatchExecutor;
use crate::application::usage_totals::UsageReceipt;
use crate::workflows::step::completion::{StepEditCompletion, StepEditDisposition};
use tauqe_core::edits::VerificationVerdict;
use tauqe_core::workflow::batch::BatchStepOutcome;
use tauqe_core::workflow::implementation::{ExecutionScope, StepTurnSettlement};

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
pub(crate) struct ReviewSequenceResult {
    pub executed_steps: Vec<StepTurnSettlement>,
    pub stop: ReviewSequenceStop,
    pub usage: Option<UsageReceipt>,
}

/// Internal disk facts are independent of the legacy completion projection.
pub(crate) struct ReviewSequenceFacts {
    pub completed: bool,
    pub applied: bool,
    pub commit: Option<String>,
    pub changed_files: Vec<String>,
    pub verification: Option<VerificationVerdict>,
}

impl ReviewSequenceResult {
    pub(crate) fn facts(&self) -> ReviewSequenceFacts {
        let edits: Vec<_> = self
            .executed_steps
            .iter()
            .filter_map(|step| step.edit.as_ref())
            .collect();
        let verification = if edits.is_empty() {
            None
        } else if edits
            .iter()
            .any(|edit| edit.verification == Some(VerificationVerdict::Failed))
        {
            Some(VerificationVerdict::Failed)
        } else if edits
            .iter()
            .any(|edit| edit.verification == Some(VerificationVerdict::Cancelled))
        {
            Some(VerificationVerdict::Cancelled)
        } else if edits.iter().any(|edit| edit.verification.is_none()) {
            None
        } else if edits
            .iter()
            .any(|edit| edit.verification == Some(VerificationVerdict::NotConfigured))
        {
            Some(VerificationVerdict::NotConfigured)
        } else {
            Some(VerificationVerdict::Passed)
        };
        ReviewSequenceFacts {
            completed: matches!(self.stop, ReviewSequenceStop::Completed),
            applied: edits.iter().any(|edit| edit.applied),
            commit: self
                .executed_steps
                .iter()
                .rev()
                .find_map(|step| step.confirmed_commit().map(str::to_owned)),
            changed_files: edits
                .into_iter()
                .flat_map(|edit| edit.changed_files.iter().cloned())
                .collect(),
            verification,
        }
    }

    pub(crate) fn edit_completion(&self) -> StepEditCompletion {
        let facts = self.facts();
        if facts.completed && !facts.applied {
            tracing::debug!("review sequence completed without disk application");
        }
        let mut completion = StepEditCompletion {
            disposition: StepEditDisposition::Completed,
            // Earlier leaves can remain committed when the rest of the member
            // stops. Keep their receipts without claiming the member completed.
            outcome: tauqe_core::edits::EditOutcome {
                changed_files: facts.changed_files,
                commit_hash: facts.commit,
                verification: facts.verification,
                ..Default::default()
            },
        };
        match &self.stop {
            ReviewSequenceStop::Completed => {
                // Preserve the historical wire flag even for a no-change run.
                completion.outcome.applied = true;
            }
            ReviewSequenceStop::Interrupted(
                ReviewStepStop::Blocked(reason) | ReviewStepStop::Exhausted(reason),
            ) => {
                completion.disposition = StepEditDisposition::Refused;
                completion.outcome.error = Some(reason.clone());
            }
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
