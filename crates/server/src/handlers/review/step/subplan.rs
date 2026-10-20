//! Ordered walk over the embedded subplan of a decomposed review finding.
//!
//! Each leaf sub-step runs in its own bounded turn with its target files
//! preloaded, its `InProgress` state is persisted before the turn starts, and a
//! completed sub-step is persisted as `Done`, so finished work survives any
//! later interruption. The helper functions here also own the subplan-scope
//! diagnostics and the persisted-subplan replacement, keeping every status
//! transition inside `ReviewStorage` and `Plan::update_item_status`.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::providers::{format_session_id, SESSION_PREFIX_REVIEW};
use tauqe_core::review::prompt::ReviewStepFailure;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    IssueStatus, ModelRole, ModelSelection, Plan, PlanItemStatus, PlanScopeEvaluation, ReviewItem,
    ReviewSession, TurnIntent,
};

use crate::handlers::model::{execute_model_turn, StepTurnOptions};
use crate::handlers::plan::preload_step_files;
use crate::state::{next_operation_id, AppState, OperationGuard};

use super::super::common::{
    append_model_comment, clear_auto_context, offload_review, publish_review, report_item_failure,
    set_item_status, turn_applied_edits,
};

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
pub(super) fn append_developer_directives(prompt: &mut String, instructions: Option<&str>) {
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
///
/// Shared with the replanning loop, which clears the superseded note once the
/// Architect reprojects the remaining horizon.
pub(super) const SUBPLAN_INTERRUPTED_NOTE_PREFIX: &str = "Subplan interrupted before completion";

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
pub(super) async fn run_review_subplan(
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
    set_item_status(
        state,
        repo_root,
        review_id,
        item_id,
        IssueStatus::InProgress,
    )
    .await;

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
        Some(target) if plan.is_leaf_item(target) == Some(true) => {
            plan.evaluate_scope(Some(target))
        }
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
