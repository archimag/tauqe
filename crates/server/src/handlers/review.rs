use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::edits::stream::json::JsonFieldStreamFilter;
use tauqe_core::providers::{
    create_provider, ChatMessage, JsonSchemaDefinition, LlmProvider, ResponseFormat, StreamEvent,
};
use tauqe_core::review::parser::ReviewFindingsOutput;
use tauqe_core::review::{
    build_review_system_prompt, build_review_user_prompt, parse_review_findings,
    parse_review_output, review_findings_response_format, ReviewStorage,
};
use tauqe_core::workflow::batch::{
    BatchExecution, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStop,
};
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    errors, events, BatchFinishStatus, BatchFinishSummary, ContextAccess, Event, ModelDeltaEvent,
    ModelErrorEvent, ModelFinishedEvent, ModelRole, ModelSelection, ModelStartedEvent,
    ModelUsageEvent, ModelUsageInfo, Plan, PlanItem, PlanItemStatus, Request, Response,
    ReviewBatchFinishedEvent, ReviewCancelParams, ReviewContentDeltaEvent, ReviewDeleteParams,
    ReviewDeleteResult, ReviewErrorEvent, ReviewExecuteParams, ReviewFinishedEvent,
    ReviewGetParams, ReviewGetResult, ReviewItem, ReviewItemStatus, ReviewListChangedEvent,
    ReviewListResult, ReviewReasoningDeltaEvent, ReviewRefineItemParams, ReviewRefineItemResult,
    ReviewSession, ReviewStartParams, ReviewStartedEvent, ReviewUpdateItemParams,
    ReviewUpdateItemResult, ReviewUpdatedEvent, TurnPhase, TurnPhaseEvent,
};
use tokio::sync::{mpsc, watch};

use crate::handlers::model::{execute_model_turn, StepTurnOptions};
use crate::handlers::plan::{
    find_duplicate_item_ids, preload_step_files, resolve_architect_provider, run_architect_stream,
    ArchitectStreamError, ArchitectStreamOutcome,
};
use crate::state::{next_operation_id, AppState, OperationGuard};
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
                    errors::INVALID_PARAMS,
                    format!("Invalid review parameters: {}", err),
                );
            }
        },
        None => ReviewStartParams::default(),
    };

    let (provider, model, reasoning_effort) = {
        let cfg = state.config.lock().await.config.clone();
        // The reviewer role list is the single source for the audit model and
        // its reasoning depth; an explicit model override replaces only the
        // model and leaves the role's reasoning effort in place.
        let reviewer_entries = cfg.models.role_entries(ModelRole::Reviewer);
        let reviewer_entry = reviewer_entries
            .iter()
            .find(|entry| params.model.as_ref().is_none_or(|m| &entry.model == m))
            .cloned()
            .or_else(|| reviewer_entries.into_iter().next());
        let model = match params.model.clone() {
            Some(m) => m,
            None => reviewer_entry
                .as_ref()
                .map(|entry| entry.model.clone())
                .unwrap_or_else(|| cfg.resolve_role(ModelRole::Reviewer)),
        };
        let reasoning_effort = reviewer_entry.and_then(|entry| entry.effort);
        // Deterministic unit tests inject a scripted provider through the
        // state's test seam; production builds construct a real network
        // provider from configuration.
        #[cfg(test)]
        let provider: Arc<dyn LlmProvider> = match state.provider_override.lock().await.clone() {
            Some(provider) => provider,
            None => match create_provider(model.provider, &cfg) {
                Ok(p) => Arc::from(p),
                Err(err) => return Response::err(req.id, errors::NO_API_KEY, err.to_string()),
            },
        };

        #[cfg(not(test))]
        let provider: Arc<dyn LlmProvider> = match create_provider(model.provider, &cfg) {
            Ok(p) => Arc::from(p),
            Err(err) => return Response::err(req.id, errors::NO_API_KEY, err.to_string()),
        };

        (provider, model, reasoning_effort)
    };

    let (repo_root, mut files, mut estimated_tokens) = {
        let ctx = state.context.read().await;
        (
            ctx.repo_root().to_path_buf(),
            ctx.read_context_files(),
            ctx.get_state().total_estimated_tokens as usize,
        )
    };

    if let Some(pattern) = params.path_pattern.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        let available_files = tauqe_core::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
        let existing_paths: std::collections::HashSet<String> = files.iter().map(|f| f.path.clone()).collect();
        // Compile once and match every repository file against the compiled
        // pattern instead of re-normalizing it per path.
        let compiled = tauqe_protocol::GlobPattern::new(pattern);
        let mut matched: Vec<String> = available_files
            .into_iter()
            .filter(|path| !existing_paths.contains(path) && compiled.matches(path))
            .collect();
        matched.sort();

        for path in matched {
            if tauqe_core::context::verify_path_within_repo(&repo_root, &path).is_err() {
                continue;
            }
            let full_path = repo_root.join(&path);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                let tokens = tauqe_core::prompt::estimate_tokens_from_bytes(content.len() as u64) as usize;
                estimated_tokens += tokens;
                files.push(tauqe_core::context::ContextFileContent {
                    path,
                    access: ContextAccess::ReadOnly,
                    content,
                });
            }
        }
    }

    if files.is_empty() {
        return Response::err(
            req.id,
            errors::EMPTY_CONTEXT,
            "Context is empty: add files to review first",
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
            model: model.clone(),
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
    // Review synthesis is a non-code operation: the enforced JSON schema turns
    // the model output into machine-readable findings instead of parsed markdown.
    let response_format = review_findings_response_format();

    let state_for_spawn = Arc::clone(state);
    let op_id_for_spawn = op_id.clone();

    tokio::spawn(async move {
        let state = state_for_spawn;
        let op_id = op_id_for_spawn;
        let cancel_rx = operation.cancel_receiver();

        let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
        let err_tx = tx.clone();
        let model_name = model.name.clone();
        let stream_cancel = cancel_rx.clone();
        let stream_task = tokio::spawn(async move {
            if let Err(err) = provider
                .stream_chat_with_effort(
                    &model_name,
                    messages,
                    Some(response_format),
                    reasoning_effort,
                    tx,
                    stream_cancel,
                )
                .await
            {
                let _ = err_tx.send(StreamEvent::Error(err.to_string())).await;
            }
        });
        operation.set_abort_handle(stream_task.abort_handle());

        // The provider streams raw Structured Output JSON; this filter decodes
        // only the `summary` string so JSON syntax never reaches the client.
        let mut filter = JsonFieldStreamFilter::<ReviewFindingsOutput>::new("summary");
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
                StreamEvent::TextDelta(chunk) => {
                    // Forward only cleaned summary text; raw JSON deltas are
                    // consumed by the filter and never leave the server.
                    if let Some(delta) = filter.push_chunk(&chunk) {
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
        // Capture the raw payload before finalizing: `finish` consumes the
        // filter, while the full response is still needed for the tolerant
        // fallback parser and the persisted `raw_markdown`.
        let raw_buffer = filter.buffer().to_string();
        if raw_buffer.trim().is_empty() {
            emit_error(&state.out, &op_id, "Model returned an empty review".to_string());
            return;
        }

        // A clean deserialization yields the typed payload; a malformed tail
        // degrades gracefully to the tolerant text parser instead of dropping
        // an otherwise usable review.
        let (summary, items) = match filter.finish() {
            Ok(output) => {
                let trimmed = output.summary.trim();
                let summary = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                (summary, parse_review_findings(&raw_buffer))
            }
            Err(_) => parse_review_output(&raw_buffer),
        };

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
            model: model.clone(),
            description: summary.or_else(|| params.user_prompt.clone()),
            user_prompt: params.user_prompt.clone(),
            target_files,
            items,
            raw_markdown: raw_buffer,
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
                session.description.clone(),
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
    let params: ReviewCancelParams = req
        .params
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();

    if params.mode == Some(tauqe_protocol::model::CancelMode::Soft) {
        state.soft_stop_active_operation();
        return Response::ok(req.id, serde_json::json!({ "cancelled": true }));
    }

    let active_op = state.take_active_operation();
    if let Some(op) = active_op {
        let _ = op.cancel_tx.send(true);
        if let Some(handle) = op.abort_handle {
            // Aborting is a last resort: the streaming task unwinds
            // cooperatively on the cancel signal, and a hard abort is deferred
            // while a git transaction is still open so a half-finished
            // checkpoint or step commit is never abandoned mid-transaction.
            let state = Arc::clone(state);
            tokio::spawn(async move {
                tokio::time::sleep(crate::handlers::model::CANCEL_UNWIND_GRACE).await;
                if !state.git_transaction_active.load(Ordering::SeqCst) {
                    handle.abort();
                }
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
        Err(err) => Response::err(req.id, errors::REVIEW_LIST_FAILED, format!("{:#}", err)),
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
            Err(err) => Response::err(req.id, errors::REVIEW_GET_FAILED, format!("{:#}", err)),
        },
        None => Response::ok_typed(req.id, &ReviewGetResult { session: None }),
    }
}

pub async fn handle_review_delete(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewDeleteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid review ID"),
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
        Err(err) => Response::err(req.id, errors::REVIEW_DELETE_FAILED, format!("{:#}", err)),
    }
}

pub async fn handle_review_update_item(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewUpdateItemParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid review item update");
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
            if let Some(notes) = params.resolution_notes.clone() {
                let trimmed = notes.trim();
                item.resolution_notes = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
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
            errors::NOT_FOUND,
            format!("Review item #{} not found", params.item_id),
        ),
        Err(err) => Response::err(req.id, errors::REVIEW_UPDATE_FAILED, format!("{:#}", err)),
    }
}

/// Handles an Architect-driven decomposition of a single review finding into an
/// embedded subplan carried by `ReviewItem::subplan`.
///
/// Plan refinement and finding decomposition are conceptually the same operation:
/// both dispatch to the Architect role, so the handler mirrors the plan horizon
/// synthesis flow and reuses its streaming infrastructure.
pub async fn handle_review_refine_item(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewRefineItemParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid review refine item params",
            );
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();

    let session = match params.review_id.as_deref() {
        Some(id) => match ReviewStorage::load_review(&repo_root, id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Response::err(
                    req.id,
                    errors::NOT_FOUND,
                    format!("Review session '{}' not found", id),
                );
            }
            Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, format!("{:#}", err)),
        },
        None => match ReviewStorage::load_latest(&repo_root) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Response::err(
                    req.id,
                    errors::NOT_FOUND,
                    "No review session is available to refine",
                );
            }
            Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, format!("{:#}", err)),
        },
    };

    let item = match session.find_item(params.item_id) {
        Some(i) => i.clone(),
        None => {
            return Response::err(
                req.id,
                errors::NOT_FOUND,
                format!(
                    "Review item #{} not found in session '{}'",
                    params.item_id, session.id
                ),
            );
        }
    };

    let operation = match state.try_begin_operation() {
        Ok(guard) => guard,
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            );
        }
    };

    execute_review_finding_refine(
        req.id,
        state,
        &repo_root,
        session,
        item,
        params.model,
        params.instructions,
        &operation,
    )
    .await
}

/// Unified engine for Architect review finding decomposition.
///
/// Drives streaming structured communication with the architect model, runs
/// multi-round context discovery on `needs_context`, validates the synthesized
/// subplan, and applies it while preserving the immutable `Done` foundation of any
/// previously completed sub-steps. Session history is recorded strictly after the
/// subplan is persisted, so a failed synthesis never poisons the audit trail.
#[allow(clippy::too_many_arguments)]
async fn execute_review_finding_refine(
    req_id: tauqe_protocol::RequestId,
    state: &Arc<AppState>,
    repo_root: &Path,
    session: ReviewSession,
    item: ReviewItem,
    custom_model: Option<ModelSelection>,
    instructions: Option<String>,
    operation: &OperationGuard,
) -> Response {
    let (
        provider,
        model,
        reasoning_effort,
        alias,
        repomap_budget,
        max_discovery_rounds,
        files_token_budget,
    ) = {
        let cfg = state.config.lock().await.config.clone();
        let architect_entry = cfg.models.role_entries(ModelRole::Architect).into_iter().next();
        let model = match &custom_model {
            Some(m) => cfg.resolve_model(m),
            None => cfg.resolve_role(ModelRole::Architect),
        };
        let reasoning_effort = cfg.models.resolve_effort(ModelRole::Architect);
        let alias = if custom_model.is_some() {
            None
        } else {
            architect_entry.and_then(|entry| entry.alias)
        };
        let provider = match resolve_architect_provider(state, &cfg, model.provider).await {
            Ok(p) => p,
            Err(err) => return Response::err(req_id, errors::NO_API_KEY, err),
        };
        (
            provider,
            model,
            reasoning_effort,
            alias,
            cfg.context.repomap_token_budget,
            cfg.context.max_discovery_rounds,
            cfg.context.files_token_budget,
        )
    };

    clear_auto_context(state).await;

    let op_id = format!("op-{}", next_operation_id());

    emit(
        &state.out,
        events::MODEL_STARTED,
        serde_json::to_value(ModelStartedEvent {
            operation_id: op_id.clone(),
            model: model.clone(),
            role: Some(ModelRole::Architect),
            alias,
            effort: reasoning_effort,
            plan_context: None,
            is_batch: false,
        })
        .ok(),
    );

    let intro_header = format!(
        "# Review Finding Decomposition: [{}#{}] {}\n*Analyzing codebase contours and decomposing the finding into executable sub-steps...*\n\n---\n\n",
        session.id, item.id, item.title
    );
    emit(
        &state.out,
        events::MODEL_TEXT_DELTA,
        serde_json::to_value(ModelDeltaEvent {
            operation_id: op_id.clone(),
            delta: intro_header.clone(),
        })
        .ok(),
    );

    {
        let ctx = state.context.read().await;
        let context_paths: Vec<String> = ctx.get_state().items.into_iter().map(|it| it.path).collect();
        let mut turn_prompt = format!(
            "# Review Finding Refinement: [{}#{}] {}\n",
            session.id, item.id, item.title
        );
        if let Some(ins) = &instructions {
            let trimmed = ins.trim();
            if !trimmed.is_empty() {
                turn_prompt.push_str(&format!("\nDirectives: {}\n", trimmed));
            }
        }
        let _ = state.history.lock().await.record_turn(turn_prompt, context_paths);
    }

    let mut discovery_round = 0usize;
    let mut feedback: Option<String> = None;
    let mut accumulated_full_text = intro_header.clone();

    let output = loop {
        emit(
            &state.out,
            events::TURN_PHASE,
            serde_json::to_value(TurnPhaseEvent {
                operation_id: op_id.clone(),
                phase: TurnPhase::Proposal,
                round: Some(discovery_round + 1),
                max_rounds: None,
                detail: Some("Review Finding Decomposition".to_string()),
            })
            .ok(),
        );

        let (context_files, repo_map) = {
            let ctx = state.context.read().await;
            let files = ctx.read_context_files();
            let context_paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
            let all_files =
                tauqe_core::git::list_repository_files(Some(repo_root)).unwrap_or_default();
            let rmap = tauqe_core::repomap::generate_repo_map(
                repo_root,
                &all_files,
                &context_paths,
                repomap_budget,
            )
            .ok();
            (files, rmap)
        };

        let sys_prompt = tauqe_core::review::prompt::build_review_finding_refine_system_prompt();
        let mut user_prompt = tauqe_core::review::prompt::build_review_finding_refine_user_prompt(
            &session,
            &item,
            &context_files,
            repo_map.as_deref(),
            instructions.as_deref(),
        );

        if let Some(ref fb) = feedback {
            user_prompt.push_str("\n\n");
            user_prompt.push_str(fb);
        }

        if discovery_round >= max_discovery_rounds {
            user_prompt.push_str(&format!(
                "\n\n<context_feedback>\nReached maximum context discovery rounds limit ({}). You MUST now finalize the subplan with status 'success' or 'blocked'. Do not request further files.\n</context_feedback>",
                max_discovery_rounds
            ));
        }

        let messages = vec![
            ChatMessage::system(sys_prompt),
            ChatMessage::user(user_prompt),
        ];

        let schema_val = tauqe_core::plan::prompt::plan_refine_response_schema_definition();
        let schema_def: JsonSchemaDefinition = match serde_json::from_value(schema_val) {
            Ok(s) => s,
            Err(err) => {
                return Response::err(req_id, errors::SCHEMA_ERROR, err.to_string());
            }
        };
        let response_format = Some(ResponseFormat::JsonSchema {
            json_schema: schema_def,
        });

        let stream_outcome = match run_architect_stream(
            state,
            Arc::clone(&provider),
            model.name.clone(),
            messages,
            response_format,
            reasoning_effort,
            operation,
            &op_id,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(ArchitectStreamError::Cancelled) => {
                clear_auto_context(state).await;
                return Response::err(
                    req_id,
                    errors::CANCELLED,
                    "Review finding decomposition was cancelled",
                );
            }
            Err(ArchitectStreamError::ModelError(err)) => {
                clear_auto_context(state).await;
                return Response::err(req_id, errors::MODEL_ERROR, err);
            }
        };

        let ArchitectStreamOutcome {
            raw_text,
            streamed_explanation_bytes,
        } = stream_outcome;

        let output = match tauqe_core::plan::prompt::parse_plan_refinement_output(&raw_text) {
            Ok(out) => out,
            Err(err) => {
                clear_auto_context(state).await;
                let err_msg = format!("Failed to parse architect refinement output: {err}");
                emit(
                    &state.out,
                    events::MODEL_ERROR,
                    serde_json::to_value(ModelErrorEvent {
                        operation_id: op_id.clone(),
                        message: err_msg.clone(),
                    })
                    .ok(),
                );
                return Response::err(req_id, errors::PARSE_ERROR, err_msg);
            }
        };

        if streamed_explanation_bytes == 0 && !output.explanation.trim().is_empty() {
            emit(
                &state.out,
                events::MODEL_TEXT_DELTA,
                serde_json::to_value(ModelDeltaEvent {
                    operation_id: op_id.clone(),
                    delta: output.explanation.clone(),
                })
                .ok(),
            );
        }
        accumulated_full_text.push_str(&output.explanation);

        if output.status.eq_ignore_ascii_case("needs_context")
            && discovery_round < max_discovery_rounds
        {
            let requests: Vec<ContextRequest> = output
                .context_requests
                .iter()
                .map(|p| ContextRequest {
                    path: p.clone(),
                    access: ContextAccess::ReadOnly,
                })
                .collect();

            let (outcome, new_ctx_state) = {
                let mut ctx = state.context.write().await;
                let available_files =
                    tauqe_core::git::list_repository_files(Some(repo_root)).unwrap_or_default();
                let oc = apply_context_requests_detailed(
                    &mut ctx,
                    &requests,
                    &available_files,
                    files_token_budget,
                );
                let s = ctx.get_state();
                (oc, s)
            };

            if let Some(limit) = outcome.limit_exceeded {
                let warn_msg = format!(
                    "Context files token budget exceeded (files_token_budget = {}). Architect requested {} new token(s) in file(s) [{}], but context already has {} tokens. The finding decomposition is too broad and should be scoped down.",
                    limit.token_budget,
                    limit.requested_tokens,
                    limit.requested_files.join(", "),
                    limit.current_tokens
                );
                tracing::warn!("{}", warn_msg);
                emit(
                    &state.out,
                    events::MODEL_FINISHED,
                    serde_json::to_value(ModelFinishedEvent {
                        operation_id: op_id.clone(),
                        full_text: format!("{}\n\n⚠️ {}", accumulated_full_text, warn_msg),
                    })
                    .ok(),
                );
                clear_auto_context(state).await;
                return Response::ok_typed(
                    req_id,
                    &ReviewRefineItemResult::Blocked { reason: warn_msg },
                );
            }

            if !outcome.added.is_empty() {
                emit(
                    &state.out,
                    events::CONTEXT_CHANGED,
                    Some(serde_json::json!({ "state": new_ctx_state })),
                );
            }

            if outcome.added.is_empty() && outcome.missing.is_empty() {
                break output;
            }

            discovery_round += 1;
            let mut notes = Vec::new();
            if !outcome.added.is_empty() {
                notes.push(format!(
                    "Added {} file(s) to context: {}",
                    outcome.added.len(),
                    outcome.added.join(", ")
                ));
            }
            if !outcome.missing.is_empty() {
                notes.push(format!(
                    "Files not found in repository: {}",
                    outcome.missing.join(", ")
                ));
            }
            let round_banner = format!(
                "\n\n---\n**[Round {}]** {}\n\n",
                discovery_round + 1,
                notes.join("; ")
            );
            accumulated_full_text.push_str(&round_banner);
            emit(
                &state.out,
                events::MODEL_TEXT_DELTA,
                serde_json::to_value(ModelDeltaEvent {
                    operation_id: op_id.clone(),
                    delta: round_banner,
                })
                .ok(),
            );

            let mut fb = String::new();
            if !outcome.missing.is_empty() {
                fb.push_str(&format!(
                    "<context_feedback>\nThe following requested files do not exist in the repository: [{}]\nPlease check <repo_map> for exact existing file paths.\n</context_feedback>\n",
                    outcome.missing.join(", ")
                ));
            }
            if !outcome.already_satisfied.is_empty() && outcome.added.is_empty() {
                fb.push_str(&format!(
                    "<context_feedback>\nThe following files are already loaded in context: [{}]\nYou may proceed with inspecting them and refining the subplan.\n</context_feedback>\n",
                    outcome.already_satisfied.join(", ")
                ));
            }
            feedback = if fb.is_empty() { None } else { Some(fb) };
            continue;
        }

        break output;
    };

    emit(
        &state.out,
        events::MODEL_FINISHED,
        serde_json::to_value(ModelFinishedEvent {
            operation_id: op_id.clone(),
            full_text: accumulated_full_text,
        })
        .ok(),
    );

    clear_auto_context(state).await;

    if output.status.eq_ignore_ascii_case("blocked") {
        let reason = output.reason.unwrap_or(output.explanation);
        let blocked_msg = format!(
            "Architect reported blockers for review finding [{}#{}]:\n\n{}",
            session.id, item.id, reason
        );
        let _ = state.history.lock().await.record_response(
            blocked_msg,
            None,
            Some(format!("Review Finding Decomposition: Blocked ({})", item.title)),
            Vec::new(),
        );
        return Response::ok_typed(req_id, &ReviewRefineItemResult::Blocked { reason });
    }

    if output.status.eq_ignore_ascii_case("needs_context") {
        return Response::err(
            req_id,
            errors::NEEDS_CONTEXT,
            format!(
                "Architect requested additional context but no discovery rounds remain (limit: {}). Narrow the finding scope or raise the discovery round limit, then refine again",
                max_discovery_rounds
            ),
        );
    }

    if !output.status.eq_ignore_ascii_case("success") {
        return Response::err(
            req_id,
            errors::UNEXPECTED_STATUS,
            format!(
                "Architect returned unsupported status '{}'; expected 'success' or 'blocked'",
                output.status
            ),
        );
    }

    if output.items.is_empty() {
        return Response::err(
            req_id,
            errors::EMPTY_ITEMS,
            "Model returned success status but an empty items array",
        );
    }

    if let Some(duplicate) = find_duplicate_item_ids(&output.items) {
        return Response::err(
            req_id,
            errors::INVALID_PLAN_TREE,
            format!(
                "Architect returned a subplan with a duplicate or empty step ID: '{}'",
                duplicate
            ),
        );
    }

    let merged = match merge_subplan_horizon(&item.subplan, output.items) {
        Ok(m) => m,
        Err(err) => return Response::err(req_id, errors::PLAN_SAVE_FAILED, err),
    };

    let review_id = session.id.clone();
    let item_id = item.id;
    let updated = match ReviewStorage::update_session_item(repo_root, Some(&review_id), item_id, |it| {
        it.subplan = merged.clone();
    }) {
        Ok(Some(it)) => it,
        Ok(None) => {
            return Response::err(
                req_id,
                errors::NOT_FOUND,
                format!("Review item #{} not found in session '{}'", item_id, review_id),
            );
        }
        Err(err) => {
            return Response::err(req_id, errors::REVIEW_UPDATE_FAILED, format!("{:#}", err));
        }
    };

    let (subplan_total, _) = updated.subplan_stats();
    tracing::info!(
        review_id = %review_id,
        item_id = item_id,
        subplan_steps = subplan_total,
        "review finding decomposed"
    );

    let history_response = format!(
        "Review finding [{}#{}] decomposed into {} sub-step(s).\n\n{}",
        review_id, item_id, subplan_total, output.explanation
    );
    let _ = state.history.lock().await.record_response(
        history_response,
        None,
        Some(format!("Review Finding Decomposition: {}", updated.title)),
        Vec::new(),
    );

    if let Ok(Some(session)) = ReviewStorage::load_review(repo_root, &review_id) {
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

    Response::ok_typed(
        req_id,
        &ReviewRefineItemResult::Success {
            item: updated,
            explanation: Some(output.explanation),
        },
    )
}

/// Collects the immutable `Done` foundation of a subplan as a flat list.
///
/// A `Done` sub-step is preserved wholesale because its whole sub-tree is
/// immutable; a completed leaf nested inside a partially-completed container is
/// hoisted so discarding scaffold never takes finished work with it.
fn collect_done_foundation(items: &[PlanItem]) -> Vec<PlanItem> {
    let mut foundation = Vec::new();
    for item in items {
        if item.status == PlanItemStatus::Done {
            foundation.push(item.clone());
        } else {
            foundation.extend(collect_done_foundation(&item.children));
        }
    }
    foundation
}

/// Collects every id inside an item's sub-tree, including the item itself.
fn collect_subtree_ids(item: &PlanItem) -> Vec<String> {
    let mut ids = vec![item.id.clone()];
    for child in &item.children {
        ids.extend(collect_subtree_ids(child));
    }
    ids
}

/// Drops any synthesized sub-step whose id collides with a completed sub-step.
fn drop_done_collisions(items: Vec<PlanItem>, done_ids: &HashSet<String>) -> Vec<PlanItem> {
    items
        .into_iter()
        .filter(|item| !done_ids.contains(&item.id.to_ascii_lowercase()))
        .map(|mut item| {
            item.children = drop_done_collisions(item.children, done_ids);
            item
        })
        .collect()
}

/// Clears concept references from a subplan tree.
///
/// A subplan carries no architectural concept of its own, so any references the
/// model emits cannot resolve against a concept tree and must not fail validation.
fn clear_concept_refs(items: &mut [PlanItem]) {
    for item in items.iter_mut() {
        item.concept_refs.clear();
        clear_concept_refs(&mut item.children);
    }
}

/// Merges a freshly synthesized subplan horizon with the finding's existing
/// `Done` foundation, preserving completed sub-steps verbatim.
///
/// Mirrors the plan horizon synthesis: the immutable `Done` sub-steps are kept,
/// colliding ids are dropped deterministically, and the merged tree is normalized
/// and validated through the shared plan domain logic, so a subplan that would
/// introduce a dependency cycle is rejected before it reaches disk.
fn merge_subplan_horizon(
    existing: &[PlanItem],
    new_horizon: Vec<PlanItem>,
) -> Result<Vec<PlanItem>, String> {
    let done_foundation = collect_done_foundation(existing);
    let done_ids: HashSet<String> = done_foundation
        .iter()
        .flat_map(collect_subtree_ids)
        .map(|id| id.to_ascii_lowercase())
        .collect();

    let mut combined = done_foundation;
    combined.extend(drop_done_collisions(new_horizon, &done_ids));

    let mut plan = Plan {
        items: combined,
        ..Default::default()
    };
    plan.normalize_leaf_files();
    plan.refresh_parent_statuses();
    clear_concept_refs(&mut plan.items);

    if let Err(errors) = plan.validate() {
        let msg = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("Synthesized subplan is invalid: {}", msg));
    }

    Ok(plan.items)
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
    Stopped {
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

/// Loads all files referenced by a finding into editable context before the turn starts.
async fn preload_item_files(state: &Arc<AppState>, repo_root: &Path, files: &[String]) {
    let requests: Vec<ContextRequest> = files
        .iter()
        .map(|f| f.trim().trim_start_matches("./").trim_start_matches('/'))
        .filter(|p| !p.is_empty())
        .map(|p| ContextRequest {
            path: p.to_string(),
            access: ContextAccess::Editable,
        })
        .collect();
    if requests.is_empty() {
        return;
    }
    let files_token_budget = state.config.lock().await.config.context.files_token_budget;
    let available_files =
        tauqe_core::git::list_repository_files(Some(repo_root)).unwrap_or_default();

    let (outcome, new_ctx_state) = {
        let mut ctx = state.context.write().await;
        let oc = apply_context_requests_detailed(
            &mut ctx,
            &requests,
            &available_files,
            files_token_budget,
        );
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

/// Clears the transient auto context layer so a retried attempt starts from a
/// clean, deterministic basis, then notifies clients of the new snapshot.
async fn clear_auto_context(state: &Arc<AppState>) {
    let new_ctx_state = {
        let mut ctx = state.context.write().await;
        ctx.clear_auto();
        ctx.get_state()
    };
    emit(
        &state.out,
        events::CONTEXT_CHANGED,
        Some(serde_json::json!({ "state": new_ctx_state })),
    );
}

/// Whether a completed turn actually applied code edits to disk.
///
/// Review findings ask the model to fix code: explanatory text alone, even when
/// accompanied by a terminal success tag, must not satisfy the finding contract.
fn turn_applied_edits(turn: &crate::handlers::model::ModelTurnResult) -> bool {
    match &turn.result {
        Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) => {
            outcome.applied && outcome.error.is_none()
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
    operation: Arc<OperationGuard>,
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
        ReviewItemStatus::Fixed | ReviewItemStatus::Skipped => return Ok(()),
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

    // A finding decomposed into an embedded subplan executes its sub-steps one by
    // one instead of a single monolithic turn.
    if item.has_subplan() {
        return run_review_subplan(state, repo_root, review_id, &session, &item, operation).await;
    }

    preload_item_files(state, repo_root, &item.files).await;
    set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::InProgress);

    // Generate the active turn marker before assembling prompts so the target
    // finding prompt and the model turn share the same marker-bound protocol.
    let marker = state.next_turn_marker().await;
    let marker_suffix = marker
        .as_ref()
        .map(|m| format!("_{}", m))
        .unwrap_or_default();

    let mut prompt = match tauqe_core::review::prompt::format_review_step_execution_prompt(
        &session,
        item_id,
        &marker_suffix,
    ) {
        Ok(p) => p,
        Err(err) => {
            set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::Todo);
            return Err(err);
        }
    };

    let (max_retries, auto_level_up, models_config) = {
        let cfg = state.config.lock().await.config.clone();
        (
            cfg.develop.max_retries,
            cfg.models.auto_level_up,
            cfg.models.clone(),
        )
    };

    // A finding with an explicitly assigned model keeps that model across every
    // retry; an unassigned finding starts on the Coder role and may escalate
    // through the coder ladder when `auto_level_up` is enabled.
    let explicit_selection = item.model.clone();
    let mut selection = explicit_selection
        .clone()
        .unwrap_or(ModelSelection::Role(ModelRole::Coder));
    let mut reasoning_effort = None;
    let mut ladder_idx = 0usize;
    let mut attempt = 0usize;

    loop {
        let workflow = state.config.lock().await.config.develop.workflow;
        let policy = crate::handlers::model::build_apply_policy(state, workflow);

        let turn_result = execute_model_turn(
            state,
            prompt.clone(),
            policy,
            StepTurnOptions {
                custom_selection: Some(selection.clone()),
                plan_context: None,
                reasoning_effort,
                marker: marker.clone(),
                shared_operation: Some(Arc::clone(&operation)),
            },
        )
        .await;

        let turn = match turn_result {
            Ok(turn) => turn,
            Err(err) => {
                set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::Todo);
                let op_id = format!("op-{}", next_operation_id());
                report_item_failure(state, &op_id, &err.message);
                return Err(err.message);
            }
        };

        // Semantic block: the coder reports that the finding preconditions
        // cannot be satisfied as stated. Review findings have no blocked status,
        // so the item returns to TODO and the batch stops without a retry cycle.
        if let Some(reason) = turn.blocked.clone() {
            set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::Todo);
            report_item_failure(state, &turn.operation_id, &reason);
            return Err(reason);
        }

        let applied = turn_applied_edits(&turn);

        if turn.is_success() && applied {
            let notes = match &turn.result {
                Some(tauqe_protocol::ModelResult::Edit { summary, .. })
                    if !summary.trim().is_empty() =>
                {
                    Some(summary.clone())
                }
                _ => None,
            };
            if let Some(notes) = notes {
                let _ = ReviewStorage::update_session_item(
                    repo_root,
                    Some(review_id),
                    item_id,
                    |it| {
                        it.status = ReviewItemStatus::Fixed;
                        if it.resolution_notes.is_none() {
                            it.resolution_notes = Some(notes.clone());
                        }
                    },
                );
                publish_review(state, repo_root, review_id);
            } else {
                set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::Fixed);
            }
            return Ok(());
        }

        if !applied && turn.error.is_none() && !turn.is_cancelled {
            if let Some(skip_reason) = tauqe_core::review::prompt::extract_review_skip_reason(
                &turn.assistant_text,
                marker.as_deref(),
            ) {
                let _ = ReviewStorage::update_session_item(
                    repo_root,
                    Some(review_id),
                    item_id,
                    |it| {
                        it.status = ReviewItemStatus::Skipped;
                        it.resolution_notes = Some(skip_reason.clone());
                    },
                );
                publish_review(state, repo_root, review_id);
                return Ok(());
            }
        }

        let reason = turn.error.clone().unwrap_or_else(|| {
            if applied {
                "Finding turn ended without verified changes or a success tag".to_string()
            } else {
                "Finding turn applied no code changes".to_string()
            }
        });

        if turn.is_cancelled || attempt >= max_retries {
            set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::Todo);
            if turn.error.is_none() && !turn.is_cancelled {
                report_item_failure(state, &turn.operation_id, &reason);
            }
            return Err(reason);
        }

        // Reset to a clean, deterministic basis before the corrective attempt.
        clear_auto_context(state).await;
        preload_item_files(state, repo_root, &item.files).await;

        if explicit_selection.is_none() && auto_level_up {
            ladder_idx += 1;
            let tier = models_config.coder_tier(ladder_idx);
            selection = ModelSelection::Specific(tier.model);
            reasoning_effort = tier.effort;
        }

        prompt = tauqe_core::review::prompt::format_review_step_retry_prompt(
            &turn.assistant_text,
            turn.error.as_deref(),
        );
        attempt += 1;
    }
}

/// Collects the leaf sub-step identifiers of a subplan in declaration order.
///
/// Only leaf steps carry executable work; conceptual container items are skipped.
fn collect_leaf_step_ids(items: &[PlanItem]) -> Vec<String> {
    fn walk(items: &[PlanItem], ids: &mut Vec<String>) {
        for item in items {
            if item.children.is_empty() {
                ids.push(item.id.clone());
            } else {
                walk(&item.children, ids);
            }
        }
    }
    let mut ids = Vec::new();
    walk(items, &mut ids);
    ids
}

/// Recursively assigns a status to the sub-step with the given identifier.
fn mark_substep_status(items: &mut [PlanItem], step_id: &str, status: PlanItemStatus) {
    for item in items.iter_mut() {
        if item.id.eq_ignore_ascii_case(step_id) {
            item.status = status;
            return;
        }
        mark_substep_status(&mut item.children, step_id, status);
    }
}

/// Recomputes derived container statuses and persists the subplan atomically.
fn persist_subplan(
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    subplan: &mut Vec<PlanItem>,
) -> Result<(), String> {
    let mut plan = Plan {
        items: subplan.clone(),
        ..Default::default()
    };
    plan.refresh_parent_statuses();
    *subplan = plan.items;

    let snapshot = subplan.clone();
    ReviewStorage::update_session_item(repo_root, Some(review_id), item_id, move |it| {
        it.subplan = snapshot.clone();
    })
    .map_err(|err| format!("{:#}", err))?;
    Ok(())
}

/// Records an interrupted subplan.
///
/// Completed sub-steps keep their `Done` status, the stop reason is preserved in
/// `resolution_notes`, and the finding becomes `Skipped` so the developer can
/// inspect the cause and decide how to proceed without losing finished work.
fn finish_subplan_interrupted(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    subplan: Vec<PlanItem>,
    reason: &str,
) {
    let mut plan = Plan {
        items: subplan,
        ..Default::default()
    };
    plan.refresh_parent_statuses();
    let subplan = plan.items;
    let notes = format!("Subplan interrupted before completion: {}", reason);

    let _ = ReviewStorage::update_session_item(repo_root, Some(review_id), item_id, move |it| {
        it.status = ReviewItemStatus::Skipped;
        it.subplan = subplan.clone();
        it.resolution_notes = Some(notes.clone());
    });
    publish_review(state, repo_root, review_id);
}

/// Executes the embedded subplan of a review finding one leaf sub-step at a time.
///
/// Each sub-step runs in its own bounded turn with its target files preloaded.
/// A completed sub-step is persisted as `Done`, so finished work survives any
/// later interruption. When a sub-step cannot apply, blocks, or the turn is
/// cancelled, the finding is marked `Skipped` with the stop reason recorded in
/// `resolution_notes`; when every sub-step applies cleanly, the finding becomes
/// `Fixed`.
async fn run_review_subplan(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    session: &ReviewSession,
    item: &ReviewItem,
    operation: Arc<OperationGuard>,
) -> Result<(), String> {
    let item_id = item.id;
    set_item_status(state, repo_root, review_id, item_id, ReviewItemStatus::InProgress);

    let marker = state.next_turn_marker().await;
    let marker_suffix = marker
        .as_ref()
        .map(|m| format!("_{}", m))
        .unwrap_or_default();

    let (max_retries, auto_level_up, models_config) = {
        let cfg = state.config.lock().await.config.clone();
        (
            cfg.develop.max_retries,
            cfg.models.auto_level_up,
            cfg.models.clone(),
        )
    };

    let explicit_selection = item.model.clone();
    let mut subplan = item.subplan.clone();
    let leaf_ids = collect_leaf_step_ids(&subplan);

    for step_id in &leaf_ids {
        let plan = Plan {
            id: session.id.clone(),
            title: session.title.clone(),
            items: subplan.clone(),
            ..Default::default()
        };

        let step = match plan.find_item(step_id) {
            Some(s) => s.clone(),
            None => continue,
        };
        if step.status == PlanItemStatus::Done {
            continue;
        }

        preload_step_files(state, repo_root, &step).await;

        let mut prompt = match tauqe_core::plan::prompt::format_plan_step_execution_prompt(
            &plan,
            step_id,
            &marker_suffix,
        ) {
            Ok(p) => p,
            Err(err) => {
                finish_subplan_interrupted(state, repo_root, review_id, item_id, subplan, &err);
                return Err(err);
            }
        };

        let mut selection = explicit_selection
            .clone()
            .unwrap_or(ModelSelection::Role(ModelRole::Coder));
        let mut reasoning_effort = None;
        let mut ladder_idx = 0usize;
        let mut attempt = 0usize;

        let step_result: Result<(), String> = loop {
            let workflow = state.config.lock().await.config.develop.workflow;
            let policy = crate::handlers::model::build_apply_policy(state, workflow);

            let turn_result = execute_model_turn(
                state,
                prompt.clone(),
                policy,
                StepTurnOptions {
                    custom_selection: Some(selection.clone()),
                    plan_context: None,
                    reasoning_effort,
                    marker: marker.clone(),
                    shared_operation: Some(Arc::clone(&operation)),
                },
            )
            .await;

            let turn = match turn_result {
                Ok(turn) => turn,
                Err(err) => break Err(err.message),
            };

            if let Some(reason) = turn.blocked.clone() {
                break Err(reason);
            }

            let applied = turn_applied_edits(&turn);

            if turn.is_success() && applied {
                break Ok(());
            }

            if turn.is_cancelled {
                break Err("Subplan execution was cancelled".to_string());
            }

            let reason = turn.error.clone().unwrap_or_else(|| {
                if applied {
                    "Sub-step finished without verified changes or a success tag".to_string()
                } else {
                    "Sub-step finished without applying any code changes".to_string()
                }
            });

            if attempt >= max_retries {
                break Err(reason);
            }

            clear_auto_context(state).await;
            preload_step_files(state, repo_root, &step).await;

            if explicit_selection.is_none() && auto_level_up {
                ladder_idx += 1;
                let tier = models_config.coder_tier(ladder_idx);
                selection = ModelSelection::Specific(tier.model);
                reasoning_effort = tier.effort;
            }

            prompt = tauqe_core::plan::prompt::format_plan_step_retry_prompt(
                &turn.assistant_text,
                turn.error.as_deref(),
            );
            attempt += 1;
        };

        match step_result {
            Ok(()) => {
                mark_substep_status(&mut subplan, step_id, PlanItemStatus::Done);
                persist_subplan(repo_root, review_id, item_id, &mut subplan)?;
                publish_review(state, repo_root, review_id);
            }
            Err(reason) => {
                let op_id = format!("op-{}", next_operation_id());
                report_item_failure(state, &op_id, &reason);
                finish_subplan_interrupted(state, repo_root, review_id, item_id, subplan, &reason);
                return Err(reason);
            }
        }
    }

    let notes = format!(
        "Subplan completed: all {} sub-step(s) applied successfully",
        leaf_ids.len()
    );
    let _ = ReviewStorage::update_session_item(repo_root, Some(review_id), item_id, move |it| {
        it.status = ReviewItemStatus::Fixed;
        if it.resolution_notes.is_none() {
            it.resolution_notes = Some(notes.clone());
        }
    });
    publish_review(state, repo_root, review_id);
    Ok(())
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
        let _ = ReviewStorage::save_active_id(self.repo_root, Some(&session_id));

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
            Err(reason) => Ok(BatchStepOutcome::failed(reason)),
        }
    }
}

/// Runs the review findings through the core [`BatchExecution`] supervisor, which
/// enforces ordered, fail-fast traversal. Findings fixed before the first failure
/// stay FIXED. Once the supervisor terminates, a typed `review/batch_finished`
/// event carries the aggregate status, finding counters, and elapsed time, so
/// clients can raise a single notice for the whole batch.
async fn run_review_batch(
    state: &Arc<AppState>,
    owner_id: &str,
    tasks: Vec<(String, u32)>,
    operation: Arc<OperationGuard>,
) -> BatchOutcome {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    if owner_id != "all" {
        let _ = ReviewStorage::save_active_id(&repo_root, Some(owner_id));
    }

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
            let session = match ReviewStorage::load_review(&repo_root, review_id) {
                Ok(Some(s)) => s,
                Ok(None) => return Response::err(req.id, errors::NOT_FOUND, format!("Review session '{}' not found", review_id)),
                Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, format!("{:#}", err)),
            };

            let item_ids: Vec<u32> = match params.item_id {
                Some(item_id) => {
                    let item = match session.find_item(item_id) {
                        Some(i) => i,
                        None => return Response::err(req.id, errors::NOT_FOUND, format!("Review item #{} not found in session '{}'", item_id, review_id)),
                    };
                    match item.status {
                        ReviewItemStatus::Discussion => {
                            return Response::err(
                                req.id,
                                errors::ITEM_IN_DISCUSSION,
                                format!("Review item #{} is in DISCUSSION status and cannot be executed autonomously until reviewed (change status to TODO first)", item.id),
                            );
                        }
                        ReviewItemStatus::Rejected => {
                            return Response::err(
                                req.id,
                                errors::ITEM_REJECTED,
                                format!("Review item #{} is REJECTED and cannot be executed", item.id),
                            );
                        }
                        ReviewItemStatus::Fixed => {
                            return Response::err(
                                req.id,
                                errors::NOTHING_TO_EXECUTE,
                                format!("Review item #{} is already FIXED", item.id),
                            );
                        }
                        ReviewItemStatus::Skipped => {
                            return Response::err(
                                req.id,
                                errors::NOTHING_TO_EXECUTE,
                                format!("Review item #{} is already SKIPPED", item.id),
                            );
                        }
                        ReviewItemStatus::Todo | ReviewItemStatus::InProgress => vec![item_id],
                    }
                }
                None => {
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
            let sessions = match ReviewStorage::load_all(&repo_root) {
                Ok(s) => s,
                Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, format!("{:#}", err)),
            };
            let mut tasks: Vec<(String, u32)> = Vec::new();
            for session in sessions {
                for item in &session.items {
                    if item.status == ReviewItemStatus::Todo || item.status == ReviewItemStatus::InProgress {
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

#[cfg(test)]
mod tests;
