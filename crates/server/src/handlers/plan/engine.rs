//! Architect plan horizon-synthesis engine.

use std::sync::Arc;

use tauqe_core::context::ContextFileContent;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::{
    format_session_id, JsonSchemaDefinition, ResponseFormat, SESSION_PREFIX_ARCHITECT,
};
use tauqe_protocol::{
    errors, events, Event, ModelDeltaEvent, ModelRole, ModelSelection, ModelStartedEvent, Plan,
    PlanListChangedEvent, PlanRefineResult, PlanUpdatedEvent, Response,
};

use crate::handlers::architect::{
    format_architect_tier, resolve_architect_provider, run_architect_discovery_loop,
    ArchitectDiscoveryError, ArchitectDiscoveryOutcome, ArchitectDiscoveryRequest,
};
use crate::handlers::common::{
    clear_auto_context, emit, preload_readonly_files, record_synthesis_history,
};
use crate::state::{AppState, OperationGuard};

use super::common::{find_duplicate_item_ids, offload_plan};
pub(super) use super::horizon::{collect_plan_context_files, PlanHorizonDiagnosticsOwned};
use super::horizon::resolve_architect_tier;

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
        serde_json::to_value(PlanUpdatedEvent {
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
            serde_json::to_value(PlanListChangedEvent { plans, active_id }).ok(),
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


