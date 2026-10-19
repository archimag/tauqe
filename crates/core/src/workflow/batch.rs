//! Batch execution mode: supervises an ordered queue of bounded turns.
//!
//! A batch groups several steps (plan positions, review findings, or any other
//! sequence of bounded turns) under a single supervisor. The supervisor owns
//! the invariants that must hold across the whole queue:
//!
//! - ordered, fail-fast traversal: the first failing or cancelled step ends the
//!   run, leaving later steps untouched;
//! - end-to-end usage accounting: token and cost totals are accumulated over
//!   every executed step so callers can report one figure per batch;
//! - interruption handling: a cancellation request observed between steps stops
//!   the queue, and a step that cancels itself is recorded as such.
//!
//! The supervisor never touches Git, the toolchain, or entity storage. Each
//! step is delegated to a [`BatchStepExecutor`], which runs one bounded turn and
//! reports its outcome. Callers that maintain plan or review statuses do so
//! inside the executor, keeping the batch free of transport and storage
//! concerns while still returning a single aggregate report.

use async_trait::async_trait;
use tauqe_protocol::ModelUsageInfo;
use tokio::sync::{mpsc, watch};

use crate::providers::StreamEvent;

/// A single unit of work scheduled inside a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchStep {
    /// Stable identifier used for diagnostics and status synchronization.
    pub id: String,
    /// Prompt that drives the bounded turn for this step.
    pub prompt: String,
}

impl BatchStep {
    /// Creates a batch step from its identifier and driving prompt.
    pub fn new(id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            prompt: prompt.into(),
        }
    }
}

/// Terminal status of a single batch step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStepStatus {
    /// The step satisfied its success contract.
    Succeeded,
    /// The step failed; the supervisor stopped at it.
    Failed,
    /// The step cannot proceed because its preconditions cannot be satisfied.
    Blocked,
    /// The step observed a cancellation request.
    Cancelled,
}

/// Outcome reported by a [`BatchStepExecutor`] for a single step.
#[derive(Debug, Clone)]
pub struct BatchStepOutcome {
    /// Terminal status of the step.
    pub status: BatchStepStatus,
    /// Token usage reported for the step, when the transport provides it.
    pub usage: Option<ModelUsageInfo>,
    /// Failure diagnostic reported for a failed step, when present.
    pub error: Option<String>,
    /// Block reason reported for a blocked step, when present.
    pub blocked_reason: Option<String>,
}

impl BatchStepOutcome {
    /// Reports a successful step without usage information.
    pub fn succeeded() -> Self {
        Self {
            status: BatchStepStatus::Succeeded,
            usage: None,
            error: None,
            blocked_reason: None,
        }
    }

    /// Reports a failed step carrying a failure diagnostic.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            status: BatchStepStatus::Failed,
            usage: None,
            error: Some(error.into()),
            blocked_reason: None,
        }
    }

    /// Reports a step that cannot proceed, carrying the coder's block report.
    pub fn blocked(reason: impl Into<String>) -> Self {
        Self {
            status: BatchStepStatus::Blocked,
            usage: None,
            error: None,
            blocked_reason: Some(reason.into()),
        }
    }

    /// Reports a step interrupted by a cancellation request.
    pub fn cancelled() -> Self {
        Self {
            status: BatchStepStatus::Cancelled,
            usage: None,
            error: None,
            blocked_reason: None,
        }
    }

    /// Attaches token usage to the outcome, returning the updated value.
    pub fn with_usage(mut self, usage: ModelUsageInfo) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Whether the outcome represents a successful step.
    pub fn is_success(&self) -> bool {
        matches!(self.status, BatchStepStatus::Succeeded)
    }
}

/// Report for a single executed step, pairing the step identity with its outcome.
#[derive(Debug, Clone)]
pub struct BatchStepReport {
    /// Identifier of the executed step.
    pub id: String,
    /// Outcome reported by the executor.
    pub outcome: BatchStepOutcome,
}

/// Aggregated token and cost usage for an entire batch run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatchUsage {
    /// Sum of prompt tokens across executed steps.
    pub prompt_tokens: u32,
    /// Sum of completion tokens across executed steps.
    pub completion_tokens: u32,
    /// Sum of total tokens across executed steps.
    pub total_tokens: u32,
    /// Sum of reported cost across executed steps.
    ///
    /// The total is known (`Some`) only while every executed step reports a
    /// cost. As soon as one step omits it, the total becomes `None` so callers
    /// can distinguish "cost unknown" from "cost is exactly zero".
    pub cost: Option<f64>,
}

impl Default for BatchUsage {
    fn default() -> Self {
        Self {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cost: Some(0.0),
        }
    }
}

impl BatchUsage {
    /// Folds a single step's usage into the running totals.
    ///
    /// Token counters saturate. The cost total stays known only while every
    /// accumulated step reports a cost; once a step omits it, the total
    /// degrades to `None`.
    pub fn accumulate(&mut self, usage: &ModelUsageInfo) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(usage.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(usage.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(usage.total_tokens);
        self.cost = match (self.cost, usage.cost) {
            (Some(total), Some(cost)) => Some(total + cost),
            _ => None,
        };
    }
}

/// Reason a batch supervisor stopped processing steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchStop {
    /// Every scheduled step ran and succeeded.
    Completed,
    /// A step failed; the supervisor stopped at it (fail-fast).
    Failed { step_id: String },
    /// A step reported a semantic block; the supervisor stopped at it.
    Blocked { step_id: String, reason: String },
    /// A cancellation request ended the batch.
    Cancelled,
    /// A soft-stop request ended the batch gracefully after the current step.
    ///
    /// Unlike [`BatchStop::Cancelled`], a soft stop never interrupts the step
    /// that is already running: the current step finishes its turn, compiler
    /// verification, and commit, and only then does the queue stop before
    /// launching the next step.
    Stopped,
}

/// Aggregate result of a batch run.
#[derive(Debug, Clone)]
pub struct BatchReport {
    /// Reports for every step that ran, in execution order.
    pub executed: Vec<BatchStepReport>,
    /// Number of steps scheduled for the run.
    pub scheduled: usize,
    /// Reason the batch stopped.
    pub stop: BatchStop,
    /// Accumulated usage across every executed step.
    pub usage: BatchUsage,
}

impl BatchReport {
    /// Whether the batch ran every scheduled step successfully.
    pub fn is_success(&self) -> bool {
        matches!(self.stop, BatchStop::Completed)
    }
}

/// Executes a single bounded turn on behalf of a [`BatchExecution`].
///
/// Implementors run one step to completion and report whether it succeeded,
/// failed, or was cancelled. Status synchronization for plan or review entities
/// belongs here, so the batch supervisor stays agnostic of entity storage.
#[async_trait]
pub trait BatchStepExecutor: Send + Sync {
    /// Runs one step, streaming UI events through `stream_tx` and observing
    /// `cancel_rx` for interruption.
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome>;
}

/// Supervisor that drives an ordered queue of bounded turns.
#[derive(Debug, Clone, Default)]
pub struct BatchExecution;

impl BatchExecution {
    /// Creates a batch supervisor.
    pub fn new() -> Self {
        Self
    }

    /// Stable identifier used in diagnostics.
    pub fn name(&self) -> &'static str {
        "batch"
    }

    /// Runs every step in order, aggregating usage and stopping on the first
    /// failure, cancellation, or soft-stop request (server-side fail-fast).
    ///
    /// A cancellation request observed between steps ends the queue before the
    /// next step starts. A soft-stop request observed between steps ends the
    /// queue just as cleanly, but leaves the step that already ran untouched, so
    /// its turn, verification, and commit all stand. Hard cancellation takes
    /// priority when both signals arrive before the same step. A step that
    /// reports [`BatchStepStatus::Cancelled`] ends the queue as well. The
    /// returned [`BatchReport`] always describes the steps that actually ran, so
    /// callers can persist progress deterministically.
    pub async fn run<E: BatchStepExecutor + ?Sized>(
        &self,
        steps: &[BatchStep],
        executor: &mut E,
        stream_tx: &mpsc::Sender<StreamEvent>,
        cancel_rx: &watch::Receiver<bool>,
        soft_stop_rx: &watch::Receiver<bool>,
    ) -> BatchReport {
        let mut executed = Vec::with_capacity(steps.len());
        let mut usage = BatchUsage::default();
        let mut stop = BatchStop::Completed;

        tracing::info!(scheduled = steps.len(), "batch started");

        for step in steps {
            // Hard cancellation wins over a soft stop when both are pending: an
            // explicit reset must never be downgraded to a graceful stop.
            if *cancel_rx.borrow() {
                stop = BatchStop::Cancelled;
                break;
            }
            // A soft-stop request observed between steps stops the queue before
            // the next step starts, so the running step is never interrupted.
            if *soft_stop_rx.borrow() {
                stop = BatchStop::Stopped;
                break;
            }

            tracing::info!(step_id = step.id.as_str(), "batch step started");

            let outcome = match executor
                .execute_step(step, stream_tx.clone(), cancel_rx.clone())
                .await
            {
                Ok(outcome) => outcome,
                Err(err) => BatchStepOutcome::failed(err.to_string()),
            };

            if let Some(ref step_usage) = outcome.usage {
                usage.accumulate(step_usage);
            }

            let status = outcome.status;
            let block_reason = outcome.blocked_reason.clone();
            tracing::info!(
                step_id = step.id.as_str(),
                status = ?status,
                total_tokens = outcome.usage.as_ref().map(|u| u.total_tokens).unwrap_or(0),
                "batch step finished"
            );
            executed.push(BatchStepReport {
                id: step.id.clone(),
                outcome,
            });

            match status {
                BatchStepStatus::Succeeded => {}
                BatchStepStatus::Failed => {
                    stop = BatchStop::Failed {
                        step_id: step.id.clone(),
                    };
                    break;
                }
                BatchStepStatus::Blocked => {
                    stop = BatchStop::Blocked {
                        step_id: step.id.clone(),
                        reason: block_reason
                            .unwrap_or_else(|| "step blocked without diagnostics".to_string()),
                    };
                    break;
                }
                BatchStepStatus::Cancelled => {
                    stop = BatchStop::Cancelled;
                    break;
                }
            }
        }

        tracing::info!(
            executed = executed.len(),
            scheduled = steps.len(),
            stop = ?stop,
            total_tokens = usage.total_tokens,
            cost = ?usage.cost,
            "batch finished"
        );

        BatchReport {
            executed,
            scheduled: steps.len(),
            stop,
            usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct ScriptedExecutor {
        script: VecDeque<BatchStepOutcome>,
        seen: Vec<String>,
    }

    impl ScriptedExecutor {
        fn new(script: impl IntoIterator<Item = BatchStepOutcome>) -> Self {
            Self {
                script: script.into_iter().collect(),
                seen: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl BatchStepExecutor for ScriptedExecutor {
        async fn execute_step(
            &mut self,
            step: &BatchStep,
            _stream_tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<BatchStepOutcome> {
            self.seen.push(step.id.clone());
            Ok(self
                .script
                .pop_front()
                .unwrap_or_else(BatchStepOutcome::succeeded))
        }
    }

    struct FailingExecutor;

    #[async_trait]
    impl BatchStepExecutor for FailingExecutor {
        async fn execute_step(
            &mut self,
            _step: &BatchStep,
            _stream_tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<BatchStepOutcome> {
            Err(anyhow::anyhow!("transport failure"))
        }
    }

    fn usage(prompt: u32, completion: u32, cost: f64) -> ModelUsageInfo {
        ModelUsageInfo {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            reasoning_tokens: None,
            cached_tokens: None,
            cost: Some(cost),
        }
    }

    fn make_steps(ids: &[&str]) -> Vec<BatchStep> {
        ids.iter()
            .map(|id| BatchStep::new(*id, format!("prompt for {id}")))
            .collect()
    }

    fn channel() -> (mpsc::Sender<StreamEvent>, mpsc::Receiver<StreamEvent>) {
        mpsc::channel(16)
    }

    #[test]
    fn batch_execution_reports_stable_name() {
        assert_eq!(BatchExecution::new().name(), "batch");
    }

    #[tokio::test]
    async fn runs_all_steps_and_aggregates_usage() {
        let steps = make_steps(&["1", "2", "3"]);
        let mut executor = ScriptedExecutor::new(vec![
            BatchStepOutcome::succeeded().with_usage(usage(10, 5, 0.01)),
            BatchStepOutcome::succeeded().with_usage(usage(20, 8, 0.02)),
            BatchStepOutcome::succeeded().with_usage(usage(30, 12, 0.03)),
        ]);
        let (stream_tx, _stream_rx) = channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (_soft_stop_tx, soft_stop_rx) = watch::channel(false);

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert!(report.is_success());
        assert_eq!(report.scheduled, 3);
        assert_eq!(report.executed.len(), 3);
        assert!(report.executed.iter().all(|r| r.outcome.is_success()));
        assert_eq!(executor.seen, vec!["1", "2", "3"]);
        assert_eq!(report.usage.prompt_tokens, 60);
        assert_eq!(report.usage.completion_tokens, 25);
        assert_eq!(report.usage.total_tokens, 85);
        let cost = report.usage.cost.unwrap();
        assert!((cost - 0.06).abs() < 1e-9);
    }

    #[tokio::test]
    async fn marks_batch_cost_unknown_when_a_step_omits_it() {
        let steps = make_steps(&["1", "2"]);
        let mut executor = ScriptedExecutor::new(vec![
            BatchStepOutcome::succeeded().with_usage(ModelUsageInfo {
                prompt_tokens: 20,
                completion_tokens: 8,
                total_tokens: 28,
                reasoning_tokens: None,
                cached_tokens: None,
                cost: None,
            }),
            BatchStepOutcome::succeeded().with_usage(usage(10, 5, 0.01)),
        ]);
        let (stream_tx, _stream_rx) = channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (_soft_stop_tx, soft_stop_rx) = watch::channel(false);

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert!(report.is_success());
        assert_eq!(report.usage.total_tokens, 43);
        assert_eq!(report.usage.cost, None);
    }

    #[tokio::test]
    async fn stops_fail_fast_on_first_failure() {
        let steps = make_steps(&["1", "2", "3"]);
        let mut executor = ScriptedExecutor::new(vec![
            BatchStepOutcome::succeeded(),
            BatchStepOutcome::failed("compile error"),
            BatchStepOutcome::succeeded(),
        ]);
        let (stream_tx, _stream_rx) = channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (_soft_stop_tx, soft_stop_rx) = watch::channel(false);

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert!(!report.is_success());
        assert_eq!(report.scheduled, 3);
        assert_eq!(report.executed.len(), 2);
        assert_eq!(
            report.stop,
            BatchStop::Failed {
                step_id: "2".to_string()
            }
        );
        assert_eq!(executor.seen, vec!["1", "2"]);
        assert_eq!(
            report.executed[1].outcome.error.as_deref(),
            Some("compile error")
        );
    }

    #[tokio::test]
    async fn stops_on_first_blocked_step() {
        let steps = make_steps(&["1", "2", "3"]);
        let mut executor = ScriptedExecutor::new(vec![
            BatchStepOutcome::succeeded(),
            BatchStepOutcome::blocked("precondition missing"),
            BatchStepOutcome::succeeded(),
        ]);
        let (stream_tx, _stream_rx) = channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (_soft_stop_tx, soft_stop_rx) = watch::channel(false);

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert!(!report.is_success());
        assert_eq!(report.scheduled, 3);
        assert_eq!(report.executed.len(), 2);
        assert_eq!(
            report.stop,
            BatchStop::Blocked {
                step_id: "2".to_string(),
                reason: "precondition missing".to_string(),
            }
        );
        assert_eq!(executor.seen, vec!["1", "2"]);
    }

    #[tokio::test]
    async fn stops_when_step_reports_cancellation() {
        let steps = make_steps(&["1", "2"]);
        let mut executor = ScriptedExecutor::new(vec![BatchStepOutcome::cancelled()]);
        let (stream_tx, _stream_rx) = channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (_soft_stop_tx, soft_stop_rx) = watch::channel(false);

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert_eq!(report.stop, BatchStop::Cancelled);
        assert_eq!(report.executed.len(), 1);
        assert_eq!(executor.seen, vec!["1"]);
    }

    #[tokio::test]
    async fn stops_before_running_when_cancel_already_set() {
        let steps = make_steps(&["1", "2"]);
        let mut executor = ScriptedExecutor::new(vec![]);
        let (stream_tx, _stream_rx) = channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (_soft_stop_tx, soft_stop_rx) = watch::channel(false);
        cancel_tx.send(true).unwrap();

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert_eq!(report.stop, BatchStop::Cancelled);
        assert!(report.executed.is_empty());
        assert!(executor.seen.is_empty());
    }

    #[tokio::test]
    async fn converts_executor_error_into_failed_step() {
        let steps = make_steps(&["1", "2"]);
        let mut executor = FailingExecutor;
        let (stream_tx, _stream_rx) = channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (_soft_stop_tx, soft_stop_rx) = watch::channel(false);

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert_eq!(
            report.stop,
            BatchStop::Failed {
                step_id: "1".to_string()
            }
        );
        assert_eq!(report.executed.len(), 1);
        assert_eq!(
            report.executed[0].outcome.error.as_deref(),
            Some("transport failure")
        );
    }

    #[tokio::test]
    async fn empty_batch_completes_without_steps() {
        let steps: Vec<BatchStep> = Vec::new();
        let mut executor = ScriptedExecutor::new(vec![]);
        let (stream_tx, _stream_rx) = channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (_soft_stop_tx, soft_stop_rx) = watch::channel(false);

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert!(report.is_success());
        assert_eq!(report.scheduled, 0);
        assert!(report.executed.is_empty());
        assert_eq!(report.usage, BatchUsage::default());
    }

    #[tokio::test]
    async fn stops_before_running_when_soft_stop_already_set() {
        let steps = make_steps(&["1", "2"]);
        let mut executor = ScriptedExecutor::new(vec![]);
        let (stream_tx, _stream_rx) = channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (soft_stop_tx, soft_stop_rx) = watch::channel(false);
        soft_stop_tx.send(true).unwrap();

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert_eq!(report.stop, BatchStop::Stopped);
        assert!(report.executed.is_empty());
        assert!(executor.seen.is_empty());
    }

    /// Executor that raises the soft-stop signal once it has run `stop_after`
    /// steps, simulating a graceful request arriving mid-batch.
    struct SoftStopAfterStep {
        soft_stop_tx: watch::Sender<bool>,
        stop_after: usize,
        seen: Vec<String>,
    }

    #[async_trait]
    impl BatchStepExecutor for SoftStopAfterStep {
        async fn execute_step(
            &mut self,
            step: &BatchStep,
            _stream_tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<BatchStepOutcome> {
            self.seen.push(step.id.clone());
            if self.seen.len() >= self.stop_after {
                let _ = self.soft_stop_tx.send(true);
            }
            Ok(BatchStepOutcome::succeeded())
        }
    }

    #[tokio::test]
    async fn soft_stop_keeps_completed_step_and_stops_before_next() {
        let steps = make_steps(&["1", "2", "3"]);
        let (soft_stop_tx, soft_stop_rx) = watch::channel(false);
        let mut executor = SoftStopAfterStep {
            soft_stop_tx,
            stop_after: 1,
            seen: Vec::new(),
        };
        let (stream_tx, _stream_rx) = channel();
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert_eq!(report.stop, BatchStop::Stopped);
        assert_eq!(report.executed.len(), 1);
        assert!(report.executed[0].outcome.is_success());
        assert_eq!(executor.seen, vec!["1"]);
    }

    #[tokio::test]
    async fn hard_cancel_takes_priority_over_soft_stop() {
        let steps = make_steps(&["1", "2"]);
        let mut executor = ScriptedExecutor::new(vec![]);
        let (stream_tx, _stream_rx) = channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (soft_stop_tx, soft_stop_rx) = watch::channel(false);
        cancel_tx.send(true).unwrap();
        soft_stop_tx.send(true).unwrap();

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

        assert_eq!(report.stop, BatchStop::Cancelled);
        assert!(report.executed.is_empty());
        assert!(executor.seen.is_empty());
    }
}
