//! Terminal edit facts of a single or sequential bounded step execution.
use crate::application::step_dispatch::StepLoopReport;
use tauqe_core::control::outcome::LoopResult;
use tauqe_core::edits::{EditOutcome, VerificationVerdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEditDisposition {
    /// The run ended; this does not imply a committed or applied change.
    Completed,
    Refused,
    Cancelled,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepEditCompletion {
    pub disposition: StepEditDisposition,
    pub outcome: EditOutcome,
}

pub fn classify_step_edit_completion(report: &StepLoopReport) -> StepEditCompletion {
    let mut completion = StepEditCompletion {
        disposition: StepEditDisposition::Completed,
        outcome: EditOutcome::default(),
    };
    match report {
        StepLoopReport::Turn(LoopResult::Success(success)) => {
            let artifact = &success.value;
            completion.outcome.changed_files = artifact.modified_paths.clone();
            completion.outcome.verification = artifact.verification;
            if let Some(error) = &artifact.commit_error {
                completion.disposition = StepEditDisposition::Refused;
                completion.outcome.error = Some(error.clone());
            } else if artifact.commit_hash.is_some() {
                completion.outcome.applied = true;
                completion.outcome.commit_hash = artifact.commit_hash.clone();
            }
        }
        StepLoopReport::Turn(LoopResult::Exhausted(_)) => {
            completion.disposition = StepEditDisposition::Refused
        }
        StepLoopReport::Cancelled => completion.disposition = StepEditDisposition::Cancelled,
        StepLoopReport::Failed(reason) => {
            completion.disposition = StepEditDisposition::Failed;
            completion.outcome.error = Some(reason.clone());
        }
        StepLoopReport::Sequence(LoopResult::Success(sequence)) if sequence.value.is_success() => {
            completion.outcome.applied = true;
            completion.outcome.changed_files = sequence
                .value
                .executed_steps
                .iter()
                .flat_map(|step| step.modified_paths.clone())
                .collect();
            completion.outcome.commit_hash = sequence
                .value
                .executed_steps
                .iter()
                .rev()
                .find_map(|step| step.commit.clone());
            // Preserve the established aggregate verdict for a completed sequence.
            completion.outcome.verification = Some(VerificationVerdict::Passed);
        }
        StepLoopReport::Turn(LoopResult::Diverged(_)) | StepLoopReport::Sequence(_) => {}
    }
    completion
}
