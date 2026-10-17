//! Architect-driven decomposition of a single review finding into an embedded
//! subplan.
//!
//! Finding decomposition and plan horizon synthesis are the same Architect
//! operation, so both share the multi-round discovery engine in `plan::engine`
//! (`run_architect_discovery_loop`). The merged subplan is persisted through the
//! core domain primitive `ReviewStorage::replace_uncompleted_subplan`, so no
//! plan-tree manipulation happens in the server crate.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::context::ContextFileContent;
use tauqe_core::providers::{JsonSchemaDefinition, ResponseFormat};
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    errors, events, ModelDeltaEvent, ModelRole, ModelSelection, ModelStartedEvent, Request,
    Response, ReviewItem, ReviewRefineItemParams, ReviewRefineItemResult, ReviewSession,
};

use crate::handlers::plan::{
    find_duplicate_item_ids, resolve_architect_provider, run_architect_discovery_loop,
    ArchitectDiscoveryError, ArchitectDiscoveryOutcome, ArchitectDiscoveryRequest,
};
use crate::state::{next_operation_id, AppState, OperationGuard};

use super::common::{
    clear_auto_context, emit, offload_review, preload_refine_item_files, publish_review,
};

/// Operation title used for finding decomposition telemetry and history.
const OPERATION_TITLE: &str = "Review Finding Decomposition";

/// Handles an Architect-driven decomposition of a single review finding into an
/// embedded subplan carried by `ReviewItem::subplan`.
///
/// Plan refinement and finding decomposition are conceptually the same operation:
/// both dispatch to the Architect role, so the handler mirrors the plan horizon
/// synthesis flow and reuses its streaming infrastructure.
pub async fn handle_review_refine_item(req: Request, state: &Arc<AppState>) -> Response {
    let params: ReviewRefineItemParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Missing or invalid review refine item params",
                );
            }
        };

    // Strict Review ID isolation: decomposition is scoped to the session the
    // caller names, never to the process-global active marker.
    let review_id = match params
        .review_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(id) => id.to_string(),
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "review_id is required: finding decomposition is scoped to an explicit session",
            );
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let load_root = repo_root.clone();
    let load_id = review_id.clone();

    let session =
        match offload_review(move || ReviewStorage::load_review(&load_root, &load_id)).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Response::err(
                    req.id,
                    errors::NOT_FOUND,
                    format!("Review session '{}' not found", review_id),
                );
            }
            Err(err) => return Response::err(req.id, errors::REVIEW_LOAD_FAILED, err),
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
/// Drives streaming structured communication with the architect model through the
/// shared [`run_architect_discovery_loop`], runs multi-round context discovery on
/// `needs_context`, validates the synthesized subplan, and persists it while
/// preserving the immutable `Done` foundation of any previously completed
/// sub-steps. Session history is recorded strictly after the subplan is persisted,
/// so a failed synthesis never poisons the audit trail.
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

    // Guarantee baseline visibility: load the finding's target files into the
    // read-only auto layer before the first generation round so the architect
    // inspects real code instead of inferring it from the repo map alone.
    preload_refine_item_files(state, repo_root, &item.files).await;

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
        "# {}: [{}#{}] {}\n*Analyzing codebase contours and decomposing the finding into executable sub-steps...*\n\n---\n\n",
        OPERATION_TITLE, session.id, item.id, item.title
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
        let context_paths: Vec<String> =
            ctx.get_state().items.into_iter().map(|it| it.path).collect();
        let mut turn_prompt = format!(
            "# {}: [{}#{}] {}\n",
            OPERATION_TITLE, session.id, item.id, item.title
        );
        if let Some(ins) = &instructions {
            let trimmed = ins.trim();
            if !trimmed.is_empty() {
                turn_prompt.push_str(&format!("\nDirectives: {}\n", trimmed));
            }
        }
        let _ = state.history.lock().await.record_turn(turn_prompt, context_paths);
    }

    let response_format = {
        let schema_val = tauqe_core::plan::prompt::plan_refine_response_schema_definition();
        match serde_json::from_value::<JsonSchemaDefinition>(schema_val) {
            Ok(schema) => ResponseFormat::JsonSchema { json_schema: schema },
            Err(err) => return Response::err(req_id, errors::SCHEMA_ERROR, err.to_string()),
        }
    };
    let system_prompt = tauqe_core::review::prompt::build_review_finding_refine_system_prompt();
    let discovery = ArchitectDiscoveryRequest {
        repo_root,
        op_id: &op_id,
        operation_title: OPERATION_TITLE,
        system_prompt: &system_prompt,
        intro_header: &intro_header,
        budget_hint: "The finding decomposition is too broad and should be scoped down.",
        response_format,
        repomap_budget,
        max_discovery_rounds,
        files_token_budget,
    };

    let build_user_prompt = |context_files: &[ContextFileContent], repo_map: Option<&str>| {
        tauqe_core::review::prompt::build_review_finding_refine_user_prompt(
            &session,
            &item,
            context_files,
            repo_map,
            instructions.as_deref(),
        )
    };

    let output = match run_architect_discovery_loop(
        state,
        provider,
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
            // Close the turn in session history: the user prompt was recorded on
            // entry, so the blocking outcome must be recorded here too or the
            // transcript keeps an unmatched user turn.
            let _ = state.history.lock().await.record_response(
                warn_msg.clone(),
                None,
                Some(format!(
                    "{}: Token Budget Exceeded ({})",
                    OPERATION_TITLE, item.title
                )),
                Vec::new(),
            );
            return Response::ok_typed(
                req_id,
                &ReviewRefineItemResult::Blocked { reason: warn_msg },
            );
        }
        Err(ArchitectDiscoveryError::Cancelled) => {
            clear_auto_context(state).await;
            return Response::err(
                req_id,
                errors::CANCELLED,
                format!("{} was cancelled", OPERATION_TITLE),
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
        let blocked_msg = format!(
            "Architect reported blockers for review finding [{}#{}]:\n\n{}",
            session.id, item.id, reason
        );
        let _ = state.history.lock().await.record_response(
            blocked_msg,
            None,
            Some(format!("{}: Blocked ({})", OPERATION_TITLE, item.title)),
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

    // Persist the merged subplan through the core domain primitive, which
    // preserves the immutable `Done` foundation and rejects dependency cycles
    // before anything reaches disk.
    let review_id = session.id.clone();
    let item_id = item.id;
    let explanation = output.explanation;
    let new_subplan = output.items;
    let save_root = repo_root.to_path_buf();
    let save_review_id = review_id.clone();
    let updated = match offload_review(move || {
        ReviewStorage::replace_uncompleted_subplan(
            &save_root,
            &save_review_id,
            item_id,
            new_subplan,
        )
    })
    .await
    {
        Ok(Some(issue)) => issue,
        Ok(None) => {
            return Response::err(
                req_id,
                errors::NOT_FOUND,
                format!(
                    "Review item #{} not found in session '{}'",
                    item_id, review_id
                ),
            );
        }
        Err(err) => return Response::err(req_id, errors::PLAN_SAVE_FAILED, err),
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
        review_id, item_id, subplan_total, explanation
    );
    let _ = state.history.lock().await.record_response(
        history_response,
        None,
        Some(format!("{}: {}", OPERATION_TITLE, updated.title)),
        Vec::new(),
    );

    publish_review(state, repo_root, &review_id).await;

    Response::ok_typed(
        req_id,
        &ReviewRefineItemResult::Success {
            item: updated,
            explanation: Some(explanation),
        },
    )
}
