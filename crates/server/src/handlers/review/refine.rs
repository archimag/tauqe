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

use tauqe_core::context::ContextFileContent;
use tauqe_core::providers::{
    format_session_id, JsonSchemaDefinition, ResponseFormat, SESSION_PREFIX_REVIEW,
};
use tauqe_core::review::prompt::refine::ReviewRefineDiagnostics;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    errors, events, IssueStatus, ModelDeltaEvent, ModelRole, ModelSelection, ModelStartedEvent,
    Request, Response, ResponseError, ReviewIssue, ReviewItem, ReviewRefineItemParams,
    ReviewRefineItemResult, ReviewSession,
};

use crate::handlers::architect::{
    format_architect_tier, resolve_architect_provider, run_architect_discovery_loop,
    ArchitectDiscoveryError, ArchitectDiscoveryOutcome, ArchitectDiscoveryRequest,
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

/// Admission rule of a decomposition round, declared by its caller.
///
/// Decomposition is reachable from three places — the `review/refineItem` RPC
/// handler, the autonomous decomposition that runs before a finding is coded,
/// and the replanning loop that reprojects a stalled horizon — so the status
/// lifecycle is declared once as a typed rule here instead of being re-derived
/// by each entry point. Every neighbouring mutation guards the same transition:
/// `handle_plan_replan` refuses a `Done` step, and `review/execute` refuses
/// finished, rejected, and untriaged findings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RefineAdmission {
    /// A developer-initiated request through `review/refineItem`.
    ///
    /// An untriaged finding is refused: human triage gates autonomous work, and
    /// a decomposition fixes the architecture of the finding, so it cannot
    /// precede triage. A `Skipped` finding stays admissible, because that is the
    /// recovery path for a subplan that stalled.
    Developer,
    /// A harness-driven round: the autonomous decomposition before coding, and
    /// the replanning that reprojects a failed sub-step.
    ///
    /// Both callers have already established that the finding is executable —
    /// `review/execute` refuses untriaged findings before the queue starts, and
    /// the replanning loop owns the finding it has just interrupted — so only
    /// the immutable terminal statuses are refused here.
    Harness,
}

impl RefineAdmission {
    /// Validates that `item` may be decomposed under this admission rule.
    ///
    /// The decision is made before any status is published, so a refused round
    /// leaves the persisted finding exactly as it was.
    fn admit(self, item: &ReviewItem) -> Result<(), ResponseError> {
        match item.status {
            // Finished work is immutable under the decision-immutability
            // invariant, exactly as `handle_plan_replan` guards a `Done` step.
            IssueStatus::Fixed => Err(refine_error(
                errors::STEP_IMMUTABLE,
                format!(
                    "Review item #{} is FIXED and immutable; a completed finding cannot be decomposed",
                    item.id
                ),
            )),
            IssueStatus::Rejected => Err(refine_error(
                errors::ITEM_REJECTED,
                format!(
                    "Review item #{} is REJECTED and cannot be decomposed",
                    item.id
                ),
            )),
            // A developer request on an untriaged finding would fix an
            // architectural decision that triage has not approved yet.
            IssueStatus::Discussion if self == Self::Developer => Err(refine_error(
                errors::ITEM_IN_DISCUSSION,
                format!(
                    "Review item #{} is in DISCUSSION status and cannot be decomposed until it is triaged (change status to TODO first)",
                    item.id
                ),
            )),
            _ => Ok(()),
        }
    }
}

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

    // The status lifecycle is validated before the operation slot is taken, so a
    // finished, rejected, or untriaged finding is refused deterministically and
    // its persisted status is never touched.
    if let Err(err) = RefineAdmission::Developer.admit(&item) {
        return Response {
            id: req.id,
            result: None,
            error: Some(err),
        };
    }

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
        RefineAdmission::Developer,
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
///
/// The size difference between the variants is inherent to the outcome: the
/// success arm carries the complete decomposed finding while the blocked arm
/// carries only a reason string. The value feeds the wire result
/// [`ReviewRefineItemResult`], which documents the same allowance, so boxing
/// the payload would only add indirection through every consumer of the
/// outcome; the size lint is therefore allowed here.
#[allow(clippy::large_enum_variant)]
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
///
/// `admission` declares the lifecycle states this caller may decompose, so the
/// status gate runs before any status is published and no entry point can
/// regress a finished finding.
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
    admission: RefineAdmission,
) -> Result<RefineOutcome, ResponseError> {
    // The gate runs before anything is published: a refused round must leave the
    // stored finding exactly as it was.
    admission.admit(item)?;

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
        level_up,
        repomap_budget,
        max_discovery_rounds,
        files_token_budget,
        max_parse_retries,
        include_repo_map,
        discovery_limits,
    ) = {
        let cfg = state.config.lock().await.config.clone();
        // Tier resolution and the lazy level-up invariant are core-owned domain
        // policy shared with plan horizon synthesis, so the server never
        // re-derives the ladder locally.
        let (architect_tier, level_up) = tauqe_core::plan::prompt::resolve_architect_tier(
            &cfg,
            item.subplan_architect.as_ref(),
            custom_model.as_ref(),
            diagnostics.as_ref().map(|d| d.failed_step_id),
            item.subplan_last_replanned_step.as_deref(),
        );
        let provider =
            match resolve_architect_provider(state, &cfg, architect_tier.model.provider).await {
                Ok(p) => p,
                Err(err) => return Err(refine_error(errors::NO_API_KEY, err)),
            };
        (
            provider,
            architect_tier,
            level_up,
            cfg.context.repomap_token_budget,
            cfg.context.max_discovery_rounds,
            cfg.context.files_token_budget,
            cfg.develop.max_retries,
            cfg.context.include_repo_map,
            tauqe_core::workflow::discovery::DiscoveryLimits::from_context_config(&cfg.context),
        )
    };
    let model = architect_tier.model.clone();
    let reasoning_effort = architect_tier.effort;

    // A repeated failure of the same sub-step runs on the escalated rung and
    // records it with the reprojected horizon, so the next wave continues up the
    // ladder instead of restarting from the configuration that already failed.
    if let Some(level) = &level_up {
        tracing::warn!(
            review_id = %session.id,
            item_id = item.id,
            from_tier = ?level.from,
            to_tier = ?level.to,
            "review finding architect level up"
        );
    }

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
            alias: None,
            effort: reasoning_effort,
            plan_context: None,
            is_batch: false,
        })
        .ok(),
    );

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
    let system_prompt =
        tauqe_core::review::prompt::refine::build_review_finding_refine_system_prompt();
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
        discovery_limits,
        max_parse_retries,
        // Decomposition is an isolated design dialogue of one review session:
        // its cache is keyed by the audit, never by the interactive REPL.
        session_id: Some(format_session_id(SESSION_PREFIX_REVIEW, &session.id)),
    };

    let build_user_prompt =
        |context_files: &[ContextFileContent], repo_map: Option<&str>, repo_dirs: Option<&str>| {
            let diagnostics_view = diagnostics.map(|d| ReviewRefineDiagnostics {
                failed_step_id: d.failed_step_id,
                failure_reason: d.failure_reason,
            });
            tauqe_core::review::prompt::refine::build_review_finding_refine_user_prompt(
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
            "Model returned success status but an empty items array".to_string(),
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
    // The sub-step whose horizon this wave reprojects is persisted with the new
    // subplan, so the next wave reads from disk whether this sub-step already
    // failed a replanning round.
    let subplan_replanned_step = diagnostics.map(|d| d.failed_step_id.to_string());
    let updated = match offload_review(move || {
        ReviewStorage::replace_uncompleted_subplan(
            &save_root,
            &save_review_id,
            item_id,
            new_subplan,
            subplan_architect,
            subplan_replanned_step,
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
fn refine_error(code: &'static str, message: String) -> ResponseError {
    ResponseError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ArchitectTier, ModelRef, ReasoningEffort};

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
        // A finding interrupted mid-subplan keeps its diagnostic instead of
        // losing it to the planning round.
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

    /// Builds a finding in the given lifecycle state.
    fn finding_in(status: IssueStatus) -> ReviewItem {
        ReviewItem::from(ReviewIssue {
            id: 12,
            title: "Guard the parser".to_string(),
            model: None,
            issue_type: tauqe_protocol::IssueType::Bug,
            status,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            plan: Vec::new(),
            note: None,
        })
    }

    #[test]
    fn developer_decomposition_refuses_untriaged_and_terminal_findings() {
        // Finished work is immutable, exactly as a `Done` plan step is.
        let err = RefineAdmission::Developer
            .admit(&finding_in(IssueStatus::Fixed))
            .expect_err("a fixed finding must be refused");
        assert_eq!(err.code, errors::STEP_IMMUTABLE);

        let err = RefineAdmission::Developer
            .admit(&finding_in(IssueStatus::Rejected))
            .expect_err("a rejected finding must be refused");
        assert_eq!(err.code, errors::ITEM_REJECTED);

        // Human triage gates autonomous work, so an untriaged finding cannot be
        // turned into an executable horizon yet.
        let err = RefineAdmission::Developer
            .admit(&finding_in(IssueStatus::Discussion))
            .expect_err("an untriaged finding must be refused");
        assert_eq!(err.code, errors::ITEM_IN_DISCUSSION);
    }

    #[test]
    fn developer_decomposition_admits_recoverable_findings() {
        for status in [
            IssueStatus::Todo,
            IssueStatus::InProgress,
            IssueStatus::Refine,
            // A stalled subplan leaves the finding skipped; asking the Architect
            // to reproject it stays a legitimate recovery path.
            IssueStatus::Skipped,
        ] {
            assert!(
                RefineAdmission::Developer
                    .admit(&finding_in(status))
                    .is_ok(),
                "{status} must stay decomposable"
            );
        }
    }

    #[test]
    fn harness_decomposition_refuses_only_immutable_statuses() {
        for status in [IssueStatus::Fixed, IssueStatus::Rejected] {
            assert!(
                RefineAdmission::Harness.admit(&finding_in(status)).is_err(),
                "{status} is immutable and must be refused"
            );
        }

        // The replanning loop reprojects a finding it has just interrupted, so
        // `Skipped` must stay admitted on the harness path.
        for status in [IssueStatus::Skipped, IssueStatus::Refine, IssueStatus::Todo] {
            assert!(
                RefineAdmission::Harness.admit(&finding_in(status)).is_ok(),
                "{status} must stay decomposable by the harness"
            );
        }
    }
}
