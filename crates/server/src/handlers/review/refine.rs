//! Architect-driven decomposition of a single review finding into an embedded
//! subplan.
//!
//! Finding decomposition and plan horizon synthesis are the same Architect
//! operation, so both share the multi-round discovery engine in
//! `handlers::architect` (`run_architect_discovery_loop`). The merged subplan is
//! persisted through the core domain primitive
//! `ReviewStorage::replace_uncompleted_subplan`, so no plan-tree manipulation
//! happens in the server crate.
//!
//! The synthesis itself lives in [`refine_finding_with_architect`], a typed
//! helper shared by the `review/refineItem` RPC handler, the autonomous
//! decomposition that runs before coding an `Architecture`-class or `Refine`
//! finding, and the replanning cycle that reprojects the remaining horizon of a
//! finding whose sub-step failed (see `step.rs`).

use std::path::Path;
use std::sync::Arc;

use tauqe_core::config::AppConfig;
use tauqe_core::context::ContextFileContent;
use tauqe_core::providers::{
    format_session_id, JsonSchemaDefinition, ResponseFormat, SESSION_PREFIX_REVIEW,
};
use tauqe_core::review::prompt::refine::ReviewRefineDiagnostics;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    errors, events, ArchitectTier, IssueStatus, ModelDeltaEvent, ModelRole, ModelSelection,
    ModelStartedEvent, Request, Response, ResponseError, ReviewIssue, ReviewItem,
    ReviewRefineItemParams, ReviewRefineItemResult, ReviewSession,
};

use crate::handlers::architect::{
    architect_alias, format_architect_tier, resolve_architect_provider,
    run_architect_discovery_loop, ArchitectDiscoveryError, ArchitectDiscoveryOutcome,
    ArchitectDiscoveryRequest,
};
use crate::handlers::common::record_synthesis_history;
use crate::handlers::plan::find_duplicate_item_ids;
use crate::state::{next_operation_id, AppState, OperationGuard};

use super::common::{
    clear_auto_context, emit, offload_review, preload_refine_item_files, publish_review,
    set_item_status,
};

/// Operation title used for finding decomposition telemetry and history.
const OPERATION_TITLE: &str = "Review Finding Decomposition";

/// Handles an Architect-driven decomposition of a single review finding into an
/// embedded subplan carried by `ReviewItem::subplan`.
///
/// Plan refinement and finding decomposition are conceptually the same operation:
/// both dispatch to the Architect role, so the handler mirrors the plan horizon
/// synthesis flow and reuses its streaming infrastructure through
/// [`refine_finding_with_architect`].
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

    match refine_finding_with_architect(
        state,
        &repo_root,
        &session,
        &item,
        params.model,
        params.instructions,
        None,
        &operation,
    )
    .await
    {
        Ok(RefineOutcome::Success { item, explanation }) => Response::ok_typed(
            req.id,
            &ReviewRefineItemResult::Success {
                item,
                explanation: Some(explanation),
            },
        ),
        Ok(RefineOutcome::Blocked { reason }) => {
            Response::ok_typed(req.id, &ReviewRefineItemResult::Blocked { reason })
        }
        Err(err) => Response {
            id: req.id,
            result: None,
            error: Some(err),
        },
    }
}

/// Typed outcome of an Architect-driven finding decomposition.
pub(super) enum RefineOutcome {
    /// The finding was decomposed and persisted with an embedded subplan.
    Success {
        item: ReviewIssue,
        explanation: String,
    },
    /// The Architect reported that the finding cannot be decomposed as stated.
    Blocked { reason: String },
}

/// Runs an Architect-driven decomposition of a single review finding.
///
/// Shared by the `review/refineItem` RPC handler and the autonomous
/// decomposition that runs before coding a `Warning`-severity or `Refine`
/// finding. The function drives streaming structured communication with the
/// architect model through the shared [`run_architect_discovery_loop`], runs
/// multi-round context discovery on `needs_context`, validates the synthesized
/// subplan, and persists it through the core domain primitive, which preserves
/// the immutable `Done` foundation and promotes the finding from `Refine` to
/// `Todo`. Session history is recorded strictly after the subplan is persisted,
/// so a failed synthesis never poisons the audit trail.
///
/// `diagnostics` carries the failure of an already executed sub-step. Supplying
/// it turns the same synthesis into horizon replanning: the Architect reprojects
/// the remaining work around the failed approach, while the immutable `Done`
/// foundation is preserved by the storage primitive exactly as it is during
/// initial decomposition. A replanning round also escalates the Architect
/// configuration recorded on the finding, so the reprojection runs on a stronger
/// rung than the one whose horizon already failed.
///
/// Planning is a lifecycle state of the finding, not an invisible operation: it
/// is published as `InProgress` for the whole synthesis, and a round that yields
/// no horizon returns the finding to the status it held before planning started,
/// so a failed or blocked round never leaves a stuck `InProgress`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn refine_finding_with_architect(
    state: &Arc<AppState>,
    repo_root: &Path,
    session: &ReviewSession,
    item: &ReviewItem,
    custom_model: Option<ModelSelection>,
    instructions: Option<String>,
    diagnostics: Option<ReviewRefineDiagnostics<'_>>,
    operation: &OperationGuard,
) -> Result<RefineOutcome, ResponseError> {
    // Planning is visible in the finding lifecycle. Publishing `InProgress`
    // before the first generation round lets the interface show planning
    // instead of an idle finding, and a successful synthesis settles it on
    // `Todo` inside `replace_uncompleted_subplan`.
    let previous_status = item.status;
    set_item_status(
        state,
        repo_root,
        &session.id,
        item.id,
        IssueStatus::InProgress,
    )
    .await;

    let outcome = refine_finding_with_architect_inner(
        state,
        repo_root,
        session,
        item,
        custom_model,
        instructions,
        diagnostics,
        operation,
    )
    .await;

    // A round that produced no horizon must not leave the finding stuck in
    // `InProgress`, so it returns to the status it held before planning began.
    if !matches!(&outcome, Ok(RefineOutcome::Success { .. })) {
        let restored = status_after_failed_planning(previous_status);
        tracing::info!(
            review_id = %session.id,
            item_id = item.id,
            previous_status = %previous_status,
            restored_status = %restored,
            "review finding planning produced no horizon; status restored"
        );
        set_item_status(state, repo_root, &session.id, item.id, restored).await;
    }

    outcome
}

/// Status a finding returns to when a planning round yields no horizon.
///
/// The planning status must never outlive the planning round, so the finding
/// returns to the status it held before planning started. A finding that was
/// itself mid-planning is demoted to `Todo`, which keeps it executable and
/// distinguishable from finished work.
fn status_after_failed_planning(previous: IssueStatus) -> IssueStatus {
    match previous {
        IssueStatus::InProgress => IssueStatus::Todo,
        other => other,
    }
}

/// Resolves the Architect configuration for one finding decomposition round.
///
/// An explicit developer selection overrides the ladder. Otherwise a replanning
/// round reads the configuration recorded on the finding and asks the ladder for
/// the next rung, so a reprojected horizon is synthesized by a stronger Architect
/// than the one that already failed; the initial decomposition of a finding
/// starts from the base rung. The returned former configuration is `Some` only
/// for a replanning round, and is `None` when no configuration was recorded.
fn resolve_finding_architect_tier(
    cfg: &AppConfig,
    recorded: Option<&ArchitectTier>,
    custom_model: Option<&ModelSelection>,
    replanning: bool,
) -> (ArchitectTier, Option<ArchitectTier>) {
    match custom_model {
        Some(selection) => {
            let model = cfg.resolve_model(selection);
            let effort = cfg
                .models
                .resolve_selection_effort(ModelRole::Architect, selection);
            (ArchitectTier::new(model, effort), None)
        }
        None if replanning => (cfg.models.next_architect_tier(recorded), recorded.cloned()),
        None => (cfg.models.architect_tier(0), None),
    }
}

/// Runs the Architect synthesis of a finding horizon.
///
/// See [`refine_finding_with_architect`] for the lifecycle contract. This
/// function performs the synthesis itself and never mutates the finding status.
#[allow(clippy::too_many_arguments)]
async fn refine_finding_with_architect_inner(
    state: &Arc<AppState>,
    repo_root: &Path,
    session: &ReviewSession,
    item: &ReviewItem,
    custom_model: Option<ModelSelection>,
    instructions: Option<String>,
    diagnostics: Option<ReviewRefineDiagnostics<'_>>,
    operation: &OperationGuard,
) -> Result<RefineOutcome, ResponseError> {
    let (
        provider,
        architect_tier,
        alias,
        escalated_from,
        repomap_budget,
        max_discovery_rounds,
        files_token_budget,
        max_parse_retries,
        include_repo_map,
    ) = {
        let cfg = state.config.lock().await.config.clone();
        let (architect_tier, escalated_from) = resolve_finding_architect_tier(
            &cfg,
            item.subplan_architect.as_ref(),
            custom_model.as_ref(),
            diagnostics.is_some(),
        );
        let alias = architect_alias(&cfg, &architect_tier);
        let provider =
            match resolve_architect_provider(state, &cfg, architect_tier.model.provider).await {
                Ok(p) => p,
                Err(err) => return Err(refine_error(errors::NO_API_KEY, err)),
            };
        (
            provider,
            architect_tier,
            alias,
            escalated_from,
            cfg.context.repomap_token_budget,
            cfg.context.max_discovery_rounds,
            cfg.context.files_token_budget,
            cfg.develop.max_retries,
            cfg.context.include_repo_map,
        )
    };
    let model = architect_tier.model.clone();
    let reasoning_effort = architect_tier.effort;

    // A replanning round runs on the escalated rung and records it with the
    // reprojected horizon, so the next wave continues up the ladder instead of
    // restarting from the configuration that already failed.
    if diagnostics.is_some() {
        tracing::warn!(
            review_id = %session.id,
            item_id = item.id,
            from_tier = ?escalated_from,
            to_tier = ?architect_tier,
            "review finding architect level up"
        );
    }

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

    let level_up_note = if diagnostics.is_some() {
        let from = escalated_from
            .as_ref()
            .map(format_architect_tier)
            .unwrap_or_else(|| "not recorded".to_string());
        format!(
            "\n> Architect level up: {} → {}",
            from,
            format_architect_tier(&architect_tier)
        )
    } else {
        String::new()
    };
    let intro_header = match &diagnostics {
        Some(diag) => format!(
            "# {}: [{}#{}] {}\n*Replanning the finding horizon around failed sub-step `{}`...*{}\n\n---\n\n",
            OPERATION_TITLE, session.id, item.id, item.title, diag.failed_step_id, level_up_note
        ),
        None => format!(
            "# {}: [{}#{}] {}\n*Analyzing codebase contours and decomposing the finding into executable sub-steps...*{}\n\n---\n\n",
            OPERATION_TITLE, session.id, item.id, item.title, level_up_note
        ),
    };
    emit(
        &state.out,
        events::MODEL_TEXT_DELTA,
        serde_json::to_value(ModelDeltaEvent {
            operation_id: op_id.clone(),
            delta: intro_header.clone(),
        })
        .ok(),
    );

    // The user turn and its response are recorded together once the decomposition
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
        (turn_prompt, context_paths)
    };

    let response_format = {
        let schema_val = tauqe_core::plan::prompt::plan_refine_response_schema_definition();
        match serde_json::from_value::<JsonSchemaDefinition>(schema_val) {
            Ok(schema) => ResponseFormat::JsonSchema {
                json_schema: schema,
            },
            Err(err) => return Err(refine_error(errors::SCHEMA_ERROR, err.to_string())),
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
        include_repo_map,
        max_discovery_rounds,
        files_token_budget,
        max_parse_retries,
        // Decomposition is an isolated design dialogue of one review session:
        // its cache is keyed by the audit, never by the interactive REPL.
        session_id: Some(format_session_id(SESSION_PREFIX_REVIEW, &session.id)),
    };

    let build_user_prompt = |context_files: &[ContextFileContent],
                             repo_map: Option<&str>,
                             repo_dirs: Option<&str>| {
        let diagnostics_view = diagnostics.map(|d| ReviewRefineDiagnostics {
            failed_step_id: d.failed_step_id,
            failure_reason: d.failure_reason,
        });
        tauqe_core::review::prompt::build_review_finding_refine_user_prompt(
            session,
            item,
            context_files,
            repo_map,
            repo_dirs,
            diagnostics_view,
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
            record_synthesis_history(
                state,
                history_turn_prompt,
                history_context_paths,
                warn_msg.clone(),
                Some(format!(
                    "{}: Token Budget Exceeded ({})",
                    OPERATION_TITLE, item.title
                )),
            )
            .await;
            return Ok(RefineOutcome::Blocked { reason: warn_msg });
        }
        Err(ArchitectDiscoveryError::Cancelled) => {
            clear_auto_context(state).await;
            return Err(refine_error(
                errors::CANCELLED,
                format!("{} was cancelled", OPERATION_TITLE),
            ));
        }
        Err(ArchitectDiscoveryError::ModelError(err)) => {
            clear_auto_context(state).await;
            return Err(refine_error(errors::MODEL_ERROR, err));
        }
        Err(ArchitectDiscoveryError::ParseError(err)) => {
            clear_auto_context(state).await;
            return Err(refine_error(errors::PARSE_ERROR, err));
        }
    };

    if output.status.eq_ignore_ascii_case("blocked") {
        let reason = output.reason.unwrap_or(output.explanation);
        let blocked_msg = format!(
            "Architect reported blockers for review finding [{}#{}]:\n\n{}",
            session.id, item.id, reason
        );
        record_synthesis_history(
            state,
            history_turn_prompt,
            history_context_paths,
            blocked_msg,
            Some(format!("{}: Blocked ({})", OPERATION_TITLE, item.title)),
        )
        .await;
        return Ok(RefineOutcome::Blocked { reason });
    }

    if output.status.eq_ignore_ascii_case("needs_context") {
        return Err(refine_error(
            errors::NEEDS_CONTEXT,
            format!(
                "Architect requested additional context but no discovery rounds remain (limit: {}). Narrow the finding scope or raise the discovery round limit, then refine again",
                max_discovery_rounds
            ),
        ));
    }

    if !output.status.eq_ignore_ascii_case("success") {
        return Err(refine_error(
            errors::UNEXPECTED_STATUS,
            format!(
                "Architect returned unsupported status '{}'; expected 'success' or 'blocked'",
                output.status
            ),
        ));
    }

    if output.items.is_empty() {
        return Err(refine_error(
            errors::EMPTY_ITEMS,
            "Model returned success status but an empty items array",
        ));
    }

    if let Some(duplicate) = find_duplicate_item_ids(&output.items) {
        return Err(refine_error(
            errors::INVALID_PLAN_TREE,
            format!(
                "Architect returned a subplan with a duplicate or empty step ID: '{}'",
                duplicate
            ),
        ));
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
    let subplan_architect = architect_tier;
    let updated = match offload_review(move || {
        ReviewStorage::replace_uncompleted_subplan(
            &save_root,
            &save_review_id,
            item_id,
            new_subplan,
            subplan_architect,
        )
    })
    .await
    {
        Ok(Some(issue)) => issue,
        Ok(None) => {
            return Err(refine_error(
                errors::NOT_FOUND,
                format!(
                    "Review item #{} not found in session '{}'",
                    item_id, review_id
                ),
            ));
        }
        Err(err) => return Err(refine_error(errors::PLAN_SAVE_FAILED, err)),
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
    record_synthesis_history(
        state,
        history_turn_prompt,
        history_context_paths,
        history_response,
        Some(format!("{}: {}", OPERATION_TITLE, updated.title)),
    )
    .await;

    publish_review(state, repo_root, &review_id).await;

    Ok(RefineOutcome::Success {
        item: updated,
        explanation,
    })
}

/// Builds a typed error for the decomposition helper, mirroring `Response::err`.
fn refine_error(code: &'static str, message: impl Into<String>) -> ResponseError {
    ResponseError {
        code: code.to_string(),
        message: message.into(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::model::{ModelEntry, ModelRef, ReasoningEffort};

    #[test]
    fn architect_tier_banner_renders_its_reasoning_depth() {
        // Escalation is visible to the developer, so the banner must name the
        // rung that actually runs, including its reasoning depth.
        let tier = ArchitectTier::new(
            ModelRef::openrouter("planner/model"),
            Some(ReasoningEffort::High),
        );
        assert_eq!(
            format_architect_tier(&tier),
            format!("openrouter:planner/model ({})", ReasoningEffort::High)
        );
        // A rung without a reasoning depth renders as a bare model reference.
        assert_eq!(
            format_architect_tier(&ArchitectTier::from(ModelRef::openrouter("planner/model"))),
            "openrouter:planner/model"
        );
    }

    #[test]
    fn failed_planning_restores_the_previous_status() {
        // A finding the developer is still examining keeps its own status: a
        // failed planning round must never silently queue it for execution.
        assert_eq!(
            status_after_failed_planning(IssueStatus::Discussion),
            IssueStatus::Discussion
        );
        assert_eq!(
            status_after_failed_planning(IssueStatus::Refine),
            IssueStatus::Refine
        );
        // A finding left `Skipped` by an interrupted subplan keeps its
        // interruption diagnostic instead of losing it to the planning round.
        assert_eq!(
            status_after_failed_planning(IssueStatus::Skipped),
            IssueStatus::Skipped
        );
    }

    #[test]
    fn failed_planning_never_leaves_in_progress() {
        // The planning status cannot outlive the round that published it.
        assert_eq!(
            status_after_failed_planning(IssueStatus::InProgress),
            IssueStatus::Todo
        );
    }

    #[test]
    fn test_resolve_finding_architect_tier_custom_model_uses_its_own_effort() {
        let cfg = AppConfig {
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
                ]),
                ..Default::default()
            },
            ..Default::default()
        };

        let selection = ModelSelection::Specific(ModelRef::openrouter("google/gemini-flash"));
        let (tier, _) = resolve_finding_architect_tier(&cfg, None, Some(&selection), false);
        assert_eq!(tier.model, ModelRef::openrouter("google/gemini-flash"));
        assert_eq!(tier.effort, Some(ReasoningEffort::High));
    }
}
