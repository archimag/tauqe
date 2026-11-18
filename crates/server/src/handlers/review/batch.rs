//! RPC adaptation for core-owned review queue execution.
use super::common::publish_review;
use crate::contracts::Project;
use crate::handlers::common::{emit, spawn_escalation_relay};
use crate::handlers::model::turn::{run_step_loop, StepLoopOptions, StepLoopReport};
use crate::handlers::PreflightVerdict;
use crate::state::{AppState, OperationGuard};
use std::path::Path;
use std::sync::Arc;
pub(super) use tauqe_core::api::ReviewBatchOutcome as BatchOutcome;
#[cfg(test)]
pub(super) use tauqe_core::api::{
    review_group_member_id as group_member_id, FailedReviewMember as FailedMember,
    ReviewBatchTelemetry as BatchTelemetry,
};
use tauqe_core::api::{ReviewBatchObserver, ReviewMemberRequest, ReviewQueueFailure};
use tauqe_core::models::ModelSelection;
use tauqe_protocol::{
    errors, events, BatchFinishStatus, BatchFinishSummary, Request, Response,
    ReviewBatchFinishedEvent, ReviewExecuteParams,
};
struct Observer {
    state: Arc<AppState>,
    operation: Arc<OperationGuard>,
    operation_id: String,
}
#[async_trait::async_trait]
impl ReviewBatchObserver for Observer {
    fn context_changed(&self, state: tauqe_core::context::ContextState) {
        crate::handlers::context::publish_context(&self.state.out, &state);
    }
    async fn relay(&self, events: tokio::sync::mpsc::Receiver<tauqe_core::providers::StreamEvent>) {
        let _ =
            spawn_escalation_relay(self.state.out.clone(), self.operation_id.clone(), events).await;
    }
    async fn publish_review(&self, root: &Path, id: &str) {
        publish_review(&self.state, root, id).await;
    }
    async fn run_member(&self, request: ReviewMemberRequest) -> StepLoopReport {
        run_step_loop(
            &self.state,
            StepLoopOptions {
                selection: request.selection,
                reasoning_effort: None,
                marker: None,
                session_id: request.session_id,
                operation: self.operation.clone(),
                plan_context: None,
                work_context: Some(request.work),
                body: request.body,
            },
        )
        .await
    }
}
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
    let operation_id = format!("op-{}", operation.id());
    let observer = Arc::new(Observer {
        state: state.clone(),
        operation: operation.clone(),
        operation_id: operation_id.clone(),
    });
    let report = state
        .application
        .execute_review_batch(
            owner_id,
            tasks,
            deferred,
            model,
            Arc::clone(&operation),
            observer,
        )
        .await;
    let status = match report.stop {
        tauqe_core::workflow::batch::BatchStop::Completed => BatchFinishStatus::Completed,
        tauqe_core::workflow::batch::BatchStop::Failed { .. } => BatchFinishStatus::Failed,
        tauqe_core::workflow::batch::BatchStop::Blocked { .. } => BatchFinishStatus::Blocked,
        tauqe_core::workflow::batch::BatchStop::Cancelled => BatchFinishStatus::Cancelled,
        tauqe_core::workflow::batch::BatchStop::Stopped => BatchFinishStatus::Stopped,
    };
    emit(
        &state.out,
        events::REVIEW_BATCH_FINISHED,
        serde_json::to_value(ReviewBatchFinishedEvent {
            operation_id,
            summary: BatchFinishSummary {
                owner_id: owner_id.into(),
                status,
                executed_count: report.executed_count,
                total_count: report.total_count,
                elapsed_seconds: report.elapsed.as_secs(),
                error: report.error,
                fixed_count: Some(report.fixed_count),
                deferred_count: Some(report.deferred),
                failed_count: Some(report.failed_count),
            },
        })
        .ok(),
    );
    drop(operation);
    report.outcome
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

    let repo_root = state.repo_root().await;

    let tauqe_core::api::PreparedReviewBatch {
        owner_id,
        members,
        addressed,
        deferred,
    } = match state
        .application
        .prepare_review_batch(&params.target.clone().project())
        .await
    {
        Ok(queue) => queue,
        Err(error) => return Response::err(req.id, queue_error_code(error.kind), error.message),
    };

    let operation = match state.try_begin_operation() {
        Ok(guard) => Arc::new(guard),
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            )
        }
    };

    // Pre-flight verification runs while the batch operation stays registered,
    // so the slot is never briefly free and a concurrent `config/set`,
    // `git/undo`, or a second `review/execute` cannot slip in mid-verification.
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
                "Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any finding.\n\n{}",
                report
            );
            return Response::err(req.id, errors::PREFLIGHT_VERIFICATION_FAILED, message);
        }
    }

    // Execution hands control to the Coder role: the discussion focus is reset
    // unconditionally so no discussion anchor survives into code generation.
    state.clear_discussion_focus().await;

    let review_id = params.target.review_id().map(str::to_string);
    let model_selection = params.model.clone();
    let developer_instructions = params.instructions.clone();
    let response = Response::ok(
        req.id,
        serde_json::json!({
            "review_id": review_id,
            "item_ids": addressed,
            "operation_id": format!("op-{}", operation.id()),
        }),
    );

    let state = Arc::clone(state);
    tokio::spawn(async move {
        match run_review_batch_with_options(
            &state,
            &owner_id,
            members,
            None,
            deferred,
            model_selection.map(crate::contracts::Project::project),
            developer_instructions,
            operation,
        )
        .await
        {
            BatchOutcome::Completed { items, telemetry } => {
                tracing::info!(
                    "Review batch '{}' completed: {} member(s), fixed: {}, deferred: {}, failed: {}",
                    owner_id,
                    items,
                    telemetry.fixed,
                    telemetry.deferred,
                    telemetry.failed
                );
            }
            BatchOutcome::Failed {
                completed,
                member,
                reason,
                telemetry,
            } => {
                tracing::warn!(
                    "Review batch '{}' stopped at {} after {} completed member(s): {} (fixed: {}, deferred: {}, failed: {})",
                    owner_id,
                    member,
                    completed,
                    reason,
                    telemetry.fixed,
                    telemetry.deferred,
                    telemetry.failed
                );
            }
            BatchOutcome::Cancelled {
                completed,
                telemetry,
            } => {
                tracing::warn!(
                    "Review batch '{}' cancelled after {} completed member(s) (fixed: {}, deferred: {}, failed: {})",
                    owner_id,
                    completed,
                    telemetry.fixed,
                    telemetry.deferred,
                    telemetry.failed
                );
            }
            BatchOutcome::Stopped {
                completed,
                telemetry,
            } => {
                tracing::info!(
                    "Review batch '{}' stopped gracefully after {} completed member(s) (fixed: {}, deferred: {}, failed: {})",
                    owner_id,
                    completed,
                    telemetry.fixed,
                    telemetry.deferred,
                    telemetry.failed
                );
            }
        }
    });

    response
}
