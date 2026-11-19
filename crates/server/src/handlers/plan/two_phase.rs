//! RPC projection for core-owned independent plan synthesis.
use crate::contracts::Project;
use crate::state::AppState;
use std::sync::Arc;
use tauqe_flow::api::{PlanHorizonFailure, PlanSynthesisError, PlanSynthesisInput};
use tauqe_protocol::{errors, PlanCreatePipelineParams, Request, RequestId, Response};
/// Handles `plan/createPipeline`: the two-phase creation of a new plan.
///
/// The directives are the only coordinate a creation carries, so the plan
/// identity is derived from them deterministically and the issued document is
/// written by the pipeline instead of being supplied by the caller.
pub async fn handle_plan_create_pipeline(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanCreatePipelineParams = match req.params {
        Some(value) => match serde_json::from_value::<PlanCreatePipelineParams>(value) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid plan create pipeline parameters: {err}"),
                )
            }
        },
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing plan create pipeline parameters",
            )
        }
    };

    synthesize(
        req.id,
        state,
        PlanSynthesisInput::Create {
            directives: params.directives,
            planner_model: params.planner_model.map(Project::project),
            critic_model: params.critic_model.map(Project::project),
            skip_critique: params.skip_critique,
        },
    )
    .await
}

pub(super) async fn synthesize(
    req_id: RequestId,
    state: &AppState,
    input: PlanSynthesisInput,
) -> Response {
    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    match state
        .application
        .synthesize_plan(input, observer, crate::reap::task_exit_deadline())
        .await
    {
        Ok(result) => Response::ok_typed(req_id, &result.project()),
        Err(error) => {
            use tauqe_flow::api::PlanPreparationError;
            let code = match &error {
                PlanSynthesisError::Busy => errors::OPERATION_IN_PROGRESS,
                PlanSynthesisError::EmptyDirectives => errors::INVALID_PARAMS,
                PlanSynthesisError::Load(_) => errors::PLAN_LOAD_FAILED,
                PlanSynthesisError::Preparation(error) => match error {
                    PlanPreparationError::MissingId | PlanPreparationError::MissingStep => {
                        errors::INVALID_PARAMS
                    }
                    PlanPreparationError::NotFound(_) => errors::NOT_FOUND,
                    PlanPreparationError::NotLeaf(_) => errors::NOT_A_LEAF,
                    PlanPreparationError::Immutable(_) => errors::STEP_IMMUTABLE,
                    PlanPreparationError::Storage(_) => errors::PLAN_LOAD_FAILED,
                },
                PlanSynthesisError::Execution(error) => error_code(error.kind),
            };
            Response::err(req_id, code, error.to_string())
        }
    }
}

fn error_code(kind: PlanHorizonFailure) -> &'static str {
    match kind {
        PlanHorizonFailure::Cancelled => errors::CANCELLED,
        PlanHorizonFailure::Model => errors::MODEL_ERROR,
        PlanHorizonFailure::EmptyItems => errors::EMPTY_ITEMS,
        PlanHorizonFailure::InvalidTree => errors::INVALID_PLAN_TREE,
        PlanHorizonFailure::Save => errors::PLAN_SAVE_FAILED,
    }
}
