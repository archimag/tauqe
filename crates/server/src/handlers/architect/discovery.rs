//! Multi-round Architect discovery loop.
//!
//! Plan horizon synthesis and review finding decomposition are the same
//! Architect operation, so both drive one implementation of the streaming
//! proposal, `needs_context` discovery, token-budget enforcement, and parse-retry
//! protocol.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::context::ContextFileContent;
use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::providers::{ChatMessage, LlmProvider, ResponseFormat};
use tauqe_core::refs::materialize::materialize_requests;
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    events, ContextAccess, LlmCommand, ModelDeltaEvent, ModelErrorEvent, ModelFinishedEvent,
    TurnPhase, TurnPhaseEvent,
};

use crate::handlers::common::{clear_auto_context, emit};
use crate::handlers::with_context_mutation;
use crate::state::{AppState, OperationGuard};
use crate::usage::UsageGuard;

use super::stream::{run_architect_stream, ArchitectStreamError, ArchitectStreamOutcome};

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
    /// Operational caps applied to the repository searches, directory listings,
    /// and targeted contours of one discovery round.
    pub(crate) discovery_limits: tauqe_core::workflow::discovery::DiscoveryLimits,
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
        discovery_limits,
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

            // A declared reference has no file on disk until the harness
            // materializes it, so the requested names are resolved into their
            // isolated snapshots before the context mutation runs. The pre-step
            // races this synthesis's cancel signal, so an abandoned run stops
            // waiting for a slow reference host instead of outliving the turn.
            let (requests, refs_feedback) =
                materialize_requests(repo_root, &requests, Some(&cancel_rx)).await;

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
                    discovery_limits.refs_token_budget,
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
                        discovery_limits,
                    )
                })
                .await
                .unwrap_or_default()
            } else {
                Vec::new()
            };

            // Targeted contour requests run through the same Discovery handler
            // as every other map request, so the loop never duplicates the
            // rendering rules or the per-round request cap. The configured file
            // budget travels with the call for the same reason: the dispatcher
            // sees the budgets the turn itself enforces instead of a bypass
            // literal that would exempt this branch from them.
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
                        files_token_budget,
                        map_budget,
                        discovery_limits,
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
            // A reference the harness could not materialize leads the feedback:
            // the model learns why the declared document is unavailable before
            // it reads the rest of the round's findings.
            for diagnostic in &refs_feedback {
                fb.push_str(diagnostic);
                fb.push('\n');
            }
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
