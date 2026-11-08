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

use std::path::Path;
use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::implementation::{
    ExecutionScope, PlanImplementationOptions, PlanImplementationPipeline,
    PlanImplementationReport, PlanImplementationStop,
};
use tauqe_protocol::{
    events, BatchFinishStatus, BatchFinishSummary, ModelSelection, PlanBatchFinishedEvent,
};
use tokio::sync::mpsc;

use crate::handlers::common::spawn_escalation_relay;
use crate::handlers::host::ServerImplementationHost;
use crate::state::{AppState, OperationGuard};

use super::super::common::{cleanup_transient_context, emit, offload_plan, publish_plan};

/// Outcome of a server-driven batch of plan steps.
///
/// The type is re-exported by the `batch` facade for the plan handler entry
/// points and the supervisor test suite, so its visibility stops exactly at the
/// plan subsystem.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::handlers::plan) enum BatchOutcome {
    Completed {
        steps: usize,
    },
    Failed {
        completed: usize,
        step_id: String,
        reason: String,
    },
    Blocked {
        completed: usize,
        step_id: String,
        reason: String,
    },
    Cancelled {
        completed: usize,
    },
    Stopped {
        completed: usize,
    },
}

/// Execution options a plan batch forwards to every step it runs.
///
/// The coder selection names the ladder rung a step starts from. It travels from
/// the RPC parameters, so a batch never invents a starting model of its own.
#[derive(Debug, Clone, Default)]
pub(in crate::handlers::plan) struct PlanBatchOptions {
    /// Coder selection the batch starts from, resolved against the coder ladder.
    pub(crate) model: Option<ModelSelection>,
    /// Targeted step or subtree to execute, if not the entire plan.
    pub(crate) step_id: Option<String>,
}

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
    let repo_root = state.context.read().await.repo_root().to_path_buf();

    // Activating the executed plan keeps the interface anchored on the run.
    {
        let active_root = repo_root.clone();
        let active_plan_id = plan_id.to_string();
        let _ = offload_plan(move || {
            PlanStorage::save_active_id(&active_root, Some(active_plan_id.as_str()))
        })
        .await;
    }
    // A batch starts from a released transient context: nothing a previous turn
    // discovered or mounted survives into the horizon it is about to run.
    cleanup_transient_context(state).await;

    let operation_id = format!("op-{}", operation.id());
    tracing::info!(
        plan_id = plan_id,
        validated = step_ids.len(),
        "plan batch started"
    );

    // The whole run shares one configuration snapshot, one operation, and one turn
    // marker: a step promoted mid-run keeps decoding the protocol it started with,
    // and a cancellation reaches every member through the same channels.
    let config = state.config.lock().await.config.clone();
    let marker = state.next_turn_marker().await;
    let host = ServerImplementationHost::new(
        Arc::clone(state),
        Arc::clone(operation),
        config.clone(),
        repo_root.clone(),
    );
    let (stream_tx, stream_rx) = mpsc::channel::<StreamEvent>(1);
    // The relay drains the batch channel for the whole run, so the executor
    // never parks on a full queue and every promotion reaches the client.
    let relay = spawn_escalation_relay(state.out.clone(), operation_id.clone(), stream_rx);

    let report = PlanImplementationPipeline::new(
        &repo_root,
        ExecutionScope::Plan {
            plan_id: plan_id.to_string(),
            step_id: options.step_id.clone(),
        },
        &host,
        config,
        PlanImplementationOptions {
            selection: options.model.clone(),
            instructions: None,
            marker,
        },
    )
    .run(
        &stream_tx,
        &operation.cancel_receiver(),
        &operation.soft_stop_receiver(),
    )
    .await;

    // The last sender closes the batch channel, so the relay publishes the
    // promotions of the run before the terminal report leaves.
    drop(stream_tx);
    if let Err(err) = relay.await {
        tracing::warn!(
            plan_id = plan_id,
            error = %err,
            "plan batch escalation relay terminated unexpectedly"
        );
    }

    // The transient context is released again once the horizon settles, so the
    // next run starts from the same clean basis.
    cleanup_transient_context(state).await;
    publish_plan_state(state, &repo_root, plan_id).await;
    finish_batch(state, &operation_id, plan_id, started_at, &report)
}

/// Publishes the durable state of the executed plan once the run settles.
async fn publish_plan_state(state: &Arc<AppState>, repo_root: &Path, plan_id: &str) {
    let load_root = repo_root.to_path_buf();
    let load_plan_id = plan_id.to_string();
    match offload_plan(move || PlanStorage::load_plan(&load_root, &load_plan_id)).await {
        Ok(Some(plan)) => publish_plan(state, repo_root, plan, plan_id).await,
        Ok(None) => tracing::warn!(plan_id = plan_id, "the executed plan was not found"),
        Err(err) => {
            tracing::warn!(plan_id = plan_id, error = %err, "failed to reload the executed plan")
        }
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
