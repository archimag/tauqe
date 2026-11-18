//! Plan batch supervisor.
//!
//! A plan batch is one implementation horizon executed by the engine's unified
//! pipeline: this module binds the plan scope to that pipeline, hands it the
//! daemon's [`ServerImplementationHost`], and translates the terminal report into
//! the typed batch outcome together with the single `plan/batch_finished` event of
//! the run.
//!
//! Scheduling the ready leaves, driving the coder ladder per step, reprojecting
//! the horizon around a semantically blocked step, honouring a hard cancel and a
//! soft stop, and settling the durable step statuses all belong to
//! `tauqe_core::workflow::implementation`, so the plan path cannot drift from the
//! review runners that execute the very same pipeline.

use std::sync::Arc;

use super::super::common::publish_plan;
use crate::handlers::common::{emit, spawn_escalation_relay};
use crate::handlers::host::ServerImplementationHost;
use crate::state::{AppState, OperationGuard};
use tauqe_core::workflow::implementation::{PlanImplementationReport, PlanImplementationStop};
use tauqe_protocol::{events, BatchFinishStatus, BatchFinishSummary, PlanBatchFinishedEvent};

pub use tauqe_core::api::{BatchOutcome, PlanBatchOptions};

/// Emits the single terminal `plan/batch_finished` event for a batch run.
///
/// Every batched turn — including the just-in-time projection of a concept-only
/// plan and the plan pipeline — funnels its terminal state through this emitter, so
/// a client that opened a batch-mode turn always receives exactly one finish
/// notification carrying the aggregate status, step counters, elapsed time, and
/// optional error.
// Each parameter is one published field of the terminal report, so grouping them
// into a struct would only restate the same values without removing a single
// argument.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_plan_batch_finished(
    state: &Arc<AppState>,
    operation_id: &str,
    owner_id: &str,
    status: BatchFinishStatus,
    executed_count: usize,
    total_count: usize,
    elapsed: std::time::Duration,
    error: Option<String>,
) {
    emit(
        &state.out,
        events::PLAN_BATCH_FINISHED,
        serde_json::to_value(PlanBatchFinishedEvent {
            operation_id: operation_id.to_string(),
            summary: BatchFinishSummary {
                owner_id: owner_id.to_string(),
                status,
                executed_count,
                total_count: total_count.max(executed_count),
                elapsed_seconds: elapsed.as_secs(),
                error,
                // A plan batch reports step progress only; the finding counters
                // belong to a review queue and stay absent from this event.
                fixed_count: None,
                deferred_count: None,
                failed_count: None,
            },
        })
        .ok(),
    );
}

/// Runs the plan steps through the shared implementation pipeline.
///
/// `step_ids` names the scope the request validated. The pipeline re-derives the
/// ready set from durable storage before every round, so a step completed by an
/// earlier round — or by the developer between rounds — is never scheduled twice.
pub(in crate::handlers::plan) async fn run_plan_batch(
    state: &Arc<AppState>,
    plan_id: &str,
    step_ids: Vec<String>,
    operation: Arc<OperationGuard>,
    options: PlanBatchOptions,
) -> BatchOutcome {
    run_plan_batch_with(state, plan_id, step_ids, &operation, &options).await
}

/// Runs one plan's horizon while borrowing the caller's operation.
///
/// A pipeline queues several plans under a single registered operation, so the
/// guard is borrowed instead of owned: the slot stays held for the whole queue and
/// a cancel delivered between plans is honoured. Every other caller keeps owning
/// its guard through [`run_plan_batch`].
pub(super) async fn run_plan_batch_with(
    state: &Arc<AppState>,
    plan_id: &str,
    step_ids: Vec<String>,
    operation: &Arc<OperationGuard>,
    options: &PlanBatchOptions,
) -> BatchOutcome {
    let started_at = std::time::Instant::now();
    tracing::info!(plan_id, validated = step_ids.len(), "plan batch started");
    let operation_id = format!("op-{}", operation.id());
    let observer = Arc::new(Observer {
        state: state.clone(),
        operation: operation.clone(),
        operation_id: operation_id.clone(),
    });
    let report = state
        .application
        .execute_plan_batch(plan_id, operation, options, observer)
        .await;
    finish_batch(state, &operation_id, plan_id, started_at, &report)
}

pub(super) struct Observer {
    pub(super) state: Arc<AppState>,
    pub(super) operation: Arc<OperationGuard>,
    pub(super) operation_id: String,
}
#[async_trait::async_trait]
impl tauqe_core::api::PlanBatchObserver for Observer {
    fn context_changed(&self, state: tauqe_core::context::ContextState) {
        crate::handlers::context::publish_context(&self.state.out, &state);
    }
    fn host(
        &self,
        config: tauqe_core::config::AppConfig,
        root: std::path::PathBuf,
    ) -> Arc<dyn tauqe_core::workflow::implementation::ImplementationHost> {
        Arc::new(ServerImplementationHost::new(
            self.state.clone(),
            self.operation.clone(),
            config,
            root,
        ))
    }
    async fn relay(&self, events: tokio::sync::mpsc::Receiver<tauqe_core::providers::StreamEvent>) {
        let _ =
            spawn_escalation_relay(self.state.out.clone(), self.operation_id.clone(), events).await;
    }
    async fn publish(&self, plan: tauqe_core::plan::Plan) {
        let root = self.state.repo_root().await;
        let id = plan.id.clone();
        publish_plan(&self.state, &root, plan, &id).await;
    }
}

/// Translates the terminal report of the pipeline into the batch contract.
fn finish_batch(
    state: &Arc<AppState>,
    operation_id: &str,
    plan_id: &str,
    started_at: std::time::Instant,
    report: &PlanImplementationReport,
) -> BatchOutcome {
    let completed = report.succeeded;
    let (status, error, outcome) = match &report.stop {
        PlanImplementationStop::Completed => (
            BatchFinishStatus::Completed,
            None,
            BatchOutcome::Completed { steps: completed },
        ),
        PlanImplementationStop::Failed { step, reason } => (
            BatchFinishStatus::Failed,
            Some(reason.clone()),
            BatchOutcome::Failed {
                completed,
                step_id: step.clone().unwrap_or_else(|| plan_id.to_string()),
                reason: reason.clone(),
            },
        ),
        PlanImplementationStop::Blocked { step, reason } => (
            BatchFinishStatus::Blocked,
            Some(reason.clone()),
            BatchOutcome::Blocked {
                completed,
                step_id: step.clone().unwrap_or_else(|| plan_id.to_string()),
                reason: reason.clone(),
            },
        ),
        PlanImplementationStop::Cancelled => (
            BatchFinishStatus::Cancelled,
            None,
            BatchOutcome::Cancelled { completed },
        ),
        PlanImplementationStop::Stopped => (
            BatchFinishStatus::Stopped,
            None,
            BatchOutcome::Stopped { completed },
        ),
    };

    tracing::info!(
        plan_id = plan_id,
        executed = report.executed,
        succeeded = report.succeeded,
        replans = report.replans,
        stop = ?report.stop,
        "plan batch supervisor finished"
    );

    emit_plan_batch_finished(
        state,
        operation_id,
        plan_id,
        status,
        report.executed,
        report.scheduled.max(report.executed),
        started_at.elapsed(),
        error,
    );

    outcome
}

#[async_trait::async_trait]
impl tauqe_core::api::PlanQueueObserver for Observer {
    fn batch_finished(
        &self,
        id: &str,
        elapsed: std::time::Duration,
        report: &PlanImplementationReport,
    ) {
        finish_batch(
            &self.state,
            &self.operation_id,
            id,
            std::time::Instant::now() - elapsed,
            report,
        );
    }
}
