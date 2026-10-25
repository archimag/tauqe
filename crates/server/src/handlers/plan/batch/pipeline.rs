//! Plan pipeline execution: a queue of plans under one operation.
//!
//! A pipeline is a super-batch: every plan is loaded and scheduled before the
//! first turn starts, then the plans execute one after another while a single
//! registered operation keeps the slot for the whole queue.

use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_protocol::{errors, PlanExecutePipelineParams, PlanScopeEvaluation, Request, Response};

use crate::state::AppState;

use super::super::common::offload_plan;
use super::supervisor::{run_plan_batch_with, BatchOutcome, PlanBatchOptions};

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

    if params.plan_ids.is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "Pipeline must reference at least one plan",
        );
    }

    let repo_root = state.context.read().await.repo_root().to_path_buf();

    // Every plan is loaded and scheduled before the first turn starts, so an
    // unknown or unschedulable plan rejects the whole pipeline instead of
    // surfacing halfway through it.
    let mut scheduled: Vec<(String, Vec<String>)> = Vec::new();
    for plan_id in &params.plan_ids {
        let load_root = repo_root.clone();
        let load_plan_id = plan_id.clone();
        let plan =
            match offload_plan(move || PlanStorage::load_plan(&load_root, &load_plan_id)).await {
                Ok(Some(plan)) => plan,
                Ok(None) => {
                    return Response::err(
                        req.id,
                        errors::NOT_FOUND,
                        format!("Plan '{}' not found", plan_id),
                    );
                }
                Err(err) => return Response::err(req.id, errors::PLAN_LOAD_FAILED, err),
            };

        let step_ids: Vec<String> = match plan.evaluate_scope(None) {
            PlanScopeEvaluation::Ready(steps) => steps.into_iter().map(|step| step.id).collect(),
            PlanScopeEvaluation::AllCompleted
                if plan.items.is_empty() && !plan.concept.is_empty() =>
            {
                // Concept-only plan: the supervisor projects its horizon.
                Vec::new()
            }
            PlanScopeEvaluation::AllCompleted => continue,
            other => {
                return Response::err(
                    req.id,
                    errors::STEP_BLOCKED,
                    format!("Plan '{}' cannot be executed: {:?}", plan.id, other),
                );
            }
        };

        scheduled.push((plan.id, step_ids));
    }

    if scheduled.is_empty() {
        return Response::err(
            req.id,
            errors::NOTHING_TO_EXECUTE,
            "Every plan in the pipeline is already completed",
        );
    }

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
    if let Err(report) = crate::handlers::run_preflight_verification(&repo_root).await {
        let message = format!(
            "Pre-flight verification failed: the repository does not build cleanly. Pipeline execution was blocked before starting any plan.\n\n{}",
            report
        );
        return Response::err(req.id, errors::PREFLIGHT_VERIFICATION_FAILED, message);
    }

    // Execution hands control to the Coder role, so no discussion anchor
    // survives into code generation.
    state.clear_discussion_focus().await;

    let plan_ids: Vec<String> = scheduled
        .iter()
        .map(|(plan_id, _)| plan_id.clone())
        .collect();
    // The registered operation runs the whole queue, so its identity travels
    // with the acknowledgement instead of being minted at emission time: a
    // client can attribute every lifecycle event of the pipeline — each plan's
    // terminal `plan/batch_finished` and its replanning phases included — to
    // the request it just sent.
    // The coder selection and the developer directives apply to every plan of
    // the queue, so each plan starts from the chosen ladder rung and prompt.
    let options = PlanBatchOptions {
        model: params.model,
        instructions: params.instructions,
    };
    let response = Response::ok(
        req.id,
        serde_json::json!({
            "plan_ids": plan_ids,
            "operation_id": format!("op-{}", operation.id()),
        }),
    );

    let state = Arc::clone(state);
    tokio::spawn(async move {
        for (plan_id, step_ids) in scheduled {
            let outcome =
                run_plan_batch_with(&state, &plan_id, step_ids, &operation, &options).await;
            match outcome {
                BatchOutcome::Completed { steps } => {
                    tracing::info!("Pipeline plan '{}' completed: {} step(s)", plan_id, steps);
                }
                other => {
                    // A failed, blocked, cancelled, or stopped plan ends the
                    // queue: the later plans depend on the state it never
                    // reached.
                    tracing::warn!("Pipeline stopped at plan '{}': {:?}", plan_id, other);
                    break;
                }
            }
        }
    });

    response
}
