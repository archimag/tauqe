//! Plan batch supervisor and automated replanning.
//!
//! The supervisor owns the whole batch lifetime: it takes or borrows the
//! operation slot, schedules the plan's leaves, drives the core
//! [`BatchExecution`] loop, engages the Architect around a semantically blocked
//! step, and publishes exactly one terminal event for the batch.

use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::batch::{BatchExecution, BatchStep, BatchStop};
use tauqe_protocol::{
    errors, events, BatchFinishStatus, BatchFinishSummary, ModelSelection, PlanBatchFinishedEvent,
    PlanRefineResult, PlanScopeEvaluation, TurnPhase, TurnPhaseEvent,
};
use tokio::sync::mpsc;

use crate::handlers::common::wait_for_cancellation;
use crate::state::{AppState, OperationGuard};

use super::super::common::{clear_auto_context, emit, offload_plan};
use super::super::engine::{execute_plan_horizon_synthesis, PlanHorizonDiagnosticsOwned};
use super::super::step::PlanStepExecutor;

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
/// The coder selection names the ladder rung a step starts from. It travels
/// from the RPC parameters, so a batch never invents a starting model of its own.
#[derive(Debug, Clone, Default)]
pub(in crate::handlers::plan) struct PlanBatchOptions {
    /// Coder selection the batch starts from, resolved against the coder ladder.
    pub(crate) model: Option<ModelSelection>,
}

/// Emits the single terminal `plan/batch_finished` event for a batch run.
///
/// Every batched turn — including JIT horizon synthesis and the plan pipeline —
/// funnels its terminal state through this emitter, so a client that opened a
/// batch-mode turn always receives exactly one finish notification carrying the
/// aggregate status, step counters, elapsed time, and optional error.
// Each parameter is one published field of the terminal report, so grouping
// them into a struct would only restate the same values without removing a
// single argument.
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
            },
        })
        .ok(),
    );
}

/// Finalizes a batch that could not launch a single step and returns the
/// matching terminal outcome.
///
/// A batch that fails before its first step — an unschedulable scope, an
/// unreadable plan, or a failed just-in-time synthesis — still owes the client
/// exactly one `plan/batch_finished` event. Producing the emitter call and the
/// [`BatchOutcome`] together keeps every early exit consistent instead of
/// duplicating the event payload at each of them.
fn reject_batch(
    state: &Arc<AppState>,
    operation_id: &str,
    plan_id: &str,
    started_at: std::time::Instant,
    scheduled: usize,
    reason: String,
) -> BatchOutcome {
    emit_plan_batch_finished(
        state,
        operation_id,
        plan_id,
        BatchFinishStatus::Failed,
        0,
        scheduled,
        started_at.elapsed(),
        Some(reason.clone()),
    );
    BatchOutcome::Failed {
        completed: 0,
        step_id: plan_id.to_string(),
        reason,
    }
}

/// Runs the plan steps through the core [`BatchExecution`] supervisor, which
/// enforces ordered, fail-fast traversal. Steps completed before the first
/// failure stay DONE. When a coder step reports a semantic block (`<blocked>`),
/// an automated closed-loop replanning cycle engages: the architect synthesizes a
/// new horizon around the failure diagnostics up to `max_replans` times.
/// Once the supervisor terminates, a typed `plan/batch_finished` event carries
/// the aggregate status, step counters, and elapsed time. `options` carries the
/// coder rung the steps start from and the developer directives appended to
/// every step prompt.
pub(in crate::handlers::plan) async fn run_plan_batch(
    state: &Arc<AppState>,
    plan_id: &str,
    step_ids: Vec<String>,
    operation: Arc<OperationGuard>,
    options: PlanBatchOptions,
) -> BatchOutcome {
    run_plan_batch_with(state, plan_id, step_ids, &operation, &options).await
}

/// Runs one plan's scheduled steps while borrowing the caller's operation.
///
/// A pipeline queues several plans under a single registered operation, so the
/// guard is borrowed instead of owned: the slot stays held for the whole queue
/// and a cancel delivered between plans is honoured. Every other caller keeps
/// owning its guard through [`run_plan_batch`].
pub(super) async fn run_plan_batch_with(
    state: &Arc<AppState>,
    plan_id: &str,
    mut current_step_ids: Vec<String>,
    operation: &Arc<OperationGuard>,
    options: &PlanBatchOptions,
) -> BatchOutcome {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    {
        let active_root = repo_root.clone();
        let active_plan_id = plan_id.to_string();
        let _ = offload_plan(move || {
            PlanStorage::save_active_id(&active_root, Some(active_plan_id.as_str()))
        })
        .await;
    }
    clear_auto_context(state).await;

    // Every lifecycle event of this batch — its replanning rounds and its
    // terminal completion — is published under the registered operation that
    // runs it, so a client can attribute all of them to the request it sent.
    let operation_id = format!("op-{}", operation.id());

    tracing::info!(
        plan_id = plan_id,
        scheduled = current_step_ids.len(),
        "plan batch started"
    );

    let started_at = std::time::Instant::now();

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    // The whole batch shares one operation and one cancel channel: a cancel
    // delivered between steps is honored instead of being lost, and the slot
    // stays registered for the batch's entire lifetime.
    let cancel_rx = operation.cancel_receiver();
    // Soft stop ends the queue gracefully after the current step: the operation
    // owns the signal, and a `model/cancel` request with `mode: "soft"` raises it
    // so the current step finishes its turn, verification, and commit before the
    // queue stops.
    let soft_stop_rx = operation.soft_stop_receiver();

    let max_replans = state.config.lock().await.config.develop.max_replans;
    let mut replans_count = 0usize;
    let mut total_executed_count = 0usize;
    let mut total_completed = 0usize;
    let mut total_scheduled = current_step_ids.len();

    let final_stop;
    let mut last_error: Option<String> = None;

    loop {
        if *cancel_rx.borrow() {
            final_stop = BatchStop::Cancelled;
            break;
        }

        if current_step_ids.is_empty() {
            // A plan that carries an architectural concept but no synthesized
            // horizon has no leaf to schedule yet. The Architect projects the
            // first horizon just-in-time, and the resulting leaves join this
            // same supervisor loop instead of the batch finishing with an empty
            // scope.
            let load_root = repo_root.clone();
            let load_plan_id = plan_id.to_string();
            let plan = match offload_plan(move || PlanStorage::load_plan(&load_root, &load_plan_id))
                .await
            {
                Ok(Some(plan)) => plan,
                Ok(None) => {
                    clear_auto_context(state).await;
                    return reject_batch(
                        state,
                        &operation_id,
                        plan_id,
                        started_at,
                        total_scheduled,
                        format!("Plan '{}' not found", plan_id),
                    );
                }
                Err(err) => {
                    clear_auto_context(state).await;
                    return reject_batch(
                        state,
                        &operation_id,
                        plan_id,
                        started_at,
                        total_scheduled,
                        err,
                    );
                }
            };

            if plan.items.is_empty() && !plan.concept.is_empty() {
                tracing::info!(
                    plan_id = plan_id,
                    "plan carries no synthesized horizon; projecting it just-in-time"
                );

                let res = execute_plan_horizon_synthesis(
                    tauqe_protocol::RequestId::Null,
                    state,
                    plan,
                    None,
                    None,
                    None,
                    "Just-in-Time Plan Refinement",
                    true,
                    operation.as_ref(),
                )
                .await;

                if *cancel_rx.borrow() {
                    final_stop = BatchStop::Cancelled;
                    break;
                }

                if let Some(err) = res.error {
                    if err.code == errors::CANCELLED {
                        final_stop = BatchStop::Cancelled;
                        break;
                    }
                    clear_auto_context(state).await;
                    return reject_batch(
                        state,
                        &operation_id,
                        plan_id,
                        started_at,
                        total_scheduled,
                        format!("Just-in-time plan refinement failed: {}", err.message),
                    );
                }

                let refine_res = res
                    .result
                    .and_then(|v| serde_json::from_value::<PlanRefineResult>(v).ok());

                match refine_res {
                    Some(PlanRefineResult::Success {
                        plan: synthesized, ..
                    }) => match synthesized.evaluate_scope(None) {
                        PlanScopeEvaluation::Ready(steps) => {
                            current_step_ids = steps.into_iter().map(|s| s.id).collect();
                            total_scheduled = current_step_ids.len();
                            tracing::info!(
                                plan_id = plan_id,
                                steps = total_scheduled,
                                "just-in-time horizon synthesized"
                            );
                            continue;
                        }
                        PlanScopeEvaluation::AllCompleted => {
                            final_stop = BatchStop::Completed;
                            break;
                        }
                        other => {
                            let msg =
                                format!("Just-in-time horizon cannot be executed: {:?}", other);
                            clear_auto_context(state).await;
                            return reject_batch(
                                state,
                                &operation_id,
                                plan_id,
                                started_at,
                                total_scheduled,
                                msg,
                            );
                        }
                    },
                    Some(PlanRefineResult::Blocked { reason }) => {
                        clear_auto_context(state).await;
                        return reject_batch(
                            state,
                            &operation_id,
                            plan_id,
                            started_at,
                            total_scheduled,
                            format!(
                                "Architect reported blockers during just-in-time refinement: {}",
                                reason
                            ),
                        );
                    }
                    None => {
                        clear_auto_context(state).await;
                        return reject_batch(
                            state,
                            &operation_id,
                            plan_id,
                            started_at,
                            total_scheduled,
                            "Just-in-time plan refinement returned an unrecognized result"
                                .to_string(),
                        );
                    }
                }
            } else {
                final_stop = BatchStop::Completed;
                break;
            }
        }

        let steps: Vec<BatchStep> = current_step_ids
            .iter()
            .map(|id| BatchStep::new(id.clone(), format!("Execute plan step {}", id)))
            .collect();

        let mut executor = PlanStepExecutor {
            state,
            repo_root: &repo_root,
            plan_id,
            batch_step_ids: &current_step_ids,
            operation,
            model: options.model.as_ref(),
        };

        // A cancel delivered while a step runs must unwind the queue at once.
        // The step itself observes the signal at its own phase boundaries and
        // kills the verification subprocess; this race is the guarantee that the
        // batch never waits for the remainder of a long verification. An
        // abandoned step is never marked DONE and never committed, and its
        // checkpoint is unwound by the existing rollback contour.
        let report = {
            let batch = BatchExecution::new();
            let run = batch.run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx);
            tokio::pin!(run);
            tokio::select! {
                biased;
                report = &mut run => Some(report),
                _ = wait_for_cancellation(cancel_rx.clone()) => None,
            }
        };
        let Some(report) = report else {
            tracing::warn!(
                plan_id = plan_id,
                "plan batch unwound mid-step by cancellation"
            );
            final_stop = BatchStop::Cancelled;
            break;
        };

        let run_executed = report.executed.len();
        total_executed_count += run_executed;

        match report.stop {
            BatchStop::Completed => {
                total_completed += run_executed;
                final_stop = BatchStop::Completed;
                break;
            }
            BatchStop::Cancelled => {
                total_completed += report
                    .executed
                    .iter()
                    .filter(|r| r.outcome.is_success())
                    .count();
                final_stop = BatchStop::Cancelled;
                break;
            }
            BatchStop::Stopped => {
                total_completed += report
                    .executed
                    .iter()
                    .filter(|r| r.outcome.is_success())
                    .count();
                final_stop = BatchStop::Stopped;
                break;
            }
            BatchStop::Failed { step_id } => {
                total_completed += run_executed.saturating_sub(1);
                last_error = report.executed.last().and_then(|r| r.outcome.error.clone());
                // A failed step ends the queue immediately and never engages the
                // Architect: replanning is reserved for a semantic block, which
                // signals a failed precondition the Architect can re-scope. An
                // unconditional abort travels exactly this path, because the
                // executor reports it as a failure carrying the developer's
                // recommendation, so no replanning cycle is started for it.
                tracing::warn!(
                    plan_id = plan_id,
                    step_id = step_id.as_str(),
                    reason = last_error.as_deref().unwrap_or("no diagnostics"),
                    "plan batch stopped on a failed step; no replanning is engaged"
                );
                final_stop = BatchStop::Failed { step_id };
                break;
            }
            BatchStop::Blocked { step_id, reason } => {
                total_completed += run_executed.saturating_sub(1);

                if replans_count < max_replans {
                    replans_count += 1;

                    tracing::warn!(
                        plan_id = plan_id,
                        blocked_step_id = step_id.as_str(),
                        replan_round = replans_count,
                        max_replans = max_replans,
                        reason = %reason,
                        "automated replanning started"
                    );

                    emit(
                        &state.out,
                        events::TURN_PHASE,
                        serde_json::to_value(TurnPhaseEvent {
                            operation_id: operation_id.clone(),
                            phase: TurnPhase::Proposal,
                            round: Some(replans_count),
                            max_rounds: Some(max_replans),
                            detail: Some(format!(
                                "Automated replanning round {}/{} around blocked step #{}",
                                replans_count, max_replans, step_id
                            )),
                        })
                        .ok(),
                    );

                    let load_root = repo_root.clone();
                    let load_plan_id = plan_id.to_string();
                    let plan = match offload_plan(move || {
                        PlanStorage::load_plan(&load_root, &load_plan_id)
                    })
                    .await
                    {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            let err_msg = format!("Plan '{}' not found", plan_id);
                            last_error = Some(err_msg.clone());
                            final_stop = BatchStop::Blocked {
                                step_id,
                                reason: err_msg,
                            };
                            break;
                        }
                        Err(err) => {
                            last_error = Some(err.clone());
                            final_stop = BatchStop::Blocked {
                                step_id,
                                reason: err,
                            };
                            break;
                        }
                    };

                    let diagnostics = PlanHorizonDiagnosticsOwned {
                        blocked_step_id: step_id.clone(),
                        blocked_reason: Some(reason.clone()),
                        toolchain_status: None,
                    };

                    let res = execute_plan_horizon_synthesis(
                        tauqe_protocol::RequestId::Null,
                        state,
                        plan,
                        Some(diagnostics),
                        None,
                        None,
                        "Automated Plan Replanning",
                        true,
                        operation.as_ref(),
                    )
                    .await;

                    if *cancel_rx.borrow() {
                        final_stop = BatchStop::Cancelled;
                        break;
                    }

                    if let Some(err) = res.error {
                        if err.code == errors::CANCELLED {
                            final_stop = BatchStop::Cancelled;
                            break;
                        }
                        let failure_msg = format!(
                            "Automated replanning failed around step '{}': {}",
                            step_id, err.message
                        );
                        last_error = Some(failure_msg.clone());
                        final_stop = BatchStop::Blocked {
                            step_id,
                            reason: failure_msg,
                        };
                        break;
                    }

                    let refine_res = res
                        .result
                        .and_then(|v| serde_json::from_value::<PlanRefineResult>(v).ok());

                    match refine_res {
                        Some(PlanRefineResult::Success {
                            plan: updated_plan, ..
                        }) => match updated_plan.evaluate_scope(None) {
                            PlanScopeEvaluation::Ready(new_steps) => {
                                current_step_ids = new_steps.into_iter().map(|s| s.id).collect();
                                total_scheduled = total_completed + current_step_ids.len();
                                tracing::info!(
                                    plan_id = plan_id,
                                    new_horizon_steps = current_step_ids.len(),
                                    replan_round = replans_count,
                                    architect = ?updated_plan.architect,
                                    "automated replanning produced a new horizon"
                                );
                                continue;
                            }
                            PlanScopeEvaluation::AllCompleted => {
                                final_stop = BatchStop::Completed;
                                break;
                            }
                            PlanScopeEvaluation::BlockedByDiscussion(items) => {
                                let msg = format!(
                                        "Scope contains item(s) in DISCUSSION status: {}. Review them first",
                                        items.join(", ")
                                    );
                                last_error = Some(msg.clone());
                                final_stop = BatchStop::Blocked {
                                    step_id,
                                    reason: msg,
                                };
                                break;
                            }
                            PlanScopeEvaluation::BlockedByBlockers(items) => {
                                let msg = format!(
                                        "Scope contains step(s) in BLOCKED status: {}. Manual intervention required",
                                        items.join(", ")
                                    );
                                last_error = Some(msg.clone());
                                final_stop = BatchStop::Blocked {
                                    step_id,
                                    reason: msg,
                                };
                                break;
                            }
                            PlanScopeEvaluation::BlockedByDependencies(items) => {
                                let msg = format!(
                                    "Scope contains step(s) whose prerequisites are not DONE: {}",
                                    items.join(", ")
                                );
                                last_error = Some(msg.clone());
                                final_stop = BatchStop::Blocked {
                                    step_id,
                                    reason: msg,
                                };
                                break;
                            }
                            other => {
                                let msg =
                                    format!("Replanned scope cannot be executed: {:?}", other);
                                last_error = Some(msg.clone());
                                final_stop = BatchStop::Blocked {
                                    step_id,
                                    reason: msg,
                                };
                                break;
                            }
                        },
                        Some(PlanRefineResult::Blocked {
                            reason: architect_reason,
                        }) => {
                            let msg = format!(
                                "Architect reported blockers during automated replanning for step '{}': {}",
                                step_id, architect_reason
                            );
                            last_error = Some(msg.clone());
                            final_stop = BatchStop::Blocked {
                                step_id,
                                reason: msg,
                            };
                            break;
                        }
                        None => {
                            let msg = format!(
                                "Automated replanning returned unrecognized result for step '{}'",
                                step_id
                            );
                            last_error = Some(msg.clone());
                            final_stop = BatchStop::Blocked {
                                step_id,
                                reason: msg,
                            };
                            break;
                        }
                    }
                } else {
                    let limit_msg = format!(
                        "Automated replanning limit reached (max_replans = {}). Step '{}' remains blocked: {}. Manual developer intervention required.",
                        max_replans, step_id, reason
                    );
                    last_error = Some(limit_msg.clone());
                    final_stop = BatchStop::Blocked {
                        step_id,
                        reason: limit_msg,
                    };
                    break;
                }
            }
        }
    }

    tracing::info!(
        plan_id = plan_id,
        executed = total_executed_count,
        scheduled = total_scheduled,
        stop = ?final_stop,
        "plan batch supervisor finished"
    );

    let (status, error) = match &final_stop {
        BatchStop::Completed => (BatchFinishStatus::Completed, None),
        BatchStop::Failed { .. } => (
            BatchFinishStatus::Failed,
            last_error.or_else(|| Some("step failed without diagnostics".to_string())),
        ),
        BatchStop::Blocked { reason, .. } => (BatchFinishStatus::Blocked, Some(reason.clone())),
        BatchStop::Cancelled => (BatchFinishStatus::Cancelled, None),
        BatchStop::Stopped => (BatchFinishStatus::Stopped, None),
    };

    let outcome = match final_stop {
        BatchStop::Completed => BatchOutcome::Completed {
            steps: total_completed,
        },
        BatchStop::Cancelled => BatchOutcome::Cancelled {
            completed: total_completed,
        },
        BatchStop::Stopped => BatchOutcome::Stopped {
            completed: total_completed,
        },
        BatchStop::Failed { step_id } => {
            let reason = error
                .clone()
                .unwrap_or_else(|| "step failed without diagnostics".to_string());
            BatchOutcome::Failed {
                completed: total_completed,
                step_id,
                reason,
            }
        }
        BatchStop::Blocked { step_id, reason } => BatchOutcome::Blocked {
            completed: total_completed,
            step_id,
            reason,
        },
    };

    clear_auto_context(state).await;

    emit_plan_batch_finished(
        state,
        &operation_id,
        plan_id,
        status,
        total_executed_count,
        total_scheduled,
        started_at.elapsed(),
        error,
    );

    outcome
}
