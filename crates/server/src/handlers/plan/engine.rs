//! Architect streaming and plan horizon-synthesis engine.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tauqe_core::context::ContextFileContent;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::{
    create_provider, ChatMessage, JsonSchemaDefinition, LlmProvider, ResponseFormat, StreamEvent,
};
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    errors, events, ContextAccess, Event, ModelDeltaEvent, ModelErrorEvent, ModelFinishedEvent,
    ModelRole, ModelSelection, ModelStartedEvent, Plan, PlanItem, PlanRefineResult, Response,
    TurnPhase, TurnPhaseEvent,
};
use tokio::sync::mpsc;

use crate::state::{AppState, OperationGuard};

use super::common::{
    clear_auto_context, emit, find_duplicate_item_ids, offload_plan, with_context_mutation,
};

/// Resolves the LLM provider for architect plan operations (refine and replan),
/// consulting the test override in test configurations before falling back to
/// network provider construction.
pub(crate) async fn resolve_architect_provider(
    _state: &Arc<AppState>,
    cfg: &tauqe_core::config::AppConfig,
    provider_kind: tauqe_protocol::ProviderKind,
) -> Result<Arc<dyn LlmProvider>, String> {
    #[cfg(test)]
    {
        if let Some(p) = _state.provider_override.lock().await.clone() {
            return Ok(p);
        }
    }
    create_provider(provider_kind, cfg)
        .map(Arc::from)
        .map_err(|err| err.to_string())
}

pub(crate) struct ArchitectStreamOutcome {
    pub(crate) raw_text: String,
    pub(crate) streamed_explanation_bytes: usize,
}

pub(crate) enum ArchitectStreamError {
    Cancelled,
    ModelError(String),
}

/// Executes a streaming structured-chat turn with the architect model, streaming
/// reasoning deltas and explanation text deltas to the client, handling cancellation
/// and errors, and committing token usage to session state.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_architect_stream(
    state: &Arc<AppState>,
    provider: Arc<dyn LlmProvider>,
    model_name: String,
    messages: Vec<ChatMessage>,
    response_format: Option<ResponseFormat>,
    reasoning_effort: Option<tauqe_protocol::ReasoningEffort>,
    operation: &OperationGuard,
    op_id: &str,
) -> Result<ArchitectStreamOutcome, ArchitectStreamError> {
    let cancel_rx = operation.cancel_receiver();
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
    let stream_cancel = cancel_rx.clone();
    let provider_stream = Arc::clone(&provider);

    let stream_task = tokio::spawn(async move {
        provider_stream
            .stream_chat_with_effort(
                &model_name,
                messages,
                response_format,
                reasoning_effort,
                tx,
                stream_cancel,
            )
            .await
    });

    operation.set_abort_handle(stream_task.abort_handle());

    let mut raw_text = String::new();
    let mut usage = None;
    let mut failure = None;
    let mut stream_cancelled = false;
    let mut passthrough: Option<bool> = None;
    let mut explanation_cursor =
        tauqe_core::edits::stream::json::decode::StringFieldCursor::new();
    let mut streamed_explanation_bytes = 0usize;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::ReasoningDelta(delta) => {
                state.out.send_event(&Event {
                    method: events::MODEL_REASONING_DELTA.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id,
                        "delta": delta,
                    })),
                });
            }
            StreamEvent::TextDelta(delta) => {
                raw_text.push_str(&delta);

                if passthrough.is_none() {
                    if let Some(c) = raw_text.chars().find(|c| !c.is_whitespace()) {
                        let plain = c != '{' && c != '`';
                        passthrough = Some(plain);
                        if plain {
                            state.out.send_event(&Event {
                                method: events::MODEL_TEXT_DELTA.to_string(),
                                params: serde_json::to_value(ModelDeltaEvent {
                                    operation_id: op_id.to_string(),
                                    delta: raw_text.clone(),
                                })
                                .ok(),
                            });
                        }
                    }
                } else if passthrough == Some(true) {
                    state.out.send_event(&Event {
                        method: events::MODEL_TEXT_DELTA.to_string(),
                        params: serde_json::to_value(ModelDeltaEvent {
                            operation_id: op_id.to_string(),
                            delta,
                        })
                        .ok(),
                    });
                }

                if passthrough != Some(true) {
                    let (field_delta, _) =
                        tauqe_core::edits::stream::json::extract_streamed_string_field(
                            &raw_text,
                            "explanation",
                            &mut explanation_cursor,
                        );
                    if let Some(d) = field_delta {
                        if !d.is_empty() {
                            streamed_explanation_bytes += d.len();
                            state.out.send_event(&Event {
                                method: events::MODEL_TEXT_DELTA.to_string(),
                                params: serde_json::to_value(ModelDeltaEvent {
                                    operation_id: op_id.to_string(),
                                    delta: d,
                                })
                                .ok(),
                            });
                        }
                    }
                }
            }
            StreamEvent::Usage(u) => usage = Some(u),
            StreamEvent::Done => break,
            // The provider may report cancellation as an explicit stream event
            // even when the turn's cancel signal has not been observed yet, so
            // the event is recorded instead of inferred from `cancel_rx` alone.
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

    let cancelled = *cancel_rx.borrow() || stream_cancelled;
    if cancelled {
        stream_task.abort();
        state.out.send_event(&Event {
            method: events::MODEL_CANCELLED.to_string(),
            params: Some(serde_json::json!({ "operation_id": op_id })),
        });
        return Err(ArchitectStreamError::Cancelled);
    }
    if let Some(err) = failure {
        stream_task.abort();
        state.out.send_event(&Event {
            method: events::MODEL_ERROR.to_string(),
            params: serde_json::to_value(ModelErrorEvent {
                operation_id: op_id.to_string(),
                message: err.clone(),
            })
            .ok(),
        });
        return Err(ArchitectStreamError::ModelError(err));
    }

    if let Err(join_err) = stream_task.await {
        let err_msg = join_err.to_string();
        state.out.send_event(&Event {
            method: events::MODEL_ERROR.to_string(),
            params: serde_json::to_value(ModelErrorEvent {
                operation_id: op_id.to_string(),
                message: err_msg.clone(),
            })
            .ok(),
        });
        return Err(ArchitectStreamError::ModelError(err_msg));
    }

    let usage = usage.unwrap_or_default();
    let cost = usage.cost.unwrap_or(0.0);
    let total_cost = {
        let mut total = state.total_cost.lock().await;
        *total += cost;
        *total
    };

    state.out.send_event(&Event {
        method: events::MODEL_USAGE.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
            operation_id: op_id.to_string(),
            usage,
            session_total_cost: total_cost,
            current_cost: Some(cost),
        })
        .ok(),
    });

    Ok(ArchitectStreamOutcome {
        raw_text,
        streamed_explanation_bytes,
    })
}

/// Static configuration for one architect discovery loop run.
///
/// Plan horizon synthesis and review finding decomposition share one multi-round
/// architect protocol: proposal and discovery telemetry, structured streaming,
/// `needs_context` feedback, and token-budget enforcement. The loop is
/// flow-agnostic; a caller supplies only the prompts, the streamed header, the
/// budget diagnostic hint, the operational budgets, and a closure that
/// assembles the per-round user prompt.
pub(crate) struct ArchitectDiscoveryRequest<'a> {
    pub(crate) repo_root: &'a Path,
    pub(crate) op_id: &'a str,
    pub(crate) operation_title: &'a str,
    pub(crate) system_prompt: &'a str,
    pub(crate) intro_header: &'a str,
    pub(crate) budget_hint: &'a str,
    pub(crate) response_format: ResponseFormat,
    pub(crate) repomap_budget: usize,
    pub(crate) max_discovery_rounds: usize,
    pub(crate) files_token_budget: usize,
}

/// Outcome of a completed architect discovery loop.
pub(crate) struct ArchitectDiscoveryOutcome {
    /// The parsed architect output of the final round.
    pub(crate) output: tauqe_core::plan::prompt::refine::PlanRefinementOutput,
    /// Set when discovery exceeded the files token budget, carrying the
    /// diagnostic the caller reports to the client as a blocked result.
    pub(crate) budget_exceeded: Option<String>,
}

/// Terminal error of the architect discovery loop.
pub(crate) enum ArchitectDiscoveryError {
    Cancelled,
    ModelError(String),
    ParseError(String),
}

/// Runs the shared architect discovery loop: streaming structured proposals,
/// resolving `needs_context` rounds against the repository, enforcing the files
/// token budget, and feeding `<context_feedback>` back to the model.
///
/// The loop emits the `Proposal` and `Discovery` turn phases, streams the
/// architect's explanation and round banners, and terminates once the model
/// finalizes a status or a discovery round adds nothing new. Cancellation,
/// provider failures, and unparseable output propagate as
/// [`ArchitectDiscoveryError`]; the caller keeps ownership of the RPC result and
/// of the session-history response, so a failed synthesis never poisons the
/// audit trail.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_architect_discovery_loop<F>(
    state: &Arc<AppState>,
    provider: Arc<dyn LlmProvider>,
    model_name: &str,
    request: ArchitectDiscoveryRequest<'_>,
    build_user_prompt: F,
    reasoning_effort: Option<tauqe_protocol::ReasoningEffort>,
    operation: &OperationGuard,
) -> Result<ArchitectDiscoveryOutcome, ArchitectDiscoveryError>
where
    F: Fn(&[ContextFileContent], Option<&str>) -> String + Send,
{
    let ArchitectDiscoveryRequest {
        repo_root,
        op_id,
        operation_title,
        system_prompt,
        intro_header,
        budget_hint,
        response_format,
        repomap_budget,
        max_discovery_rounds,
        files_token_budget,
    } = request;

    let mut discovery_round = 0usize;
    let mut feedback: Option<String> = None;
    let mut accumulated_full_text = intro_header.to_string();

    let output = loop {
        emit(
            &state.out,
            events::TURN_PHASE,
            serde_json::to_value(TurnPhaseEvent {
                operation_id: op_id.to_string(),
                phase: TurnPhase::Proposal,
                round: Some(discovery_round + 1),
                max_rounds: None,
                detail: Some(operation_title.to_string()),
            })
            .ok(),
        );

        // Snapshot the context manager under a short-lived read guard, then
        // release the guard before any disk or git work. Reading the files and
        // generating the repo map are offloaded so they never stall an async
        // worker thread.
        let ctx_snapshot = state.context.read().await.clone();
        let context_files = crate::handlers::offload(move || ctx_snapshot.read_context_files())
            .await
            .unwrap_or_default();
        let context_paths: Vec<String> =
            context_files.iter().map(|f| f.path.clone()).collect();
        let repo_map = {
            let root = repo_root.to_path_buf();
            crate::handlers::offload(move || {
                let all_files =
                    tauqe_core::git::list_repository_files(Some(&root)).unwrap_or_default();
                tauqe_core::repomap::generate_repo_map(
                    &root,
                    &all_files,
                    &context_paths,
                    repomap_budget,
                )
                .ok()
            })
            .await
            .unwrap_or(None)
        };

        let mut user_prompt = build_user_prompt(&context_files, repo_map.as_deref());

        if let Some(ref fb) = feedback {
            user_prompt.push_str("\n\n");
            user_prompt.push_str(fb);
        }

        if discovery_round >= max_discovery_rounds {
            user_prompt.push_str(&format!(
                "\n\n<context_feedback>\nReached maximum context discovery rounds limit ({}). You MUST now finalize the plan with status 'success' or 'blocked'. Do not request further files.\n</context_feedback>",
                max_discovery_rounds
            ));
        }

        let messages = vec![
            ChatMessage::system(system_prompt.to_string()),
            ChatMessage::user(user_prompt),
        ];

        let stream_outcome = match run_architect_stream(
            state,
            Arc::clone(&provider),
            model_name.to_string(),
            messages,
            Some(response_format.clone()),
            reasoning_effort,
            operation,
            op_id,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(ArchitectStreamError::Cancelled) => {
                return Err(ArchitectDiscoveryError::Cancelled);
            }
            Err(ArchitectStreamError::ModelError(err)) => {
                return Err(ArchitectDiscoveryError::ModelError(err));
            }
        };

        let ArchitectStreamOutcome {
            raw_text,
            streamed_explanation_bytes,
        } = stream_outcome;

        let output = match tauqe_core::plan::prompt::parse_plan_refinement_output(&raw_text) {
            Ok(out) => out,
            Err(err) => {
                let err_msg = format!("Failed to parse architect refinement output: {err}");
                emit(
                    &state.out,
                    events::MODEL_ERROR,
                    serde_json::to_value(ModelErrorEvent {
                        operation_id: op_id.to_string(),
                        message: err_msg.clone(),
                    })
                    .ok(),
                );
                return Err(ArchitectDiscoveryError::ParseError(err_msg));
            }
        };

        if streamed_explanation_bytes == 0 && !output.explanation.trim().is_empty() {
            emit(
                &state.out,
                events::MODEL_TEXT_DELTA,
                serde_json::to_value(ModelDeltaEvent {
                    operation_id: op_id.to_string(),
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

            let searches = output.repo_searches.clone();

            let ctx_root = repo_root.to_path_buf();
            let (outcome, new_ctx_state) = match with_context_mutation(state, move |ctx| {
                let available_files = tauqe_core::git::list_repository_files(Some(&ctx_root))
                    .ok()
                    .unwrap_or_default();
                let oc = apply_context_requests_detailed(
                    ctx,
                    &requests,
                    &available_files,
                    files_token_budget,
                );
                let new_state = ctx.get_state();
                (oc, new_state)
            })
            .await
            {
                Ok(result) => result,
                Err(err) => return Err(ArchitectDiscoveryError::ModelError(err)),
            };

            if let Some(limit) = outcome.limit_exceeded {
                let warn_msg = format!(
                    "Context files token budget exceeded (files_token_budget = {}). Architect requested {} new token(s) in file(s) [{}], but context already has {} tokens. {}",
                    limit.token_budget,
                    limit.requested_tokens,
                    limit.requested_files.join(", "),
                    limit.current_tokens,
                    budget_hint
                );
                tracing::warn!("{}", warn_msg);
                emit(
                    &state.out,
                    events::MODEL_TEXT_DELTA,
                    serde_json::to_value(ModelDeltaEvent {
                        operation_id: op_id.to_string(),
                        delta: format!("\n\n⚠️ {}\n\n", warn_msg),
                    })
                    .ok(),
                );
                emit(
                    &state.out,
                    events::MODEL_FINISHED,
                    serde_json::to_value(ModelFinishedEvent {
                        operation_id: op_id.to_string(),
                        full_text: format!("{}\n\n⚠️ {}", accumulated_full_text, warn_msg),
                    })
                    .ok(),
                );
                clear_auto_context(state).await;
                return Ok(ArchitectDiscoveryOutcome {
                    output,
                    budget_exceeded: Some(warn_msg),
                });
            }

            let search_results = if !searches.is_empty() {
                let s_root = repo_root.to_path_buf();
                crate::handlers::offload(move || {
                    let available_files =
                        tauqe_core::git::list_repository_files(Some(&s_root)).unwrap_or_default();
                    tauqe_core::workflow::discovery::execute_repo_searches(
                        &s_root,
                        &searches,
                        &available_files,
                    )
                })
                .await
                .unwrap_or_default()
            } else {
                Vec::new()
            };

            if !outcome.added.is_empty() {
                emit(
                    &state.out,
                    events::CONTEXT_CHANGED,
                    Some(serde_json::json!({ "state": new_ctx_state })),
                );
            }

            // A round that only re-requests files already present in context is
            // not a dead end: the model still needs their feedback and another
            // analysis round, so the loop terminates only when a round adds
            // nothing new, reports nothing missing, executes no searches, and leaves no requested file
            // already satisfied.
            if outcome.added.is_empty()
                && outcome.missing.is_empty()
                && outcome.already_satisfied.is_empty()
                && search_results.is_empty()
            {
                break output;
            }

            discovery_round += 1;
            tracing::info!(
                operation = operation_title,
                round = discovery_round,
                added = outcome.added.len(),
                missing = outcome.missing.len(),
                searches = search_results.len(),
                "architect discovery round"
            );
            emit(
                &state.out,
                events::TURN_PHASE,
                serde_json::to_value(TurnPhaseEvent {
                    operation_id: op_id.to_string(),
                    phase: TurnPhase::Discovery,
                    round: Some(discovery_round),
                    max_rounds: Some(max_discovery_rounds),
                    detail: Some(format!("Discovery round {}", discovery_round)),
                })
                .ok(),
            );

            let mut notes = Vec::new();
            if !outcome.added.is_empty() {
                notes.push(format!(
                    "Added {} file(s) to context: {}",
                    outcome.added.len(),
                    outcome.added.join(", ")
                ));
            }
            if !search_results.is_empty() {
                notes.push(format!(
                    "Executed {} repository search(es)",
                    search_results.len()
                ));
            }
            if !outcome.missing.is_empty() {
                notes.push(format!(
                    "Files not found in repository: {}",
                    outcome.missing.join(", ")
                ));
            }
            if !outcome.already_satisfied.is_empty() && outcome.added.is_empty() && search_results.is_empty() {
                notes.push(format!(
                    "Files already in context: {}",
                    outcome.already_satisfied.join(", ")
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
                    operation_id: op_id.to_string(),
                    delta: round_banner,
                })
                .ok(),
            );

            let mut fb = String::new();
            if !search_results.is_empty() {
                let search_feedback =
                    tauqe_core::workflow::discovery::format_search_feedback(&search_results);
                fb.push_str(&search_feedback);
                fb.push('\n');
            }
            if !outcome.missing.is_empty() {
                fb.push_str(&format!(
                    "<context_feedback>\nThe following requested files do not exist in the repository: [{}]\nPlease check <repo_map> for exact existing file paths.\n</context_feedback>\n",
                    outcome.missing.join(", ")
                ));
            }
            if !outcome.already_satisfied.is_empty() && outcome.added.is_empty() && search_results.is_empty() {
                fb.push_str(&format!(
                    "<context_feedback>\nThe following files are already loaded in context: [{}]\nYou may proceed with inspecting them and refining the plan.\n</context_feedback>\n",
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
            operation_id: op_id.to_string(),
            full_text: accumulated_full_text,
        })
        .ok(),
    );

    clear_auto_context(state).await;

    Ok(ArchitectDiscoveryOutcome {
        output,
        budget_exceeded: None,
    })
}

/// Diagnostics describing a failed or blocked step for horizon synthesis.
pub(super) struct PlanHorizonDiagnosticsOwned {
    pub blocked_step_id: String,
    pub blocked_reason: Option<String>,
    pub toolchain_status: Option<String>,
}

/// Normalizes a declared plan file path into the repository-relative form used
/// for context preloading, trimming whitespace and stripping leading `./` and
/// `/` markers so cosmetic spelling differences collapse to one key.
fn normalize_preload_path(path: &str) -> &str {
    path.trim().trim_start_matches("./").trim_start_matches('/')
}

/// Recursively collects every declared target (`files`) and reference
/// (`readonly_files`) path across the whole plan, deduplicating normalized
/// paths while preserving first-seen order.
///
/// Leaves are the authoritative holders of declared files (see the leaf-only
/// file invariant), but the walk visits every node so a plan carrying residual
/// parent files still contributes them. The result feeds
/// `preload_readonly_files`, giving the architect visibility of every file an
/// existing step already targets or references rather than a single step's.
pub(super) fn collect_plan_context_files(plan: &Plan) -> Vec<String> {
    fn walk(items: &[PlanItem], out: &mut Vec<String>, seen: &mut HashSet<String>) {
        for item in items {
            for path in item.files.iter().chain(item.readonly_files.iter()) {
                let normalized = normalize_preload_path(path);
                if normalized.is_empty() {
                    continue;
                }
                if seen.insert(normalized.to_string()) {
                    out.push(normalized.to_string());
                }
            }
            walk(&item.children, out, seen);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(&plan.items, &mut out, &mut seen);
    out
}

/// Loads declared target and reference files into the read-only auto context
/// layer before the architect begins deep planning or replanning, so every
/// existing step's real code is visible on the first generation round. Declared
/// target files are never capped by the files token budget, matching the
/// step-execution preload contract.
async fn preload_readonly_files(state: &Arc<AppState>, repo_root: &Path, files: &[String]) {
    let requests: Vec<ContextRequest> = files
        .iter()
        .map(String::as_str)
        .map(normalize_preload_path)
        .filter(|p| !p.is_empty())
        .map(|p| ContextRequest {
            path: p.to_string(),
            access: ContextAccess::ReadOnly,
        })
        .collect();
    if requests.is_empty() {
        return;
    }
    let root = repo_root.to_path_buf();
    let preloaded = with_context_mutation(state, move |ctx| {
        let available_files = tauqe_core::git::list_repository_files(Some(&root))
            .ok()
            .unwrap_or_default();
        let outcome = apply_context_requests_detailed(ctx, &requests, &available_files, usize::MAX);
        let new_state = ctx.get_state();
        (outcome, new_state)
    })
    .await;

    if let Ok((outcome, new_ctx_state)) = preloaded {
        if !outcome.added.is_empty() {
            emit(
                &state.out,
                events::CONTEXT_CHANGED,
                Some(serde_json::json!({ "state": new_ctx_state })),
            );
        }
    }
}

/// Unified engine for architect plan horizon synthesis.
///
/// Drives streaming structured communication with the architect model, runs multi-round
/// context discovery on `needs_context`, validates the synthesized horizon, and applies it
/// atomically via `PlanStorage::replace_uncompleted_horizon`.
///
/// Enforces strict session history transactionality: `state.history.record_response` is
/// called strictly AFTER `replace_uncompleted_horizon` succeeds on disk. Any persistence
/// or validation error returns early without poisoning the interaction audit trail.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_plan_horizon_synthesis(
    req_id: tauqe_protocol::RequestId,
    state: &Arc<AppState>,
    plan: Plan,
    diagnostics: Option<PlanHorizonDiagnosticsOwned>,
    custom_model: Option<ModelSelection>,
    instructions: Option<String>,
    operation_title: &str,
    operation: &OperationGuard,
) -> Response {
    let repo_root = state.context.read().await.repo_root().to_path_buf();

    tracing::info!(
        plan_id = %plan.id,
        operation = operation_title,
        replanning = diagnostics.is_some(),
        blocked_step = ?diagnostics.as_ref().map(|d| d.blocked_step_id.as_str()),
        "plan horizon synthesis started"
    );

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
        let architect_entry = cfg
            .models
            .role_entries(ModelRole::Architect)
            .into_iter()
            .next();
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

    // Guarantee baseline visibility for both deep planning and replanning:
    // collect every existing step's declared target and reference files and
    // load them into the read-only auto layer before the first generation
    // round, so the architect sees the real code instead of inferring it from
    // the repo map alone.
    let plan_files = collect_plan_context_files(&plan);
    preload_readonly_files(state, &repo_root, &plan_files).await;

    let op_id = format!("op-{}", crate::state::next_operation_id());

    state.out.send_event(&Event {
        method: events::MODEL_STARTED.to_string(),
        params: serde_json::to_value(ModelStartedEvent {
            operation_id: op_id.clone(),
            model: model.clone(),
            role: Some(ModelRole::Architect),
            alias,
            effort: reasoning_effort,
            plan_context: None,
            is_batch: false,
        })
        .ok(),
    });

    let intro_header = match &diagnostics {
        Some(diag) => format!(
            "# {}: [{}] {}\n*Replanning uncompleted horizon around blocked step `{}`...*\n\n---\n\n",
            operation_title, plan.id, plan.title, diag.blocked_step_id
        ),
        None => format!(
            "# {}: [{}] {}\n*Analyzing codebase contours and decomposing architectural plan...*\n\n---\n\n",
            operation_title, plan.id, plan.title
        ),
    };
    state.out.send_event(&Event {
        method: events::MODEL_TEXT_DELTA.to_string(),
        params: serde_json::to_value(ModelDeltaEvent {
            operation_id: op_id.clone(),
            delta: intro_header.clone(),
        })
        .ok(),
    });

    {
        let ctx = state.context.read().await;
        let context_paths: Vec<String> = ctx
            .get_state()
            .items
            .into_iter()
            .map(|it| it.path)
            .collect();
        let mut history_turn_prompt =
            format!("# {}: [{}] {}\n", operation_title, plan.id, plan.title);
        if let Some(ref diag) = diagnostics {
            history_turn_prompt.push_str(&format!("\nBlocked Step: #{}", diag.blocked_step_id));
            if let Some(ref r) = diag.blocked_reason {
                history_turn_prompt.push_str(&format!("\nReason: {}", r));
            }
        }
        if let Some(ins) = &instructions {
            let trimmed = ins.trim();
            if !trimmed.is_empty() {
                history_turn_prompt.push_str(&format!("\nDirectives: {}\n", trimmed));
            }
        }
        let _ = state
            .history
            .lock()
            .await
            .record_turn(history_turn_prompt, context_paths);
    }

    let response_format = {
        let schema_val = tauqe_core::plan::prompt::plan_refine_response_schema_definition();
        match serde_json::from_value::<JsonSchemaDefinition>(schema_val) {
            Ok(schema) => ResponseFormat::JsonSchema { json_schema: schema },
            Err(err) => return Response::err(req_id, errors::SCHEMA_ERROR, err.to_string()),
        }
    };
    let system_prompt = tauqe_core::plan::prompt::build_plan_horizon_system_prompt();
    let discovery = ArchitectDiscoveryRequest {
        repo_root: repo_root.as_path(),
        op_id: &op_id,
        operation_title,
        system_prompt: &system_prompt,
        intro_header: &intro_header,
        budget_hint: "The plan is too broad and should be manually decomposed or scoped down.",
        response_format,
        repomap_budget,
        max_discovery_rounds,
        files_token_budget,
    };

    let build_user_prompt = |context_files: &[ContextFileContent], repo_map: Option<&str>| {
        let diag_ref = diagnostics.as_ref().map(|d| {
            tauqe_core::plan::prompt::PlanHorizonDiagnostics {
                blocked_step_id: &d.blocked_step_id,
                blocked_reason: d.blocked_reason.as_deref(),
                toolchain_status: d.toolchain_status.as_deref(),
            }
        });
        tauqe_core::plan::prompt::build_plan_horizon_user_prompt(
            &plan,
            context_files,
            repo_map,
            diag_ref,
            instructions.as_deref(),
        )
    };

    let output = match run_architect_discovery_loop(
        state,
        Arc::clone(&provider),
        &model.name,
        discovery,
        build_user_prompt,
        reasoning_effort,
        operation,
    )
    .await
    {
        Ok(ArchitectDiscoveryOutcome {
            output,
            budget_exceeded: None,
        }) => output,
        Ok(ArchitectDiscoveryOutcome {
            budget_exceeded: Some(warn_msg),
            ..
        }) => {
            let _ = state.history.lock().await.record_response(
                warn_msg.clone(),
                None,
                Some(format!(
                    "{}: Token Budget Exceeded ({})",
                    operation_title, plan.title
                )),
                Vec::new(),
            );
            return Response::ok_typed(req_id, &PlanRefineResult::Blocked { reason: warn_msg });
        }
        Err(ArchitectDiscoveryError::Cancelled) => {
            clear_auto_context(state).await;
            return Response::err(
                req_id,
                errors::CANCELLED,
                format!("{} was cancelled", operation_title),
            );
        }
        Err(ArchitectDiscoveryError::ModelError(err)) => {
            clear_auto_context(state).await;
            return Response::err(req_id, errors::MODEL_ERROR, err);
        }
        Err(ArchitectDiscoveryError::ParseError(err)) => {
            clear_auto_context(state).await;
            return Response::err(req_id, errors::PARSE_ERROR, err);
        }
    };

    if output.status.eq_ignore_ascii_case("blocked") {
        let reason = output.reason.unwrap_or(output.explanation);
        tracing::warn!(
            plan_id = %plan.id,
            reason = %reason,
            "architect reported blockers"
        );
        let blocked_msg =
            format!("Architect reported blockers for plan [{}]:\n\n{}", plan.id, reason);
        let _ = state.history.lock().await.record_response(
            blocked_msg,
            None,
            Some(format!("{}: Blocked ({})", operation_title, plan.title)),
            Vec::new(),
        );
        return Response::ok_typed(req_id, &PlanRefineResult::Blocked { reason });
    }

    if output.status.eq_ignore_ascii_case("needs_context") {
        return Response::err(
            req_id,
            errors::NEEDS_CONTEXT,
            format!(
                "Architect requested additional context but no discovery rounds remain (limit: {}). Narrow the plan scope or raise the discovery round limit, then refine again",
                max_discovery_rounds
            ),
        );
    }

    // Accept only an explicit success: an unknown status (for example a stray
    // `needs_context` that survived the round limit) must never be treated as a
    // completed refinement, or it could overwrite the plan with an unvalidated tree.
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
                "Architect returned a plan with a duplicate or empty step ID: '{}'",
                duplicate
            ),
        );
    }

    // Replace the uncompleted horizon while preserving the immutable Done foundation.
    let new_steps = output.items.len();
    let horizon_root = repo_root.clone();
    let horizon_plan_id = plan.id.clone();
    let horizon_items = output.items;
    let updated_plan = match offload_plan(move || {
        PlanStorage::replace_uncompleted_horizon(&horizon_root, &horizon_plan_id, horizon_items)
    })
    .await
    {
        Ok(p) => p,
        Err(err) => return Response::err(req_id, errors::PLAN_SAVE_FAILED, err),
    };

    // Strict history transactionality: record the assistant response ONLY after
    // the synthesized horizon has been atomically written to disk.
    let (total_items, completed_items) = updated_plan.count_stats();
    tracing::info!(
        plan_id = %updated_plan.id,
        total_items = total_items,
        completed_items = completed_items,
        new_steps = new_steps,
        "plan horizon synthesized"
    );
    let history_response = format!(
        "Plan [{}] horizon synthesized successfully ({} total, {} completed).\n\n{}",
        updated_plan.id, total_items, completed_items, output.explanation
    );
    let _ = state.history.lock().await.record_response(
        history_response,
        None,
        Some(format!("{}: {}", operation_title, updated_plan.title)),
        Vec::new(),
    );

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(tauqe_protocol::PlanUpdatedEvent {
            plan: updated_plan.clone(),
            is_save: false,
        })
        .ok(),
    );

    let list_root = repo_root.clone();
    if let Ok(plans) = offload_plan(move || PlanStorage::load_all(&list_root)).await {
        let active_root = repo_root.clone();
        let active_id = offload_plan(move || PlanStorage::load_active_id(&active_root))
            .await
            .ok()
            .flatten();
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(tauqe_protocol::PlanListChangedEvent { plans, active_id }).ok(),
        );
    }

    Response::ok_typed(
        req_id,
        &PlanRefineResult::Success {
            plan: updated_plan,
            explanation: Some(output.explanation),
        },
    )
}
