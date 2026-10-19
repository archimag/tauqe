//! Per-finding bounded-turn execution.
//!
//! A review finding runs as a single isolated bounded turn, or, when the
//! Architect has decomposed it, as an ordered walk over its embedded subplan's
//! leaf sub-steps. Each attempt drives the shared `execute_model_turn` entry
//! point with the finding's target files preloaded and may escalate along the
//! coder ladder when `auto_level_up` is enabled. Domain failures are confined to
//! the current finding, while infrastructure failures abort the batch. Finding
//! status and the immutable `Done` foundation of a partially completed subplan
//! are persisted through `ReviewStorage`, so no plan-tree manipulation happens
//! in the server crate.
//!
//! Three invariants govern this module:
//! - #c-1: an `Architecture`-class or `Refine` finding without a subplan is
//!   decomposed by the Architect autonomously before any coding starts. The
//!   decision reads the authoritative `issue_type` category, never the coarse
//!   presentation severity projected from it.
//! - #c-2: the active leaf sub-step is marked `InProgress` and persisted on disk
//!   (with a `review/updated` event) for honest telemetry; the finding becomes
//!   `Fixed` only once every remaining leaf sub-step is `Done`.
//! - #c-3: a sub-step that fails with a domain failure is replanned up to
//!   `max_replans` times: the Architect reprojects the remaining horizon from the
//!   immutable `Done` foundation and the failure diagnostics, and the finding is
//!   resumed from the synthesized horizon instead of being abandoned.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::providers::{format_session_id, SESSION_PREFIX_REVIEW};
use tauqe_core::review::prompt::refine::ReviewRefineDiagnostics;
use tauqe_core::review::prompt::ReviewStepFailure;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    IssueStatus, IssueType, ModelRole, ModelSelection, Plan, PlanItemStatus, PlanScopeEvaluation,
    ReviewItem, ReviewSession, TurnIntent,
};

use crate::handlers::model::{execute_model_turn, StepTurnOptions};
use crate::handlers::plan::preload_step_files;
use crate::state::{next_operation_id, AppState, OperationGuard};

use super::common::{
    append_model_comment, clear_auto_context, offload_review, preload_item_files, publish_review,
    report_item_failure, set_item_status, skip_finding_with_diagnostic, turn_applied_edits,
};
use super::refine::{refine_finding_with_architect, RefineOutcome};

/// Executes one finding in an isolated turn.
///
/// The finding becomes `Fixed` only when the turn (or its subplan) satisfies the
/// success contract; on any failure it returns to `Todo`. The transient auto
/// context layer is always cleared once the step finishes, so a retried attempt
/// starts from a clean, deterministic basis.
///
/// `step_id` targets a single leaf sub-step of an already decomposed finding,
/// `model` overrides the Coder model, and `instructions` are appended to the
/// execution prompt as developer directives.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_review_step(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    step_id: Option<&str>,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    let res = run_review_step_inner(
        state,
        repo_root,
        review_id,
        item_id,
        step_id,
        model,
        instructions,
        operation,
    )
    .await;
    clear_auto_context(state).await;
    res
}

#[allow(clippy::too_many_arguments)]
async fn run_review_step_inner(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    step_id: Option<&str>,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    // Every finding turn of one audit extends the same provider prefix cache, so
    // the session identifier is derived from the review, not from the dialogue.
    let session_id = Some(format_session_id(SESSION_PREFIX_REVIEW, review_id));
    let (mut session, mut item) = reload_finding(repo_root, review_id, item_id).await?;

    match item.status {
        // A finished finding is not re-executed. The sole exception is a targeted
        // uncompleted sub-step of an interrupted subplan: the finding was marked
        // `Skipped` after its subplan stalled, yet the developer may resume the
        // remaining step directly.
        IssueStatus::Fixed | IssueStatus::Skipped => {
            let (total, done) = item.subplan_stats();
            let targeted_incomplete = step_id.is_some() && item.has_subplan() && done < total;
            if !targeted_incomplete {
                return Ok(());
            }
        }
        IssueStatus::Discussion => {
            return Err(ReviewStepFailure::infrastructure(format!(
                "Review item #{} is in DISCUSSION status and cannot be executed until reviewed",
                item.id
            )));
        }
        IssueStatus::Rejected => {
            return Err(ReviewStepFailure::infrastructure(format!(
                "Review item #{} is REJECTED and cannot be executed",
                item.id
            )));
        }
        IssueStatus::Refine | IssueStatus::Todo | IssueStatus::InProgress => {}
    }

    // #c-1: an architecture-class or Refine finding must be decomposed by the
    // Architect before the Coder touches it. The harness performs the
    // decomposition autonomously so coding never starts from an undecomposed
    // finding.
    let needs_refinement = !item.has_subplan()
        && (item.issue_type == IssueType::Architecture || item.status == IssueStatus::Refine);
    if needs_refinement {
        tracing::info!(
            review_id = review_id,
            item_id = item_id,
            issue_type = %item.issue_type,
            status = %item.status,
            "autonomous architect decomposition before coding"
        );
        match refine_finding_with_architect(
            state,
            repo_root,
            &session,
            &item,
            None,
            None,
            None,
            &operation,
        )
        .await
        {
            Ok(RefineOutcome::Success { explanation, .. }) => {
                tracing::info!(
                    review_id = review_id,
                    item_id = item_id,
                    explanation = %explanation,
                    "autonomous decomposition succeeded"
                );
            }
            Ok(RefineOutcome::Blocked { reason }) => {
                skip_finding_with_diagnostic(state, repo_root, review_id, item_id, "", &reason)
                    .await;
                return Err(ReviewStepFailure::domain(reason));
            }
            Err(err) => return Err(ReviewStepFailure::infrastructure(err.message)),
        }
        clear_auto_context(state).await;
        (session, item) = reload_finding(repo_root, review_id, item_id).await?;
    }

    // A finding decomposed into an embedded subplan executes its sub-steps one by
    // one instead of a single monolithic turn.
    if item.has_subplan() {
        return run_review_subplan_with_replans(
            state,
            repo_root,
            review_id,
            &session,
            &item,
            step_id,
            model,
            instructions,
            operation,
        )
        .await;
    }

    preload_item_files(state, repo_root, &item.files).await;
    set_item_status(state, repo_root, review_id, item_id, IssueStatus::InProgress).await;
    tracing::info!(review_id = review_id, item_id = item_id, "review step started");

    // Generate the active turn marker before assembling prompts so the target
    // finding prompt and the model turn share the same marker-bound protocol.
    let marker = state.next_turn_marker().await;
    let marker_suffix = marker
        .as_ref()
        .map(|m| format!("_{}", m))
        .unwrap_or_default();

    let mut prompt = match tauqe_core::review::prompt::format_review_step_execution_prompt(
        &session,
        item_id,
        &marker_suffix,
    ) {
        Ok(p) => p,
        Err(err) => {
            set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo).await;
            return Err(ReviewStepFailure::infrastructure(err));
        }
    };
    append_developer_directives(&mut prompt, instructions.as_deref());

    let (max_retries, auto_level_up, models_config) = {
        let cfg = state.config.lock().await.config.clone();
        (
            cfg.develop.max_retries,
            cfg.models.auto_level_up,
            cfg.models.clone(),
        )
    };

    // An explicit request-level model or an explicitly assigned finding model
    // pins the Coder and disables ladder escalation; an unassigned finding
    // starts on the Coder role and may escalate through the coder ladder when
    // `auto_level_up` is enabled.
    let explicit_selection = model.or_else(|| item.model.clone());
    let mut selection = explicit_selection
        .clone()
        .unwrap_or(ModelSelection::Role(ModelRole::Coder));
    let mut reasoning_effort = None;
    let mut ladder_idx = 0usize;
    let mut attempt = 0usize;

    loop {
        let workflow = state.config.lock().await.config.develop.workflow;
        let policy = crate::handlers::model::build_apply_policy(state, workflow);

        let turn_result = execute_model_turn(
            state,
            prompt.clone(),
            policy,
            StepTurnOptions {
                intent: TurnIntent::IsolatedStep,
                custom_selection: Some(selection.clone()),
                plan_context: None,
                reasoning_effort,
                marker: marker.clone(),
                shared_operation: Some(Arc::clone(&operation)),
                session_id: session_id.clone(),
            },
        )
        .await;

        let turn = match turn_result {
            Ok(turn) => turn,
            Err(err) => {
                // Infrastructure failure: the turn could not even start (missing
                // credentials, operation slot busy, ...). Fatal to the batch.
                set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo).await;
                let op_id = format!("op-{}", next_operation_id());
                report_item_failure(state, &op_id, &err.message);
                return Err(ReviewStepFailure::infrastructure(err.message));
            }
        };

        // Semantic block: the coder reports that the finding preconditions
        // cannot be satisfied as stated. This is a domain failure: the finding
        // is skipped with diagnostics and the batch continues without a retry
        // cycle.
        if let Some(reason) = turn.blocked.clone() {
            report_item_failure(state, &turn.operation_id, &reason);
            skip_finding_with_diagnostic(
                state,
                repo_root,
                review_id,
                item_id,
                &turn.assistant_text,
                &reason,
            )
            .await;
            return Err(ReviewStepFailure::domain(reason));
        }

        let applied = turn_applied_edits(&turn);

        if turn.is_success() && applied {
            let notes = match &turn.result {
                Some(tauqe_protocol::ModelResult::Edit { summary, .. })
                    if !summary.trim().is_empty() =>
                {
                    Some(summary.clone())
                }
                _ => None,
            };
            if let Some(notes) = notes {
                let save_root = repo_root.to_path_buf();
                let review_id_owned = review_id.to_string();
                let notes_clone = notes.clone();
                let _ = offload_review(move || {
                    ReviewStorage::update_session_item(
                        &save_root,
                        &review_id_owned,
                        item_id,
                        |it| {
                            it.status = IssueStatus::Fixed;
                            if it.note.is_none() {
                                it.note = Some(notes_clone);
                            }
                        },
                    )
                })
                .await;
                publish_review(state, repo_root, review_id).await;
            } else {
                set_item_status(state, repo_root, review_id, item_id, IssueStatus::Fixed).await;
            }
            return Ok(());
        }

        if !applied && turn.error.is_none() && !turn.is_cancelled {
            // The registry-bound skip parser is the single source of truth for
            // the `<skip>` tag, so the finding is skipped through the typed
            // protocol outcome instead of a second ad-hoc scanner.
            if let Some(skip_reason) =
                tauqe_core::edits::protocol::xml::extract_skip(&turn.assistant_text)
            {
                let save_root = repo_root.to_path_buf();
                let review_id_owned = review_id.to_string();
                let skip_clone = skip_reason.clone();
                let _ = offload_review(move || {
                    ReviewStorage::update_session_item(
                        &save_root,
                        &review_id_owned,
                        item_id,
                        |it| {
                            it.status = IssueStatus::Skipped;
                            it.note = Some(skip_clone);
                        },
                    )
                })
                .await;
                publish_review(state, repo_root, review_id).await;
                return Ok(());
            }
        }

        let reason = turn.error.clone().unwrap_or_else(|| {
            if applied {
                "Finding turn ended without verified changes or a success tag".to_string()
            } else {
                "Finding turn applied no code changes".to_string()
            }
        });

        // Explicit cancellation is an infrastructure failure: fatal to the batch.
        if turn.is_cancelled {
            set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo).await;
            return Err(ReviewStepFailure::infrastructure(reason));
        }

        // A turn that produced no result at all died at the transport or
        // provider layer (the provider error is already reported upstream), so
        // it aborts the batch instead of being retried or skipped.
        if turn.result.is_none() {
            set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo).await;
            return Err(ReviewStepFailure::infrastructure(reason));
        }

        // Domain failure: the turn produced a result but did not resolve the
        // finding. Retry within the attempt budget, then skip the finding with
        // diagnostics so the queue advances to the next one.
        if attempt >= max_retries {
            if turn.error.is_none() {
                report_item_failure(state, &turn.operation_id, &reason);
            }
            skip_finding_with_diagnostic(
                state,
                repo_root,
                review_id,
                item_id,
                &turn.assistant_text,
                &reason,
            )
            .await;
            return Err(ReviewStepFailure::domain(reason));
        }

        // Reset to a clean, deterministic basis before the corrective attempt.
        clear_auto_context(state).await;
        preload_item_files(state, repo_root, &item.files).await;

        if explicit_selection.is_none() && auto_level_up {
            let from_tier = ladder_idx;
            ladder_idx += 1;
            let tier = models_config.coder_tier(ladder_idx);
            tracing::warn!(
                review_id = review_id,
                item_id = item_id,
                from_tier = from_tier,
                to_tier = ladder_idx,
                to_model = %tier.model,
                effort = ?tier.effort,
                reason = %reason,
                "review step coder escalation"
            );
            selection = ModelSelection::Specific(tier.model);
            reasoning_effort = tier.effort;
        }

        prompt = tauqe_core::review::prompt::format_review_step_retry_prompt(
            &turn.assistant_text,
            turn.error.as_deref(),
        );
        attempt += 1;
    }
}

/// Renders why a review subplan scope cannot be executed right now.
///
/// The shared scope evaluator is the single source of truth for readiness, so
/// its stall variants are translated into a developer-facing diagnostic here
/// instead of being re-derived from the plan tree.
fn format_scope_stall(step_id: Option<&str>, evaluation: &PlanScopeEvaluation) -> String {
    let target = step_id.unwrap_or("subplan");
    match evaluation {
        PlanScopeEvaluation::BlockedByDiscussion(items) => format!(
            "Sub-step scope '{}' is still in DISCUSSION: {}",
            target,
            items.join(", ")
        ),
        PlanScopeEvaluation::BlockedByBlockers(items) => format!(
            "Sub-step scope '{}' is BLOCKED: {}",
            target,
            items.join(", ")
        ),
        PlanScopeEvaluation::BlockedByDependencies(items) => format!(
            "Sub-step scope '{}' has unmet dependencies: {}",
            target,
            items.join(", ")
        ),
        PlanScopeEvaluation::StepBlocked => format!("Sub-step '{}' is BLOCKED", target),
        PlanScopeEvaluation::Cancelled => format!("Sub-step scope '{}' is CANCELLED", target),
        PlanScopeEvaluation::NotFound => {
            format!("Sub-step '{}' not found in the subplan", target)
        }
        // `Ready` and `AllCompleted` never reach the stall diagnostic.
        PlanScopeEvaluation::Ready(_) | PlanScopeEvaluation::AllCompleted => {
            format!("Sub-step scope '{}' is already complete", target)
        }
    }
}

/// Appends the developer directives section to an execution prompt when present.
fn append_developer_directives(prompt: &mut String, instructions: Option<&str>) {
    if let Some(dirs) = instructions.map(str::trim).filter(|d| !d.is_empty()) {
        prompt.push_str("\n## Developer Directives:\n");
        prompt.push_str(dirs);
        prompt.push('\n');
    }
}

/// Persists the working subplan atomically.
///
/// Container statuses are derived, so they are recomputed before the write and
/// the replacement itself is delegated to `ReviewStorage::update_subplan`, the
/// single storage entry point that normalizes and stores a subplan.
async fn persist_subplan(
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    plan: &mut Plan,
) -> Result<(), String> {
    plan.refresh_parent_statuses();
    let subplan = plan.items.clone();
    let save_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    offload_review(move || {
        ReviewStorage::update_subplan(&save_root, &review_id_owned, item_id, subplan)
    })
    .await?;
    Ok(())
}

/// Prefix of the resolution note recorded when a subplan stops before completion.
const SUBPLAN_INTERRUPTED_NOTE_PREFIX: &str = "Subplan interrupted before completion";

/// Records an interrupted subplan.
///
/// Completed sub-steps keep their `Done` status, the stop reason and the model's
/// final comment are preserved in `resolution_notes`, and the finding becomes
/// `Skipped` so the developer can inspect the cause and decide how to proceed
/// without losing finished work.
async fn finish_subplan_interrupted(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    plan: &mut Plan,
    assistant_text: &str,
    reason: &str,
) {
    plan.refresh_parent_statuses();
    let subplan = plan.items.clone();
    let notes = append_model_comment(
        format!("{}: {}", SUBPLAN_INTERRUPTED_NOTE_PREFIX, reason),
        assistant_text,
    );

    let save_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    let _ = offload_review(move || {
        ReviewStorage::update_session_item(&save_root, &review_id_owned, item_id, move |it| {
            it.status = IssueStatus::Skipped;
            it.plan = subplan;
            it.note = Some(notes);
        })
    })
    .await;
    publish_review(state, repo_root, review_id).await;
}

/// Executes the embedded subplan of a review finding one leaf sub-step at a time.
///
/// Each sub-step runs in its own bounded turn with its target files preloaded.
/// The active sub-step is marked `InProgress` and persisted before its turn
/// starts (#c-2). A completed sub-step is persisted as `Done`, so finished work
/// survives any later interruption. When a sub-step cannot apply, blocks, or the
/// turn is cancelled, it is reverted to `Todo` and the finding is marked
/// `Skipped` with the stop reason recorded in `resolution_notes`. When every
/// remaining leaf sub-step is `Done`, the finding becomes `Fixed`; otherwise it
/// returns to `Todo`.
///
/// When `step_id` targets a single leaf, only that sub-step runs; the finding
/// becomes `Fixed` only if the whole subplan is then complete.
#[allow(clippy::too_many_arguments)]
async fn run_review_subplan(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    session: &ReviewSession,
    item: &ReviewItem,
    step_id: Option<&str>,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    // The sub-plan walk of one review session keeps a single warm provider
    // prefix cache across its sub-steps and their retries.
    let session_id = Some(format_session_id(SESSION_PREFIX_REVIEW, review_id));
    let item_id = item.id;
    set_item_status(state, repo_root, review_id, item_id, IssueStatus::InProgress).await;

    let marker = state.next_turn_marker().await;
    let marker_suffix = marker
        .as_ref()
        .map(|m| format!("_{}", m))
        .unwrap_or_default();

    let (max_retries, auto_level_up, models_config) = {
        let cfg = state.config.lock().await.config.clone();
        (
            cfg.develop.max_retries,
            cfg.models.auto_level_up,
            cfg.models.clone(),
        )
    };

    let explicit_selection = model.or_else(|| item.model.clone());

    // The embedded subplan is the working state of this walk: every status
    // transition goes through `Plan::update_item_status`, so container statuses
    // stay derived and readiness follows the shared scope evaluator.
    let mut plan = Plan {
        id: session.id.clone(),
        title: session.title.clone(),
        items: item.subplan.clone(),
        ..Default::default()
    };

    // Resolve the leaf sub-steps this call executes through the shared scope
    // evaluator, so dependency ordering and terminal states are derived exactly
    // as they are for a plan.
    let evaluation = match step_id {
        Some(target) if plan.is_leaf_item(target) == Some(true) => plan.evaluate_scope(Some(target)),
        Some(target) => {
            set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo).await;
            return Err(ReviewStepFailure::infrastructure(format!(
                "Sub-step '{}' not found in review item #{}",
                target, item.id
            )));
        }
        None => plan.evaluate_scope(None),
    };

    let run_ids: Vec<String> = match evaluation {
        PlanScopeEvaluation::Ready(steps) => steps.into_iter().map(|step| step.id).collect(),
        // Nothing actionable is left: every leaf is already `Done`.
        PlanScopeEvaluation::AllCompleted => Vec::new(),
        stall => {
            set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo).await;
            return Err(ReviewStepFailure::infrastructure(format_scope_stall(
                step_id, &stall,
            )));
        }
    };

    for current_step_id in &run_ids {
        let step_snapshot = match plan.find_item(current_step_id) {
            Some(step) if step.status != PlanItemStatus::Done => step.clone(),
            _ => continue,
        };

        preload_step_files(state, repo_root, &step_snapshot).await;

        // #c-2: mark the active sub-step InProgress and persist it atomically.
        if !plan.update_item_status(current_step_id, PlanItemStatus::InProgress) {
            continue;
        }
        persist_subplan(repo_root, review_id, item_id, &mut plan)
            .await
            .map_err(ReviewStepFailure::infrastructure)?;
        publish_review(state, repo_root, review_id).await;

        let step = match plan.find_item(current_step_id) {
            Some(step) => step.clone(),
            None => continue,
        };

        // Track the model's most recent response for this sub-step so a later
        // interruption can preserve it alongside the stop reason.
        let mut step_assistant_text = String::new();

        let mut prompt = match tauqe_core::plan::prompt::format_plan_step_execution_prompt(
            &plan,
            current_step_id,
            &marker_suffix,
        ) {
            Ok(p) => p,
            Err(err) => {
                plan.update_item_status(current_step_id, PlanItemStatus::Todo);
                finish_subplan_interrupted(
                    state,
                    repo_root,
                    review_id,
                    item_id,
                    &mut plan,
                    &step_assistant_text,
                    &err,
                )
                .await;
                return Err(ReviewStepFailure::infrastructure(err));
            }
        };
        append_developer_directives(&mut prompt, instructions.as_deref());

        let mut selection = explicit_selection
            .clone()
            .unwrap_or(ModelSelection::Role(ModelRole::Coder));
        let mut reasoning_effort = None;
        let mut ladder_idx = 0usize;
        let mut attempt = 0usize;

        let step_result: Result<(), ReviewStepFailure> = loop {
            let workflow = state.config.lock().await.config.develop.workflow;
            let policy = crate::handlers::model::build_apply_policy(state, workflow);

            let turn_result = execute_model_turn(
                state,
                prompt.clone(),
                policy,
                StepTurnOptions {
                    intent: TurnIntent::IsolatedStep,
                    custom_selection: Some(selection.clone()),
                    plan_context: None,
                    reasoning_effort,
                    marker: marker.clone(),
                    shared_operation: Some(Arc::clone(&operation)),
                    session_id: session_id.clone(),
                },
            )
            .await;

            let turn = match turn_result {
                Ok(turn) => turn,
                Err(err) => break Err(ReviewStepFailure::infrastructure(err.message)),
            };

            step_assistant_text = turn.assistant_text.clone();

            if let Some(reason) = turn.blocked.clone() {
                break Err(ReviewStepFailure::domain(reason));
            }

            let applied = turn_applied_edits(&turn);

            if turn.is_success() && applied {
                break Ok(());
            }

            if turn.is_cancelled {
                break Err(ReviewStepFailure::infrastructure(
                    "Subplan execution was cancelled".to_string(),
                ));
            }

            if turn.result.is_none() {
                let reason = turn
                    .error
                    .clone()
                    .unwrap_or_else(|| "Sub-step turn failed without a result".to_string());
                break Err(ReviewStepFailure::infrastructure(reason));
            }

            let reason = turn.error.clone().unwrap_or_else(|| {
                if applied {
                    "Sub-step finished without verified changes or a success tag".to_string()
                } else {
                    "Sub-step finished without applying any code changes".to_string()
                }
            });

            if attempt >= max_retries {
                break Err(ReviewStepFailure::domain(reason));
            }

            clear_auto_context(state).await;
            preload_step_files(state, repo_root, &step).await;

            if explicit_selection.is_none() && auto_level_up {
                let from_tier = ladder_idx;
                ladder_idx += 1;
                let tier = models_config.coder_tier(ladder_idx);
                tracing::warn!(
                    review_id = review_id,
                    item_id = item_id,
                    step_id = current_step_id.as_str(),
                    from_tier = from_tier,
                    to_tier = ladder_idx,
                    to_model = %tier.model,
                    effort = ?tier.effort,
                    reason = %reason,
                    "review subplan step coder escalation"
                );
                selection = ModelSelection::Specific(tier.model);
                reasoning_effort = tier.effort;
            }

            prompt = tauqe_core::plan::prompt::format_plan_step_retry_prompt(
                &turn.assistant_text,
                turn.error.as_deref(),
            );
            attempt += 1;
        };

        clear_auto_context(state).await;

        match step_result {
            Ok(()) => {
                plan.update_item_status(current_step_id, PlanItemStatus::Done);
                persist_subplan(repo_root, review_id, item_id, &mut plan)
                    .await
                    .map_err(ReviewStepFailure::infrastructure)?;
                publish_review(state, repo_root, review_id).await;
            }
            Err(failure) => {
                // Revert the interrupted sub-step before recording the stop, and
                // attribute the failure to it so the supervisor can replan the
                // remaining horizon around the failed approach.
                plan.update_item_status(current_step_id, PlanItemStatus::Todo);
                let failure = failure.with_step(current_step_id.as_str());
                let reason = failure.reason().to_string();
                let op_id = format!("op-{}", next_operation_id());
                report_item_failure(state, &op_id, &reason);
                finish_subplan_interrupted(
                    state,
                    repo_root,
                    review_id,
                    item_id,
                    &mut plan,
                    &step_assistant_text,
                    &reason,
                )
                .await;
                return Err(failure);
            }
        }
    }
    clear_auto_context(state).await;

    // #c-2: a finding is Fixed only when every remaining leaf sub-step is Done.
    let (total, done) = plan.count_stats();
    if total > 0 && done == total {
        let notes = format!(
            "Subplan completed: all {} sub-step(s) applied successfully",
            total
        );
        let save_root = repo_root.to_path_buf();
        let review_id_owned = review_id.to_string();
        let _ = offload_review(move || {
            ReviewStorage::update_session_item(&save_root, &review_id_owned, item_id, move |it| {
                it.status = IssueStatus::Fixed;
                if it.note.is_none() {
                    it.note = Some(notes);
                }
            })
        })
        .await;
        publish_review(state, repo_root, review_id).await;
    } else {
        set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo).await;
    }
    Ok(())
}

/// Reloads the review session and the target finding from disk.
///
/// Execution and replanning both need the persisted state of the finding: the
/// subplan mutated by a failed sub-step, the status written by an interruption,
/// and the horizon synthesized by the Architect.
async fn reload_finding(
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
) -> Result<(ReviewSession, ReviewItem), ReviewStepFailure> {
    let load_root = repo_root.to_path_buf();
    let load_id = review_id.to_string();
    let session =
        match offload_review(move || ReviewStorage::load_review(&load_root, &load_id)).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Err(ReviewStepFailure::infrastructure(format!(
                    "Review session '{}' not found",
                    review_id
                )));
            }
            Err(err) => return Err(ReviewStepFailure::infrastructure(err)),
        };
    let item = match session.find_item(item_id) {
        Some(i) => i.clone(),
        None => {
            return Err(ReviewStepFailure::infrastructure(format!(
                "Review item #{} not found in session '{}'",
                item_id, review_id
            )));
        }
    };
    Ok((session, item))
}

/// Whether a subplan still contains the given sub-step id.
fn subplan_contains(items: &[tauqe_protocol::PlanItem], step_id: &str) -> bool {
    items
        .iter()
        .any(|item| item.id == step_id || subplan_contains(&item.children, step_id))
}

/// Resumes a finding after the Architect reprojected its remaining horizon.
///
/// The interrupted sub-step reverted to `Todo` and the finding was marked
/// `Skipped` with an interruption note. That note describes a superseded horizon,
/// so it is cleared here and the finding returns to `Todo` before the retry; a
/// successful retry then records its own outcome note instead of inheriting the
/// diagnostic of an approach that no longer exists.
async fn resume_finding_after_replan(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
) {
    let save_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    let _ = offload_review(move || {
        ReviewStorage::update_session_item(&save_root, &review_id_owned, item_id, |it| {
            it.status = IssueStatus::Todo;
            let superseded = it
                .note
                .as_deref()
                .is_some_and(|note| note.starts_with(SUBPLAN_INTERRUPTED_NOTE_PREFIX));
            if superseded {
                it.note = None;
            }
        })
    })
    .await;
    publish_review(state, repo_root, review_id).await;
}

/// Executes a decomposed finding's subplan, replanning the remaining horizon
/// around a failed sub-step up to `max_replans` times.
///
/// [`run_review_subplan`] confines a failure to the current finding: the failed
/// sub-step returns to `Todo`, the completed foundation stays `Done`, and the
/// finding is marked `Skipped` with an interruption note. When the failure is a
/// domain failure attributable to a concrete sub-step and the replan budget
/// allows it, the Architect reprojects the remaining horizon from the immutable
/// `Done` foundation and the failure diagnostics — exactly as plan replanning
/// does — and the finding is resumed from the synthesized horizon. An exhausted
/// budget or a blocked Architect leaves the finding `Skipped`, so the batch
/// advances instead of retrying a hopeless approach.
#[allow(clippy::too_many_arguments)]
async fn run_review_subplan_with_replans(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    session: &ReviewSession,
    item: &ReviewItem,
    step_id: Option<&str>,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    let max_replans = state.config.lock().await.config.develop.max_replans;
    let mut replans_count = 0usize;
    let mut session = session.clone();
    let mut item = item.clone();
    let mut target = step_id.map(str::to_string);

    loop {
        let failure = match run_review_subplan(
            state,
            repo_root,
            review_id,
            &session,
            &item,
            target.as_deref(),
            model.clone(),
            instructions.clone(),
            Arc::clone(&operation),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(failure) => failure,
        };

        // Only a domain failure attributable to a concrete sub-step carries a
        // diagnostic the Architect can reproject the horizon from: an
        // infrastructure failure aborts the batch, and a failure without a step
        // has no failed approach to replace.
        let failed_step = if failure.is_domain() {
            failure.failed_step().map(str::to_string)
        } else {
            None
        };
        let Some(failed_step) = failed_step else {
            return Err(failure);
        };

        if replans_count >= max_replans {
            tracing::warn!(
                review_id = review_id,
                item_id = item.id,
                failed_step_id = failed_step.as_str(),
                max_replans = max_replans,
                "review finding replanning limit reached"
            );
            return Err(failure);
        }
        replans_count += 1;

        tracing::warn!(
            review_id = review_id,
            item_id = item.id,
            failed_step_id = failed_step.as_str(),
            replan_round = replans_count,
            max_replans = max_replans,
            reason = failure.reason(),
            "review finding replanning started"
        );

        // The Architect must see the subplan as persisted after the failed
        // sub-step was reverted, so the finding is reloaded before synthesis.
        let reloaded = reload_finding(repo_root, review_id, item.id).await?;
        session = reloaded.0;
        item = reloaded.1;

        match refine_finding_with_architect(
            state,
            repo_root,
            &session,
            &item,
            None,
            None,
            Some(ReviewRefineDiagnostics {
                failed_step_id: failed_step.as_str(),
                failure_reason: Some(failure.reason()),
            }),
            &operation,
        )
        .await
        {
            Ok(RefineOutcome::Success { .. }) => {}
            Ok(RefineOutcome::Blocked { reason }) => {
                skip_finding_with_diagnostic(state, repo_root, review_id, item.id, "", &reason)
                    .await;
                return Err(ReviewStepFailure::domain(reason));
            }
            Err(err) => return Err(ReviewStepFailure::infrastructure(err.message)),
        }

        // Resume the finding from the synthesized horizon: the interruption note
        // recorded for the failed sub-step is superseded by the replan and must
        // not survive a successful retry.
        resume_finding_after_replan(state, repo_root, review_id, item.id).await;
        let reloaded = reload_finding(repo_root, review_id, item.id).await?;
        session = reloaded.0;
        item = reloaded.1;

        // A targeted sub-step that the synthesized horizon replaced can no
        // longer be resumed by id; the retry then walks the whole remaining
        // horizon instead of failing on a vanished target.
        let vanished = target
            .as_deref()
            .is_some_and(|current| !subplan_contains(&item.subplan, current));
        if vanished {
            target = None;
        }
    }
}
