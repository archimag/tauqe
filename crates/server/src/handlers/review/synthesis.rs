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
use tauqe_core::workflow::batch::BatchStop;
use tauqe_protocol::review::{PostAnalysisMode, ReviewLaunchConfig};
use tauqe_protocol::{
    errors, events, ContextAccess, ModelRole, ModelSelection, ModelUsageEvent, ModelUsageInfo,
    Request, Response, ReviewCancelParams, ReviewContentDeltaEvent, ReviewFinishedEvent,
    ReviewIssue, ReviewItem, ReviewListChangedEvent, ReviewReasoningDeltaEvent, ReviewSession,
    ReviewStartParams, ReviewStartedEvent, ReviewUpdatedEvent,
};
use tokio::sync::mpsc;

use crate::handlers::common::withdraw_streamed_attempt;
use crate::state::{next_operation_id, AppState};

use super::analysis::run_review_post_analysis;
use super::common::{emit, emit_error, offload_review};
use super::groups::run_group_audits;
pub async fn handle_review_start(mut req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewStartParams = match req.params.take() {
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

    // A preset launch is a grouped audit: the session is materialized from
    // the configured preset and every group runs under the core batch
    // supervisor, so it never reads the interactive context window. The
    // plain context and pattern launch below keeps its live-streamed single
    // audit — the degenerate implicit `Default` group.
    let preset_name = params.preset.as_deref().map(str::trim);
    if let Some(preset_name) = preset_name.filter(|preset| !preset.is_empty()) {
        return start_preset_review(req, state, &params, preset_name.to_string()).await;
    }

    let (provider, model, reasoning_effort, max_pattern_batch, files_token_budget) = {
        let cfg = state.config.lock().await.config.clone();
        // The request carries the whole reviewer selection — the model together
        // with the reasoning depth of the configured entry the developer picked
        // — so the depth is read straight from the DTO instead of being
        // re-derived by matching the model against the configured entries,
        // which could never distinguish two rungs of the same model. An absent
        // selection runs the base rung of the reviewer role.
        let selection = params
            .model
            .clone()
            .unwrap_or(ModelSelection::Role(ModelRole::Reviewer));
        let model = cfg.resolve_model(&selection);
        let reasoning_effort = cfg
            .models
            .resolve_selection_effort(ModelRole::Reviewer, &selection);
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

        // The pattern expansion is bounded by the same context budgets
        // that govern discovery, so one wide glob cannot drag an unbounded
        // file set into the audit window.
        (
            provider,
            model,
            reasoning_effort,
            cfg.context.max_pattern_batch,
            cfg.context.files_token_budget,
        )
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

    // Pattern-matched files the configured budgets kept out of the audit.
    // Reported on the started event so a truncated expansion is never silent.
    let mut omitted_files = 0usize;

    // Audit coverage is the union of every declared line: a repository file is
    // selected as soon as one pattern matches it, so several paths widen the
    // audit instead of competing for it. Blank lines narrow nothing and are
    // dropped before compilation (c-1, c-6).
    let patterns: Vec<String> = params
        .path_patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_string)
        .collect();

    if !patterns.is_empty() {
        let existing_paths: HashSet<String> = files.iter().map(|f| f.path.clone()).collect();
        let listing_patterns = patterns.clone();
        let root = repo_root.clone();
        // Listing the repository and reading every matched file is blocking git
        // and filesystem work, so it is offloaded off the async worker thread.
        let listing = crate::handlers::offload(move || {
            let available_files =
                tauqe_core::git::list_repository_files(Some(&root)).unwrap_or_default();
            let compiled: Vec<tauqe_protocol::GlobPattern> = listing_patterns
                .iter()
                .map(|pattern| tauqe_protocol::GlobPattern::new(pattern))
                .collect();
            let mut matched: Vec<String> = available_files
                .into_iter()
                .filter(|path| compiled.iter().any(|pattern| pattern.matches(path)))
                .collect();
            // The repository match count is taken before the context filter, so a
            // pattern whose files are already loaded still selects something,
            // while zero repository matches prove that the pattern names nothing.
            let repo_matches = matched.len();
            matched.retain(|path| !existing_paths.contains(path));
            matched.sort();
            let total_matched = matched.len();
            // One wide glob may match the whole tree, so the batch is cut at
            // the configured pattern limit before any file is read.
            matched.truncate(max_pattern_batch);

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
            (read, total_matched, repo_matches)
        })
        .await;

        let (matched_reads, total_matched) = match listing {
            Ok((matched_reads, total_matched, repo_matches)) => {
                // A pattern that selects nothing is a mistyped or unsupported
                // path, never a narrower audit: launching anyway would review a
                // different file set while the developer believes the pattern
                // chose it. A partial selection stays a lawful run reported
                // through `omitted_files`, so only a zero match is refused.
                if repo_matches == 0 {
                    return Response::err(
                        req.id,
                        errors::INVALID_PARAMS,
                        format!(
                            "Review path patterns matched no repository files: {}",
                            patterns.join(", ")
                        ),
                    );
                }
                (matched_reads, total_matched)
            }
            Err(err) => {
                // The listing failed at the infrastructure level, which says
                // nothing about the pattern itself: the audit proceeds with the
                // context files already loaded instead of rejecting a pattern the
                // harness never evaluated.
                tracing::warn!(
                    error = %err,
                    "Failed to list repository files for the review path pattern"
                );
                (Vec::new(), 0)
            }
        };

        let mut added = 0usize;
        for (path, content) in matched_reads {
            let tokens =
                tauqe_core::prompt::estimate_tokens_from_bytes(content.len() as u64) as usize;
            // The expansion shares the discovery files token budget, so it
            // stops at the first file that would push the audit window past
            // the configured limit.
            if estimated_tokens + tokens > files_token_budget {
                break;
            }
            estimated_tokens += tokens;
            files.push(tauqe_core::context::ContextFileContent {
                path,
                access: ContextAccess::ReadOnly,
                content,
            });
            added += 1;
        }
        omitted_files = total_matched.saturating_sub(added);
        if omitted_files > 0 {
            tracing::warn!(
                omitted = omitted_files,
                matched = total_matched,
                "path pattern expansion reached the configured batch or token budget; remaining matched files were not loaded"
            );
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
            omitted_files,
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
                // The retry decorator withdrew the previous attempt because it
                // broke after partial output: the filter holds a prefix of a
                // document the retry regenerates from its beginning, so it is
                // restarted instead of accumulating both payloads into one
                // unparsable body, and the client drops the partial summary it
                // already rendered.
                StreamEvent::ResetStream => {
                    withdraw_streamed_attempt(&state.out, &op_id, || {
                        filter = JsonFieldStreamFilter::<ReviewFindingsOutput>::new("summary");
                    });
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
            // Every synthesized finding gets its UUIDv7 identity before the
            // session is persisted, so later layers can reference it.
            items: with_finding_identities(issues),
            raw_markdown: raw_buffer,
            // A launch performs exactly one implicit `Default` audit and asks for
            // no post-analysis, so the stored session keeps the degenerate
            // single-audit shape the protocol layer resolves on its own (#c-4).
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
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
/// Launches a grouped audit from a named review preset.
///
/// The preset is the single source of the session shape: its groups are
/// materialized with fresh identities, persisted as the immutable launch
/// snapshot, and audited one by one under the core batch supervisor. A
/// preset launch never reads the interactive context window, so the empty
/// context refusal of the plain launch does not apply here.
async fn start_preset_review(
    req: Request,
    state: &Arc<AppState>,
    params: &ReviewStartParams,
    preset_name: String,
) -> Response {
    let (preset, model) = {
        let cfg = state.config.lock().await.config.clone();
        let preset = cfg
            .review
            .presets
            .iter()
            .find(|preset| preset.name.trim() == preset_name.as_str())
            .cloned();
        let preset = match preset {
            Some(preset) => preset,
            None => {
                return Response::err(
                    req.id,
                    errors::NOT_FOUND,
                    format!("Review preset '{preset_name}' not found"),
                )
            }
        };
        // An absent selection runs the base rung of the reviewer role; the
        // supervisor resolves its own transport from the session model.
        let selection = params
            .model
            .clone()
            .unwrap_or(ModelSelection::Role(ModelRole::Reviewer));
        (preset, cfg.resolve_model(&selection))
    };

    let groups = preset.to_launch_groups();
    if groups.is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            format!("Review preset '{preset_name}' declares no groups"),
        );
    }
    let group_count = groups.len();
    let post_analysis_mode = preset.post_analysis;
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let user_prompt = params.user_prompt.clone();

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
    // The grouped events the supervisor emits name the registered
    // operation, so the launch keeps that identifier instead of minting a
    // second one the client cannot correlate.
    let op_id = format!("op-{}", operation.id());
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let review_id = generate_uuid_v7();

    // The declared audit paths are the launch snapshot's targets: literal
    // paths and patterns both, exactly as configured.
    let mut target_files: Vec<String> = groups
        .iter()
        .flat_map(|group| group.paths.iter())
        .cloned()
        .collect();
    target_files.sort();
    target_files.dedup();
    // The launch is named by configuration: the preset's title is the
    // session title, never a streamed summary.
    let title = preset
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| preset.name.trim().to_string());

    let session = ReviewSession {
        // The UUIDv7 minted before the batch carries a millisecond
        // timestamp plus random bits, so two preset launches never share
        // a session file, and the same string keys the provider session.
        id: review_id.clone(),
        title,
        created_at,
        model: model.clone(),
        description: user_prompt.clone(),
        user_prompt: user_prompt.clone(),
        target_files,
        // Findings are appended by the group supervisor, so the snapshot
        // stores the empty list the batch starts from.
        items: Vec::new(),
        raw_markdown: String::new(),
        groups: groups.clone(),
        post_analysis: None,
        // Immutable launch snapshot: a later edit of the project
        // configuration never rewrites a stored session.
        launch_config: Some(ReviewLaunchConfig {
            preset: Some(preset_name.clone()),
            mode: post_analysis_mode,
            groups,
        }),
    };

    emit(
        &state.out,
        events::REVIEW_STARTED,
        serde_json::to_value(ReviewStartedEvent {
            operation_id: op_id.clone(),
            model: model.clone(),
            // The audit window is per group: the launch event announces
            // the session, and every group's own started event carries its
            // real file count and token estimate.
            files_count: 0,
            estimated_tokens: 0,
            omitted_files: 0,
        })
        .ok(),
    );

    let state_for_spawn = Arc::clone(state);
    let op_id_for_spawn = op_id.clone();

    tokio::spawn(async move {
        let state = state_for_spawn;
        let op_id = op_id_for_spawn;

        // A cancel delivered while the launch was being acknowledged must
        // not persist an empty session the developer already abandoned, so
        // the signal is re-checked right before the snapshot touches disk.
        if *operation.cancel_receiver().borrow() {
            emit(
                &state.out,
                events::REVIEW_CANCELLED,
                Some(serde_json::json!({ "operation_id": op_id })),
            );
            return;
        }
        // The supervisor appends every group's findings to the stored
        // session, so the launch snapshot is persisted before the batch
        // starts and a group that fails withholds only its own findings.
        let save_root = repo_root.clone();
        let session_save = session.clone();
        let saved =
            offload_review(move || ReviewStorage::save_session(&save_root, &session_save)).await;
        if let Err(err) = saved {
            emit_error(&state.out, &op_id, err);
            return;
        }
        tracing::info!(
            review_id = review_id.as_str(),
            preset = preset_name.as_str(),
            groups = group_count,
            "preset review session persisted"
        );

        let batch_op = Arc::clone(&operation);
        let batch = run_group_audits(&state, &repo_root, &session, batch_op).await;
        let report = match batch {
            Ok(report) => report,
            Err(err) => {
                // The supervisor could not construct the reviewer transport,
                // so no group ran and the launch surfaces the failure.
                emit_error(&state.out, &op_id, err);
                return;
            }
        };

        match &report.stop {
            // A hard cancellation mirrors the plain launch's terminal event;
            // the supervisor already reported each interrupted group.
            BatchStop::Cancelled => {
                emit(
                    &state.out,
                    events::REVIEW_CANCELLED,
                    Some(serde_json::json!({ "operation_id": op_id })),
                );
                return;
            }
            // An infrastructure failure stopped the queue: the failing
            // group's terminal event carries the diagnostic, so the launch
            // reports the stop without inventing a second reason.
            BatchStop::Failed { step_id } => {
                emit_error(
                    &state.out,
                    &op_id,
                    format!("Review group batch stopped on failed group '{step_id}'"),
                );
                return;
            }
            BatchStop::Blocked { step_id, reason } => {
                emit_error(
                    &state.out,
                    &op_id,
                    format!("Review group batch stopped on blocked group '{step_id}': {reason}"),
                );
                return;
            }
            // A completed or softly stopped queue leaves a durable record of
            // every group that ran, so the launch finishes normally.
            BatchStop::Completed | BatchStop::Stopped => {}
        }

        // The supervisor appended its findings to the stored session, so the
        // durable record — not the empty launch snapshot — is what the finish
        // event and an automatic classification read.
        let load_root = repo_root.clone();
        let load_id = review_id.clone();
        let stored = offload_review(move || ReviewStorage::load_review(&load_root, &load_id)).await;
        let mut final_session = match stored {
            Ok(Some(stored)) => stored,
            Ok(None) => {
                emit_error(
                    &state.out,
                    &op_id,
                    format!("Review session '{review_id}' disappeared after the group batch"),
                );
                return;
            }
            Err(err) => {
                emit_error(&state.out, &op_id, err);
                return;
            }
        };

        // The automatic classification is a layer over a fully completed
        // queue: a cancelled, failed, or softly stopped run keeps the
        // findings as the durable record instead of spending another model
        // call the developer already interrupted.
        if post_analysis_mode == PostAnalysisMode::Automatic && report.is_success() {
            let root = repo_root.as_path();
            let guard = operation.as_ref();
            match run_review_post_analysis(&state, root, &final_session, guard, true).await {
                Ok(analyzed) => final_session = analyzed,
                Err(err) => {
                    // The findings stay durable and published by the
                    // supervisor; only the classification layer failed.
                    emit_error(&state.out, &op_id, err.message);
                    return;
                }
            }
        }

        // Recording the review appends to `history.jsonl` behind an `fsync`
        // barrier, so it runs on the blocking pool under the exclusive
        // history guard instead of stalling an async worker thread.
        let history_model = model.to_string();
        let history_description = final_session.description.clone();
        let history_files = final_session.target_files.clone();
        let history_items = final_session.items.len();
        let history_state = Arc::clone(&state);
        let recorded = crate::handlers::with_history_mutation(&history_state, move |history| {
            history.record_review(
                history_model,
                user_prompt,
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

        // The finished event carries the batch's aggregate usage: each group
        // already reported its own, and the session total comes from the
        // shared ledger rather than a second accumulated counter.
        let aggregate = ModelUsageInfo {
            prompt_tokens: report.usage.prompt_tokens,
            completion_tokens: report.usage.completion_tokens,
            total_tokens: report.usage.total_tokens,
            reasoning_tokens: None,
            cached_tokens: None,
            cost: report.usage.cost,
        };
        let current_cost = report.usage.cost.unwrap_or(0.0);
        let session_total_cost = state.usage.lock().await.session.cost;
        emit(
            &state.out,
            events::REVIEW_FINISHED,
            serde_json::to_value(ReviewFinishedEvent {
                operation_id: op_id,
                session: final_session,
                usage: Some(aggregate),
                session_total_cost: Some(session_total_cost),
                current_cost: Some(current_cost),
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

/// Projects synthesized findings into review items carrying a UUIDv7 identity.
///
/// The reviewer emits findings without identifiers, while the post-analysis
/// layer and the Architect decomposition reference a finding by its UUIDv7.
/// Minting the identity before the session is persisted guarantees that every
/// stored finding of a single audit is addressable, exactly as the grouped
/// audit guarantees it through `ReviewStorage::append_group_findings`. An
/// identity a finding already carries is preserved, so a projection never
/// mints a second one for the same finding.
fn with_finding_identities(issues: Vec<ReviewIssue>) -> Vec<ReviewItem> {
    let mut items = Vec::with_capacity(issues.len());
    for issue in issues {
        let mut item = ReviewItem::from(issue);
        if item.uuid.is_none() {
            item.uuid = Some(generate_uuid_v7());
        }
        items.push(item);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::review::uuid::is_valid_uuid_v7;
    use tauqe_protocol::{IssueStatus, IssueType};

    /// Builds a finding exactly as the reviewer parser emits it: a complete
    /// issue without an identity.
    fn synthesized_issue(id: u32, title: &str) -> ReviewIssue {
        ReviewIssue {
            id,
            title: title.to_string(),
            uuid: None,
            group: None,
            model: None,
            issue_type: IssueType::Quality,
            status: IssueStatus::Discussion,
            description: "Description".to_string(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            plan: Vec::new(),
            note: None,
        }
    }

    #[test]
    fn every_synthesized_finding_receives_a_valid_uuid_v7() {
        let first = synthesized_issue(1, "First finding");
        let second = synthesized_issue(2, "Second finding");
        let items = with_finding_identities(vec![first, second]);
        assert_eq!(items.len(), 2);

        let mut identities = Vec::new();
        for item in &items {
            let identity = item.uuid.clone().expect("finding identity");
            assert!(is_valid_uuid_v7(&identity), "not a UUIDv7: {identity}");
            identities.push(identity);
        }
        assert_ne!(identities[0], identities[1], "identities are unique");
        assert_eq!(items[0].title, "First finding");
        assert_eq!(items[1].id, 2);
        assert_eq!(items[0].status, IssueStatus::Discussion);
    }

    #[test]
    fn an_identity_a_finding_already_carries_is_preserved() {
        let existing = generate_uuid_v7();
        let mut issue = synthesized_issue(1, "Carried identity");
        issue.uuid = Some(existing.clone());

        let items = with_finding_identities(vec![issue]);
        assert_eq!(items[0].uuid.as_deref(), Some(existing.as_str()));
    }
}
