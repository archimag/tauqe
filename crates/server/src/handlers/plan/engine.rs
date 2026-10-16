//! Architect streaming and plan horizon-synthesis engine.

use std::sync::Arc;

use tauqe_core::edits::protocol::ContextRequest;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::{
    create_provider, ChatMessage, JsonSchemaDefinition, LlmProvider, ResponseFormat, StreamEvent,
};
use tauqe_core::workflow::common::apply_context_requests_detailed;
use tauqe_protocol::{
    errors, events, ContextAccess, Event, ModelDeltaEvent, ModelErrorEvent, ModelFinishedEvent,
    ModelRole, ModelSelection, ModelStartedEvent, Plan, PlanRefineResult, Response, TurnPhase,
    TurnPhaseEvent,
};
use tokio::sync::mpsc;

use crate::state::{AppState, OperationGuard};

use super::common::{clear_auto_context, emit, find_duplicate_item_ids};

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

/// Diagnostics describing a failed or blocked step for horizon synthesis.
pub(super) struct PlanHorizonDiagnosticsOwned {
    pub blocked_step_id: String,
    pub blocked_reason: Option<String>,
    pub toolchain_status: Option<String>,
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

    let mut discovery_round = 0usize;
    let mut feedback: Option<String> = None;
    let mut accumulated_full_text = intro_header.clone();

    let output = loop {
        state.out.send_event(&Event {
            method: events::TURN_PHASE.to_string(),
            params: serde_json::to_value(TurnPhaseEvent {
                operation_id: op_id.clone(),
                phase: TurnPhase::Proposal,
                round: Some(discovery_round + 1),
                max_rounds: None,
                detail: Some(operation_title.to_string()),
            })
            .ok(),
        });

        // Read the context file contents under a short-lived read guard, then
        // release the guard before the blocking git and Tree-sitter work. That
        // work is offloaded so it never stalls an async worker thread.
        let context_files = {
            let ctx = state.context.read().await;
            ctx.read_context_files()
        };
        let context_paths: Vec<String> =
            context_files.iter().map(|f| f.path.clone()).collect();
        let repo_map = {
            let repo_root = repo_root.clone();
            crate::handlers::offload(move || {
                let all_files =
                    tauqe_core::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
                tauqe_core::repomap::generate_repo_map(
                    &repo_root,
                    &all_files,
                    &context_paths,
                    repomap_budget,
                )
                .ok()
            })
            .await
            .unwrap_or(None)
        };

        let sys_prompt = tauqe_core::plan::prompt::build_plan_horizon_system_prompt();
        let diag_ref = diagnostics
            .as_ref()
            .map(|d| tauqe_core::plan::prompt::PlanHorizonDiagnostics {
                blocked_step_id: &d.blocked_step_id,
                blocked_reason: d.blocked_reason.as_deref(),
                toolchain_status: d.toolchain_status.as_deref(),
            });

        let mut user_prompt = tauqe_core::plan::prompt::build_plan_horizon_user_prompt(
            &plan,
            &context_files,
            repo_map.as_deref(),
            diag_ref,
            instructions.as_deref(),
        );

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
                    format!("{} was cancelled", operation_title),
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
                state.out.send_event(&Event {
                    method: events::MODEL_ERROR.to_string(),
                    params: serde_json::to_value(ModelErrorEvent {
                        operation_id: op_id.clone(),
                        message: err_msg.clone(),
                    })
                    .ok(),
                });
                return Response::err(req_id, errors::PARSE_ERROR, err_msg);
            }
        };

        if streamed_explanation_bytes == 0 && !output.explanation.trim().is_empty() {
            state.out.send_event(&Event {
                method: events::MODEL_TEXT_DELTA.to_string(),
                params: serde_json::to_value(ModelDeltaEvent {
                    operation_id: op_id.clone(),
                    delta: output.explanation.clone(),
                })
                .ok(),
            });
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
                    tauqe_core::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
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
                    "Context files token budget exceeded (files_token_budget = {}). Architect requested {} new token(s) in file(s) [{}], but context already has {} tokens. The plan is too broad and should be manually decomposed or scoped down.",
                    limit.token_budget,
                    limit.requested_tokens,
                    limit.requested_files.join(", "),
                    limit.current_tokens
                );
                tracing::warn!("{}", warn_msg);
                state.out.send_event(&Event {
                    method: events::MODEL_TEXT_DELTA.to_string(),
                    params: serde_json::to_value(ModelDeltaEvent {
                        operation_id: op_id.clone(),
                        delta: format!("\n\n⚠️ {}\n\n", warn_msg),
                    })
                    .ok(),
                });
                state.out.send_event(&Event {
                    method: events::MODEL_FINISHED.to_string(),
                    params: serde_json::to_value(ModelFinishedEvent {
                        operation_id: op_id.clone(),
                        full_text: format!("{}\n\n⚠️ {}", accumulated_full_text, warn_msg),
                    })
                    .ok(),
                });
                clear_auto_context(state).await;
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

            if !outcome.added.is_empty() {
                state.out.send_event(&Event {
                    method: events::CONTEXT_CHANGED.to_string(),
                    params: Some(serde_json::json!({ "state": new_ctx_state })),
                });
            }

            if outcome.added.is_empty() && outcome.missing.is_empty() {
                break output;
            }

            discovery_round += 1;
            tracing::info!(
                plan_id = %plan.id,
                round = discovery_round,
                added = outcome.added.len(),
                missing = outcome.missing.len(),
                "architect discovery round"
            );
            state.out.send_event(&Event {
                method: events::TURN_PHASE.to_string(),
                params: serde_json::to_value(TurnPhaseEvent {
                    operation_id: op_id.clone(),
                    phase: TurnPhase::Discovery,
                    round: Some(discovery_round),
                    max_rounds: Some(max_discovery_rounds),
                    detail: Some(format!("Discovery round {}", discovery_round)),
                })
                .ok(),
            });
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
            if !outcome.already_satisfied.is_empty() && outcome.added.is_empty() {
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
            state.out.send_event(&Event {
                method: events::MODEL_TEXT_DELTA.to_string(),
                params: serde_json::to_value(ModelDeltaEvent {
                    operation_id: op_id.clone(),
                    delta: round_banner,
                })
                .ok(),
            });

            let mut fb = String::new();
            if !outcome.missing.is_empty() {
                fb.push_str(&format!(
                    "<context_feedback>\nThe following requested files do not exist in the repository: [{}]\nPlease check <repo_map> for exact existing file paths.\n</context_feedback>\n",
                    outcome.missing.join(", ")
                ));
            }
            if !outcome.already_satisfied.is_empty() && outcome.added.is_empty() {
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

    state.out.send_event(&Event {
        method: events::MODEL_FINISHED.to_string(),
        params: serde_json::to_value(ModelFinishedEvent {
            operation_id: op_id.clone(),
            full_text: accumulated_full_text,
        })
        .ok(),
    });

    clear_auto_context(state).await;

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
    let updated_plan =
        match PlanStorage::replace_uncompleted_horizon(&repo_root, &plan.id, output.items) {
            Ok(p) => p,
            Err(err) => {
                return Response::err(req_id, errors::PLAN_SAVE_FAILED, format!("{:#}", err))
            }
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
        })
        .ok(),
    );

    if let Ok(plans) = PlanStorage::load_all(&repo_root) {
        let active_id = PlanStorage::load_active_id(&repo_root).ok().flatten();
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
