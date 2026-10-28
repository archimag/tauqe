//! Plan batch orchestration facade.
//!
//! Batch execution is decomposed into focused submodules: the supervisor
//! (`supervisor`) drives ordered, fail-fast traversal of the plan's leaves with
//! automated replanning around a semantically blocked step, and the pipeline
//! queue (`pipeline`) runs several plans under one registered operation. This
//! facade owns the request entry points that launch a batch, a refine, or a
//! replan, and re-exports the supervisor surface the facade and the test suite
//! depend on.

mod pipeline;
mod supervisor;

use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_protocol::{
    errors, PlanExecuteParams, PlanItemStatus, PlanRefineParams, PlanReplanParams,
    PlanScopeEvaluation, Request, Response,
};

use crate::state::AppState;

use super::common::offload_plan;
use super::engine::{execute_plan_horizon_synthesis, PlanHorizonDiagnosticsOwned};

pub use pipeline::handle_plan_execute_pipeline;
pub(super) use supervisor::{run_plan_batch, BatchOutcome, PlanBatchOptions};

pub async fn handle_plan_execute(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanExecuteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid plan execute params",
            )
        }
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
            // A plan that carries an architectural concept but no synthesized
            // horizon has no leaf to schedule yet: the batch projects the first
            // horizon just-in-time, so the request is not rejected as a
            // completed scope.
            if plan.items.is_empty() && !plan.concept.is_empty() {
                Vec::<tauqe_protocol::PlanItem>::new()
            } else {
                return Response::err(
                    req.id,
                    errors::NOTHING_TO_EXECUTE,
                    "All steps in this scope are already completed",
                );
            }
        }
        PlanScopeEvaluation::Cancelled => {
            return Response::err(
                req.id,
                errors::STEP_CANCELLED,
                "Step is CANCELLED and cannot be executed",
            );
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
    // The coder selection travels with the request, so every step of the batch
    // starts from the chosen ladder rung. The plan itself is the single source
    // of per-step instructions (concept #c-1).
    let options = PlanBatchOptions {
        model: params.model,
    };
    // The identity of the registered operation travels with the acknowledgement,
    // so a client can attribute every lifecycle event of the batch — the
    // replanning rounds and the terminal `plan/batch_finished` included — to the
    // request it just sent.
    let operation_id = format!("op-{}", operation.id());
    let response = Response::ok(
        req.id,
        serde_json::json!({
            "plan_id": plan.id,
            "step_ids": step_ids,
            "operation_id": operation_id,
        }),
    );

    let state = Arc::clone(state);
    let plan_id = plan.id.clone();
    tokio::spawn(async move {
        match run_plan_batch(&state, &plan_id, step_ids, operation, options).await {
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
    // Strict wire-contract parse: the typed DTO — which declares the legacy
    // `id` alias — is the single parser, so a malformed field answers
    // INVALID_PARAMS instead of degrading to a silent active-plan fallback
    // or dropping the model selection (Conventions §2.5).
    let (plan_id, custom_model, instructions) = match req.params {
        Some(val) => match serde_json::from_value::<PlanRefineParams>(val) {
            Ok(params) => (params.plan_id, params.model, params.instructions),
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid plan refine parameters: {}", err),
                );
            }
        },
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing plan refine parameters",
            );
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
        false,
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
    // Strict wire-contract parse: the typed DTO — which already declares the
    // legacy `id`/`item_id` aliases — is the single parser, so a malformed
    // field answers INVALID_PARAMS instead of degrading to empty identifiers
    // or dropping the model selection (Conventions §2.5).
    let (plan_id, step_id, blocked_reason, custom_model, instructions) = match req.params {
        Some(val) => match serde_json::from_value::<PlanReplanParams>(val) {
            Ok(params) => (
                params.plan_id,
                params.step_id,
                params.reason,
                params.model,
                params.instructions,
            ),
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid plan replan parameters: {}", err),
                );
            }
        },
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing plan replan parameters",
            );
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
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "Missing blocked step ID to replan",
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

    let step = match plan.find_item(&step_id) {
        Some(item) => item.clone(),
        None => {
            return Response::err(
                req.id,
                errors::NOT_FOUND,
                format!(
                    "Step '{}' not found in plan '{}'",
                    step_id, effective_plan_id
                ),
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
        false,
        &operation,
    )
    .await
}
