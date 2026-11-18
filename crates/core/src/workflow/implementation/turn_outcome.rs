//! Classify a bounded turn for the implementation supervisor.

use crate::control::outcome::{LoopResult, ResponseEnvelope};
use crate::control::refine::{StructuredRefineOutcome, StructuredRefineStatus};
use crate::control::turn::TurnArtifact;
use crate::edits::VerificationVerdict;
use crate::llm::{LlmCapability, LlmCommand};
use crate::workflow::implementation::{RefinementResult, StepFailure, StepTurnResult};

/// Classifies the terminal result of a control-loop step turn.
///
/// The category decides the pipeline behavior: a blocked or exhausted turn is a
/// domain verdict the step loop may retry on a stronger coder tier, while a
/// broken transport or a change set the harness could not commit is fatal to the
/// whole run.
pub fn classify_step_turn(result: LoopResult<TurnArtifact>) -> StepTurnResult {
    match result {
        LoopResult::Success(success) => {
            if let Some(err) = success.value.commit_error.clone() {
                return StepTurnResult::Failed(StepFailure::infrastructure(format!(
                    "Failed to commit the change set of the step: {err}"
                )));
            }
            if let Some(reason) = blocked_reason(&success.response) {
                return StepTurnResult::Blocked(reason);
            }
            if success.response.has(LlmCapability::Abort) {
                return StepTurnResult::Abort(abort_message(&success.response));
            }
            let summary = envelope_text(&success.response)
                .unwrap_or_else(|| success.value.modified_paths.join(", "));
            let verdict = success.value.verification;
            // A repository that declares no verification command did not fail its
            // verification: the harness simply had nothing to run, and the Git
            // transaction accepts the commit exactly as it does for a passed run.
            let verified = match &verdict {
                Some(VerificationVerdict::Passed) | Some(VerificationVerdict::NotConfigured) => {
                    true
                }
                Some(_) => false,
                None => declares_success(&success.response),
            };
            if verified {
                StepTurnResult::Success {
                    summary,
                    commit: success.value.commit_hash,
                }
            } else if verdict.is_some() {
                StepTurnResult::Failed(StepFailure::domain(
                    "The change set of the step did not pass toolchain verification",
                ))
            } else {
                tracing::warn!(
                    remaining = ?success.response.remaining_capabilities(),
                    commands = ?success.response.commands(),
                    "step turn produced neither a verified change set nor a declared outcome"
                );
                StepTurnResult::Failed(StepFailure::domain(
                    "The step turn produced neither a verified change set nor a declared outcome",
                ))
            }
        }
        LoopResult::Diverged(envelope) => {
            if let Some(reason) = blocked_reason(&envelope) {
                return StepTurnResult::Blocked(reason);
            }
            if envelope.has(LlmCapability::Abort) {
                return StepTurnResult::Abort(abort_message(&envelope));
            }
            StepTurnResult::Failed(StepFailure::domain(
                "The step turn diverged without a completed change set",
            ))
        }
        LoopResult::Exhausted(report) => StepTurnResult::Failed(StepFailure::domain(report.reason)),
    }
}

/// Renders the conversational text a turn produced, if any.
fn envelope_text(envelope: &ResponseEnvelope) -> Option<String> {
    let mut envelope = envelope.clone();
    let mut combined = String::new();
    for command in envelope.take(LlmCapability::TextMessage) {
        if let LlmCommand::Text(command) = command {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&command.content);
        }
    }
    if combined.trim().is_empty() {
        None
    } else {
        Some(combined)
    }
}

/// Renders the unconditional abort a turn declared, together with the host action
/// it requires.
///
/// An abort is a host-environment verdict rather than a content failure, so the
/// supervisor reports the exact instruction the developer must perform instead of
/// a summary of the turn.
fn abort_message(envelope: &ResponseEnvelope) -> String {
    let mut envelope = envelope.clone();
    envelope
        .take(LlmCapability::Abort)
        .into_iter()
        .find_map(|command| match command {
            LlmCommand::Abort(command) => Some(
                crate::control::outcome::AbortReport {
                    reason: command.reason,
                    recommendation: command.recommendation,
                }
                .to_message(),
            ),
            _ => None,
        })
        .unwrap_or_else(|| "The step declared an unconditional abort.".to_string())
}

/// Whether a turn declared a terminal success outcome.
fn declares_success(envelope: &ResponseEnvelope) -> bool {
    let mut envelope = envelope.clone();
    envelope
        .take_any(&[
            LlmCapability::CommitMessage,
            LlmCapability::TurnError,
            LlmCapability::TurnSkip,
        ])
        .into_iter()
        .any(|command| {
            matches!(
                command,
                LlmCommand::CommitMessage(_) | LlmCommand::TurnSkip(_)
            )
        })
}

/// Extracts the semantic block a turn declared, if any.
///
/// The block is the verdict of the model about the approved decision, so every
/// contour that classifies a turn reads it from the decoded command instead of
/// inspecting the wording it produced.
fn blocked_reason(envelope: &ResponseEnvelope) -> Option<String> {
    let mut envelope = envelope.clone();
    envelope
        .take(LlmCapability::Blocked)
        .into_iter()
        .find_map(|command| match command {
            LlmCommand::Blocked(command) => {
                let details = command.details.trim();
                Some(if details.is_empty() {
                    command.reason.trim().to_string()
                } else {
                    format!("{}\n\n{}", command.reason.trim(), details)
                })
            }
            _ => None,
        })
}

/// Map the structured synthesis verdict into the supervising workflow result.
pub fn classify_refinement(outcome: StructuredRefineOutcome) -> RefinementResult {
    let StructuredRefineOutcome {
        output,
        tier: synthesized_tier,
        status,
    } = outcome;
    match status {
        StructuredRefineStatus::Synthesized => match output {
            Some(output) if !output.items.is_empty() => RefinementResult::Synthesized {
                items: output.items,
                tier: synthesized_tier,
            },
            _ => RefinementResult::Blocked {
                reason: "The Architect reported a synthesized horizon without any step".to_string(),
                tier: synthesized_tier,
            },
        },
        StructuredRefineStatus::Blocked { reason } => RefinementResult::Blocked {
            reason,
            tier: synthesized_tier,
        },
        StructuredRefineStatus::BudgetExceeded { diagnostic } => RefinementResult::Blocked {
            reason: diagnostic,
            tier: synthesized_tier,
        },
        // An abort is a diagnosis of the same weight as a blocker: the plan
        // or the group records why the horizon could not be reprojected, and
        // the message carries the host action the developer must perform
        // before the wave can run again.
        StructuredRefineStatus::Aborted(report) => RefinementResult::Blocked {
            reason: report.to_message(),
            tier: synthesized_tier,
        },
        StructuredRefineStatus::Cancelled => RefinementResult::Blocked {
            reason: "The Architect wave was cancelled".to_string(),
            tier: synthesized_tier,
        },
    }
}

#[cfg(test)]
mod refinement_tests {
    use super::*;
    use crate::control::outcome::AbortReport;
    use crate::models::{ArchitectTier, ModelRef, ReasoningEffort};
    use crate::plan::prompt::PlanRefinementOutput;
    use crate::plan::PlanItem;

    fn tier() -> ArchitectTier {
        let mut tier = ArchitectTier::new(
            ModelRef::openrouter("architect/final"),
            Some(ReasoningEffort::High),
        );
        tier.attempts = Some(4);
        tier.level_up = Some(false);
        tier
    }

    fn output(items: Vec<PlanItem>) -> PlanRefinementOutput {
        serde_json::from_value(serde_json::json!({ "status": "success", "explanation": "Prepared horizon", "items": items })).unwrap()
    }

    #[test]
    fn synthesized_refinement_requires_steps_and_keeps_the_final_tier() {
        let item = PlanItem {
            id: "1".into(),
            title: "Step".into(),
            files: vec!["client.rs".into()],
            ..Default::default()
        };
        assert_eq!(
            classify_refinement(StructuredRefineOutcome {
                output: Some(output(vec![item.clone()])),
                tier: tier(),
                status: StructuredRefineStatus::Synthesized
            }),
            RefinementResult::Synthesized {
                items: vec![item],
                tier: tier()
            }
        );
        for payload in [None, Some(output(Vec::new()))] {
            assert_eq!(
                classify_refinement(StructuredRefineOutcome {
                    output: payload,
                    tier: tier(),
                    status: StructuredRefineStatus::Synthesized
                }),
                RefinementResult::Blocked {
                    reason: "The Architect reported a synthesized horizon without any step".into(),
                    tier: tier()
                }
            );
        }
    }

    #[test]
    fn unsuccessful_refinement_statuses_keep_their_diagnostics_and_final_tier() {
        let cases = [
            (
                StructuredRefineStatus::Blocked {
                    reason: "scope blocked".into(),
                },
                "scope blocked",
            ),
            (
                StructuredRefineStatus::BudgetExceeded {
                    diagnostic: "context budget exceeded".into(),
                },
                "context budget exceeded",
            ),
            (
                StructuredRefineStatus::Aborted(AbortReport {
                    reason: " tool missing ".into(),
                    recommendation: " install tool ".into(),
                }),
                "Execution aborted: tool missing\n\nRecommended action:\ninstall tool",
            ),
            (
                StructuredRefineStatus::Cancelled,
                "The Architect wave was cancelled",
            ),
        ];
        for (status, reason) in cases {
            assert_eq!(
                classify_refinement(StructuredRefineOutcome {
                    output: Some(output(Vec::new())),
                    tier: tier(),
                    status
                }),
                RefinementResult::Blocked {
                    reason: reason.into(),
                    tier: tier()
                }
            );
        }
    }
}
