//! Plan batch orchestration and the refine/replan/execute entry points.

use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::batch::{BatchExecution, BatchStep, BatchStop};
use tauqe_protocol::{
    errors, events, BatchFinishStatus, BatchFinishSummary, PlanBatchFinishedEvent,
    PlanExecuteParams, PlanItemStatus, PlanRefineParams, PlanRefineResult, PlanReplanParams,
    PlanScopeEvaluation, Request, Response, TurnPhase, TurnPhaseEvent,
};
use tokio::sync::mpsc;

use crate::state::{AppState, OperationGuard};

use super::common::{clear_auto_context, emit, offload_plan};
use super::engine::{execute_plan_horizon_synthesis, PlanHorizonDiagnosticsOwned};
use super::step::PlanStepExecutor;

/// Outcome of a server-driven batch of plan steps.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BatchOutcome {
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

/// Runs the plan steps through the core [`BatchExecution`] supervisor, which
/// enforces ordered, fail-fast traversal. Steps completed before the first
/// failure stay DONE. When a coder step reports a semantic block (`<blocked>`),
/// an automated closed-loop replanning cycle engages: the architect synthesizes a
/// new horizon around the failure diagnostics up to `max_replans` times.
/// Once the supervisor terminates, a typed `plan/batch_finished` event carries
/// the aggregate status, step counters, and elapsed time.
pub(super) async fn run_plan_batch(
    state: &Arc<AppState>,
    plan_id: &str,
    mut current_step_ids: Vec<String>,
    operation: Arc<OperationGuard>,
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
            final_stop = BatchStop::Completed;
            break;
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
            operation: &operation,
        };

        let report = BatchExecution::new()
            .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
            .await;

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
                            operation_id: format!("op-{}", crate::state::next_operation_id()),
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
                            final_stop = BatchStop::Blocked { step_id, reason: err_msg };
                            break;
                        }
                        Err(err) => {
                            last_error = Some(err.clone());
                            final_stop = BatchStop::Blocked { step_id, reason: err };
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
                        &operation,
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
                        Some(PlanRefineResult::Success { plan: updated_plan, .. }) => {
                            match updated_plan.evaluate_scope(None) {
                                PlanScopeEvaluation::Ready(new_steps) => {
                                    current_step_ids =
                                        new_steps.into_iter().map(|s| s.id).collect();
                                    total_scheduled = total_completed + current_step_ids.len();
                                    tracing::info!(
                                        plan_id = plan_id,
                                        new_horizon_steps = current_step_ids.len(),
                                        replan_round = replans_count,
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
                                    final_stop = BatchStop::Blocked { step_id, reason: msg };
                                    break;
                                }
                                PlanScopeEvaluation::BlockedByBlockers(items) => {
                                    let msg = format!(
                                        "Scope contains step(s) in BLOCKED status: {}. Manual intervention required",
                                        items.join(", ")
                                    );
                                    last_error = Some(msg.clone());
                                    final_stop = BatchStop::Blocked { step_id, reason: msg };
                                    break;
                                }
                                PlanScopeEvaluation::BlockedByDependencies(items) => {
                                    let msg = format!(
                                        "Scope contains step(s) whose prerequisites are not DONE: {}",
                                        items.join(", ")
                                    );
                                    last_error = Some(msg.clone());
                                    final_stop = BatchStop::Blocked { step_id, reason: msg };
                                    break;
                                }
                                other => {
                                    let msg = format!(
                                        "Replanned scope cannot be executed: {:?}",
                                        other
                                    );
                                    last_error = Some(msg.clone());
                                    final_stop = BatchStop::Blocked { step_id, reason: msg };
                                    break;
                                }
                            }
                        }
                        Some(PlanRefineResult::Blocked { reason: architect_reason }) => {
                            let msg = format!(
                                "Architect reported blockers during automated replanning for step '{}': {}",
                                step_id, architect_reason
                            );
                            last_error = Some(msg.clone());
                            final_stop = BatchStop::Blocked { step_id, reason: msg };
                            break;
                        }
                        None => {
                            let msg = format!(
                                "Automated replanning returned unrecognized result for step '{}'",
                                step_id
                            );
                            last_error = Some(msg.clone());
                            final_stop = BatchStop::Blocked { step_id, reason: msg };
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
        BatchStop::Blocked { step_id, reason } => {
            BatchOutcome::Blocked {
                completed: total_completed,
                step_id,
                reason,
            }
        }
    };

    clear_auto_context(state).await;

    emit(
        &state.out,
        events::PLAN_BATCH_FINISHED,
        serde_json::to_value(PlanBatchFinishedEvent {
            operation_id: format!("op-{}", crate::state::next_operation_id()),
            summary: BatchFinishSummary {
                owner_id: plan_id.to_string(),
                status,
                executed_count: total_executed_count,
                total_count: total_scheduled.max(total_executed_count),
                elapsed_seconds: started_at.elapsed().as_secs(),
                error,
            },
        })
        .ok(),
    );

    outcome
}

pub async fn handle_plan_execute(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanExecuteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid plan execute params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let load_root = repo_root.clone();
    let load_plan_id = params.plan_id.clone();
    let plan = match offload_plan(move || PlanStorage::load_plan(&load_root, &load_plan_id)).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::err(
                req.id,
                errors::NOT_FOUND,
                format!("Plan '{}' not found", params.plan_id),
            );
        }
        Err(err) => return Response::err(req.id, errors::PLAN_LOAD_FAILED, err),
    };

    let steps = match plan.evaluate_scope(params.step_id.as_deref()) {
        PlanScopeEvaluation::Ready(steps) => steps,
        PlanScopeEvaluation::BlockedByDiscussion(items) => {
            return Response::err(
                req.id,
                errors::STEP_IN_DISCUSSION,
                format!(
                    "Scope contains item(s) in DISCUSSION status: {}. Review them (change status to TODO) first",
                    items.join(", ")
                ),
            );
        }
        PlanScopeEvaluation::BlockedByBlockers(items) => {
            return Response::err(
                req.id,
                errors::STEP_BLOCKED,
                format!(
                    "Scope contains step(s) in BLOCKED status: {}. Resolve the reported blockers and replan before executing",
                    items.join(", ")
                ),
            );
        }
        PlanScopeEvaluation::BlockedByDependencies(items) => {
            return Response::err(
                req.id,
                errors::STEP_BLOCKED_BY_DEPENDENCIES,
                format!(
                    "Scope contains step(s) whose prerequisites are not yet DONE: {}. Complete the prerequisites first",
                    items.join(", ")
                ),
            );
        }
        PlanScopeEvaluation::StepBlocked => {
            return Response::err(
                req.id,
                errors::STEP_BLOCKED,
                "Target step is BLOCKED; resolve the reported blocker and replan before executing",
            );
        }
        PlanScopeEvaluation::AllCompleted => {
            return Response::err(req.id, errors::NOTHING_TO_EXECUTE, "All steps in this scope are already completed");
        }
        PlanScopeEvaluation::Cancelled => {
            return Response::err(req.id, errors::STEP_CANCELLED, "Step is CANCELLED and cannot be executed");
        }
        PlanScopeEvaluation::NotFound => {
            return Response::err(req.id, errors::NOT_FOUND, "Target item not found in plan");
        }
    };

    let operation = match state.try_begin_operation() {
        Ok(guard) => Arc::new(guard),
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            );
        }
    };

    // Pre-flight verification runs while the batch operation stays registered,
    // so the slot is never briefly free and a concurrent `config/set`,
    // `git/undo`, or a second `plan/execute` cannot slip in mid-verification.
    if let Err(report) = crate::handlers::run_preflight_verification(&repo_root).await {
        let message = format!(
            "Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any step.\n\n{}",
            report
        );
        return Response::err(req.id, errors::PREFLIGHT_VERIFICATION_FAILED, message);
    }

    // Execution hands control to the Coder role: the discussion focus is reset
    // unconditionally so no discussion anchor survives into code generation.
    state.clear_discussion_focus().await;

    let step_ids: Vec<String> = steps.iter().map(|s| s.id.clone()).collect();
    let response = Response::ok(
        req.id,
        serde_json::json!({ "plan_id": plan.id, "step_ids": step_ids }),
    );

    let state = Arc::clone(state);
    let plan_id = plan.id.clone();
    tokio::spawn(async move {
        match run_plan_batch(&state, &plan_id, step_ids, operation).await {
            BatchOutcome::Completed { steps } => {
                tracing::info!("Plan '{}' batch completed: {} step(s)", plan_id, steps);
            }
            BatchOutcome::Failed {
                completed,
                step_id,
                reason,
            } => {
                tracing::warn!(
                    "Plan '{}' batch stopped at step '{}' after {} completed step(s): {}",
                    plan_id,
                    step_id,
                    completed,
                    reason
                );
            }
            BatchOutcome::Blocked {
                completed,
                step_id,
                reason,
            } => {
                tracing::warn!(
                    "Plan '{}' batch blocked at step '{}' after {} completed step(s): {}",
                    plan_id,
                    step_id,
                    completed,
                    reason
                );
            }
            BatchOutcome::Cancelled { completed } => {
                tracing::warn!(
                    "Plan '{}' batch cancelled after {} completed step(s)",
                    plan_id,
                    completed
                );
            }
            BatchOutcome::Stopped { completed } => {
                tracing::info!(
                    "Plan '{}' batch stopped gracefully after {} completed step(s)",
                    plan_id,
                    completed
                );
            }
        }
    });

    response
}

pub async fn handle_plan_refine(req: Request, state: &Arc<AppState>) -> Response {
    let (plan_id, custom_model, instructions) = match req.params {
        Some(val) => {
            if let Ok(params) = serde_json::from_value::<PlanRefineParams>(val.clone()) {
                (params.plan_id, params.model, params.instructions)
            } else if let Some(obj) = val.as_object() {
                let id = obj
                    .get("plan_id")
                    .or_else(|| obj.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let model = obj
                    .get("model")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let instructions = obj
                    .get("instructions")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                (id, model, instructions)
            } else {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Invalid plan refine parameters",
                );
            }
        }
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing plan refine parameters",
            )
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let effective_plan_id = if plan_id.trim().is_empty() {
        let active_root = repo_root.clone();
        offload_plan(move || PlanStorage::load_active_id(&active_root))
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    } else {
        plan_id
    };

    if effective_plan_id.trim().is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "Missing plan ID and no active plan is set",
        );
    }

    let load_root = repo_root.clone();
    let load_plan_id = effective_plan_id.clone();
    let plan = match offload_plan(move || PlanStorage::load_plan(&load_root, &load_plan_id)).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::err(
                req.id,
                errors::NOT_FOUND,
                format!("Plan '{}' not found", effective_plan_id),
            );
        }
        Err(err) => return Response::err(req.id, errors::PLAN_LOAD_FAILED, err),
    };

    let operation = match state.try_begin_operation() {
        Ok(guard) => guard,
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            );
        }
    };

    tracing::info!(plan_id = %plan.id, "plan refine requested");

    execute_plan_horizon_synthesis(
        req.id,
        state,
        plan,
        None,
        custom_model,
        instructions,
        "Deep Plan Refinement",
        &operation,
    )
    .await
}

/// Handles an addressed replanning request for a blocked plan step.
///
/// Synthesizes the unfinished horizon of the plan around the failure diagnostics of the
/// blocked step, preserving the completed (DONE) foundation and updating the plan
/// atomically via `execute_plan_horizon_synthesis`.
pub async fn handle_plan_replan(req: Request, state: &Arc<AppState>) -> Response {
    let (plan_id, step_id, blocked_reason, custom_model, instructions) = match req.params {
        Some(val) => match serde_json::from_value::<PlanReplanParams>(val.clone()) {
            Ok(params) => (
                params.plan_id,
                params.step_id,
                params.reason,
                params.model,
                params.instructions,
            ),
            Err(_) => match val.as_object() {
                Some(obj) => {
                    let plan_id = obj
                        .get("plan_id")
                        .or_else(|| obj.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let step_id = obj
                        .get("step_id")
                        .or_else(|| obj.get("item_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let blocked_reason = obj
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let custom_model = obj
                        .get("model")
                        .and_then(|v| serde_json::from_value(v.clone()).ok());
                    let instructions = obj
                        .get("instructions")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    (plan_id, step_id, blocked_reason, custom_model, instructions)
                }
                None => {
                    return Response::err(
                        req.id,
                        errors::INVALID_PARAMS,
                        "Invalid plan replan parameters",
                    )
                }
            },
        },
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing plan replan parameters",
            )
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let effective_plan_id = if plan_id.trim().is_empty() {
        let active_root = repo_root.clone();
        offload_plan(move || PlanStorage::load_active_id(&active_root))
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    } else {
        plan_id
    };

    if effective_plan_id.trim().is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "Missing plan ID and no active plan is set",
        );
    }
    if step_id.trim().is_empty() {
        return Response::err(req.id, errors::INVALID_PARAMS, "Missing blocked step ID to replan");
    }

    let load_root = repo_root.clone();
    let load_plan_id = effective_plan_id.clone();
    let plan = match offload_plan(move || PlanStorage::load_plan(&load_root, &load_plan_id)).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::err(
                req.id,
                errors::NOT_FOUND,
                format!("Plan '{}' not found", effective_plan_id),
            );
        }
        Err(err) => return Response::err(req.id, errors::PLAN_LOAD_FAILED, err),
    };

    let step = match plan.find_item(&step_id) {
        Some(item) => item.clone(),
        None => {
            return Response::err(
                req.id,
                errors::NOT_FOUND,
                format!("Step '{}' not found in plan '{}'", step_id, effective_plan_id),
            );
        }
    };

    if !step.is_leaf() {
        return Response::err(
            req.id,
            errors::NOT_A_LEAF,
            format!(
                "Step '{}' ({}) is not a leaf and cannot be replanned",
                step.id, step.title
            ),
        );
    }
    if step.status == PlanItemStatus::Done {
        return Response::err(
            req.id,
            errors::STEP_IMMUTABLE,
            format!(
                "Step '{}' is DONE and immutable; completed steps cannot be replanned",
                step.id
            ),
        );
    }

    let operation = match state.try_begin_operation() {
        Ok(guard) => guard,
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            );
        }
    };

    let diagnostics = PlanHorizonDiagnosticsOwned {
        blocked_step_id: step.id.clone(),
        blocked_reason,
        toolchain_status: None,
    };

    tracing::info!(
        plan_id = %plan.id,
        step_id = step.id.as_str(),
        "plan replan requested"
    );

    execute_plan_horizon_synthesis(
        req.id,
        state,
        plan,
        Some(diagnostics),
        custom_model,
        instructions,
        "Plan Replanning",
        &operation,
    )
    .await
}
