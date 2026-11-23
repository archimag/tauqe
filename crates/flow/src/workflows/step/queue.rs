//! Execute one scheduled queue of leaves without choosing its successor.

use super::attempts::{LeafExecutor, LeafOutcome};
use super::completion::step_status_error;
use async_trait::async_trait;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::batch::{
    BatchExecution, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStepStatus, BatchStop,
};
use tauqe_core::workflow::implementation::{StepFailure, StepTurnResult, StepTurnSettlement};
use tokio::sync::{mpsc, watch};

pub(crate) enum QueueStop {
    Completed,
    Cancelled,
    Stopped,
    Blocked {
        step_id: String,
        reason: String,
    },
    Failed {
        step_id: String,
        outcome: Box<LeafOutcome>,
    },
}

pub(crate) struct QueueReport {
    pub stop: QueueStop,
    pub scheduled: usize,
    pub executed: usize,
    pub succeeded: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Settlements of completed leaves, in execution order. A cancelled leaf
    /// is included only if its confirmed commit was recorded as Done.
    pub completed: Vec<(String, StepTurnSettlement)>,
}

pub(crate) async fn run_queue(
    leaf: LeafExecutor<'_>,
    stream_tx: &mpsc::Sender<StreamEvent>,
    cancel_rx: &watch::Receiver<bool>,
    soft_stop_rx: &watch::Receiver<bool>,
) -> QueueReport {
    let steps: Vec<_> = leaf
        .config
        .queue
        .iter()
        .map(|id| BatchStep::new(id.clone(), format!("Execute plan step {id}")))
        .collect();
    let mut executor = QueueExecutor {
        leaf,
        terminal: None,
        completed: Vec::new(),
    };
    let batch = BatchExecution::new()
        .run(&steps, &mut executor, stream_tx, cancel_rx, soft_stop_rx)
        .await;
    let stop = match batch.stop {
        BatchStop::Completed => QueueStop::Completed,
        BatchStop::Cancelled => QueueStop::Cancelled,
        BatchStop::Stopped => QueueStop::Stopped,
        BatchStop::Failed { step_id } | BatchStop::Blocked { step_id, .. } => {
            executor.terminal.unwrap_or_else(|| QueueStop::Failed {
                step_id,
                outcome: Box::new(LeafOutcome::Infrastructure(StepFailure::infrastructure(
                    "The batch stopped without a typed leaf outcome",
                ))),
            })
        }
    };
    let mut report = QueueReport {
        stop,
        scheduled: batch.scheduled,
        executed: batch.executed.len(),
        succeeded: 0,
        skipped: 0,
        failed: 0,
        completed: executor.completed,
    };
    for entry in batch.executed {
        match entry.outcome.status {
            BatchStepStatus::Succeeded => report.succeeded += 1,
            BatchStepStatus::Skipped => report.skipped += 1,
            BatchStepStatus::Failed | BatchStepStatus::Blocked | BatchStepStatus::Cancelled => {
                report.failed += 1;
            }
        }
    }
    report
}

struct QueueExecutor<'a> {
    leaf: LeafExecutor<'a>,
    completed: Vec<(String, StepTurnSettlement)>,
    // BatchExecution stops at the first failure or block. Keep the originating
    // outcome here so its owner never has to recover it from batch diagnostics.
    terminal: Option<QueueStop>,
}

#[async_trait]
impl BatchStepExecutor for QueueExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        let outcome = self.leaf.run(&step.id, stream_tx, cancel_rx).await;
        let batch_outcome = match outcome {
            LeafOutcome::AlreadyDone => BatchStepOutcome::succeeded(),
            LeafOutcome::Completed(receipt) => {
                self.completed.push((step.id.clone(), receipt));
                BatchStepOutcome::succeeded()
            }
            LeafOutcome::Cancelled(receipt) => {
                if let Some(receipt) =
                    receipt.filter(|receipt| receipt.confirmed_commit().is_some())
                {
                    self.completed.push((step.id.clone(), receipt));
                }
                BatchStepOutcome::cancelled()
            }
            LeafOutcome::Blocked(reason, _) => {
                let batch_outcome = BatchStepOutcome::blocked(reason.clone());
                self.terminal = Some(QueueStop::Blocked {
                    step_id: step.id.clone(),
                    reason,
                });
                batch_outcome
            }
            outcome => {
                let batch_outcome = BatchStepOutcome::failed(failure_reason(&outcome));
                self.terminal = Some(QueueStop::Failed {
                    step_id: step.id.clone(),
                    outcome: Box::new(outcome),
                });
                batch_outcome
            }
        };
        Ok(batch_outcome)
    }
}

pub(crate) fn failure_reason(outcome: &LeafOutcome) -> String {
    match outcome {
        LeafOutcome::Refused(reason)
        | LeafOutcome::Exhausted(reason, _)
        | LeafOutcome::Aborted(reason) => reason.clone(),
        LeafOutcome::Infrastructure(failure) | LeafOutcome::Unreverted(failure) => {
            failure.reason().to_string()
        }
        LeafOutcome::TransitionFailed { failure, attempt } => {
            let commit = attempt.as_ref().and_then(|attempt| {
                if let StepTurnResult::Success { commit, .. } = &attempt.result {
                    commit.as_deref()
                } else {
                    attempt.confirmed_commit()
                }
            });
            step_status_error(failure.reason(), commit)
        }
        LeafOutcome::AlreadyDone
        | LeafOutcome::Completed(_)
        | LeafOutcome::Cancelled(_)
        | LeafOutcome::Blocked(..) => "Expected a failed leaf outcome".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::step::attempts::{ImplementationRunConfig, LeafOperations};
    use tauqe_core::plan::{Plan, PlanImplementation, PlanItem};
    use tauqe_core::workflow::implementation::{
        ImplementationFrame, StepTurnRequest, StepTurnSettlement,
    };
    use tauqe_core::workflow::EditProtocolKind;

    struct RefuseCompletion(Plan);

    #[async_trait]
    impl LeafOperations for RefuseCompletion {
        async fn read(&self) -> Result<ImplementationFrame, StepFailure> {
            Ok(ImplementationFrame::from_plan(self.0.clone(), None))
        }

        fn session_id(&self) -> String {
            "test-queue".into()
        }

        async fn begin(&self, step_id: &str) -> Result<Option<PlanImplementation>, StepFailure> {
            assert_eq!(step_id, "1");
            Ok(None)
        }

        async fn finish(&self, step_id: &str, outcome: &LeafOutcome) -> Result<(), StepFailure> {
            assert_eq!(step_id, "1");
            assert!(matches!(outcome, LeafOutcome::Completed(receipt)
                if matches!(&receipt.result, StepTurnResult::Success { commit, .. }
                    if commit.as_deref() == Some("abc1234"))));
            Err(StepFailure::infrastructure("completion write refused"))
        }

        async fn attempt(&self, request: StepTurnRequest) -> (StepTurnSettlement, String) {
            assert_eq!(request.queue.active_index, 0);
            (
                StepTurnResult::Success {
                    summary: "verified".into(),
                    commit: Some("abc1234".into()),
                }
                .into(),
                String::new(),
            )
        }
    }

    #[tokio::test]
    async fn completion_write_failure_retains_commit_and_stops_the_queue() {
        let plan = Plan::from_parts(
            "test-plan".into(),
            "Queue test".into(),
            None,
            0,
            0,
            Vec::new(),
            ["1", "2"]
                .into_iter()
                .map(|id| PlanItem {
                    id: id.into(),
                    title: format!("Step {id}"),
                    files: vec![format!("{id}.rs")],
                    ..Default::default()
                })
                .collect(),
        );
        let operations = RefuseCompletion(plan);
        let config = ImplementationRunConfig {
            models: Default::default(),
            protocol: EditProtocolKind::Structured,
            marker: None,
            selection: None,
            instructions: None,
            queue: vec!["1".into(), "2".into()],
        };
        let (stream_tx, _stream_rx) = mpsc::channel(1);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (_stop_tx, stop_rx) = watch::channel(false);
        let report = run_queue(
            LeafExecutor {
                operations: &operations,
                config: &config,
            },
            &stream_tx,
            &cancel_rx,
            &stop_rx,
        )
        .await;

        assert_eq!(report.scheduled, 2);
        assert_eq!(report.executed, 1);
        assert_eq!(report.succeeded, 0);
        assert_eq!(report.failed, 1);
        let QueueStop::Failed { step_id, outcome } = report.stop else {
            panic!("expected a terminal leaf failure");
        };
        assert_eq!(step_id, "1");
        assert!(matches!(
            outcome.as_ref(),
            LeafOutcome::TransitionFailed {
                attempt: Some(_),
                ..
            }
        ));
        let diagnostic = failure_reason(&outcome);
        assert!(diagnostic.contains("completion write refused"));
        assert!(diagnostic.contains("abc1234"));
    }
}
