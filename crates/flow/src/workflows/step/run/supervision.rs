//! Adopt the context and terminal file facts of an isolated technical turn.

use super::{StepAttemptReport, StepLoopReport, StepResources};
use crate::api::OperationGuard;
use crate::workflows::step::StepAttempt;
use crate::workflows::turn::{StepRecovery, TurnOutcome};
use tauqe_core::blocking::run_blocking;
use tauqe_core::edits::{EditOutcome, VerificationVerdict};
use tauqe_core::execution::EventSink;
use tauqe_core::execution::ExecutionError;
use tauqe_core::workflow::policy::owned::Settlement;
use tauqe_core::workflow::policy::ApplyPolicy;

impl StepResources {
    pub(super) async fn supervise_step_attempt(
        &self,
        operation: &OperationGuard,
        operation_id: &str,
        events: &EventSink,
        attempt: StepAttempt,
        policy: Box<dyn ApplyPolicy>,
    ) -> StepLoopReport {
        // The shared supervisor owns the computation and restoration. Only the
        // computation's handle is installed in the operation slot.
        let report = attempt.run(operation, policy).await;
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!(operation_id, %error, "step technical supervisor failed");
                return StepLoopReport::Failed(error);
            }
        };
        let mut context = report.context;
        let mut outcome = report.outcome;
        let (edit, recovery) = match report.settlement {
            Settlement::Finalized { edit, .. } => (Some(edit), StepRecovery::FinalizationAttempted),
            Settlement::NoChanges { .. } => {
                let verification = match &outcome {
                    Ok(TurnOutcome::Settled(artifact)) => {
                        artifact.edit.as_ref().and_then(|edit| edit.verification)
                    }
                    _ => None,
                };
                (
                    Some(EditOutcome {
                        applied: true,
                        commit_hash: None,
                        error: None,
                        changed_files: Vec::new(),
                        verification: Some(
                            verification.unwrap_or(VerificationVerdict::NotConfigured),
                        ),
                    }),
                    StepRecovery::FinalizationAttempted,
                )
            }
            Settlement::RolledBack(rollback) => (
                None,
                StepRecovery::RolledBack(std::sync::Arc::new(rollback)),
            ),
        };
        if matches!(recovery, StepRecovery::FinalizationAttempted) && edit.is_some() {
            if let Ok(TurnOutcome::Settled(artifact)) = &mut outcome {
                artifact.edit = edit.clone();
            }
        }
        let cancelled = report.cancelled
            || operation.hard_cancellation().is_cancelled()
            || matches!(
                &outcome,
                Err(crate::workflows::turn::RoundError::Execution(
                    ExecutionError::Cancelled
                ))
            );
        if !report.observed {
            if edit
                .as_ref()
                .is_some_and(|edit| edit.applied && !edit.changed_files.is_empty())
            {
                run_blocking(|| context.clear_transient());
            }
            if let Err(error) = self.context.merge_turn_context(context).await {
                tracing::warn!(%error, "Failed to merge the step context");
            }
        }
        if let Some(error) = recovery.diagnostic() {
            events.emit_text_delta(format!("\n\n{error}\n\n"));
        }
        StepLoopReport::Turn(Box::new(StepAttemptReport {
            outcome,
            edit,
            recovery,
            cancelled,
        }))
    }
}
