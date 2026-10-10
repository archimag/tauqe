use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tauqe_core::providers::{create_provider, ChatMessage, LlmProvider, StreamEvent};
use tauqe_core::review::{
    build_review_system_prompt, build_review_user_prompt, parse_review_findings, ReviewStorage,
};
use tauqe_protocol::{
    events, Event, ModelUsageEvent, ModelUsageInfo, Request, Response, ReviewContentDeltaEvent,
    ReviewDeleteParams, ReviewDeleteResult, ReviewErrorEvent, ReviewExecuteItemParams,
    ReviewFinishedEvent, ReviewGetParams, ReviewGetResult, ReviewItemStatus,
    ReviewListChangedEvent, ReviewListResult, ReviewReasoningDeltaEvent, ReviewSession,
    ReviewStartParams, ReviewStartedEvent, ReviewUpdateItemParams, ReviewUpdateItemResult,
    ReviewUpdatedEvent,
};
use tokio::sync::{mpsc, watch};

use crate::handlers::model::start_model_turn;
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
            None => {
                let selection = state.current_selection.lock().await;
                cfg.resolve_model(&selection)
            }
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

pub async fn handle_review_execute_item(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewExecuteItemParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid review execute item params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let session = match ReviewStorage::load_review(&repo_root, &params.review_id) {
        Ok(Some(s)) => s,
        Ok(None) => return Response::err(req.id, "NOT_FOUND", format!("Review session '{}' not found", params.review_id)),
        Err(err) => return Response::err(req.id, "REVIEW_LOAD_FAILED", format!("{:#}", err)),
    };

    let item = match session.find_item(params.item_id) {
        Some(i) => i,
        None => return Response::err(req.id, "NOT_FOUND", format!("Review item #{} not found in session '{}'", params.item_id, params.review_id)),
    };

    if item.status == ReviewItemStatus::Discussion {
        return Response::err(
            req.id,
            "ITEM_IN_DISCUSSION",
            format!("Review item #{} is in DISCUSSION status and cannot be executed autonomously until reviewed (change status to TODO first)", item.id),
        );
    }

    if item.status == ReviewItemStatus::Rejected {
        return Response::err(
            req.id,
            "ITEM_REJECTED",
            format!("Review item #{} is REJECTED and cannot be executed", item.id),
        );
    }

    if let Ok(Some(_)) = ReviewStorage::update_session_item_status(
        &repo_root,
        Some(&params.review_id),
        params.item_id,
        ReviewItemStatus::InProgress,
    ) {
        let _ = ReviewStorage::save_active_id(&repo_root, Some(&params.review_id));
        if let Ok(Some(updated_session)) = ReviewStorage::load_review(&repo_root, &params.review_id) {
            emit(
                &state.out,
                events::REVIEW_UPDATED,
                serde_json::to_value(ReviewUpdatedEvent { session: updated_session }).ok(),
            );
        }
        if let Ok(reviews) = ReviewStorage::load_all(&repo_root) {
            let active_id = Some(params.review_id.clone());
            emit(
                &state.out,
                events::REVIEW_LIST_CHANGED,
                serde_json::to_value(ReviewListChangedEvent { reviews, active_id }).ok(),
            );
        }
    }

    let prompt = match tauqe_core::review::prompt::format_review_step_execution_prompt(&session, params.item_id, "") {
        Ok(p) => p,
        Err(err) => return Response::err(req.id, "PROMPT_BUILD_FAILED", err),
    };

    let item_model = item.model.clone();
    match start_model_turn(state, prompt, item_model).await {
        Ok(op_id) => Response::ok(req.id, serde_json::json!({
            "operation_id": op_id,
            "review_id": params.review_id,
            "item_id": params.item_id,
        })),
        Err(err) => Response {
            id: req.id,
            result: None,
            error: Some(err),
        },
    }
}
