//! Plan pipeline execution: a queue of plans under one operation.
//!
//! A pipeline is a super-batch: every plan is loaded and scheduled before the
//! first turn starts, then the plans execute one after another while a single
//! registered operation keeps the slot for the whole queue.

use std::sync::Arc;

use tauqe_protocol::{errors, PlanExecutePipelineParams, Request, Response};

use crate::handlers::PreflightVerdict;
use crate::state::AppState;

use super::supervisor::PlanBatchOptions;

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

    let queue = match state.application.prepare_plan_queue(&params.plan_ids).await {
        Ok(queue) => queue,
        Err(error) => {
            let code = match error.kind {
                tauqe_core::api::PlanQueueFailure::Empty => errors::INVALID_PARAMS,
                tauqe_core::api::PlanQueueFailure::Load => errors::PLAN_LOAD_FAILED,
                tauqe_core::api::PlanQueueFailure::NotFound => errors::NOT_FOUND,
                tauqe_core::api::PlanQueueFailure::Blocked => errors::STEP_BLOCKED,
                tauqe_core::api::PlanQueueFailure::Completed => errors::NOTHING_TO_EXECUTE,
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

    // Pre-flight verification runs while the pipeline is already registered, so
    // no competing mutation can slip in between verification and the first turn.
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
                "Pre-flight verification was cancelled; pipeline execution did not start",
            );
        }
        PreflightVerdict::Failed(report) => {
            let message = format!(
                "Pre-flight verification failed: the repository does not build cleanly. Pipeline execution was blocked before starting any plan.\n\n{}",
                report
            );
            return Response::err(req.id, errors::PREFLIGHT_VERIFICATION_FAILED, message);
        }
    }

    // Execution hands control to the Coder role, so no discussion anchor
    // survives into code generation.
    state.clear_discussion_focus().await;

    let plan_ids: Vec<String> = queue
        .scheduled
        .iter()
        .map(|(plan_id, _)| plan_id.clone())
        .collect();
    // The registered operation runs the whole queue, so its identity travels
    // with the acknowledgement instead of being minted at emission time: a
    // client can attribute every lifecycle event of the pipeline — each plan's
    // terminal `plan/batch_finished` and its replanning phases included — to
    // the request it just sent.
    // The coder selection applies to every plan of the queue, so each plan
    // starts from the chosen ladder rung. The plan itself is the single source
    // of per-step instructions (concept #c-1).
    let options = PlanBatchOptions {
        model: params.model.map(crate::contracts::Project::project),
        step_id: None,
    };
    let response = Response::ok(
        req.id,
        serde_json::json!({
            "plan_ids": plan_ids,
            "operation_id": format!("op-{}", operation.id()),
        }),
    );

    let observer = Arc::new(super::supervisor::Observer {
        state: state.clone(),
        operation: operation.clone(),
        operation_id: format!("op-{}", operation.id()),
    });
    let application = state.application.clone();
    tokio::spawn(async move {
        application
            .execute_plan_queue(queue, operation, options, observer)
            .await;
    });

    response
}
