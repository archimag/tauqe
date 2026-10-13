use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::providers::{create_provider, ChatMessage, LlmProvider, StreamEvent};
use tauqe_core::review::{
    build_review_system_prompt, build_review_user_prompt, parse_review_findings, ReviewStorage,
};
use tauqe_core::workflow::batch::{
    BatchExecution, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStop,
};
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    events, ContextAccess, Event, ModelRole, ModelSelection, ModelUsageEvent, ModelUsageInfo,
    Request, Response, ReviewContentDeltaEvent, ReviewDeleteParams, ReviewDeleteResult,
    ReviewErrorEvent, ReviewExecuteParams, ReviewFinishedEvent, ReviewGetParams, ReviewGetResult,
    ReviewItemStatus, ReviewListChangedEvent, ReviewListResult, ReviewReasoningDeltaEvent,
    ReviewSession, ReviewStartParams, ReviewStartedEvent, ReviewUpdateItemParams,
    ReviewUpdateItemResult, ReviewUpdatedEvent,
};
use tokio::sync::{mpsc, watch};

use crate::handlers::model::execute_model_turn;
use crate::state::{next_operation_id, ActiveOperation, AppState};
use crate::transport::OutChannel;

fn emit(out: &OutChannel, method: &str, params: Option<serde_json::Value>) {
    out.send_event(&Event {
        method: method.to_string(),
        params,
    });
}

fn emit_error(out: &OutChannel, operation_id: &str, message: String) {
    emit(
        out,
        events::REVIEW_ERROR,
        serde_json::to_value(ReviewErrorEvent {
            operation_id: operation_id.to_string(),
            message,
        })
        .ok(),
    );
}

pub async fn handle_review_start(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewStartParams = match req.params {
        Some(p) => match serde_json::from_value(p) {
            Ok(p) => p,
            Err(err) => {
                return Response::err(
                    req.id,
                    "INVALID_PARAMS",
                    format!("Invalid review parameters: {}", err),
                );
            }
        },
        None => ReviewStartParams::default(),
    };

    let (provider, model) = {
        let cfg = state.config.lock().await;
        let model = match params.model.clone() {
            Some(m) => m,
            None => cfg.resolve_role(ModelRole::Reviewer),
        };
        let provider: Arc<dyn LlmProvider> = match create_provider(model.provider, &cfg) {
            Ok(p) => Arc::from(p),
            Err(err) => return Response::err(req.id, "NO_API_KEY", err.to_string()),
        };
        (provider, model)
    };

    let (repo_root, files, estimated_tokens) = {
        let ctx = state.context.read().await;
        (
            ctx.repo_root().to_path_buf(),
            ctx.read_context_files(),
            ctx.get_state().total_estimated_tokens as usize,
        )
    };
    if files.is_empty() {
        return Response::err(
            req.id,
            "EMPTY_CONTEXT",
            "Context is empty: add files to review first",
        );
    }

    let (cancel_tx, cancel_rx) = watch::channel(false);
    {
        let mut active = state.active_cancel.lock().await;
        if active.is_some() {
            return Response::err(req.id, "BUSY", "Another operation is already running");
        }
        *active = Some(ActiveOperation {
            cancel_tx,
            abort_handle: None,
        });
    }

    let op_id = format!("op-{}", next_operation_id());
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    emit(
        &state.out,
        events::REVIEW_STARTED,
        serde_json::to_value(ReviewStartedEvent {
            operation_id: op_id.clone(),
            model: model.to_string(),
            files_count: files.len(),
            estimated_tokens,
        })
        .ok(),
    );

    let target_files: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let messages = vec![
        ChatMessage::system(build_review_system_prompt()),
        ChatMessage::user(build_review_user_prompt(
            &files,
            params.user_prompt.as_deref(),
        )),
    ];

    let state_for_spawn = Arc::clone(state);
    let op_id_for_spawn = op_id.clone();

    tokio::spawn(async move {
        let state = state_for_spawn;
        let op_id = op_id_for_spawn;

        let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
        let err_tx = tx.clone();
        let model_name = model.name.clone();
        let stream_cancel = cancel_rx.clone();
        let stream_task = tokio::spawn(async move {
            if let Err(err) = provider
                .stream_chat(&model_name, messages, None, tx, stream_cancel)
                .await
            {
                let _ = err_tx.send(StreamEvent::Error(err.to_string())).await;
            }
        });
        if let Some(op) = state.active_cancel.lock().await.as_mut() {
            op.abort_handle = Some(stream_task.abort_handle());
        }

        let mut raw_markdown = String::new();
        let mut usage: Option<ModelUsageInfo> = None;
        let mut failure: Option<String> = None;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::ReasoningDelta(delta) => emit(
                    &state.out,
                    events::REVIEW_REASONING_DELTA,
                    serde_json::to_value(ReviewReasoningDeltaEvent {
                        operation_id: op_id.clone(),
                        delta,
                    })
                    .ok(),
                ),
                StreamEvent::TextDelta(delta) => {
                    raw_markdown.push_str(&delta);
                    emit(
                        &state.out,
                        events::REVIEW_CONTENT_DELTA,
                        serde_json::to_value(ReviewContentDeltaEvent {
                            operation_id: op_id.clone(),
                            delta,
                        })
                        .ok(),
                    );
                }
                StreamEvent::Usage(u) => usage = Some(u),
                StreamEvent::Done | StreamEvent::Cancelled => break,
                StreamEvent::Error(err) => {
                    failure = Some(err);
                    break;
                }
                _ => {}
            }
        }

        let cancelled = *cancel_rx.borrow();
        *state.active_cancel.lock().await = None;

        if cancelled {
            stream_task.abort();
            emit(
                &state.out,
                events::REVIEW_CANCELLED,
                Some(serde_json::json!({ "operation_id": op_id })),
            );
            return;
        }
        if let Some(message) = failure {
            stream_task.abort();
            emit_error(&state.out, &op_id, message);
            return;
        }
        if raw_markdown.trim().is_empty() {
            emit_error(&state.out, &op_id, "Model returned an empty review".to_string());
            return;
        }

        let title = params
            .user_prompt
            .as_deref()
            .and_then(|p| p.lines().next())
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| l.chars().take(60).collect::<String>())
            .unwrap_or_else(|| format!("Review {}", created_at));

        let session = ReviewSession {
            id: format!("rev-{}", created_at),
            title,
            created_at,
            model: model.to_string(),
            description: params.user_prompt.clone(),
            user_prompt: params.user_prompt.clone(),
            target_files,
            items: parse_review_findings(&raw_markdown),
            raw_markdown,
        };
        if let Err(err) = ReviewStorage::save_session(&repo_root, &session) {
            emit_error(&state.out, &op_id, format!("{:#}", err));
            return;
        }

        emit(
            &state.out,
            events::REVIEW_UPDATED,
            serde_json::to_value(ReviewUpdatedEvent {
                session: session.clone(),
            })
            .ok(),
        );

        if let Ok(reviews) = ReviewStorage::load_all(&repo_root) {
            let active_id = ReviewStorage::load_active_id(&repo_root).ok().flatten();
            emit(
                &state.out,
                events::REVIEW_LIST_CHANGED,
                serde_json::to_value(ReviewListChangedEvent { reviews, active_id }).ok(),
            );
        }

        {
            let mut history = state.history.lock().await;
            let _ = history.record_review(
                model.to_string(),
                params.user_prompt.clone(),
                session.target_files.clone(),
                session.items.len(),
            );
        }

        let usage = usage.unwrap_or_default();
        let cost = usage.cost.unwrap_or(0.0);
        let total_cost = {
            let mut total = state.total_cost.lock().await;
            *total += cost;
            *total
        };

        emit(
            &state.out,
            events::MODEL_USAGE,
            serde_json::to_value(ModelUsageEvent {
                operation_id: op_id.clone(),
                usage: usage.clone(),
                session_total_cost: total_cost,
                current_cost: Some(cost),
            })
            .ok(),
        );
        emit(
            &state.out,
            events::REVIEW_FINISHED,
            serde_json::to_value(ReviewFinishedEvent {
                operation_id: op_id,
                session,
                usage: Some(usage),
                session_total_cost: Some(total_cost),
                current_cost: Some(cost),
            })
            .ok(),
        );
    });

    Response::ok(req.id, serde_json::json!({ "operation_id": op_id }))
}

pub async fn handle_review_cancel(req: Request, state: &Arc<AppState>) -> Response {
    let active_op = state.active_cancel.lock().await.take();
    if let Some(op) = active_op {
        let _ = op.cancel_tx.send(true);
        if let Some(handle) = op.abort_handle {
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(600)).await;
                handle.abort();
            });
        }
    }
    Response::ok(req.id, serde_json::json!({ "cancelled": true }))
}

pub async fn handle_review_list(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match ReviewStorage::load_all(&repo_root) {
        Ok(reviews) => {
            let active_id = ReviewStorage::load_active_id(&repo_root).ok().flatten();
            Response::ok_typed(req.id, &ReviewListResult { reviews, active_id })
        }
        Err(err) => Response::err(req.id, "REVIEW_LIST_FAILED", format!("{:#}", err)),
    }
}

pub async fn handle_review_get(req: Request, state: &Arc<AppState>) -> Response {
    let target_id = req.params.and_then(|p| {
        if let Ok(params) = serde_json::from_value::<ReviewGetParams>(p.clone()) {
            if let Some(id) = params.id {
                if !id.trim().is_empty() {
                    return Some(id);
                }
            }
        }
        if let Ok(val) = serde_json::from_value::<serde_json::Value>(p) {
            if let Some(id_str) = val.get("id").and_then(|v| v.as_str()) {
                if !id_str.trim().is_empty() {
                    return Some(id_str.to_string());
                }
            }
        }
        None
    });

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let review_id = match &target_id {
        Some(id) => Some(id.clone()),
        None => ReviewStorage::load_active_id(&repo_root)
            .ok()
            .flatten()
            .or_else(|| {
                ReviewStorage::list_reviews(&repo_root)
                    .ok()
                    .and_then(|reviews| reviews.into_iter().next().map(|r| r.id))
            }),
    };

    if target_id.is_none() {
        if let Ok(None) = ReviewStorage::load_active_id(&repo_root) {
            if let Some(ref id) = review_id {
                let _ = ReviewStorage::save_active_id(&repo_root, Some(id.as_str()));
            }
        }
    }

    match review_id {
        Some(id) => match ReviewStorage::load_review(&repo_root, &id) {
            Ok(session) => Response::ok_typed(req.id, &ReviewGetResult { session }),
            Err(err) => Response::err(req.id, "REVIEW_GET_FAILED", format!("{:#}", err)),
        },
        None => Response::ok_typed(req.id, &ReviewGetResult { session: None }),
    }
}

pub async fn handle_review_delete(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewDeleteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid review ID"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match ReviewStorage::delete_review(&repo_root, &params.id) {
        Ok(deleted) => {
            if deleted {
                if let Ok(reviews) = ReviewStorage::load_all(&repo_root) {
                    let active_id = ReviewStorage::load_active_id(&repo_root).ok().flatten();
                    emit(
                        &state.out,
                        events::REVIEW_LIST_CHANGED,
                        serde_json::to_value(ReviewListChangedEvent { reviews, active_id }).ok(),
                    );
                }
            }
            Response::ok_typed(req.id, &ReviewDeleteResult { success: deleted })
        }
        Err(err) => Response::err(req.id, "REVIEW_DELETE_FAILED", format!("{:#}", err)),
    }
}

pub async fn handle_review_update_item(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewUpdateItemParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid review item update");
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let result = ReviewStorage::update_session_item(
        &repo_root,
        params.review_id.as_deref(),
        params.item_id,
        |item| {
            if let Some(status) = params.status {
                item.status = status;
            }
        },
    );

    match result {
        Ok(Some(item)) => {
            let session = match params.review_id.as_deref() {
                Some(id) => ReviewStorage::load_review(&repo_root, id).ok().flatten(),
                None => ReviewStorage::load_latest(&repo_root).ok().flatten(),
            };
            if let Some(session) = session {
                emit(
                    &state.out,
                    events::REVIEW_UPDATED,
                    serde_json::to_value(ReviewUpdatedEvent { session }).ok(),
                );
            }
            if let Ok(reviews) = ReviewStorage::load_all(&repo_root) {
                let active_id = ReviewStorage::load_active_id(&repo_root).ok().flatten();
                emit(
                    &state.out,
                    events::REVIEW_LIST_CHANGED,
                    serde_json::to_value(ReviewListChangedEvent { reviews, active_id }).ok(),
                );
            }
            Response::ok_typed(req.id, &ReviewUpdateItemResult { item })
        }
        Ok(None) => Response::err(
            req.id,
            "NOT_FOUND",
            format!("Review item #{} not found", params.item_id),
        ),
        Err(err) => Response::err(req.id, "REVIEW_UPDATE_FAILED", format!("{:#}", err)),
    }
}

/// Outcome of a server-driven batch of review findings.
#[derive(Debug, PartialEq, Eq)]
enum BatchOutcome {
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
}

fn publish_review(state: &Arc<AppState>, repo_root: &Path, review_id: &str) {
    if let Ok(Some(session)) = ReviewStorage::load_review(repo_root, review_id) {
        emit(
            &state.out,
            events::REVIEW_UPDATED,
            serde_json::to_value(ReviewUpdatedEvent { session }).ok(),
        );
    }
    if let Ok(reviews) = ReviewStorage::load_all(repo_root) {
        let active_id = ReviewStorage::load_active_id(repo_root).ok().flatten();
        emit(
            &state.out,
            events::REVIEW_LIST_CHANGED,
            serde_json::to_value(ReviewListChangedEvent { reviews, active_id }).ok(),
        );
    }
}

fn set_item_status(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    status: ReviewItemStatus,
) {
    if let Ok(Some(_)) =
        ReviewStorage::update_session_item_status(repo_root, Some(review_id), item_id, status)
    {
        publish_review(state, repo_root, review_id);
    }
}

fn report_item_failure(state: &Arc<AppState>, operation_id: &str, message: &str) {
    emit(
        &state.out,
        events::MODEL_ERROR,
        Some(serde_json::json!({
            "operation_id": operation_id,
            "message": message,
        })),
    );
}

/// Loads the file referenced by a finding into editable context before the turn starts.
async fn preload_item_file(state: &Arc<AppState>, repo_root: &Path, file_path: &str) {
    let clean_path = file_path.trim().trim_start_matches("./").trim_start_matches('/');
    if clean_path.is_empty() {
        return;
    }
    let max_files = state.config.lock().await.context.max_files;
    let available_files =
        tauqe_core::git::list_repository_files(Some(repo_root)).unwrap_or_default();
    let requests = vec![ContextRequest {
        path: clean_path.to_string(),
        access: ContextAccess::Editable,
    }];

    let (outcome, new_ctx_state) = {
        let mut ctx = state.context.write().await;
        let oc = apply_context_requests_detailed(&mut ctx, &requests, &available_files, max_files);
        let s = ctx.get_state();
        (oc, s)
    };

    if !outcome.added.is_empty() {
        emit(
            &state.out,
            events::CONTEXT_CHANGED,
            Some(serde_json::json!({ "state": new_ctx_state })),
        );
    }
}

/// Whether a completed turn actually applied code edits to disk.
///
/// Review findings ask the model to fix code: explanatory text alone, even when
/// accompanied by a terminal success tag, must not satisfy the finding contract.
fn turn_applied_edits(turn: &crate::handlers::model::ModelTurnResult) -> bool {
    match &turn.result {
        Some(tauqe_protocol::ModelResult::Edit { applied, error, .. }) => {
            *applied && error.is_none()
        }
        _ => false,
    }
}

/// Executes one finding in an isolated turn. The finding becomes FIXED only when the
/// turn satisfies the success contract; on any failure it returns to TODO.
async fn run_review_step(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
) -> Result<(), String> {
    let session = match ReviewStorage::load_review(repo_root, review_id) {
        Ok(Some(s)) => s,
        Ok(None) => return Err(format!("Review session '{}' not found", review_id)),
        Err(err) => return Err(format!("{:#}", err)),
    };
    let item = match session.find_item(item_id) {
        Some(i) => i.clone(),
        None => {
            return Err(format!(
                "Review item #{} not found in session '{}'",
                item_id, review_id
            ));
        }
    };

    match item.status {
        ReviewItemStatus::Fixed => return Ok(()),
        ReviewItemStatus::Discussion => {
            return Err(format!(
                "Review item #{} is in DISCUSSION status and cannot be executed until reviewed",
                item.id
            ));
        }
        ReviewItemStatus::Rejected => {
            return Err(format!("Review item #{} is REJECTED and cannot be executed", item.id));
        }
        ReviewItemStatus::Todo | ReviewItemStatus::InProgress => {}
    }

    if let Some(file_path) = &item.file_path {
        preload_item_file(state, repo_root, file_path).await;
    }
    set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::InProgress);

    let prompt = match tauqe_core::review::prompt::format_review_step_execution_prompt(
        &session, item_id, "",
    ) {
        Ok(p) => p,
        Err(err) => {
            set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::Todo);
            return Err(err);
        }
    };

    let step_selection = item
        .model
        .clone()
        .unwrap_or(ModelSelection::Role(ModelRole::Coder));

    let workflow_name = state.config.lock().await.develop.workflow.trim().to_lowercase();
    let policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy> = match workflow_name.as_str() {
        "naive" => Box::new(tauqe_core::workflow::policy::DirectApplyPolicy::new()),
        _ => Box::new(tauqe_core::workflow::git_policy::GitTransactionPolicy::new()),
    };

    match execute_model_turn(state, prompt, Some(step_selection), None, policy).await {
        Ok(turn) if turn.is_success() && turn_applied_edits(&turn) => {
            set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::Fixed);
            Ok(())
        }
        Ok(turn) => {
            set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::Todo);
            let applied = turn_applied_edits(&turn);
            let reason = turn.error.clone().unwrap_or_else(|| {
                if applied {
                    "Finding turn ended without verified changes or a success tag".to_string()
                } else {
                    "Finding turn applied no code changes".to_string()
                }
            });
            if turn.error.is_none() && !turn.is_cancelled {
                report_item_failure(state, &turn.operation_id, &reason);
            }
            Err(reason)
        }
        Err(err) => {
            set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::Todo);
            let op_id = format!("op-{}", next_operation_id());
            report_item_failure(state, &op_id, &err.message);
            Err(err.message)
        }
    }
}

/// Adapter that executes a single review finding as a [`BatchStepExecutor`] step.
///
/// Finding status synchronization (InProgress/Fixed/Todo) and file preloading
/// stay inside the executor, keeping the batch supervisor agnostic of review
/// storage.
struct ReviewItemExecutor<'a> {
    state: &'a Arc<AppState>,
    repo_root: &'a Path,
    review_id: &'a str,
}

#[async_trait]
impl BatchStepExecutor for ReviewItemExecutor<'_> {
    async fn execute_step(
        &mut self,
        step: &BatchStep,
        _stream_tx: mpsc::Sender<StreamEvent>,
        _cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<BatchStepOutcome> {
        let item_id = match step.id.parse::<u32>() {
            Ok(id) => id,
            Err(_) => {
                return Ok(BatchStepOutcome::failed(format!(
                    "Invalid review item id: {}",
                    step.id
                )));
            }
        };
        match run_review_step(self.state, self.repo_root, self.review_id, item_id).await {
            Ok(()) => Ok(BatchStepOutcome::succeeded()),
            Err(reason) => Ok(BatchStepOutcome::failed(reason)),
        }
    }
}

/// Runs the review findings through the core [`BatchExecution`] supervisor, which
/// enforces ordered, fail-fast traversal. Findings fixed before the first failure
/// stay FIXED.
async fn run_review_batch(
    state: &Arc<AppState>,
    review_id: &str,
    item_ids: Vec<u32>,
) -> BatchOutcome {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let _ = ReviewStorage::save_active_id(&repo_root, Some(review_id));

    let steps: Vec<BatchStep> = item_ids
        .iter()
        .map(|id| BatchStep::new(id.to_string(), format!("Fix review finding #{}", id)))
        .collect();

    let (stream_tx, _stream_rx) = mpsc::channel::<StreamEvent>(1);
    let (_cancel_tx, cancel_rx) = watch::channel(false);

    let mut executor = ReviewItemExecutor {
        state,
        repo_root: &repo_root,
        review_id,
    };

    let report = BatchExecution::new()
        .run(&steps, &mut executor, &stream_tx, &cancel_rx)
        .await;

    match report.stop {
        BatchStop::Completed => BatchOutcome::Completed {
            items: report.executed.len(),
        },
        BatchStop::Cancelled => BatchOutcome::Cancelled {
            completed: report.executed.len(),
        },
        BatchStop::Failed { step_id } => {
            let completed = report.executed.len().saturating_sub(1);
            let item_id = step_id.parse::<u32>().unwrap_or(0);
            let reason = report
                .executed
                .last()
                .and_then(|r| r.outcome.error.clone())
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
            let completed = report.executed.len().saturating_sub(1);
            let item_id = step_id.parse::<u32>().unwrap_or(0);
            BatchOutcome::Failed {
                completed,
                item_id,
                reason,
            }
        }
    }
}

pub async fn handle_review_execute(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewExecuteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid review execute params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let session = match ReviewStorage::load_review(&repo_root, &params.review_id) {
        Ok(Some(s)) => s,
        Ok(None) => return Response::err(req.id, "NOT_FOUND", format!("Review session '{}' not found", params.review_id)),
        Err(err) => return Response::err(req.id, "REVIEW_LOAD_FAILED", format!("{:#}", err)),
    };

    let item_ids: Vec<u32> = match params.item_id {
        Some(item_id) => {
            let item = match session.find_item(item_id) {
                Some(i) => i,
                None => return Response::err(req.id, "NOT_FOUND", format!("Review item #{} not found in session '{}'", item_id, params.review_id)),
            };
            match item.status {
                ReviewItemStatus::Discussion => {
                    return Response::err(
                        req.id,
                        "ITEM_IN_DISCUSSION",
                        format!("Review item #{} is in DISCUSSION status and cannot be executed autonomously until reviewed (change status to TODO first)", item.id),
                    );
                }
                ReviewItemStatus::Rejected => {
                    return Response::err(
                        req.id,
                        "ITEM_REJECTED",
                        format!("Review item #{} is REJECTED and cannot be executed", item.id),
                    );
                }
                ReviewItemStatus::Fixed => {
                    return Response::err(
                        req.id,
                        "NOTHING_TO_EXECUTE",
                        format!("Review item #{} is already FIXED", item.id),
                    );
                }
                ReviewItemStatus::Todo | ReviewItemStatus::InProgress => vec![item_id],
            }
        }
        None => {
            let discussion: Vec<String> = session
                .items
                .iter()
                .filter(|i| i.status == ReviewItemStatus::Discussion)
                .map(|i| format!("#{}", i.id))
                .collect();
            if !discussion.is_empty() {
                return Response::err(
                    req.id,
                    "ITEM_IN_DISCUSSION",
                    format!(
                        "Review session contains item(s) in DISCUSSION status: {}. Review them (change status to TODO) before executing the batch",
                        discussion.join(", ")
                    ),
                );
            }
            let ids: Vec<u32> = session
                .items
                .iter()
                .filter(|i| {
                    i.status == ReviewItemStatus::Todo || i.status == ReviewItemStatus::InProgress
                })
                .map(|i| i.id)
                .collect();
            if ids.is_empty() {
                return Response::err(
                    req.id,
                    "NOTHING_TO_EXECUTE",
                    "No TODO findings to execute in this review session",
                );
            }
            ids
        }
    };

    if state.active_cancel.lock().await.is_some() {
        return Response::err(
            req.id,
            "OPERATION_IN_PROGRESS",
            "Another operation is already running",
        );
    }

    if let Err(report) = crate::handlers::run_preflight_verification(&repo_root).await {
        let message = format!(
            "Pre-flight verification failed: the repository does not build cleanly. Batch execution was blocked before starting any finding.\n\n{}",
            report
        );
        return Response::err(req.id, "PREFLIGHT_VERIFICATION_FAILED", message);
    }

    let response = Response::ok(
        req.id,
        serde_json::json!({ "review_id": params.review_id, "item_ids": item_ids }),
    );

    let state = Arc::clone(state);
    let review_id = params.review_id.clone();
    tokio::spawn(async move {
        match run_review_batch(&state, &review_id, item_ids).await {
            BatchOutcome::Completed { items } => {
                tracing::info!("Review '{}' batch completed: {} finding(s)", review_id, items);
            }
            BatchOutcome::Failed {
                completed,
                item_id,
                reason,
            } => {
                tracing::warn!(
                    "Review '{}' batch stopped at finding #{} after {} fixed finding(s): {}",
                    review_id,
                    item_id,
                    completed,
                    reason
                );
            }
            BatchOutcome::Cancelled { completed } => {
                tracing::warn!(
                    "Review '{}' batch cancelled after {} fixed finding(s)",
                    review_id,
                    completed
                );
            }
        }
    });

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);
    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tauqe-test-review-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::create_dir_all(&path);
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_state(dir: &TestDir) -> Arc<AppState> {
        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.path().to_path_buf(),
            hist_tx,
            out,
        )
    }

    fn test_item(id: u32, status: ReviewItemStatus, file: &str) -> tauqe_protocol::ReviewItem {
        tauqe_protocol::ReviewItem {
            id,
            title: format!("Finding {}", id),
            model: None,
            severity: tauqe_protocol::ReviewSeverity::Info,
            status,
            file_path: Some(file.to_string()),
            line_range: None,
            body: "Details".to_string(),
        }
    }

    fn test_session(id: &str, items: Vec<tauqe_protocol::ReviewItem>) -> ReviewSession {
        ReviewSession {
            id: id.to_string(),
            title: format!("Session {}", id),
            created_at: 100,
            model: "test-model".to_string(),
            description: None,
            user_prompt: None,
            target_files: vec!["foo.rs".to_string()],
            items,
            raw_markdown: String::new(),
        }
    }

    #[tokio::test]
    async fn test_handle_review_start_defaults_to_reviewer_role() {
        let dir = TestDir::new();
        let target_file = dir.path().join("file.rs");
        std::fs::write(&target_file, "fn foo() {}\n").unwrap();

        let mut config = tauqe_core::config::AppConfig::default();
        config.models.reviewer = Some(tauqe_protocol::ModelRef::openrouter("test/reviewer-model"));
        config.models.advisor = Some(tauqe_protocol::ModelRef::openrouter("test/advisor-model"));

        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);

        state
            .context
            .write()
            .await
            .add_file("file.rs", ContextAccess::ReadOnly)
            .unwrap();

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::REVIEW_START.to_string(),
            params: None,
        };
        let res = handle_review_start(req, &state).await;
        let err = res.error.expect("should fail without api key");
        assert_eq!(err.code, "NO_API_KEY");
    }

    #[tokio::test]
    async fn test_handle_review_execute_validations() {
        let dir = TestDir::new();
        let state = test_state(&dir);
        let make_req = |id: u64, params: Option<serde_json::Value>| Request {
            id: tauqe_protocol::RequestId::Number(id),
            method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
            params,
        };

        let res = handle_review_execute(make_req(1, None), &state).await;
        assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

        let res = handle_review_execute(
            make_req(2, Some(serde_json::json!({ "review_id": "non-existent", "item_id": 1 }))),
            &state,
        )
        .await;
        assert_eq!(res.error.unwrap().code, "NOT_FOUND");

        let session = test_session(
            "rev-val-1",
            vec![
                test_item(1, ReviewItemStatus::Discussion, "foo.rs"),
                test_item(2, ReviewItemStatus::Rejected, "foo.rs"),
                test_item(3, ReviewItemStatus::Fixed, "foo.rs"),
            ],
        );
        ReviewStorage::save_session(dir.path(), &session).unwrap();

        let cases = [
            (Some(1), "ITEM_IN_DISCUSSION"),
            (Some(2), "ITEM_REJECTED"),
            (Some(3), "NOTHING_TO_EXECUTE"),
            (Some(9), "NOT_FOUND"),
            (None, "ITEM_IN_DISCUSSION"),
        ];
        for (idx, (item_id, code)) in cases.iter().enumerate() {
            let params = serde_json::json!({ "review_id": "rev-val-1", "item_id": item_id });
            let res = handle_review_execute(make_req(10 + idx as u64, Some(params)), &state).await;
            assert_eq!(res.error.unwrap().code, *code);
        }
    }

    #[tokio::test]
    async fn test_review_batch_fail_fast_resets_status_and_preloads_file() {
        let dir = TestDir::new();
        let _ = tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();

        let target_path = dir.path().join("src");
        std::fs::create_dir_all(&target_path).unwrap();
        std::fs::write(target_path.join("target.rs"), "fn target() {}\n").unwrap();
        tauqe_core::git::cmd::stage_files(dir.path(), &["src/target.rs".to_string()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add target", false).unwrap();

        let session = test_session(
            "rev-exec-1",
            vec![
                test_item(1, ReviewItemStatus::Todo, "./src/target.rs"),
                test_item(2, ReviewItemStatus::Todo, "./src/target.rs"),
            ],
        );
        ReviewStorage::save_session(dir.path(), &session).unwrap();
        let state = test_state(&dir);

        // Without an API key the first turn cannot start: the batch must stop at finding 1.
        let outcome = run_review_batch(&state, "rev-exec-1", vec![1, 2]).await;
        match outcome {
            BatchOutcome::Failed {
                completed, item_id, ..
            } => {
                assert_eq!(completed, 0);
                assert_eq!(item_id, 1);
            }
            other => panic!("Expected Failed, got {:?}", other),
        }

        // The finding file was preloaded as editable before the turn.
        {
            let ctx = state.context.read().await;
            assert!(ctx.is_editable("src/target.rs").unwrap());
        }

        // Fail-Fast: statuses are back to TODO and nothing is marked FIXED.
        let updated = ReviewStorage::load_review(dir.path(), "rev-exec-1")
            .unwrap()
            .unwrap();
        assert!(updated.items.iter().all(|i| i.status == ReviewItemStatus::Todo));

        let active_id = ReviewStorage::load_active_id(dir.path()).unwrap();
        assert_eq!(active_id.as_deref(), Some("rev-exec-1"));
    }

    #[tokio::test]
    async fn test_review_batch_skips_fixed_and_stops_on_rejected() {
        let dir = TestDir::new();
        let session = test_session(
            "rev-skip",
            vec![
                test_item(1, ReviewItemStatus::Fixed, "foo.rs"),
                test_item(2, ReviewItemStatus::Rejected, "foo.rs"),
            ],
        );
        ReviewStorage::save_session(dir.path(), &session).unwrap();
        let state = test_state(&dir);

        let outcome = run_review_batch(&state, "rev-skip", vec![1, 2]).await;
        match outcome {
            BatchOutcome::Failed {
                completed, item_id, ..
            } => {
                assert_eq!(completed, 1);
                assert_eq!(item_id, 2);
            }
            other => panic!("Expected Failed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_review_execute_blocked_by_failed_preflight() {
        let dir = TestDir::new();
        let session = test_session(
            "rev-preflight",
            vec![
                test_item(1, ReviewItemStatus::Todo, "foo.rs"),
                test_item(2, ReviewItemStatus::Todo, "foo.rs"),
            ],
        );
        ReviewStorage::save_session(dir.path(), &session).unwrap();
        // A malformed manifest makes the deterministic toolchain pipeline fail.
        std::fs::write(dir.path().join("Cargo.toml"), "[package\n").unwrap();
        let state = test_state(&dir);

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
            params: Some(serde_json::json!({ "review_id": "rev-preflight" })),
        };
        let res = handle_review_execute(req, &state).await;

        let err = res.error.expect("preflight must block the batch");
        assert_eq!(err.code, "PREFLIGHT_VERIFICATION_FAILED");
        assert!(err.message.contains("Pre-flight verification failed"));

        // The failure is detected before any turn starts: no active operation.
        assert!(state.active_cancel.lock().await.is_none());

        // No finding was touched: every status stays TODO.
        let reloaded = ReviewStorage::load_review(dir.path(), "rev-preflight")
            .unwrap()
            .unwrap();
        assert!(reloaded.items.iter().all(|i| i.status == ReviewItemStatus::Todo));
    }

    #[test]
    fn test_turn_applied_edits_requires_applied_change() {
        use crate::handlers::model::ModelTurnResult;

        let applied = ModelTurnResult {
            operation_id: "op-1".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "fix".to_string(),
                edits: vec![],
                proposal: None,
                applied: true,
                error: None,
                changed_files: vec!["src/lib.rs".to_string()],
                commit_hash: Some("abc".to_string()),
            }),
            assistant_text: "done".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: false,
            blocked: None,
        };
        assert!(applied.is_success());
        assert!(turn_applied_edits(&applied));

        // Explanatory text with a terminal success tag satisfies an interactive
        // turn but must not complete a code-modifying review finding.
        let answer = ModelTurnResult {
            operation_id: "op-2".to_string(),
            result: Some(tauqe_protocol::ModelResult::Answer {
                text: "Nothing to fix.".to_string(),
            }),
            assistant_text: "Nothing to fix.".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: true,
            blocked: None,
        };
        assert!(answer.is_success());
        assert!(!turn_applied_edits(&answer));

        let failed = ModelTurnResult {
            operation_id: "op-3".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "fix".to_string(),
                edits: vec![],
                proposal: None,
                applied: false,
                error: Some("compiler error".to_string()),
                changed_files: vec![],
                commit_hash: None,
            }),
            assistant_text: String::new(),
            is_cancelled: false,
            error: Some("compiler error".to_string()),
            has_terminal_success: false,
            blocked: None,
        };
        assert!(!turn_applied_edits(&failed));
    }
}
