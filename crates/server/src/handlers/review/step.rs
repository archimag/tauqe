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

use std::path::Path;
use std::sync::Arc;

use tauqe_core::review::prompt::ReviewStepFailure;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    IssueStatus, ModelRole, ModelSelection, Plan, PlanItem, PlanItemStatus, ReviewItem,
    ReviewSession,
};

use crate::handlers::model::{execute_model_turn, StepTurnOptions};
use crate::handlers::plan::preload_step_files;
use crate::state::{next_operation_id, AppState, OperationGuard};

use super::common::{
    clear_auto_context, offload_review, preload_item_files, publish_review, report_item_failure,
    set_item_status, skip_finding_with_diagnostic, turn_applied_edits,
};

/// Executes one finding in an isolated turn.
///
/// The finding becomes `Fixed` only when the turn satisfies the success
/// contract; on any failure it returns to `Todo`. The transient auto context
/// layer is always cleared once the step finishes, so a retried attempt starts
/// from a clean, deterministic basis.
pub(super) async fn run_review_step(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    let res = run_review_step_inner(state, repo_root, review_id, item_id, operation).await;
    clear_auto_context(state).await;
    res
}

async fn run_review_step_inner(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    let load_root = repo_root.to_path_buf();
    let load_id = review_id.to_string();
    let session = match offload_review(move || ReviewStorage::load_review(&load_root, &load_id)).await {
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

    match item.status {
        IssueStatus::Fixed | IssueStatus::Skipped => return Ok(()),
        // A finding still under discussion, or already rejected, cannot be
        // executed. This is a configuration precondition rather than a code
        // failure, so it stays fatal instead of overwriting a terminal status
        // with `Skipped`.
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
        // A finding awaiting architectural decomposition is a precondition,
        // not a code failure: it must be decomposed into steps (R) before
        // the Coder is allowed to execute it.
        IssueStatus::Refine => {
            return Err(ReviewStepFailure::infrastructure(format!(
                "Review item #{} is in REFINE status and must be decomposed into steps by the Architect before execution",
                item.id
            )));
        }
        IssueStatus::Todo | IssueStatus::InProgress => {}
    }

    // A finding decomposed into an embedded subplan executes its sub-steps one by
    // one instead of a single monolithic turn.
    if item.has_subplan() {
        return run_review_subplan(state, repo_root, review_id, &session, &item, operation).await;
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

    let (max_retries, auto_level_up, models_config) = {
        let cfg = state.config.lock().await.config.clone();
        (
            cfg.develop.max_retries,
            cfg.models.auto_level_up,
            cfg.models.clone(),
        )
    };

    // A finding with an explicitly assigned model keeps that model across every
    // retry; an unassigned finding starts on the Coder role and may escalate
    // through the coder ladder when `auto_level_up` is enabled.
    let explicit_selection = item.model.clone();
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
            if let Some(skip_reason) = tauqe_core::review::prompt::extract_review_skip_reason(
                &turn.assistant_text,
                marker.as_deref(),
            ) {
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
/// A completed sub-step is persisted as `Done`, so finished work survives any
/// later interruption. When a sub-step cannot apply, blocks, or the turn is
/// cancelled, the finding is marked `Skipped` with the stop reason recorded in
/// `resolution_notes`; when every sub-step applies cleanly, the finding becomes
/// `Fixed`.
async fn run_review_subplan(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    session: &ReviewSession,
    item: &ReviewItem,
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

    let explicit_selection = item.model.clone();
    let mut subplan = item.subplan.clone();
    let leaf_ids = collect_leaf_step_ids(&subplan);

    for step_id in &leaf_ids {
        let plan = Plan {
            id: session.id.clone(),
            title: session.title.clone(),
            items: subplan.clone(),
            ..Default::default()
        };

        let step = match plan.find_item(step_id) {
            Some(s) => s.clone(),
            None => continue,
        };
        if step.status == PlanItemStatus::Done {
            continue;
        }

        preload_step_files(state, repo_root, &step).await;

        let mut prompt = match tauqe_core::plan::prompt::format_plan_step_execution_prompt(
            &plan,
            step_id,
            &marker_suffix,
        ) {
            Ok(p) => p,
            Err(err) => {
                finish_subplan_interrupted(state, repo_root, review_id, item_id, subplan, &err).await;
                return Err(ReviewStepFailure::infrastructure(err));
            }
        };

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
                    step_id = step_id,
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
                mark_substep_status(&mut subplan, step_id, PlanItemStatus::Done);
                persist_subplan(repo_root, review_id, item_id, &mut subplan)
                    .await
                    .map_err(ReviewStepFailure::infrastructure)?;
                publish_review(state, repo_root, review_id).await;
            }
            Err(failure) => {
                let reason = failure.reason().to_string();
                let op_id = format!("op-{}", next_operation_id());
                report_item_failure(state, &op_id, &reason);
                finish_subplan_interrupted(state, repo_root, review_id, item_id, subplan, &reason).await;
                return Err(failure);
            }
        }
    }
    clear_auto_context(state).await;

    let notes = format!(
        "Subplan completed: all {} sub-step(s) applied successfully",
        leaf_ids.len()
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
    Ok(())
}
