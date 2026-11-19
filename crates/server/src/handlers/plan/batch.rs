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
#[cfg(test)]
mod supervisor;

use std::sync::Arc;

use tauqe_protocol::{
    errors, PlanExecuteParams, PlanRefineParams, PlanReplanParams, Request, Response,
};

use crate::state::AppState;

use super::two_phase::synthesize;
use tauqe_flow::api::PlanSynthesisInput;

pub use pipeline::handle_plan_execute_pipeline;
#[cfg(test)]
pub(super) use supervisor::run_plan_batch;
#[cfg(test)]
pub(super) use tauqe_flow::api::BatchOutcome;
pub(super) use tauqe_flow::api::PlanBatchOptions;

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

    let options = PlanBatchOptions {
        step_id: params.step_id,
        model: params.model.map(crate::contracts::Project::project),
    };
    match state
        .application
        .execute_plan(params.plan_id, options, observer(state))
        .await
    {
        Ok(started) => Response::ok(
            req.id,
            serde_json::json!({
                "plan_id": started.plan_id,
                "step_ids": started.step_ids,
                "operation_id": started.operation_id,
            }),
        ),
        Err(error) => plan_launch_error(req.id, error),
    }
}

fn observer(state: &AppState) -> Arc<crate::publication::Publisher> {
    Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    })
}

fn plan_launch_error(
    id: tauqe_protocol::RequestId,
    error: tauqe_flow::api::PlanLaunchError,
) -> Response {
    use tauqe_flow::api::{PlanExecutionFailure, PlanLaunchError, PlanQueueFailure};
    let code = match &error {
        PlanLaunchError::Busy => errors::OPERATION_IN_PROGRESS,
        PlanLaunchError::Cancelled(_) => errors::CANCELLED,
        PlanLaunchError::Verification(_) => errors::PREFLIGHT_VERIFICATION_FAILED,
        PlanLaunchError::Scope(error) => match error.kind {
            PlanExecutionFailure::NothingToExecute => errors::NOTHING_TO_EXECUTE,
            PlanExecutionFailure::NotFound => errors::NOT_FOUND,
            PlanExecutionFailure::PlanLoadFailed => errors::PLAN_LOAD_FAILED,
            PlanExecutionFailure::StepBlocked => errors::STEP_BLOCKED,
            PlanExecutionFailure::StepBlockedByDependencies => errors::STEP_BLOCKED_BY_DEPENDENCIES,
            PlanExecutionFailure::StepCancelled => errors::STEP_CANCELLED,
            PlanExecutionFailure::StepInDiscussion => errors::STEP_IN_DISCUSSION,
        },
        PlanLaunchError::Queue(error) => match error.kind {
            PlanQueueFailure::Empty => errors::INVALID_PARAMS,
            PlanQueueFailure::Load => errors::PLAN_LOAD_FAILED,
            PlanQueueFailure::NotFound => errors::NOT_FOUND,
            PlanQueueFailure::Blocked => errors::STEP_BLOCKED,
            PlanQueueFailure::Completed => errors::NOTHING_TO_EXECUTE,
        },
    };
    Response::err(id, code, error.to_string())
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

    synthesize(
        req.id,
        state,
        PlanSynthesisInput::Refine {
            plan_id,
            directives: instructions,
            planner_model: custom_model.map(crate::contracts::Project::project),
            critic_model: critic_model.map(crate::contracts::Project::project),
            skip_critique,
        },
    )
    .await
}

/// Handles an addressed replanning request for a blocked plan step.
///
/// Synthesizes the unfinished horizon of the plan around the failure diagnostics of the
/// blocked step, preserving the completed (DONE) foundation and updating the plan
/// atomically.
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

    synthesize(
        req.id,
        state,
        PlanSynthesisInput::Replan {
            plan_id,
            step_id,
            reason: blocked_reason,
            model: custom_model.map(crate::contracts::Project::project),
            directives: instructions,
        },
    )
    .await
}
