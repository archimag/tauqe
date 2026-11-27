//! Prepare an architectural group's stored implementation horizon.

use super::ReviewBatchExecutor;
use crate::application::usage_totals::{ReceiptAccumulator, UsageReceipt};
use crate::workflows::architect::{self, ArchitectScope, HorizonVerdict, PlanHorizonContour};
use tauqe_core::providers::ModelUsageInfo;
use tauqe_core::review::{
    prompt::build_review_group_refine_user_prompt, storage::InitialGroupHorizonError,
    ReviewGroupKind,
};
use tauqe_core::workflow::batch::BatchStepOutcome;

pub(super) enum PreparationFailure {
    Local(String),
    Fatal(String),
    Cancelled,
}

pub(super) async fn prepare_group(
    executor: &ReviewBatchExecutor<'_>,
    review_id: &str,
    group_id: &str,
) -> (Option<UsageReceipt>, Result<(), PreparationFailure>) {
    let session = match executor.documents.review(review_id).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return (
                None,
                Err(PreparationFailure::Fatal(format!(
                    "Review session '{review_id}' not found"
                ))),
            )
        }
        Err(error) => return (None, Err(PreparationFailure::Fatal(error))),
    };
    let Some(group) = session.groups.iter().find(|group| group.id == group_id) else {
        return (
            None,
            Err(PreparationFailure::Fatal(format!(
                "Review group '{group_id}' not found in session '{review_id}'"
            ))),
        );
    };
    if group.kind != ReviewGroupKind::Architectural || !group.implementation.items.is_empty() {
        return (None, Ok(()));
    }
    if executor.operation.hard_cancellation().is_cancelled() {
        return (None, Err(PreparationFailure::Cancelled));
    }
    let findings = session
        .items
        .iter()
        .filter(|item| item.group.as_deref() == Some(group_id))
        .cloned()
        .collect::<Vec<_>>();
    let task = build_review_group_refine_user_prompt(&session, group, &findings, None, None);
    let tier = tauqe_core::plan::prompt::resolve_architect_tier(
        executor.execution.config(),
        None,
        None,
        None,
        None,
    )
    .0;
    let id = format!("op-{}", crate::api::next_operation_id());
    architect::publish_architect_started(executor.observer.as_ref(), &id, &tier, true);
    let (verdict, summary) = architect::run_horizon_wave(
        &executor.step.execution,
        &executor.step.context,
        executor.operation,
        executor.execution,
        PlanHorizonContour {
            operation_id: id.clone(),
            task,
            context: executor.step.context.context_for_turn().await,
            scope: ArchitectScope::Review(review_id.to_owned()),
            tier,
            intro_header: format!(
                "# Review Group Horizon: [{review_id}:{group_id}] {}\n\n",
                group.title
            ),
        },
        executor.observer.clone(),
    )
    .await;
    architect::publish_horizon_terminal(executor.observer.as_ref(), &id, &verdict, &summary);
    let receipt = summary.receipt;
    let result = match verdict {
        HorizonVerdict::Synthesized { output, tier } => {
            if executor.operation.hard_cancellation().is_cancelled() {
                Err(PreparationFailure::Cancelled)
            } else {
                // An admitted blocking write must settle even if cancellation arrives.
                let saved = executor
                    .documents
                    .write_initial_group_horizon(review_id, group_id, output.items, tier)
                    .await;
                if saved.is_ok() {
                    executor
                        .documents
                        .publish_review_snapshot(review_id, executor.observer.as_ref())
                        .await;
                }
                if executor.operation.hard_cancellation().is_cancelled() {
                    Err(PreparationFailure::Cancelled)
                } else {
                    match saved {
                        Ok(_) => match super::resolve_group_frame(
                            executor.documents,
                            review_id,
                            group_id,
                        )
                        .await
                        {
                            Ok(frame) if !frame.plan.implementation.items.is_empty() => Ok(()),
                            Ok(_) => Err(PreparationFailure::Fatal(
                                "Saved review group horizon is empty".into(),
                            )),
                            Err(error) => Err(PreparationFailure::Fatal(error.into_reason())),
                        },
                        Err(
                            error @ (InitialGroupHorizonError::EmptyHorizon
                            | InitialGroupHorizonError::InvalidTree(_)),
                        ) => Err(PreparationFailure::Local(format!(
                            "Invalid synthesized review group horizon: {error}"
                        ))),
                        Err(error) => Err(PreparationFailure::Fatal(error.to_string())),
                    }
                }
            }
        }
        HorizonVerdict::Blocked { reason, .. } => Err(PreparationFailure::Local(reason)),
        HorizonVerdict::EmptyHorizon { .. } => Err(PreparationFailure::Local(
            "The Architect returned an empty review group horizon".into(),
        )),
        HorizonVerdict::InvalidTree { duplicate, .. } => Err(PreparationFailure::Local(format!(
            "Duplicate or empty review group step ID: '{duplicate}'"
        ))),
        HorizonVerdict::Aborted { report, .. } => {
            Err(PreparationFailure::Fatal(report.to_message()))
        }
        HorizonVerdict::Failed(reason) => Err(PreparationFailure::Fatal(reason)),
        HorizonVerdict::Cancelled => Err(PreparationFailure::Cancelled),
    };
    (receipt, result)
}

/// Project distinct accounting windows without charging the ledger again.
pub(super) fn with_receipts(
    outcome: BatchStepOutcome,
    architect: Option<&UsageReceipt>,
    coder: Option<&UsageReceipt>,
) -> BatchStepOutcome {
    if architect.is_none() && coder.is_none() {
        return outcome;
    }
    let mut receipts = ReceiptAccumulator::default();
    for receipt in architect.into_iter().chain(coder) {
        receipts.include(receipt);
    }
    let usage = receipts.usage();
    let optional = |field: fn(&ModelUsageInfo) -> Option<u32>| {
        let values = architect
            .into_iter()
            .chain(coder)
            .map(|receipt| field(&receipt.batch_usage()))
            .collect::<Vec<_>>();
        values
            .iter()
            .any(Option::is_some)
            .then(|| values.into_iter().flatten().fold(0u32, u32::saturating_add))
    };
    outcome.with_usage(ModelUsageInfo {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
        reasoning_tokens: optional(|value| value.reasoning_tokens),
        cached_tokens: optional(|value| value.cached_tokens),
        cost: usage.cost,
    })
}
