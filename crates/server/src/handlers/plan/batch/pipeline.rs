//! Plan pipeline execution: a queue of plans under one operation.
//!
//! A pipeline is a super-batch: every plan is loaded and scheduled before the
//! first turn starts, then the plans execute one after another while a single
//! registered operation keeps the slot for the whole queue.

use std::sync::Arc;

use tauqe_protocol::{errors, PlanExecutePipelineParams, Request, Response};

use crate::state::AppState;

/// Runs a queue of plans sequentially under one registered operation.
///
/// A pipeline is a super-batch: every plan is loaded and scheduled before the
/// first turn starts, then the plans execute one after another while the
/// operation slot stays held. A plan that carries only its architectural
/// concept has no leaf to schedule yet, so the shared batch supervisor projects
/// its first horizon just-in-time instead of skipping it.
pub async fn handle_plan_execute_pipeline(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanExecutePipelineParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(params) => params,
            None => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Missing or invalid plan execute pipeline params",
                );
            }
        };

    match state
        .application
        .execute_plan_pipeline(
            params.plan_ids,
            params.model.map(crate::contracts::Project::project),
            super::observer(state),
        )
        .await
    {
        Ok(started) => Response::ok(
            req.id,
            serde_json::json!({
                "plan_ids": started.plan_ids,
                "operation_id": started.operation_id,
            }),
        ),
        Err(error) => super::plan_launch_error(req.id, error),
    }
}
