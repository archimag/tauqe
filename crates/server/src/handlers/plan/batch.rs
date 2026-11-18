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

use tauqe_protocol::{
    errors, PlanExecuteParams, PlanRefineParams, PlanReplanParams, Request, Response,
};

use crate::handlers::PreflightVerdict;
use crate::state::AppState;

use super::engine::{execute_plan_horizon_synthesis, PlanHorizonDiagnosticsOwned};
use super::two_phase::{execute_two_phase_plan_synthesis, TwoPhaseRequest};

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

    let (plan, step_ids) = match state
        .application
        .prepare_plan_execution(&params.plan_id, params.step_id.as_deref())
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            let code = match error.kind {
                tauqe_core::api::PlanExecutionFailure::NothingToExecute => {
                    errors::NOTHING_TO_EXECUTE
                }
                tauqe_core::api::PlanExecutionFailure::NotFound => errors::NOT_FOUND,
                tauqe_core::api::PlanExecutionFailure::PlanLoadFailed => errors::PLAN_LOAD_FAILED,
                tauqe_core::api::PlanExecutionFailure::StepBlocked => errors::STEP_BLOCKED,
                tauqe_core::api::PlanExecutionFailure::StepBlockedByDependencies => {
                    errors::STEP_BLOCKED_BY_DEPENDENCIES
                }
                tauqe_core::api::PlanExecutionFailure::StepCancelled => errors::STEP_CANCELLED,
                tauqe_core::api::PlanExecutionFailure::StepInDiscussion => {
                    errors::STEP_IN_DISCUSSION
                }
            };
            return Response::err(req.id, code, error.message);
        }
    };
    let repo_root = state.repo_root().await;

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
    // The gate inherits the operation's cancellation channel, so a hard cancel
    // tears the verification down instead of waiting the build/test suite out.
    let preflight =
        crate::handlers::run_preflight_verification(&repo_root, operation.cancel_receiver()).await;
    match preflight {
        PreflightVerdict::Passed => {}
        PreflightVerdict::Cancelled => {
            return Response::err(
                req.id,
                errors::CANCELLED,
                "Pre-flight verification was cancelled; batch execution did not start",
            );
        }
        PreflightVerdict::Failed(report) => {
            let message = format!(
                "Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any step.\n\n{}",
                report
            );
            return Response::err(req.id, errors::PREFLIGHT_VERIFICATION_FAILED, message);
        }
    }

    // Execution hands control to the Coder role: the discussion focus is reset
    // unconditionally so no discussion anchor survives into code generation.
    state.clear_discussion_focus().await;

    // The coder selection travels with the request, so every step of the batch
    // starts from the chosen ladder rung. The plan itself is the single source
    let options = PlanBatchOptions {
        model: params.model.map(crate::contracts::Project::project),
        step_id: params.step_id,
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
    let (plan_id, custom_model, critic_model, skip_critique, instructions) = match req.params {
        Some(val) => match serde_json::from_value::<PlanRefineParams>(val) {
            Ok(params) => (
                params.plan_id,
                params.model,
                params.critic_model,
                params.skip_critique,
                params.instructions,
            ),
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

    let repo_root = state.repo_root().await;
    let plan = match state
        .application
        .documents_at(&repo_root)
        .prepare_plan_refinement(plan_id)
        .await
    {
        Ok(plan) => plan,
        Err(error) => return plan_preparation_error(req.id, error),
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

    tracing::info!(
        plan_id = %plan.id,
        critique = critic_model.is_some() && !skip_critique,
        "plan refine requested"
    );

    execute_two_phase_plan_synthesis(
        req.id,
        state,
        TwoPhaseRequest {
            plan,
            directives: instructions,
            planner_model: custom_model.map(crate::contracts::Project::project),
            critic_model: critic_model.map(crate::contracts::Project::project),
            skip_critique,
            creation: false,
            operation_title: "Deep Plan Refinement",
        },
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

    let repo_root = state.repo_root().await;
    let tauqe_core::api::PreparedPlanReplan { plan, step } = match state
        .application
        .documents_at(&repo_root)
        .prepare_plan_replan(plan_id, step_id)
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => return plan_preparation_error(req.id, error),
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
        custom_model.map(crate::contracts::Project::project),
        instructions,
        "Plan Replanning",
        false,
        &operation,
    )
    .await
}

fn plan_preparation_error(
    id: tauqe_protocol::RequestId,
    error: tauqe_core::api::PlanPreparationError,
) -> Response {
    use tauqe_core::api::PlanPreparationError;
    let code = match &error {
        PlanPreparationError::MissingId | PlanPreparationError::MissingStep => {
            errors::INVALID_PARAMS
        }
        PlanPreparationError::NotFound(_) => errors::NOT_FOUND,
        PlanPreparationError::NotLeaf(_) => errors::NOT_A_LEAF,
        PlanPreparationError::Immutable(_) => errors::STEP_IMMUTABLE,
        PlanPreparationError::Storage(_) => errors::PLAN_LOAD_FAILED,
    };
    Response::err(id, code, error.to_string())
}
