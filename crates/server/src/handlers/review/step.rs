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
//! Two invariants govern this module:
//! - #c-1: an `Architecture`-class or `Refine` finding without a subplan is
//!   decomposed by the Architect autonomously before any coding starts. The
//!   decision reads the authoritative `issue_type` category, never the coarse
//!   presentation severity projected from it.
//! - #c-2: the active leaf sub-step is marked `InProgress` and persisted on disk
//!   (with a `review/updated` event) for honest telemetry; the finding becomes
//!   `Fixed` only once every remaining leaf sub-step is `Done`.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::review::prompt::ReviewStepFailure;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    IssueStatus, IssueType, ModelRole, ModelSelection, Plan, PlanItem, PlanItemStatus, ReviewItem,
    ReviewSession,
};

use crate::handlers::model::{execute_model_turn, StepTurnOptions};
use crate::handlers::plan::preload_step_files;
use crate::state::{next_operation_id, AppState, OperationGuard};

use super::common::{
    clear_auto_context, offload_review, preload_item_files, publish_review, report_item_failure,
    set_item_status, skip_finding_with_diagnostic, turn_applied_edits,
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
    let load_root = repo_root.to_path_buf();
    let load_id = review_id.to_string();
    let mut session =
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
    let mut item = match session.find_item(item_id) {
        Some(i) => i.clone(),
        None => {
            return Err(ReviewStepFailure::infrastructure(format!(
                "Review item #{} not found in session '{}'",
                item_id, review_id
            )));
        }
    };

    match item.status {
        // A finished finding is not re-executed. The sole exception is a targeted
        // uncompleted sub-step of an interrupted subplan: the finding was marked
        // `Skipped` after its subplan stalled, yet the developer may resume the
        // remaining step directly.
        IssueStatus::Fixed | IssueStatus::Skipped => {
            let targeted_incomplete =
                step_id.is_some() && item.has_subplan() && has_uncompleted_leaves(&item.subplan);
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
                skip_finding_with_diagnostic(state, repo_root, review_id, item_id, &reason).await;
                return Err(ReviewStepFailure::domain(reason));
            }
            Err(err) => return Err(ReviewStepFailure::infrastructure(err.message)),
        }
        clear_auto_context(state).await;

        let reload_root = repo_root.to_path_buf();
        let reload_id = review_id.to_string();
        session =
            match offload_review(move || ReviewStorage::load_review(&reload_root, &reload_id))
                .await
            {
                Ok(Some(s)) => s,
                Ok(None) => {
                    return Err(ReviewStepFailure::infrastructure(format!(
                        "Review session '{}' not found after decomposition",
                        review_id
                    )));
                }
                Err(err) => return Err(ReviewStepFailure::infrastructure(err)),
            };
        item = match session.find_item(item_id) {
            Some(i) => i.clone(),
            None => {
                return Err(ReviewStepFailure::infrastructure(format!(
                    "Review item #{} not found in session '{}' after decomposition",
                    item_id, review_id
                )));
            }
        };
    }

    // A finding decomposed into an embedded subplan executes its sub-steps one by
    // one instead of a single monolithic turn.
    if item.has_subplan() {
        return run_review_subplan(
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
                custom_selection: Some(selection.clone()),
                plan_context: None,
                reasoning_effort,
                marker: marker.clone(),
                shared_operation: Some(Arc::clone(&operation)),
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
            skip_finding_with_diagnostic(state, repo_root, review_id, item_id, &reason).await;
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
            skip_finding_with_diagnostic(state, repo_root, review_id, item_id, &reason).await;
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

/// Collects the leaf sub-step identifiers of a subplan in declaration order.
///
/// Only leaf steps carry executable work; conceptual container items are skipped.
fn collect_leaf_step_ids(items: &[PlanItem]) -> Vec<String> {
    fn walk(items: &[PlanItem], ids: &mut Vec<String>) {
        for item in items {
            if item.children.is_empty() {
                ids.push(item.id.clone());
            } else {
                walk(&item.children, ids);
            }
        }
    }
    let mut ids = Vec::new();
    walk(items, &mut ids);
    ids
}

/// Locates a leaf sub-step by identifier, recursing into containers.
fn find_leaf_step<'a>(items: &'a [PlanItem], step_id: &str) -> Option<&'a PlanItem> {
    fn walk<'a>(items: &'a [PlanItem], step_id: &str) -> Option<&'a PlanItem> {
        for item in items {
            if item.children.is_empty() {
                if item.id.eq_ignore_ascii_case(step_id) {
                    return Some(item);
                }
            } else if let Some(found) = walk(&item.children, step_id) {
                return Some(found);
            }
        }
        None
    }
    walk(items, step_id)
}

/// Reports whether any leaf sub-step remains executable (not `Done`/`Cancelled`).
fn has_uncompleted_leaves(items: &[PlanItem]) -> bool {
    items.iter().any(|item| {
        if item.children.is_empty() {
            item.status != PlanItemStatus::Done && item.status != PlanItemStatus::Cancelled
        } else {
            has_uncompleted_leaves(&item.children)
        }
    })
}

/// Returns the declared dependencies of a sub-step that are not yet `Done`.
fn unmet_substep_dependencies(subplan: &[PlanItem], step: &PlanItem) -> Vec<String> {
    step.depends_on
        .iter()
        .filter(|dep| {
            find_leaf_step(subplan, dep)
                .map(|d| d.status != PlanItemStatus::Done && d.status != PlanItemStatus::Cancelled)
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

/// Recursively assigns a status to the sub-step with the given identifier.
fn mark_substep_status(items: &mut [PlanItem], step_id: &str, status: PlanItemStatus) {
    for item in items.iter_mut() {
        if item.id.eq_ignore_ascii_case(step_id) {
            item.status = status;
            return;
        }
        mark_substep_status(&mut item.children, step_id, status);
    }
}

/// Returns an `InProgress` sub-step to `Todo` after an interrupted attempt.
fn revert_substep_in_progress(items: &mut [PlanItem], step_id: &str) {
    for item in items.iter_mut() {
        if item.id.eq_ignore_ascii_case(step_id) {
            if item.status == PlanItemStatus::InProgress {
                item.status = PlanItemStatus::Todo;
            }
            return;
        }
        revert_substep_in_progress(&mut item.children, step_id);
    }
}

/// Counts actionable and completed leaves as `(total, done)`.
///
/// `Cancelled` leaves are excluded from the total, mirroring how a plan's
/// progress metric drops terminal, non-actionable outcomes.
fn subplan_progress(items: &[PlanItem]) -> (usize, usize) {
    fn walk(items: &[PlanItem], total: &mut usize, done: &mut usize) {
        for item in items {
            if item.children.is_empty() {
                if item.status != PlanItemStatus::Cancelled {
                    *total += 1;
                    if item.status == PlanItemStatus::Done {
                        *done += 1;
                    }
                }
            } else {
                walk(&item.children, total, done);
            }
        }
    }
    let mut total = 0;
    let mut done = 0;
    walk(items, &mut total, &mut done);
    (total, done)
}

/// Appends the developer directives section to an execution prompt when present.
fn append_developer_directives(prompt: &mut String, instructions: Option<&str>) {
    if let Some(dirs) = instructions.map(str::trim).filter(|d| !d.is_empty()) {
        prompt.push_str("\n## Developer Directives:\n");
        prompt.push_str(dirs);
        prompt.push('\n');
    }
}

/// Recomputes derived container statuses and persists the subplan atomically.
async fn persist_subplan(
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    subplan: &mut Vec<PlanItem>,
) -> Result<(), String> {
    let mut plan = Plan {
        items: subplan.clone(),
        ..Default::default()
    };
    plan.refresh_parent_statuses();
    *subplan = plan.items;

    let snapshot = subplan.clone();
    let save_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    offload_review(move || {
        ReviewStorage::update_session_item(&save_root, &review_id_owned, item_id, move |it| {
            it.plan = snapshot.clone();
        })
    })
    .await?;
    Ok(())
}

/// Records an interrupted subplan.
///
/// Completed sub-steps keep their `Done` status, the stop reason is preserved in
/// `resolution_notes`, and the finding becomes `Skipped` so the developer can
/// inspect the cause and decide how to proceed without losing finished work.
async fn finish_subplan_interrupted(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    subplan: Vec<PlanItem>,
    reason: &str,
) {
    let mut plan = Plan {
        items: subplan,
        ..Default::default()
    };
    plan.refresh_parent_statuses();
    let subplan = plan.items;
    let notes = format!("Subplan interrupted before completion: {}", reason);

    let save_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    let _ = offload_review(move || {
        ReviewStorage::update_session_item(&save_root, &review_id_owned, item_id, move |it| {
            it.status = IssueStatus::Skipped;
            it.plan = subplan.clone();
            it.note = Some(notes.clone());
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
    let mut subplan = item.subplan.clone();

    // Determine which leaf sub-steps this call executes.
    let run_ids: Vec<String> = match step_id {
        Some(target) => match find_leaf_step(&subplan, target) {
            Some(step) if step.children.is_empty() => {
                if step.status == PlanItemStatus::Done {
                    Vec::new()
                } else {
                    let unmet = unmet_substep_dependencies(&subplan, step);
                    if !unmet.is_empty() {
                        set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo)
                            .await;
                        return Err(ReviewStepFailure::infrastructure(format!(
                            "Sub-step '{}' depends on unfinished sub-step(s): {}",
                            step.id,
                            unmet.join(", ")
                        )));
                    }
                    vec![step.id.clone()]
                }
            }
            _ => {
                set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo).await;
                return Err(ReviewStepFailure::infrastructure(format!(
                    "Sub-step '{}' not found in review item #{}",
                    target, item.id
                )));
            }
        },
        None => collect_leaf_step_ids(&subplan),
    };

    for current_step_id in &run_ids {
        let step_snapshot = match find_leaf_step(&subplan, current_step_id) {
            Some(s) => s.clone(),
            None => continue,
        };
        if step_snapshot.status == PlanItemStatus::Done {
            continue;
        }

        preload_step_files(state, repo_root, &step_snapshot).await;

        // #c-2: mark the active sub-step InProgress and persist it atomically.
        mark_substep_status(&mut subplan, current_step_id, PlanItemStatus::InProgress);
        persist_subplan(repo_root, review_id, item_id, &mut subplan)
            .await
            .map_err(ReviewStepFailure::infrastructure)?;
        publish_review(state, repo_root, review_id).await;

        let plan = Plan {
            id: session.id.clone(),
            title: session.title.clone(),
            items: subplan.clone(),
            ..Default::default()
        };
        let step = match plan.find_item(current_step_id) {
            Some(s) => s.clone(),
            None => continue,
        };

        let mut prompt = match tauqe_core::plan::prompt::format_plan_step_execution_prompt(
            &plan,
            current_step_id,
            &marker_suffix,
        ) {
            Ok(p) => p,
            Err(err) => {
                revert_substep_in_progress(&mut subplan, current_step_id);
                finish_subplan_interrupted(state, repo_root, review_id, item_id, subplan, &err)
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
                    custom_selection: Some(selection.clone()),
                    plan_context: None,
                    reasoning_effort,
                    marker: marker.clone(),
                    shared_operation: Some(Arc::clone(&operation)),
                },
            )
            .await;

            let turn = match turn_result {
                Ok(turn) => turn,
                Err(err) => break Err(ReviewStepFailure::infrastructure(err.message)),
            };

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
                mark_substep_status(&mut subplan, current_step_id, PlanItemStatus::Done);
                persist_subplan(repo_root, review_id, item_id, &mut subplan)
                    .await
                    .map_err(ReviewStepFailure::infrastructure)?;
                publish_review(state, repo_root, review_id).await;
            }
            Err(failure) => {
                // Revert the interrupted sub-step before recording the stop.
                revert_substep_in_progress(&mut subplan, current_step_id);
                let reason = failure.reason().to_string();
                let op_id = format!("op-{}", next_operation_id());
                report_item_failure(state, &op_id, &reason);
                finish_subplan_interrupted(state, repo_root, review_id, item_id, subplan, &reason)
                    .await;
                return Err(failure);
            }
        }
    }
    clear_auto_context(state).await;

    // #c-2: a finding is Fixed only when every remaining leaf sub-step is Done.
    let (total, done) = subplan_progress(&subplan);
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
                    it.note = Some(notes.clone());
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
