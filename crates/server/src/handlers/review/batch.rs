//! Review findings batch supervisor and execution entry point.
//!
//! `handle_review_execute` resolves the findings to run (a single session, a
//! single finding, or every Todo/InProgress finding across all sessions),
//! performs a deterministic pre-flight verification, and hands control to the
//! core `BatchExecution` supervisor through `ReviewItemExecutor`. The supervisor
//! enforces ordered, fail-fast traversal; findings fixed before the first failure
//! stay `Fixed`. Once it terminates, a typed `review/batch_finished` event
//! carries the aggregate status, finding counters, and elapsed time, so clients
//! can raise a single notice for the whole batch.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::ReviewStorage;
use tauqe_core::workflow::batch::{
    BatchExecution, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStop,
};
use tauqe_protocol::{
    errors, events, BatchFinishStatus, BatchFinishSummary, IssueStatus, Request, Response,
    ReviewBatchFinishedEvent, ReviewExecuteParams,
};
use tokio::sync::{mpsc, watch};

use crate::state::{next_operation_id, AppState, OperationGuard};

use super::common::{clear_auto_context, emit, offload_review};
use super::step::run_review_step;

/// Outcome of a server-driven batch of review findings.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BatchOutcome {
    Completed {
        items: usize,
    },
    Failed {
        completed: usize,
        item_id: u32,
        reason: String,
    },
    Cancelled {
        completed: usize,
    },
    Stopped {
        completed: usize,
    },
}

/// Adapter that executes a single review finding as a [`BatchStepExecutor`] step.
///
/// Finding status synchronization (InProgress/Fixed/Todo) and file preloading
/// stay inside the executor, keeping the batch supervisor agnostic of review
/// storage. Each step identifier carries `{session_id}:{item_id}` to allow
/// seamless traversal across multiple review sessions.
struct ReviewItemExecutor<'a> {
    state: &'a Arc<AppState>,
    repo_root: &'a Path,
    operation: &'a Arc<OperationGuard>,
}

#[async_trait]
impl BatchStepExecutor for ReviewItemExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        _stream_tx: mpsc::Sender<StreamEvent>,
        _cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        let (session_id, item_id) = if let Some((sess, item_part)) = step.id.split_once(':') {
            match item_part.parse::<u32>() {
                Ok(id) => (sess.to_string(), id),
                Err(_) => {
                    return Ok(BatchStepOutcome::failed(format!(
                        "Invalid review item id: {}",
                        step.id
                    )));
                }
            }
        } else {
            return Ok(BatchStepOutcome::failed(format!(
                "Invalid review step id (missing session prefix): {}",
                step.id
            )));
        };

        // Switch the active review session to match the finding being executed.
        let save_root = self.repo_root.to_path_buf();
        let session_id_owned = session_id.clone();
        let _ = offload_review(move || ReviewStorage::save_active_id(&save_root, Some(&session_id_owned))).await;

        match run_review_step(
            self.state,
            self.repo_root,
            &session_id,
            item_id,
            Arc::clone(self.operation),
        )
        .await
        {
            Ok(()) => Ok(BatchStepOutcome::succeeded()),
            Err(failure) => {
                if failure.is_domain() {
                    // The finding is already marked `Skipped` with diagnostics.
                    // Report the step as successful so the supervisor advances
                    // to the next finding instead of aborting the queue.
                    Ok(BatchStepOutcome::succeeded())
                } else {
                    Ok(BatchStepOutcome::failed(failure.into_reason()))
                }
            }
        }
    }
}

/// Runs the review findings through the core [`BatchExecution`] supervisor, which
/// enforces ordered, fail-fast traversal. Findings fixed before the first failure
/// stay FIXED. Once the supervisor terminates, a typed `review/batch_finished`
/// event carries the aggregate status, finding counters, and elapsed time, so
/// clients can raise a single notice for the whole batch.
pub(super) async fn run_review_batch(
    state: &Arc<AppState>,
    owner_id: &str,
    tasks: Vec<(String, u32)>,
    operation: Arc<OperationGuard>,
) -> BatchOutcome {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    if owner_id != "all" {
        let save_root = repo_root.clone();
        let owner_owned = owner_id.to_string();
        let _ = offload_review(move || ReviewStorage::save_active_id(&save_root, Some(&owner_owned))).await;
    }
    clear_auto_context(state).await;

    let started_at = std::time::Instant::now();

    let steps: Vec<BatchStep> = tasks
        .iter()
        .map(|(session_id, item_id)| {
            BatchStep::new(
                format!("{}:{}", session_id, item_id),
                format!("Fix review finding #{} in session '{}'", item_id, session_id),
            )
        })
        .collect();

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    // The whole batch shares one operation and one cancel channel: a cancel
    // delivered between findings is honored instead of being lost, and the slot
    // stays registered for the batch's entire lifetime.
    let cancel_rx = operation.cancel_receiver();
    // Soft stop ends the queue gracefully after the current finding: the
    // operation owns the signal, and a `model/cancel` request with `mode: "soft"`
    // raises it so the current finding finishes its turn, verification, and
    // commit before the queue stops.
    let soft_stop_rx = operation.soft_stop_receiver();

    let mut executor = ReviewItemExecutor {
        state,
        repo_root: &repo_root,
        operation: &operation,
    };

    let report = BatchExecution::new()
        .run(&steps, &mut executor, &stream_tx, &cancel_rx, &soft_stop_rx)
        .await;

    let executed_count = report.executed.len();
    let total_count = report.scheduled;
    let (status, error) = match &report.stop {
        BatchStop::Completed => (BatchFinishStatus::Completed, None),
        BatchStop::Failed { .. } => (
            BatchFinishStatus::Failed,
            report.executed.last().and_then(|r| r.outcome.error.clone()),
        ),
        BatchStop::Blocked { reason, .. } => (BatchFinishStatus::Blocked, Some(reason.clone())),
        BatchStop::Cancelled => (BatchFinishStatus::Cancelled, None),
        BatchStop::Stopped => (BatchFinishStatus::Stopped, None),
    };

    let parse_step_item_id = |step_id: &str| -> u32 {
        if let Some((_, item_part)) = step_id.split_once(':') {
            item_part.parse::<u32>().unwrap_or(0)
        } else {
            step_id.parse::<u32>().unwrap_or(0)
        }
    };

    let outcome = match report.stop {
        BatchStop::Completed => BatchOutcome::Completed {
            items: executed_count,
        },
        BatchStop::Cancelled => BatchOutcome::Cancelled {
            completed: executed_count,
        },
        BatchStop::Stopped => BatchOutcome::Stopped {
            completed: executed_count,
        },
        BatchStop::Failed { step_id } => {
            let completed = executed_count.saturating_sub(1);
            let item_id = parse_step_item_id(&step_id);
            let reason = error
                .clone()
                .unwrap_or_else(|| "finding failed without diagnostics".to_string());
            BatchOutcome::Failed {
                completed,
                item_id,
                reason,
            }
        }
        BatchStop::Blocked { step_id, reason } => {
            // Review findings have no blocked status; a semantic block stops
            // the batch and is reported as a failed finding.
            let completed = executed_count.saturating_sub(1);
            let item_id = parse_step_item_id(&step_id);
            BatchOutcome::Failed {
                completed,
                item_id,
                reason,
            }
        }
    };

    clear_auto_context(state).await;

    emit(
        &state.out,
        events::REVIEW_BATCH_FINISHED,
        serde_json::to_value(ReviewBatchFinishedEvent {
            operation_id: format!("op-{}", next_operation_id()),
            summary: BatchFinishSummary {
                owner_id: owner_id.to_string(),
                status,
                executed_count,
                total_count,
                elapsed_seconds: started_at.elapsed().as_secs(),
                error,
            },
        })
        .ok(),
    );

    outcome
}

pub async fn handle_review_execute(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewExecuteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid review execute params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();

    let (owner_id, tasks) = match params.review_id.as_deref() {
        Some(review_id) => {
            let load_root = repo_root.clone();
            let load_id = review_id.to_string();
            let session = match offload_review(move || ReviewStorage::load_review(&load_root, &load_id)).await {
                Ok(Some(s)) => s,
                Ok(None) => return Response::err(req.id, errors::NOT_FOUND, format!("Review session '{}' not found", review_id)),
                Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, err),
            };

            let item_ids: Vec<u32> = match params.item_id {
                Some(item_id) => {
                    let item = match session.find_item(item_id) {
                        Some(i) => i,
                        None => return Response::err(req.id, errors::NOT_FOUND, format!("Review item #{} not found in session '{}'", item_id, review_id)),
                    };
                    match item.status {
                        IssueStatus::Discussion => {
                            return Response::err(
                                req.id,
                                errors::ITEM_IN_DISCUSSION,
                                format!("Review item #{} is in DISCUSSION status and cannot be executed autonomously until reviewed (change status to TODO first)", item.id),
                            );
                        }
                        IssueStatus::Rejected => {
                            return Response::err(
                                req.id,
                                errors::ITEM_REJECTED,
                                format!("Review item #{} is REJECTED and cannot be executed", item.id),
                            );
                        }
                        IssueStatus::Fixed => {
                            return Response::err(
                                req.id,
                                errors::NOTHING_TO_EXECUTE,
                                format!("Review item #{} is already FIXED", item.id),
                            );
                        }
                        IssueStatus::Skipped => {
                            return Response::err(
                                req.id,
                                errors::NOTHING_TO_EXECUTE,
                                format!("Review item #{} is already SKIPPED", item.id),
                            );
                        }
                        IssueStatus::Refine => {
                            return Response::err(
                                req.id,
                                errors::ITEM_IN_REFINE,
                                format!("Review item #{} is in REFINE status and cannot be executed autonomously until it is decomposed into steps (press R)", item.id),
                            );
                        }
                        IssueStatus::Todo | IssueStatus::InProgress => vec![item_id],
                    }
                }
                None => {
                    let ids: Vec<u32> = session
                        .items
                        .iter()
                        .filter(|i| {
                            i.status == IssueStatus::Todo || i.status == IssueStatus::InProgress
                        })
                        .map(|i| i.id)
                        .collect();
                    if ids.is_empty() {
                        return Response::err(
                            req.id,
                            errors::NOTHING_TO_EXECUTE,
                            "No TODO findings to execute in this review session",
                        );
                    }
                    ids
                }
            };
            let tasks: Vec<(String, u32)> = item_ids.into_iter().map(|id| (review_id.to_string(), id)).collect();
            (review_id.to_string(), tasks)
        }
        None => {
            if params.item_id.is_some() {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "item_id cannot be specified when review_id is omitted",
                );
            }
            let list_root = repo_root.clone();
            let sessions = match offload_review(move || ReviewStorage::load_all(&list_root)).await {
                Ok(s) => s,
                Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, err),
            };
            let mut tasks: Vec<(String, u32)> = Vec::new();
            for session in sessions {
                for item in &session.items {
                    if item.status == IssueStatus::Todo || item.status == IssueStatus::InProgress {
                        tasks.push((session.id.clone(), item.id));
                    }
                }
            }
            if tasks.is_empty() {
                return Response::err(
                    req.id,
                    errors::NOTHING_TO_EXECUTE,
                    "No TODO findings to execute across all review sessions",
                );
            }
            ("all".to_string(), tasks)
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
    // `git/undo`, or a second `review/execute` cannot slip in mid-verification.
    if let Err(report) = crate::handlers::run_preflight_verification(&repo_root).await {
        let message = format!(
            "Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any finding.\n\n{}",
            report
        );
        return Response::err(req.id, errors::PREFLIGHT_VERIFICATION_FAILED, message);
    }

    // Execution hands control to the Coder role: the discussion focus is reset
    // unconditionally so no discussion anchor survives into code generation.
    state.clear_discussion_focus().await;

    let item_ids: Vec<u32> = tasks.iter().map(|(_, id)| *id).collect();
    let response = Response::ok(
        req.id,
        serde_json::json!({ "review_id": params.review_id, "item_ids": item_ids }),
    );

    let state = Arc::clone(state);
    tokio::spawn(async move {
        match run_review_batch(&state, &owner_id, tasks, operation).await {
            BatchOutcome::Completed { items } => {
                tracing::info!("Review batch '{}' completed: {} finding(s)", owner_id, items);
            }
            BatchOutcome::Failed {
                completed,
                item_id,
                reason,
            } => {
                tracing::warn!(
                    "Review batch '{}' stopped at finding #{} after {} fixed finding(s): {}",
                    owner_id,
                    item_id,
                    completed,
                    reason
                );
            }
            BatchOutcome::Cancelled { completed } => {
                tracing::warn!(
                    "Review batch '{}' cancelled after {} fixed finding(s)",
                    owner_id,
                    completed
                );
            }
            BatchOutcome::Stopped { completed } => {
                tracing::info!(
                    "Review batch '{}' stopped gracefully after {} fixed finding(s)",
                    owner_id,
                    completed
                );
            }
        }
    });

    response
}
