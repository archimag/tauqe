//! Architect streaming and plan horizon-synthesis engine.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tauqe_core::context::ContextFileContent;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::{
    create_provider, format_session_id, ChatMessage, JsonSchemaDefinition, LlmProvider,
    ResponseFormat, StreamEvent, SESSION_PREFIX_ARCHITECT,
};
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    errors, events, ArchitectTier, ContextAccess, Event, LlmCommand, ModelDeltaEvent,
    ModelErrorEvent, ModelFinishedEvent, ModelRole, ModelSelection, ModelStartedEvent, Plan,
    PlanItem, PlanRefineResult, Response, TurnPhase, TurnPhaseEvent,
};
use tokio::sync::mpsc;

use crate::state::{AppState, OperationGuard};
use crate::usage::UsageGuard;

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

/// The Architect escalation applied to one replanning wave.
///
/// `from` carries the configuration recorded on the plan, which is `None` when
/// the plan predates provenance recording; `to` is the rung the ladder handed
/// out for this wave.
struct ArchitectLevelUp {
    from: Option<ArchitectTier>,
    to: ArchitectTier,
}

/// Renders an Architect configuration for user-facing banners and logs.
fn format_architect_tier(tier: &ArchitectTier) -> String {
    match tier.effort {
        Some(effort) => format!("{} ({})", tier.model, effort),
        None => tier.model.to_string(),
    }
}

/// Alias of the configured Architect entry backing a resolved rung, if any.
///
/// The alias is looked up by the rung's model and reasoning depth, so an
/// escalated wave shows the label of the configuration it actually runs on
/// instead of the base entry's label.
fn architect_alias(cfg: &tauqe_core::config::AppConfig, tier: &ArchitectTier) -> Option<String> {
    cfg.models
        .role_entries(ModelRole::Architect)
        .into_iter()
        .find(|entry| entry.model == tier.model && entry.effort == tier.effort)
        .and_then(|entry| entry.alias)
}

/// Resolves the Architect configuration for one horizon synthesis round.
///
/// An explicit developer selection is an override: the architect ladder is not
/// consulted and no escalation is reported. Otherwise a replanning wave
/// (`diagnostics` present) extracts the configuration recorded on the plan and
/// asks the ladder for the next rung, while the first synthesis of a horizon
/// always starts from the base rung. The returned [`ArchitectLevelUp`] is
/// `Some` only when a replanning wave escalated the Architect.
fn resolve_architect_tier(
    cfg: &tauqe_core::config::AppConfig,
    plan: &Plan,
    custom_model: Option<&ModelSelection>,
    diagnostics: Option<&PlanHorizonDiagnosticsOwned>,
) -> (ArchitectTier, Option<String>, Option<ArchitectLevelUp>) {
    if let Some(selection) = custom_model {
        let model = cfg.resolve_model(selection);
        let effort = cfg
            .models
            .resolve_selection_effort(ModelRole::Architect, selection);
        let tier = ArchitectTier::new(model, effort);
        let alias = architect_alias(cfg, &tier);
        return (tier, alias, None);
    }

    let tier = match diagnostics {
        Some(_) => cfg.models.next_architect_tier(plan.architect.as_ref()),
        None => cfg.models.architect_tier(0),
    };
    let level_up = diagnostics.map(|_| ArchitectLevelUp {
        from: plan.architect.clone(),
        to: tier.clone(),
    });
    let alias = architect_alias(cfg, &tier);
    (tier, alias, level_up)
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
///
/// `session_id` binds the round to the architect's long-lived design context, so
/// the provider keeps one warm prompt-prefix cache across the discovery rounds of
/// a synthesis and the later replanning waves of the same plan or finding.
///
/// `usage` is the window of the whole synthesis, not of this one round: every
/// round reports into the same window, so a multi-round synthesis is accounted
/// as a single operation instead of one artificial operation per round.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_architect_stream(
    state: &Arc<AppState>,
    provider: Arc<dyn LlmProvider>,
    model_name: String,
    messages: Vec<ChatMessage>,
    response_format: Option<ResponseFormat>,
    reasoning_effort: Option<tauqe_protocol::ReasoningEffort>,
    session_id: Option<String>,
    operation: &OperationGuard,
    op_id: &str,
    usage: &UsageGuard,
) -> Result<ArchitectStreamOutcome, ArchitectStreamError> {
    let cancel_rx = operation.cancel_receiver();
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(100);
    let stream_cancel = cancel_rx.clone();
    let provider_stream = Arc::clone(&provider);

    let stream_task = tokio::spawn(async move {
        provider_stream
            .stream_chat_with_session(
                &model_name,
                messages,
                response_format,
                reasoning_effort,
                session_id.as_deref(),
                tx,
                stream_cancel,
            )
            .await
    });

    operation.set_abort_handle(stream_task.abort_handle());

    let mut raw_text = String::new();
    let mut failure = None;
    let mut stream_cancelled = false;
    let mut passthrough: Option<bool> = None;
    let mut explanation_cursor = tauqe_core::edits::stream::json::decode::StringFieldCursor::new();
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
            StreamEvent::Usage(reported) => usage.observe(&reported).await,
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
        operation.clear_abort_handle();
        state.out.send_event(&Event {
            method: events::MODEL_CANCELLED.to_string(),
            params: Some(serde_json::json!({ "operation_id": op_id })),
        });
        return Err(ArchitectStreamError::Cancelled);
    }
    if let Some(err) = failure {
        stream_task.abort();
        operation.clear_abort_handle();
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

    // The streamed task has resolved, so its handle is dropped from the
    // operation slot immediately: the discovery loop keeps working between
    // rounds, and a hard-cancel arriving there must not target a task that is
    // already finished.
    let join_result = stream_task.await;
    operation.clear_abort_handle();

    if let Err(join_err) = join_result {
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

    // The aggregate snapshot travels through `model/usageTotals`; the legacy
    // per-operation channel is mirrored from that same snapshot, so both
    // channels always agree instead of accumulating independently.
    crate::handlers::model::emit_legacy_usage_event(state, op_id).await;

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
    /// Whether the eager global symbol map is rendered for this synthesis.
    ///
    /// When eager map generation is disabled the loop renders the compact
    /// directory outline instead, so the Architect still learns where the
    /// subsystems live without a full Tree-sitter pass over the repository.
    pub(crate) include_repo_map: bool,
    pub(crate) max_discovery_rounds: usize,
    pub(crate) files_token_budget: usize,
    /// Retry budget for a generation round whose structured response could not
    /// be parsed.
    ///
    /// A truncated or empty payload is a transport artifact as much as a
    /// semantic failure, so the round is regenerated before the synthesis is
    /// abandoned.
    pub(crate) max_parse_retries: usize,
    /// Provider session the whole synthesis binds to, so the backend keeps one
    /// warm prompt-prefix cache across its discovery rounds.
    pub(crate) session_id: Option<String>,
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
/// The repository map is generated once before the first round and reused by
/// every round, so a discovery round never shrinks the map or rewrites the
/// prompt prefix the provider has already cached.
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
    F: Fn(&[ContextFileContent], Option<&str>, Option<&str>) -> String + Send,
{
    // The whole synthesis is one accounted operation: every discovery round
    // reports into the same open window, and the window closes exactly once on
    // every terminal path — success, cancellation, and failure alike — so the
    // tokens and time the synthesis already consumed are charged instead of
    // dropped.
    let usage = state.begin_usage().await;
    let outcome = run_architect_discovery_rounds(
        state,
        provider,
        model_name,
        request,
        build_user_prompt,
        reasoning_effort,
        operation,
        &usage,
    )
    .await;
    usage.flush().await;
    outcome
}

/// Runs the discovery rounds of one synthesis inside an open usage window.
#[allow(clippy::too_many_arguments)]
async fn run_architect_discovery_rounds<F>(
    state: &Arc<AppState>,
    provider: Arc<dyn LlmProvider>,
    model_name: &str,
    request: ArchitectDiscoveryRequest<'_>,
    build_user_prompt: F,
    reasoning_effort: Option<tauqe_protocol::ReasoningEffort>,
    operation: &OperationGuard,
    usage: &UsageGuard,
) -> Result<ArchitectDiscoveryOutcome, ArchitectDiscoveryError>
where
    F: Fn(&[ContextFileContent], Option<&str>, Option<&str>) -> String + Send,
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
        include_repo_map,
        max_discovery_rounds,
        files_token_budget,
        max_parse_retries,
        session_id,
    } = request;

    // One receiver is observed for the whole loop, so a cancel signal raised
    // while discovery commands run stays visible to every later round.
    let cancel_rx = operation.cancel_receiver();
    let mut discovery_round = 0usize;
    // Consecutive unparsable responses of the current generation round. An
    // interrupted stream and an empty answer are the same failure: the provider
    // never delivered a complete structured payload. Both are transient, so the
    // round is regenerated instead of aborting the synthesis on the first
    // occurrence. The counter resets as soon as a round parses.
    let mut parse_failures = 0usize;
    // Append-only discovery feedback: each round extends the previous request
    // instead of rewriting it, so the model keeps the full history of search
    // results and documentation it already received.
    let mut feedback = String::new();
    let mut accumulated_full_text = intro_header.to_string();

    // The repository outline is deterministic for the whole loop, so it is
    // rendered once from the turn's opening context and reused by every round.
    // An outline regenerated after a discovery round added files to the context
    // would drop those nodes and rewrite the middle of the prompt, discarding
    // the prefix the provider has already cached. Exactly one outline is
    // rendered: symbol contours when the eager map is enabled, the compact
    // directory tree otherwise.
    let opening_context_paths: Vec<String> = {
        let ctx = state.context.read().await;
        ctx.get_state()
            .items
            .into_iter()
            .map(|item| item.path)
            .collect()
    };
    let outline_root = repo_root.to_path_buf();
    let (repo_map, repo_dirs) = crate::handlers::offload(move || {
        let all_files =
            tauqe_core::git::list_repository_files(Some(&outline_root)).unwrap_or_default();
        if include_repo_map {
            let map = tauqe_core::repomap::generate_repo_map(
                &outline_root,
                &all_files,
                &opening_context_paths,
                repomap_budget,
            )
            .ok()
            .filter(|map| !map.trim().is_empty());
            (map, None)
        } else {
            let tree = tauqe_core::repomap::dirs::generate_repo_dirs(&all_files);
            let dirs = if tree.trim().is_empty() {
                None
            } else {
                Some(tree)
            };
            (None, dirs)
        }
    })
    .await
    .unwrap_or((None, None));

    let output = loop {
        // Every round launches a fresh provider request, so the cancel signal is
        // re-checked here instead of only inside the streaming task.
        if *cancel_rx.borrow() {
            emit(
                &state.out,
                events::MODEL_CANCELLED,
                Some(serde_json::json!({ "operation_id": op_id })),
            );
            return Err(ArchitectDiscoveryError::Cancelled);
        }

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
        // release the guard before any disk work. Reading the files is offloaded
        // so it never stalls an async worker thread.
        let ctx_snapshot = state.context.read().await.clone();
        let context_files = crate::handlers::offload(move || ctx_snapshot.read_context_files())
            .await
            .unwrap_or_default();

        let mut user_prompt =
            build_user_prompt(&context_files, repo_map.as_deref(), repo_dirs.as_deref());

        if !feedback.is_empty() {
            user_prompt.push_str("\n\n");
            user_prompt.push_str(&feedback);
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
            session_id.clone(),
            operation,
            op_id,
            usage,
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

        let parsed = if raw_text.trim().is_empty() {
            Err("model returned an empty structured response".to_string())
        } else {
            tauqe_core::plan::prompt::parse_plan_refinement_output(&raw_text)
                .map_err(|err| err.to_string())
        };

        let output = match parsed {
            Ok(out) => {
                parse_failures = 0;
                out
            }
            Err(err) if parse_failures < max_parse_retries => {
                parse_failures += 1;
                let retry_note = format!(
                    "\n\n⚠️ Unparsable architect response ({}). Regenerating the round ({}/{}).\n\n",
                    err, parse_failures, max_parse_retries
                );
                tracing::warn!(
                    operation = operation_title,
                    round = discovery_round + 1,
                    attempt = parse_failures,
                    max_attempts = max_parse_retries,
                    error = %err,
                    "architect structured response could not be parsed; regenerating the round"
                );
                accumulated_full_text.push_str(&retry_note);
                emit(
                    &state.out,
                    events::MODEL_TEXT_DELTA,
                    serde_json::to_value(ModelDeltaEvent {
                        operation_id: op_id.to_string(),
                        delta: retry_note,
                    })
                    .ok(),
                );
                continue;
            }
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
            let repo_map_requests = output.repo_maps.clone();

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

            // Targeted contour requests run through the same Discovery handler
            // as every other map request, so the loop never duplicates the
            // rendering rules or the per-round request cap.
            let map_outcome = if repo_map_requests.is_empty() {
                tauqe_core::workflow::discovery::DiscoveryRoundOutcome::default()
            } else {
                let commands: Vec<LlmCommand> = repo_map_requests
                    .iter()
                    .map(|path| LlmCommand::RepoMap { path: path.clone() })
                    .collect();
                let m_root = repo_root.to_path_buf();
                let map_budget = repomap_budget;
                match with_context_mutation(state, move |ctx| {
                    let available_files = tauqe_core::git::list_repository_files(Some(&m_root))
                        .ok()
                        .unwrap_or_default();
                    tauqe_core::workflow::discovery::execute_discovery_commands_sync(
                        ctx,
                        &m_root,
                        &available_files,
                        &commands,
                        usize::MAX,
                        map_budget,
                        false,
                    )
                })
                .await
                {
                    Ok(outcome) => outcome,
                    Err(err) => return Err(ArchitectDiscoveryError::ModelError(err)),
                }
            };

            if !outcome.added.is_empty() {
                emit(
                    &state.out,
                    events::CONTEXT_CHANGED,
                    Some(serde_json::json!({ "state": new_ctx_state })),
                );
            }

            // Loading context files and searching the repository are the
            // longest non-streaming stretches of a round, so the cancel signal
            // is re-checked before deciding whether another provider round is
            // worth launching.
            if *cancel_rx.borrow() {
                emit(
                    &state.out,
                    events::MODEL_CANCELLED,
                    Some(serde_json::json!({ "operation_id": op_id })),
                );
                return Err(ArchitectDiscoveryError::Cancelled);
            }

            // A round that only re-requests files already present in context is
            // not a dead end: the model still needs their feedback and another
            // analysis round, so the loop terminates only when a round adds
            // nothing new, reports nothing missing, executes no searches,
            // renders no contours, and leaves no requested file already
            // satisfied.
            if outcome.added.is_empty()
                && outcome.missing.is_empty()
                && outcome.already_satisfied.is_empty()
                && search_results.is_empty()
                && map_outcome.repo_maps_loaded == 0
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
            if map_outcome.repo_maps_loaded > 0 {
                notes.push(format!(
                    "Rendered {} targeted repository map(s)",
                    map_outcome.repo_maps_loaded
                ));
            }
            if !outcome.missing.is_empty() {
                notes.push(format!(
                    "Files not found in repository: {}",
                    outcome.missing.join(", ")
                ));
            }
            if !outcome.already_satisfied.is_empty()
                && outcome.added.is_empty()
                && search_results.is_empty()
            {
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
            let map_feedback = map_outcome.combined_feedback();
            if !map_feedback.is_empty() {
                fb.push_str(&map_feedback);
                fb.push('\n');
            }
            if !outcome.missing.is_empty() {
                fb.push_str(&format!(
                    "<context_feedback>\nThe following requested files do not exist in the repository: [{}]\nPlease check <repo_map> for exact existing file paths.\n</context_feedback>\n",
                    outcome.missing.join(", ")
                ));
            }
            if !outcome.already_satisfied.is_empty()
                && outcome.added.is_empty()
                && search_results.is_empty()
            {
                fb.push_str(&format!(
                    "<context_feedback>\nThe following files are already loaded in context: [{}]\nYou may proceed with inspecting them and refining the plan.\n</context_feedback>\n",
                    outcome.already_satisfied.join(", ")
                ));
            }
            if !fb.is_empty() {
                if !feedback.is_empty() {
                    feedback.push('\n');
                }
                feedback.push_str(&fb);
            }
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

/// Records an Architect synthesis interaction atomically once it reaches a
/// terminal outcome.
///
/// The user turn and its response are written together under a single history
/// mutation, only after the synthesis has produced a durable result. Recording
/// the turn before the discovery loop would leave an orphaned user entry in the
/// transcript whenever the round is cancelled, fails, or is rejected before a
/// response exists. Persistence is `fsync`-bounded, so it runs on the blocking
/// pool through `crate::handlers::with_history_mutation` instead of stalling an
/// async worker thread.
async fn record_synthesis_history(
    state: &Arc<AppState>,
    turn_prompt: String,
    context_paths: Vec<String>,
    response_message: String,
    response_summary: Option<String>,
) {
    let _ = crate::handlers::with_history_mutation(state, move |history| {
        let _ = history.record_turn(turn_prompt, context_paths);
        let _ = history.record_response(response_message, None, response_summary, Vec::new());
    })
    .await;
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
    is_batch: bool,
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
        architect_tier,
        alias,
        level_up,
        repomap_budget,
        max_discovery_rounds,
        files_token_budget,
        max_parse_retries,
        include_repo_map,
    ) = {
        let cfg = state.config.lock().await.config.clone();
        let (architect_tier, alias, level_up) =
            resolve_architect_tier(&cfg, &plan, custom_model.as_ref(), diagnostics.as_ref());
        let provider =
            match resolve_architect_provider(state, &cfg, architect_tier.model.provider).await {
                Ok(p) => p,
                Err(err) => return Response::err(req_id, errors::NO_API_KEY, err),
            };
        (
            provider,
            architect_tier,
            alias,
            level_up,
            cfg.context.repomap_token_budget,
            cfg.context.max_discovery_rounds,
            cfg.context.files_token_budget,
            cfg.develop.max_retries,
            cfg.context.include_repo_map,
        )
    };
    // The resolved rung is recorded with the synthesized horizon so a later
    // wave can reproduce or escalate it instead of re-deriving one. A replan
    // wave escalates that recorded configuration rather than retrying the one
    // that already failed, so the transition is journalled once here.
    if let Some(level) = &level_up {
        tracing::warn!(
            plan_id = %plan.id,
            operation = operation_title,
            blocked_step_id = diagnostics
                .as_ref()
                .map(|d| d.blocked_step_id.as_str())
                .unwrap_or_default(),
            from_tier = ?level.from,
            to_tier = ?level.to,
            "architect level up"
        );
    }

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
            model: architect_tier.model.clone(),
            role: Some(ModelRole::Architect),
            alias,
            effort: architect_tier.effort,
            plan_context: None,
            is_batch,
        })
        .ok(),
    });

    let level_up_note = level_up
        .as_ref()
        .map(|level| {
            let from = level
                .from
                .as_ref()
                .map(format_architect_tier)
                .unwrap_or_else(|| "not recorded".to_string());
            format!(
                "\n> Architect level up: {} → {}",
                from,
                format_architect_tier(&level.to)
            )
        })
        .unwrap_or_default();

    let intro_header = match &diagnostics {
        Some(diag) => format!(
            "# {}: [{}] {}\n*Replanning uncompleted horizon around blocked step `{}`...*{}\n\n---\n\n",
            operation_title, plan.id, plan.title, diag.blocked_step_id, level_up_note
        ),
        None => format!(
            "# {}: [{}] {}\n*Analyzing codebase contours and decomposing architectural plan...*{}\n\n---\n\n",
            operation_title, plan.id, plan.title, level_up_note
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

    // The user turn and its response are recorded together once the synthesis
    // reaches a terminal outcome, so a cancelled, failed, or rejected round
    // leaves no orphaned user turn in the transcript. The prompt and context
    // paths are captured here because they describe the state the Architect
    // actually saw.
    let (history_turn_prompt, history_context_paths) = {
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
        (history_turn_prompt, context_paths)
    };

    let response_format = {
        let schema_val = tauqe_core::plan::prompt::plan_refine_response_schema_definition();
        match serde_json::from_value::<JsonSchemaDefinition>(schema_val) {
            Ok(schema) => ResponseFormat::JsonSchema {
                json_schema: schema,
            },
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
        include_repo_map,
        max_discovery_rounds,
        files_token_budget,
        max_parse_retries,
        // Horizon synthesis is an isolated design dialogue: its cache is keyed
        // by the plan, never by the interactive REPL session.
        session_id: Some(format_session_id(SESSION_PREFIX_ARCHITECT, &plan.id)),
    };

    let build_user_prompt = |context_files: &[ContextFileContent],
                             repo_map: Option<&str>,
                             repo_dirs: Option<&str>| {
        let diag_ref =
            diagnostics
                .as_ref()
                .map(|d| tauqe_core::plan::prompt::PlanHorizonDiagnostics {
                    blocked_step_id: &d.blocked_step_id,
                    blocked_reason: d.blocked_reason.as_deref(),
                    toolchain_status: d.toolchain_status.as_deref(),
                });
        tauqe_core::plan::prompt::build_plan_horizon_user_prompt(
            &plan,
            context_files,
            repo_map,
            repo_dirs,
            diag_ref,
            instructions.as_deref(),
        )
    };

    let output = match run_architect_discovery_loop(
        state,
        Arc::clone(&provider),
        &architect_tier.model.name,
        discovery,
        build_user_prompt,
        architect_tier.effort,
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
            record_synthesis_history(
                state,
                history_turn_prompt,
                history_context_paths,
                warn_msg.clone(),
                Some(format!(
                    "{}: Token Budget Exceeded ({})",
                    operation_title, plan.title
                )),
            )
            .await;
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
        let blocked_msg = format!(
            "Architect reported blockers for plan [{}]:\n\n{}",
            plan.id, reason
        );
        record_synthesis_history(
            state,
            history_turn_prompt,
            history_context_paths,
            blocked_msg,
            Some(format!("{}: Blocked ({})", operation_title, plan.title)),
        )
        .await;
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
    let horizon_architect = architect_tier;
    let updated_plan = match offload_plan(move || {
        PlanStorage::replace_uncompleted_horizon(
            &horizon_root,
            &horizon_plan_id,
            horizon_items,
            horizon_architect,
        )
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
    record_synthesis_history(
        state,
        history_turn_prompt,
        history_context_paths,
        history_response,
        Some(format!("{}: {}", operation_title, updated_plan.title)),
    )
    .await;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ModelEntry, ModelRef, ReasoningEffort};

    #[test]
    fn test_resolve_architect_tier_custom_model_uses_its_own_effort() {
        let cfg = tauqe_core::config::AppConfig {
            models: tauqe_core::config::ModelsConfig {
                primary: vec![ModelRef::openrouter("base/primary").into()],
                architect: Some(vec![
                    ModelEntry::new(
                        ModelRef::openrouter("deepseek/deepseek-flash"),
                        Some(ReasoningEffort::Max),
                        None,
                    ),
                    ModelEntry::new(
                        ModelRef::openrouter("google/gemini-flash"),
                        Some(ReasoningEffort::High),
                        None,
                    ),
                    ModelEntry::from(ModelRef::openrouter("google/gemini-argon")),
                ]),
                ..Default::default()
            },
            ..Default::default()
        };

        let plan = Plan {
            id: "test-plan".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        };

        // 1. None selects tier 0 (deepseek with max)
        let (tier0, _, _) = resolve_architect_tier(&cfg, &plan, None, None);
        assert_eq!(
            tier0.model,
            tauqe_protocol::ModelRef::openrouter("deepseek/deepseek-flash")
        );
        assert_eq!(tier0.effort, Some(ReasoningEffort::Max));

        // 2. Custom selection of gemini-flash must use its OWN effort (high), NOT max from deepseek!
        let selection = ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter(
            "google/gemini-flash",
        ));
        let (tier_custom, _, _) = resolve_architect_tier(&cfg, &plan, Some(&selection), None);
        assert_eq!(
            tier_custom.model,
            tauqe_protocol::ModelRef::openrouter("google/gemini-flash")
        );
        assert_eq!(tier_custom.effort, Some(ReasoningEffort::High));

        // 3. Custom selection of gemini-argon without effort returns None
        let selection_argon = ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter(
            "google/gemini-argon",
        ));
        let (tier_argon, _, _) =
            resolve_architect_tier(&cfg, &plan, Some(&selection_argon), None);
        assert_eq!(
            tier_argon.model,
            tauqe_protocol::ModelRef::openrouter("google/gemini-argon")
        );
        assert_eq!(tier_argon.effort, None);
    }
}
