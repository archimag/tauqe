//! RPC adaptation for core-owned review queue execution.
use crate::contracts::Project;
use crate::state::AppState;
#[cfg(test)]
use crate::state::OperationGuard;
use std::sync::Arc;
#[cfg(test)]
use tauqe_core::models::ModelSelection;
#[cfg(test)]
pub(super) use tauqe_flow::api::ReviewBatchOutcome as BatchOutcome;
use tauqe_flow::api::ReviewQueueFailure;
#[cfg(test)]
pub(super) use tauqe_flow::api::{
    review_group_member_id as group_member_id, FailedReviewMember as FailedMember,
    ReviewBatchTelemetry as BatchTelemetry,
};
use tauqe_protocol::{errors, Request, Response, ReviewExecuteParams};
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_review_batch_with_options(
    state: &Arc<AppState>,
    owner_id: &str,
    tasks: Vec<String>,
    _step_id: Option<String>,
    deferred: usize,
    model: Option<ModelSelection>,
    _instructions: Option<String>,
    operation: Arc<OperationGuard>,
) -> BatchOutcome {
    state
        .application
        .test_run_review_batch(owner_id, tasks, deferred, model, operation, observer(state))
        .await
}

fn observer(state: &AppState) -> Arc<crate::publication::Publisher> {
    Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    })
}

#[cfg(test)]
pub(super) async fn run_review_batch(
    state: &Arc<AppState>,
    owner_id: &str,
    tasks: Vec<(String, u32)>,
    operation: Arc<OperationGuard>,
) -> BatchOutcome {
    let members = tasks
        .into_iter()
        .map(|(session, id)| format!("{session}:{id}"))
        .collect();
    run_review_batch_with_options(state, owner_id, members, None, 0, None, None, operation).await
}
fn queue_error_code(kind: ReviewQueueFailure) -> &'static str {
    match kind {
        ReviewQueueFailure::ArchitecturalGroupExecutionRequired => {
            errors::ARCHITECTURAL_GROUP_EXECUTION_REQUIRED
        }
        ReviewQueueFailure::GroupUnresolvedQuestions => errors::GROUP_UNRESOLVED_QUESTIONS,
        ReviewQueueFailure::InvalidParams => errors::INVALID_PARAMS,
        ReviewQueueFailure::ItemInDiscussion => errors::ITEM_IN_DISCUSSION,
        ReviewQueueFailure::ItemRejected => errors::ITEM_REJECTED,
        ReviewQueueFailure::NothingToExecute => errors::NOTHING_TO_EXECUTE,
        ReviewQueueFailure::NotFound => errors::NOT_FOUND,
        ReviewQueueFailure::ReviewLoadFailed => errors::REVIEW_LOAD_FAILED,
    }
}
pub async fn handle_review_execute(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewExecuteParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid review execute params",
            )
        }
    };

    use tauqe_flow::api::ReviewLaunchError;
    match state
        .application
        .execute_review(
            params.target.project(),
            params.model.map(Project::project),
            observer(state),
        )
        .await
    {
        Ok(started) => Response::ok(
            req.id,
            serde_json::json!({
                "review_id": started.review_id,
                "item_ids": started.item_ids,
                "operation_id": started.operation_id,
            }),
        ),
        Err(error) => {
            let code = match &error {
                ReviewLaunchError::Busy => errors::OPERATION_IN_PROGRESS,
                ReviewLaunchError::Queue(error) => queue_error_code(error.kind),
                ReviewLaunchError::Cancelled => errors::CANCELLED,
                ReviewLaunchError::Verification(_) => errors::PREFLIGHT_VERIFICATION_FAILED,
            };
            Response::err(req.id, code, error.to_string())
        }
    }
}
