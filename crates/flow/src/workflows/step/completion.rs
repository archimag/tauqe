//! Terminal edit facts of an isolated bounded step execution.
use super::run::{StepAttemptReport, StepLoopReport};
use crate::workflows::turn::StepRecovery;
use crate::workflows::turn::{RoundError, TurnOutcome};
use tauqe_core::edits::{EditOutcome, VerificationVerdict};
use tauqe_core::execution::ResponseEnvelope;
use tauqe_core::llm::{BlockedCommand, LlmCapability, LlmCommand};
use tauqe_core::workflow::implementation::{StepFailure, StepTurnResult, StepTurnSettlement};

const ROUND_BUDGET_EXHAUSTED: &str = "Turn round budget exhausted";

pub(crate) fn step_response(report: &StepAttemptReport) -> Option<&ResponseEnvelope> {
    match &report.outcome {
        Ok(TurnOutcome::Settled(artifact)) => Some(&artifact.response),
        Ok(TurnOutcome::ProtocolDefect(response)) => Some(response),
        _ => None,
    }
}

/// The model answer and final policy settlement of one coder attempt travel together.
pub(crate) struct CoderAttempt {
    pub settlement: StepTurnSettlement,
    pub answer: String,
}

pub(crate) fn step_answer(report: &StepAttemptReport) -> String {
    step_response(report)
        .and_then(|response| {
            response
                .commands()
                .iter()
                .rev()
                .find_map(|command| match command {
                    LlmCommand::Text(message) => Some(message.content.clone()),
                    _ => None,
                })
        })
        .unwrap_or_default()
}

fn format_step_blocker(blocked: &BlockedCommand) -> String {
    let reason = blocked.reason().trim();
    let details = blocked.details().trim();
    if details.is_empty() {
        reason.into()
    } else {
        format!("{reason}\n\n{details}")
    }
}

/// Interpret the attempt without letting the verdict discard its disk receipt.
pub(crate) fn classify_step_attempt(report: &StepAttemptReport) -> StepTurnSettlement {
    let mut settlement = StepTurnSettlement {
        result: classify_step_result(report),
        edit: report.edit.clone(),
    };
    let commit = settlement.confirmed_commit().map(str::to_owned);
    if let StepTurnResult::Success { commit: saved, .. } = &mut settlement.result {
        *saved = commit;
    }
    settlement
}

pub(crate) fn step_status_error(error: &str, commit: Option<&str>) -> String {
    match commit {
        Some(hash) => {
            format!("Commit {hash} was created, but the step status could not be saved: {error}")
        }
        None => error.to_owned(),
    }
}

fn classify_step_result(report: &StepAttemptReport) -> StepTurnResult {
    if report.cancelled {
        return StepTurnResult::Cancelled;
    }
    if report.recovery.unconfirmed() {
        let primary = match &report.outcome {
            Err(error) => Some(error.to_string()),
            Ok(TurnOutcome::Exhausted { reason }) => Some(reason.clone()),
            Ok(TurnOutcome::ProtocolDefect(_)) => {
                Some("The step turn diverged without a completed change set".into())
            }
            _ => report.edit.as_ref().and_then(|edit| edit.error.clone()),
        };
        let diagnostic = report
            .recovery
            .diagnostic()
            .unwrap_or_else(|| "The file transaction did not confirm recovery".into());
        return StepTurnResult::Failed(StepFailure::unreverted(match primary {
            Some(primary) => format!("{primary}\n\nRecovery: {diagnostic}"),
            None => diagnostic,
        }));
    }
    if matches!(report.recovery, StepRecovery::FinalizationAttempted) && report.edit.is_none() {
        return StepTurnResult::Failed(StepFailure::unreverted(
            "Finalization was attempted without a disk receipt",
        ));
    }
    if let Some(error) = report.edit.as_ref().and_then(|edit| edit.error.as_ref()) {
        return StepTurnResult::Failed(StepFailure::infrastructure(format!(
            "Failed to commit the change set of the step: {error}"
        )));
    }
    let domain = |reason: &str| StepTurnResult::Failed(StepFailure::domain(reason));
    let divergence = || domain("The step turn diverged without a completed change set");
    let response = match &report.outcome {
        Ok(TurnOutcome::Settled(artifact)) => Some(&artifact.response),
        Ok(TurnOutcome::ProtocolDefect(_)) => return divergence(),
        Ok(TurnOutcome::Exhausted { reason }) => return domain(reason),
        Err(RoundError::BudgetExhausted { .. }) => return domain(ROUND_BUDGET_EXHAUSTED),
        Err(error) if report.edit.is_none() => {
            return StepTurnResult::Failed(StepFailure::infrastructure(error.to_string()));
        }
        // The task may have lost its response after the disk worker finalized.
        Err(_) => None,
    };
    let verified = match report.edit.as_ref().and_then(|edit| edit.verification) {
        Some(VerificationVerdict::Passed | VerificationVerdict::NotConfigured) => true,
        Some(_) => return domain("The change set of the step did not pass toolchain verification"),
        None => false,
    };
    if let Some(response) = response {
        for command in response.commands() {
            match command {
                LlmCommand::Blocked(blocked) => {
                    return StepTurnResult::Blocked(format_step_blocker(blocked));
                }
                LlmCommand::Abort(abort) => {
                    return StepTurnResult::Abort(format!(
                        "Execution aborted: {}\n\nRecommended action:\n{}",
                        abort.reason().trim(),
                        abort.recommendation().trim()
                    ))
                }
                LlmCommand::TurnError(error) => return domain(error.error_message()),
                _ => {}
            }
        }
    }
    if !verified
        && !response.is_some_and(|response| {
            response.has(LlmCapability::TurnSkip) || response.has(LlmCapability::CommitMessage)
        })
    {
        return divergence();
    }
    let text = response
        .into_iter()
        .flat_map(|response| response.commands())
        .filter_map(|command| match command {
            LlmCommand::Text(text) if !text.content.is_empty() => Some(text.content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let summary = if text.trim().is_empty() {
        report
            .edit
            .as_ref()
            .map(|edit| edit.changed_files.join(", "))
            .unwrap_or_default()
    } else {
        text
    };
    StepTurnResult::Success {
        summary,
        commit: None,
    }
}

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

pub(crate) fn classify_step_edit_completion(report: &StepLoopReport) -> StepEditCompletion {
    let mut completion = StepEditCompletion {
        disposition: StepEditDisposition::Completed,
        outcome: EditOutcome::default(),
    };
    match report {
        StepLoopReport::Turn(report) => {
            if let Some(edit) = &report.edit {
                completion.outcome = edit.clone();
                // The step publication historically reports a committed edit;
                // direct application remains visible in the disk receipt.
                completion.outcome.applied = edit.error.is_none() && edit.commit_hash.is_some();
                if edit.error.is_some() {
                    completion.outcome.commit_hash = None;
                }
            }
            completion.disposition = if report.cancelled {
                StepEditDisposition::Cancelled
            } else if report.recovery.unconfirmed()
                || matches!(report.recovery, StepRecovery::FinalizationAttempted)
                    && report.edit.is_none()
            {
                StepEditDisposition::Failed
            } else if completion.outcome.error.is_some()
                || matches!(
                    &report.outcome,
                    Ok(TurnOutcome::Exhausted { .. }) | Err(RoundError::BudgetExhausted { .. })
                )
            {
                StepEditDisposition::Refused
            } else if let Err(error) = &report.outcome {
                completion.outcome.error = Some(error.to_string());
                StepEditDisposition::Failed
            } else {
                StepEditDisposition::Completed
            };
            if let Some(error) = report.recovery.diagnostic() {
                completion.outcome.error = Some(match completion.outcome.error.take() {
                    Some(primary) => format!("{primary}\n\nRecovery: {error}"),
                    None => error,
                });
            }
        }
        StepLoopReport::Failed(reason) => {
            completion.disposition = StepEditDisposition::Failed;
            completion.outcome.error = Some(reason.clone());
        }
    }
    completion
}

#[cfg(test)]
mod tests;
