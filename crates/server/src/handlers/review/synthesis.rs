//! Primary audit synthesis and cancellation handlers.
//!
//! Starting a review streams the reviewer model's Structured Output through a
//! `JsonFieldStreamFilter`, so only the cleaned `summary` reaches the client
//! while raw JSON syntax stays server-side. Cancellation supports both a soft
//! stop (drain the current turn) and an immediate hard stop.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tauqe_core::edits::stream::json::JsonFieldStreamFilter;
use tauqe_core::providers::{
    create_provider, format_session_id, ChatMessage, LlmProvider, StreamEvent,
    SESSION_PREFIX_REVIEW,
};
use tauqe_core::review::parser::ReviewFindingsOutput;
use tauqe_core::review::{
    build_review_system_prompt, build_review_user_prompt, parse_review_findings,
    parse_review_output, review_findings_response_format, uuid::generate_uuid_v7, ReviewStorage,
};
use tauqe_protocol::{
    errors, events, ContextAccess, ModelRole, ModelUsageEvent, ModelUsageInfo, Request, Response,
    ReviewCancelParams, ReviewContentDeltaEvent, ReviewFinishedEvent, ReviewItem,
    ReviewListChangedEvent, ReviewReasoningDeltaEvent, ReviewSession, ReviewStartParams,
    ReviewStartedEvent, ReviewUpdatedEvent,
};
use tokio::sync::mpsc;

use crate::state::{next_operation_id, AppState};

use super::common::{emit, emit_error, offload_review};

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

    let (repo_root, ctx_snapshot, mut estimated_tokens) = {
        let ctx = state.context.read().await;
        (
            ctx.repo_root().to_path_buf(),
            ctx.clone(),
            ctx.get_state().total_estimated_tokens as usize,
        )
    };

    let mut files = match crate::handlers::offload(move || ctx_snapshot.read_context_files()).await
    {
        Ok(files) => files,
        Err(err) => {
            tracing::warn!(error = %err, "Failed to read context files in offload");
            Vec::new()
        }
    };

    if let Some(pattern) = params
        .path_pattern
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let existing_paths: HashSet<String> = files.iter().map(|f| f.path.clone()).collect();
        let pattern = pattern.to_string();
        let root = repo_root.clone();
        // Listing the repository and reading every matched file is blocking git
        // and filesystem work, so it is offloaded off the async worker thread.
        let matched_files = crate::handlers::offload(move || {
            let available_files =
                tauqe_core::git::list_repository_files(Some(&root)).unwrap_or_default();
            let compiled = tauqe_protocol::GlobPattern::new(&pattern);
            let mut matched: Vec<String> = available_files
                .into_iter()
                .filter(|path| !existing_paths.contains(path) && compiled.matches(path))
                .collect();
            matched.sort();

            let mut read = Vec::new();
            for path in matched {
                if tauqe_core::context::verify_path_within_repo(&root, &path).is_err() {
                    continue;
                }
                let full_path = root.join(&path);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    read.push((path, content));
                }
            }
            read
        })
        .await
        .unwrap_or_default();

        for (path, content) in matched_files {
            let tokens =
                tauqe_core::prompt::estimate_tokens_from_bytes(content.len() as u64) as usize;
            estimated_tokens += tokens;
            files.push(tauqe_core::context::ContextFileContent {
                path,
                access: ContextAccess::ReadOnly,
                content,
            });
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
    // A review audit is one long-lived logical context: the synthesis, the
    // refinement of its findings, and every finding and sub-plan turn share one
    // identifier, so the backend keeps a single warm prompt-prefix cache. The
    // identifier is minted before the streamed call because the request needs it.
    let review_id = generate_uuid_v7();
    let session_id = format_session_id(SESSION_PREFIX_REVIEW, &review_id);

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
        // Open the shared usage ledger before the provider call: this audit
        // reports both the usage the provider returns and the duration the
        // harness measures, so every operation kind feeds one snapshot.
        let usage_guard = state.begin_usage().await;

        let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
        let err_tx = tx.clone();
        let model_name = model.name.clone();
        let stream_cancel = cancel_rx.clone();
        let stream_task = tokio::spawn(async move {
            if let Err(err) = provider
                .stream_chat_with_session(
                    &model_name,
                    messages,
                    Some(response_format),
                    reasoning_effort,
                    Some(session_id.as_str()),
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
        let mut stream_cancelled = false;

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
                StreamEvent::Done => break,
                // The provider may report cancellation as an explicit stream
                // event even when the turn's cancel signal has not been observed
                // yet, so the event is recorded instead of inferred from
                // `cancel_rx` alone.
                StreamEvent::Cancelled => {
                    stream_cancelled = true;
                    break;
                }
                StreamEvent::Error(err) => {
                    failure = Some(err);
                    break;
                }
                _ => {}
            }
        }

        // The streamed provider call has resolved, so the slot must stop
        // pointing at it: a later hard-cancel would otherwise abort an already
        // finished task while the operation is still finalizing and persisting.
        operation.clear_abort_handle();

        // Account for the tokens, cost, and duration the audit consumed through
        // the shared ledger before any early return, so a failed or cancelled
        // audit is still charged and leaves no in-flight window behind for the
        // next operation.
        let reported_usage = usage.unwrap_or_default();
        usage_guard.observe(&reported_usage).await;
        usage_guard.flush().await;
        let usage_snapshot = state.usage.lock().await.clone();

        let cancelled = *cancel_rx.borrow() || stream_cancelled;

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
            emit_error(
                &state.out,
                &op_id,
                "Model returned an empty review".to_string(),
            );
            return;
        }

        // A clean deserialization yields the typed payload; a malformed tail
        // degrades gracefully to the tolerant text parser instead of dropping
        // an otherwise usable review.
        let (parsed_title, summary, issues) = match filter.finish() {
            Ok(output) => {
                let trimmed = output.summary.trim();
                let summary = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                let title = output.title.trim();
                let title = if title.is_empty() {
                    None
                } else {
                    Some(title.to_string())
                };
                (title, summary, parse_review_findings(&raw_buffer))
            }
            Err(_) => parse_review_output(&raw_buffer),
        };

        // The reviewer role synthesizes a concise session title; when it omits
        // one, fall back to the first line of the user's prompt and finally to
        // a time-derived placeholder.
        let title = parsed_title
            .or_else(|| {
                params
                    .user_prompt
                    .as_deref()
                    .and_then(|p| p.lines().next())
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .map(|l| l.chars().take(60).collect::<String>())
            })
            .unwrap_or_else(|| format!("Review {}", created_at));

        let session = ReviewSession {
            // The UUIDv7 minted before the streamed call carries a millisecond
            // timestamp plus random bits, so two reviews started within the same
            // second never share a session file, and the same string keys the
            // provider session.
            id: review_id,
            title,
            created_at,
            model: model.clone(),
            description: summary.or_else(|| params.user_prompt.clone()),
            user_prompt: params.user_prompt.clone(),
            target_files,
            items: issues.into_iter().map(ReviewItem::from).collect(),
            raw_markdown: raw_buffer,
        };
        // A cancel delivered after the stream resolved — during parsing, or
        // across a storage round-trip — must not persist an artifact the
        // developer already abandoned, so the signal is re-checked right before
        // the session touches disk.
        if *cancel_rx.borrow() {
            emit(
                &state.out,
                events::REVIEW_CANCELLED,
                Some(serde_json::json!({ "operation_id": op_id })),
            );
            return;
        }

        let save_root = repo_root.clone();
        let session_clone = session.clone();
        if let Err(err) =
            offload_review(move || ReviewStorage::save_session(&save_root, &session_clone)).await
        {
            emit_error(&state.out, &op_id, err);
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

        let list_root = repo_root.clone();
        if let Ok(reviews) = offload_review(move || ReviewStorage::load_all(&list_root)).await {
            let active_root = repo_root.clone();
            let active_id = offload_review(move || ReviewStorage::load_active_id(&active_root))
                .await
                .unwrap_or(None);
            emit(
                &state.out,
                events::REVIEW_LIST_CHANGED,
                serde_json::to_value(ReviewListChangedEvent { reviews, active_id }).ok(),
            );
        }

        // Recording the review appends to `history.jsonl` behind an `fsync`
        // barrier, so it runs on the blocking pool under the exclusive history
        // guard instead of stalling an async worker thread.
        let history_model = model.to_string();
        let history_prompt = params.user_prompt.clone();
        let history_description = session.description.clone();
        let history_files = session.target_files.clone();
        let history_items = session.items.len();
        // The blocking mutation closure takes ownership of its own handle, so
        // the shared state stays usable afterwards for cost aggregation and
        // the terminal events.
        let history_state = Arc::clone(&state);
        let recorded = crate::handlers::with_history_mutation(&history_state, move |history| {
            history.record_review(
                history_model,
                history_prompt,
                history_description,
                history_files,
                history_items,
            )
        })
        .await;
        if let Err(err) = recorded {
            tracing::warn!(
                error = %err,
                "Failed to record the review in interaction history"
            );
        }

        let usage = reported_usage;
        let cost = usage.cost.unwrap_or(0.0);
        // The session total comes from the authoritative ledger rather than a
        // second, independently accumulated counter.
        let session_total_cost = usage_snapshot.session.cost;

        emit(
            &state.out,
            events::MODEL_USAGE,
            serde_json::to_value(ModelUsageEvent {
                operation_id: op_id.clone(),
                usage: usage.clone(),
                session_total_cost,
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
                session_total_cost: Some(session_total_cost),
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

    // A hard stop signals cancellation but keeps the operation slot registered:
    // the cancelled workflow keeps mutating state (rolling back its git
    // transaction, releasing the session lock) until it unwinds and drops its
    // guard. Clearing the slot immediately would let a client start
    // `git/undo`, `config/set`, or a new turn against a half-unwound turn. A
    // watchdog force-aborts a stalled task after the grace period.
    state.hard_cancel_active_operation();
    Response::ok(req.id, serde_json::json!({ "cancelled": true }))
}
