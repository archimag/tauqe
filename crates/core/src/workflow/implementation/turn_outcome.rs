//! Classify Architect refinement for the implementation supervisor.

use crate::plan::prompt::{StructuredRefineOutcome, StructuredRefineStatus};
use crate::workflow::implementation::RefinementResult;

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
    use crate::models::{ArchitectTier, ModelRef, ReasoningEffort};
    use crate::plan::prompt::{AbortReport, PlanRefinementOutput};
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
