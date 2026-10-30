//! Execution of the unified remediation plan of an architectural review group.
//!
//! An architectural group collects findings that manifest one shared root cause,
//! so it is executed exclusively through its unified `subplan`: its symptoms are
//! never addressed one finding at a time. This module owns that walk end to end —
//! the just-in-time Architect synthesis of a horizon the group does not own yet
//! (concept `c-2`), the ordered execution of every leaf sub-step, and the
//! group-wide finding lifecycle that publishes the outcome as one transition
//! (concept `c-3`).
//!
//! Three invariants govern the walk:
//! - The unit of completion is the whole group plan: the findings of the group
//!   become `Fixed` only once no leaf sub-step is left unfinished.
//! - Completed sub-steps are immutable. A failed sub-step returns to `Todo`, the
//!   `Done` foundation stays untouched, and the remaining horizon is reprojected
//!   by the Architect up to `develop.max_replans` times.
//! - Every status transition is persisted through the group primitives of
//!   `ReviewStorage`, so the durable record and the batch outcome never diverge.
//!
//! The scope diagnostics of a plan walk live here as the single shared
//! implementation: a finding's embedded subplan and a group's unified plan are
//! both walks over a [`Plan`], so `step::subplan` reuses them instead of keeping
//! a second copy.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::providers::{format_session_id, SESSION_PREFIX_REVIEW};
use tauqe_core::review::prompt::{
    apply_group_architectural_context, ReviewGroupRefineDiagnostics, ReviewStepFailure,
};
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::model::{TurnWorkContext, TurnWorkItem, TurnWorkQueue};
use tauqe_protocol::{
    IssueStatus, ModelRole, ModelSelection, Plan, PlanItem, PlanItemStatus, PlanScopeEvaluation,
    ReviewGroup, ReviewIssue, ReviewItem, ReviewSession, TurnIntent,
};

use crate::handlers::common::append_developer_directives;
use crate::handlers::model::{build_apply_policy, execute_model_turn, StepTurnOptions};
use crate::handlers::plan::preload_step_files;
use crate::state::{AppState, OperationGuard};

use super::common::{
    append_model_comment, clear_auto_context, offload_review, publish_review, report_item_failure,
    turn_applied_edits,
};
use super::refine::{group_findings, refine_group_with_architect, GroupRefineOutcome};

/// Separator binding the session identifier to the group identifier of a
/// group-plan batch member.
const GROUP_STEP_SEPARATOR: &str = ":g:";

/// Prefix of the note recorded when a group plan stops before completing.
///
/// The prefix is what a resumed walk recognizes as a superseded horizon, so the
/// interruption diagnostic is cleared once the Architect reprojects the plan.
pub(super) const GROUP_PLAN_INTERRUPTED_NOTE_PREFIX: &str =
    "Group root-cause plan interrupted before completion";

/// Encodes the batch-member identifier of one architectural group plan.
pub(super) fn group_step_id(session_id: &str, group_id: &str) -> String {
    format!("{session_id}{GROUP_STEP_SEPARATOR}{group_id}")
}

/// Decodes a batch-member identifier into the session and the group it names.
///
/// A plain finding target carries no separator and resolves to `None`, so a
/// supervisor routes a queue member without knowing how it was minted. Neither
/// identifier may contain the separator: both are minted by the harness.
pub(super) fn parse_group_step_id(step_id: &str) -> Option<(&str, &str)> {
    let (session_id, group_id) = step_id.split_once(GROUP_STEP_SEPARATOR)?;
    if session_id.is_empty() || group_id.is_empty() {
        return None;
    }
    Some((session_id, group_id))
}

/// Reloads the review session and the target group from disk.
///
/// Execution and replanning both need the persisted state of the group: the
/// horizon mutated by a failed sub-step, the lifecycle status written by an
/// interruption, and the plan synthesized by the Architect.
pub(super) async fn reload_group(
    repo_root: &Path,
    review_id: &str,
    group_id: &str,
) -> Result<(ReviewSession, ReviewGroup), ReviewStepFailure> {
    let load_root = repo_root.to_path_buf();
    let load_id = review_id.to_string();
    let session =
        match offload_review(move || ReviewStorage::load_review(&load_root, &load_id)).await {
            Ok(Some(session)) => session,
            Ok(None) => {
                return Err(ReviewStepFailure::infrastructure(format!(
                    "Review session '{}' not found",
                    review_id
                )));
            }
            Err(err) => return Err(ReviewStepFailure::infrastructure(err)),
        };
    let group = match session.groups.iter().find(|group| group.id == group_id) {
        Some(group) => group.clone(),
        None => {
            return Err(ReviewStepFailure::infrastructure(format!(
                "Review group '{}' not found in session '{}'",
                group_id, review_id
            )));
        }
    };
    Ok((session, group))
}

/// Renders why a plan scope cannot be executed right now.
///
/// The shared scope evaluator is the single source of truth for readiness, so its
/// stall variants are translated into a developer-facing diagnostic here instead
/// of being re-derived from the plan tree.
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

/// Finds the first leaf sub-step whose dependencies can never be satisfied.
///
/// Every leaf is asked through the shared scope evaluator, so readiness is never
/// re-derived here: the identifier named for replanning is exactly the sub-step
/// the evaluator reports as blocked by unmet dependencies.
fn dependency_stalled_step(plan: &Plan) -> Option<String> {
    fn walk(plan: &Plan, items: &[PlanItem]) -> Option<String> {
        for item in items {
            if item.is_leaf() {
                if matches!(
                    plan.evaluate_scope(Some(item.id.as_str())),
                    PlanScopeEvaluation::BlockedByDependencies(_)
                ) {
                    return Some(item.id.clone());
                }
            } else if let Some(found) = walk(plan, &item.children) {
                return Some(found);
            }
        }
        None
    }
    walk(plan, &plan.items)
}

/// Classifies a stalled plan scope for the batch supervisor.
///
/// A scope stalls on dependencies when its leaf sub-steps wait for work that will
/// never become `Done`: the plan structure itself is wrong, so the horizon has to
/// be reprojected around the stalled dependencies. That outcome is therefore a
/// domain failure attributed to the stalled sub-step, and the supervisor replans
/// the remaining horizon instead of aborting the batch. Every other stall is a
/// lifecycle or structural precondition the harness cannot repair by replanning.
///
/// Both the embedded subplan of a review finding and the unified plan of an
/// architectural group are walks over a `Plan`, so the classification is shared.
pub(super) fn classify_scope_stall(
    plan: &Plan,
    step_id: Option<&str>,
    stall: &PlanScopeEvaluation,
) -> ReviewStepFailure {
    let reason = format_scope_stall(step_id, stall);
    match stall {
        PlanScopeEvaluation::BlockedByDependencies(_) => {
            let failure = ReviewStepFailure::domain(reason);
            match dependency_stalled_step(plan) {
                Some(step) => failure.with_step(step),
                None => failure,
            }
        }
        _ => ReviewStepFailure::infrastructure(reason),
    }
}

/// Renders the developer-facing label of a group: its title, or its identifier
/// when the title was never stated.
fn group_label(group: &ReviewGroup) -> &str {
    let title = group.title.trim();
    if title.is_empty() {
        group.id.as_str()
    } else {
        title
    }
}

/// Collects the leaf sub-steps of a horizon in plan order.
///
/// A group plan executes its leaves, so the queue of a running sub-step lists
/// exactly the members the walk advances through and `[i/N]` addresses the leaf
/// that runs.
fn leaf_substeps(plan: &Plan, step_id: &str) -> TurnWorkQueue {
    fn collect<'a>(items: &'a [PlanItem], leaves: &mut Vec<&'a PlanItem>) {
        for item in items {
            if item.is_leaf() {
                leaves.push(item);
            } else {
                collect(&item.children, leaves);
            }
        }
    }

    let mut leaves = Vec::new();
    collect(&plan.items, &mut leaves);
    let active_index = leaves
        .iter()
        .position(|item| item.id == step_id)
        .unwrap_or_default();
    TurnWorkQueue {
        items: leaves
            .into_iter()
            .map(|item| TurnWorkItem {
                id: item.id.clone(),
                title: item.title.clone(),
            })
            .collect(),
        active_index,
    }
}

/// Builds the declared work context of one sub-step of a group plan.
///
/// The running sub-step is addressed inside the group it remediates, so the
/// client renders the group root cause, the sub-step queue, and the position of
/// the executing leaf from typed fields.
fn group_step_work_context(
    session: &ReviewSession,
    group: &ReviewGroup,
    plan: &Plan,
    step_id: &str,
) -> TurnWorkContext {
    TurnWorkContext::ReviewGroupStep {
        review_id: session.id.clone(),
        review_title: session.title.clone(),
        group_id: group.id.clone(),
        group_title: group_label(group).to_string(),
        queue: leaf_substeps(plan, step_id),
    }
}
/// Renders the resolution note recorded on every finding once the group plan
/// completed.
fn group_completion_note(group: &ReviewGroup, steps: usize) -> String {
    let label = group_label(group);
    format!(
        "Group root-cause plan '{}' completed: all {} step(s) applied successfully",
        label, steps
    )
}

/// Renders the resolution note recorded on every finding when a group plan stops
/// before completing.
///
/// The diagnostic opens with [`GROUP_PLAN_INTERRUPTED_NOTE_PREFIX`], which is what
/// a resumed walk recognizes as a superseded horizon, and keeps the model's final
/// comment beside the reason it stopped.
fn group_interruption_note(reason: &str, assistant_text: &str) -> String {
    append_model_comment(
        format!("{}: {}", GROUP_PLAN_INTERRUPTED_NOTE_PREFIX, reason),
        assistant_text,
    )
}

/// Applies one finding-level update to every member of the group atomically.
///
/// A group plan is one coherent change set, so its findings move through their
/// statuses together: the primitive loads the session once, updates every member,
/// and saves once, so the transition is published as a single edit of the stored
/// session.
///
/// Returns `Err` when the write does not land on disk — an unknown group or a
/// storage failure — so a terminal transition never diverges from the durable
/// record. The failure is journaled here; the caller reports the client-facing
/// error under the operation it already owns.
async fn update_group_findings<F>(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    group_id: &str,
    what: &str,
    updater: F,
) -> Result<(), String>
where
    F: FnMut(&mut ReviewIssue) + Send + 'static,
{
    let save_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    let group_id_owned = group_id.to_string();
    let updated = offload_review(move || {
        ReviewStorage::update_session_group_findings(
            &save_root,
            &review_id_owned,
            &group_id_owned,
            updater,
        )
    })
    .await;

    match updated {
        Ok(Some(_)) => {
            publish_review(state, repo_root, review_id).await;
            Ok(())
        }
        Ok(None) => {
            let message = format!(
                "Review group '{}' of review '{}' was not found; the {} was not persisted",
                group_id, review_id, what
            );
            tracing::warn!(review_id = review_id, group_id = group_id, "{}", message);
            Err(message)
        }
        Err(err) => {
            tracing::warn!(
                review_id = review_id,
                group_id = group_id,
                error = %err,
                "the review group finding update was not persisted"
            );
            Err(err)
        }
    }
}

/// Marks every finding of the group `InProgress` before its plan starts.
async fn begin_group_plan(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    group: &ReviewGroup,
) -> Result<(), String> {
    update_group_findings(
        state,
        repo_root,
        review_id,
        &group.id,
        "'InProgress' status update",
        |it| {
            it.status = IssueStatus::InProgress;
        },
    )
    .await
}

/// Marks every finding of the group `Fixed` once its whole plan verified.
///
async fn complete_group_plan(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    group: &ReviewGroup,
    steps: usize,
) -> Result<(), String> {
    let note = group_completion_note(group, steps);
    update_group_findings(
        state,
        repo_root,
        review_id,
        &group.id,
        "'Fixed' status update",
        move |it| {
            it.status = IssueStatus::Fixed;
            it.note = Some(note.clone());
        },
    )
    .await
}

/// Records an interrupted group plan.
///
/// The stop reason and the model's final comment are preserved in the resolution
/// notes and every finding returns to `Todo`, so the remaining work stays
/// executable and the developer reads why the root cause was not removed.
///
/// Returns `Err` when the interruption cannot be persisted, so the caller reports
/// an infrastructure failure instead of letting the batch advance past findings
/// the durable record still shows as executing.
async fn interrupt_group_plan(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    group: &ReviewGroup,
    assistant_text: &str,
    reason: &str,
) -> Result<(), String> {
    let notes = group_interruption_note(reason, assistant_text);
    update_group_findings(
        state,
        repo_root,
        review_id,
        &group.id,
        "interruption record",
        move |it| {
            it.status = IssueStatus::Todo;
            it.note = Some(notes.clone());
        },
    )
    .await
}

/// Resumes every finding of the group after the Architect reprojected its horizon.
///
/// The interrupted walk left an interruption note that describes a superseded
/// horizon, so the note is cleared here and the findings return to `Todo` before
/// the retry. A successful retry then records its own outcome note instead of
/// inheriting the diagnostic of an approach that no longer exists.
async fn resume_group_after_replan(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    group: &ReviewGroup,
) {
    let _ = update_group_findings(
        state,
        repo_root,
        review_id,
        &group.id,
        "'Todo' status",
        |it| {
            it.status = IssueStatus::Todo;
            let superseded = it
                .note
                .as_deref()
                .is_some_and(|note| note.starts_with(GROUP_PLAN_INTERRUPTED_NOTE_PREFIX));
            if superseded {
                it.note = None;
            }
        },
    )
    .await;
}

/// Persists the working group plan atomically.
///
/// Container statuses are derived, so they are recomputed before the write and
/// the replacement itself is delegated to `ReviewStorage::update_group_subplan`,
/// the single storage entry point that normalizes and stores a group horizon.
async fn persist_group_subplan(
    repo_root: &Path,
    review_id: &str,
    group_id: &str,
    plan: &mut Plan,
) -> Result<(), String> {
    plan.refresh_parent_statuses();
    let subplan = plan.items.clone();
    let save_root = repo_root.to_path_buf();
    let review_id_owned = review_id.to_string();
    let group_id_owned = group_id.to_string();
    let saved = offload_review(move || {
        ReviewStorage::update_group_subplan(&save_root, &review_id_owned, &group_id_owned, subplan)
    })
    .await?;
    match saved {
        Some(_) => Ok(()),
        None => Err(format!(
            "Review group '{}' not found in review '{}'; the group plan was not persisted",
            group_id, review_id
        )),
    }
}

/// Carries a group-wide status write that could not be persisted into the failure
/// the caller reports.
///
/// A write that never landed leaves the durable session in its previous state, so
/// the outcome is an infrastructure failure instead of the domain verdict the
/// stored record does not hold.
fn with_status_write(failure: ReviewStepFailure, write: Result<(), String>) -> ReviewStepFailure {
    match write {
        Ok(()) => failure,
        Err(storage_err) => ReviewStepFailure::infrastructure(format!(
            "{}; additionally, the review group status was not persisted: {storage_err}",
            failure.reason()
        )),
    }
}

/// Narrows the findings of a group to the manifestations its horizon still owes.
///
/// The architectural context of a sub-step presents only the findings that
/// still require remediation: a finding already resolved, skipped, or
/// rejected names work the walk must not redo, and showing it to the Coder
/// invites duplicate fixes. The narrowing is local to the step context;
/// `group_findings` keeps resolving the complete membership for every other
/// consumer.
///
/// The statuses are read from the session snapshot as it was loaded, before
/// `begin_group_plan` marks the group `InProgress`: at that point `Todo` is
/// exactly the set of manifestations no walk has started to execute.
fn pending_group_findings(session: &ReviewSession, group_id: &str) -> Vec<ReviewItem> {
    group_findings(session, group_id)
        .into_iter()
        .filter(|finding| matches!(finding.status, IssueStatus::Todo))
        .collect()
}

/// Executes the unified plan of one architectural group.
///
/// A group that owns no horizon yet is decomposed by the Architect just in time
/// (#c-2), so execution never starts from an undecomposed architectural group.
/// The walk then marks the group `InProgress`, executes the leaf sub-steps in
/// dependency order, and marks every finding `Fixed` once no leaf is left
/// unfinished (#c-3). A step that cannot be applied returns to `Todo`, keeps the
/// completed `Done` foundation intact, and is reported together with the sub-step
/// it belongs to, so the caller can replan the remaining horizon.
///
/// `target_step` executes a single leaf of the horizon; the whole remaining
/// horizon is walked otherwise. The group completes only when the plan is
/// complete, so a targeted run of one sub-step leaves the findings executing.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_group_plan(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    session: &ReviewSession,
    group: &ReviewGroup,
    target_step: Option<&str>,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    // A group plan keeps a single warm provider prefix cache across its sub-steps
    // and their retries, keyed by the audit it belongs to.
    let session_id = Some(format_session_id(SESSION_PREFIX_REVIEW, review_id));
    let mut session = session.clone();
    let mut group = group.clone();

    // #c-2: an architectural group is executed exclusively through its unified
    // plan, so a group that owns no horizon is decomposed by the Architect just
    // in time, before the Coder is asked for anything.
    if group.subplan.is_empty() {
        match refine_group_with_architect(
            state, repo_root, &session, &group, None, None, None, &operation,
            // The synthesis runs immediately before this group's steps inside the
            // running batch, so it never settles the shared turn itself.
            true,
        )
        .await
        {
            Ok(GroupRefineOutcome::Success { explanation }) => {
                tracing::info!(
                    review_id = review_id,
                    group_id = %group.id,
                    explanation = %explanation,
                    "review group plan synthesized just in time"
                );
                // The synthesized horizon lives on the group, so it is read back
                // from disk instead of trusting an in-memory copy.
                let reloaded = reload_group(repo_root, review_id, &group.id).await?;
                session = reloaded.0;
                group = reloaded.1;
            }
            Ok(GroupRefineOutcome::Blocked { reason }) => {
                tracing::warn!(
                    review_id = review_id,
                    group_id = %group.id,
                    reason = %reason,
                    "review group was not decomposed; its plan is not executed"
                );
                let interrupted =
                    interrupt_group_plan(state, repo_root, review_id, &group, "", &reason).await;
                return Err(with_status_write(
                    ReviewStepFailure::domain(reason),
                    interrupted,
                ));
            }
            Err(err) => {
                let failure = ReviewStepFailure::infrastructure(err.message);
                let interrupted =
                    interrupt_group_plan(state, repo_root, review_id, &group, "", failure.reason())
                        .await;
                return Err(with_status_write(failure, interrupted));
            }
        }
    }

    // #c-3: the whole group enters `InProgress` before its first step runs, so the
    // interface reports the group as executing instead of one finding.
    if let Err(err) = begin_group_plan(state, repo_root, review_id, &group).await {
        report_item_failure(state, &format!("op-{}", operation.id()), &err);
        return Err(ReviewStepFailure::infrastructure(err));
    }

    // The group horizon is the working state of this walk: every status
    // transition goes through `Plan::update_item_status`, so container statuses
    // stay derived and readiness follows the shared scope evaluator.
    // The architectural context inherits only the manifestations the
    // horizon still owes; findings already resolved or rejected must not
    // invite duplicate work during the walk.
    let findings = pending_group_findings(&session, &group.id);
    let mut plan = Plan {
        id: session.id.clone(),
        title: session.title.clone(),
        items: group.subplan.clone(),
        ..Default::default()
    };
    // The plan must inherit the architectural decision of its group, so the
    // sub-step prompt renders the shared root cause and its manifestations.
    apply_group_architectural_context(&mut plan, &group, &findings);

    let marker = state.next_turn_marker().await;
    let protocol = state.config.lock().await.config.develop.protocol;

    let (max_retries, auto_level_up, models_config) = {
        let cfg = state.config.lock().await.config.clone();
        (
            cfg.develop.max_retries,
            cfg.models.auto_level_up,
            cfg.models.clone(),
        )
    };

    let explicit_selection = model;

    // Resolve the leaf sub-steps this call executes through the shared scope
    // evaluator, so dependency ordering and terminal states are derived exactly
    // as they are for a plan.
    let evaluation = match target_step {
        Some(target) if plan.is_leaf_item(target) == Some(true) => {
            plan.evaluate_scope(Some(target))
        }
        Some(target) => {
            let failure = ReviewStepFailure::infrastructure(format!(
                "Sub-step '{}' not found in the plan of review group '{}'",
                target, group.id
            ));
            let interrupted =
                interrupt_group_plan(state, repo_root, review_id, &group, "", failure.reason())
                    .await;
            return Err(with_status_write(failure, interrupted));
        }
        None => plan.evaluate_scope(None),
    };

    let run_ids: Vec<String> = match evaluation {
        PlanScopeEvaluation::Ready(steps) => steps.into_iter().map(|step| step.id).collect(),
        // Nothing actionable is left: every leaf is already `Done`, so the
        // terminal check below decides whether the group completed.
        PlanScopeEvaluation::AllCompleted => Vec::new(),
        stall => {
            let failure = classify_scope_stall(&plan, target_step, &stall);
            let interrupted =
                interrupt_group_plan(state, repo_root, review_id, &group, "", failure.reason())
                    .await;
            return Err(with_status_write(failure, interrupted));
        }
    };

    for current_step_id in &run_ids {
        let step_snapshot = match plan.find_item(current_step_id) {
            Some(step) if step.status != PlanItemStatus::Done => step.clone(),
            _ => continue,
        };

        preload_step_files(state, repo_root, &step_snapshot).await;

        // The active sub-step is persisted as `InProgress` before its turn starts,
        // so any interruption leaves an honest record of the step that ran.
        if !plan.update_item_status(current_step_id, PlanItemStatus::InProgress) {
            continue;
        }
        persist_group_subplan(repo_root, review_id, &group.id, &mut plan)
            .await
            .map_err(ReviewStepFailure::infrastructure)?;
        publish_review(state, repo_root, review_id).await;

        let step = match plan.find_item(current_step_id) {
            Some(step) => step.clone(),
            None => continue,
        };

        let mut prompt = match tauqe_core::plan::prompt::format_plan_step_execution_prompt(
            &plan,
            current_step_id,
            protocol,
            marker.as_deref(),
        ) {
            Ok(prompt) => prompt,
            Err(err) => {
                // The horizon itself is defective, so the step returns to `Todo`
                // and the reverted plan is persisted before the failure surfaces.
                plan.update_item_status(current_step_id, PlanItemStatus::Todo);
                let persisted =
                    persist_group_subplan(repo_root, review_id, &group.id, &mut plan).await;
                let reason = match persisted {
                    Ok(()) => err,
                    Err(storage_err) => format!(
                        "{err}; additionally, the reverted group plan was not persisted: {storage_err}"
                    ),
                };
                // The interruption is recorded on the group as well, so the
                // durable session explains why the horizon could not run.
                let interrupted =
                    interrupt_group_plan(state, repo_root, review_id, &group, "", &reason).await;
                return Err(with_status_write(
                    ReviewStepFailure::infrastructure(reason),
                    interrupted,
                ));
            }
        };
        append_developer_directives(&mut prompt, instructions.as_deref());

        // Declare the work this sub-step executes: the running leaf of the group
        // horizon and its position among the leaves the walk advances through.
        let work_context = group_step_work_context(&session, &group, &plan, current_step_id);
        let mut selection = explicit_selection
            .clone()
            .unwrap_or(ModelSelection::Role(ModelRole::Coder));
        let mut ladder_idx = models_config.coder_tier_index(Some(&selection));
        let mut reasoning_effort =
            models_config.resolve_selection_effort(ModelRole::Coder, &selection);
        let mut attempt = 0usize;

        let step_result: Result<(), ReviewStepFailure> = loop {
            let workflow = state.config.lock().await.config.develop.workflow;
            let policy = build_apply_policy(state, workflow);

            let turn_result = execute_model_turn(
                state,
                prompt.clone(),
                policy,
                StepTurnOptions {
                    intent: TurnIntent::IsolatedStep,
                    custom_selection: Some(selection.clone()),
                    plan_context: None,
                    work_context: Some(work_context.clone()),
                    reasoning_effort,
                    marker: marker.clone(),
                    shared_operation: Some(Arc::clone(&operation)),
                    session_id: session_id.clone(),
                    // A group sub-step streams the turn's live phases into the
                    // client channel, so verifying one step stays visible.
                    phase_out: Some(state.out.clone()),
                },
            )
            .await;

            let turn = match turn_result {
                Ok(turn) => turn,
                Err(err) => break Err(ReviewStepFailure::infrastructure(err.error.message)),
            };

            // A semantic block is attributed to the sub-step that could not be
            // implemented within the architectural decision of the group.
            if let Some(reason) = turn.blocked.clone() {
                break Err(ReviewStepFailure::domain(reason));
            }

            let applied = turn_applied_edits(&turn);

            if turn.is_success() && applied {
                break Ok(());
            }

            if turn.is_cancelled {
                break Err(ReviewStepFailure::infrastructure(
                    "Group plan execution was cancelled".to_string(),
                ));
            }

            if turn.result.is_none() {
                let reason = turn
                    .error
                    .clone()
                    .unwrap_or_else(|| "Group plan sub-step produced no result".to_string());
                break Err(ReviewStepFailure::infrastructure(reason));
            }

            let reason = turn.error.clone().unwrap_or_else(|| {
                if applied {
                    "Group plan sub-step finished without verified changes or a success tag"
                        .to_string()
                } else {
                    "Group plan sub-step applied no code changes".to_string()
                }
            });

            if attempt >= max_retries {
                break Err(ReviewStepFailure::domain(reason));
            }

            // Reset to a clean, deterministic basis before the corrective attempt.
            clear_auto_context(state).await;
            preload_step_files(state, repo_root, &step).await;

            // A rung without a stronger successor is saturated, and a manual run on
            // a model the ladder excludes cannot escalate at all: the retry keeps the
            // current model instead of silently entering another one.
            let next_rung = if auto_level_up {
                models_config.next_coder_tier(ladder_idx)
            } else {
                None
            };
            if let Some((next_idx, tier)) = next_rung {
                tracing::warn!(
                    review_id = review_id,
                    group_id = %group.id,
                    step_id = current_step_id.as_str(),
                    from_tier = ladder_idx,
                    to_tier = next_idx,
                    to_model = %tier.model,
                    effort = ?tier.effort,
                    reason = %reason,
                    "review group sub-step coder escalation"
                );
                ladder_idx = next_idx;
                selection = ModelSelection::Specific(tier.model);
                reasoning_effort = tier.effort;
            }

            prompt = tauqe_core::plan::prompt::format_plan_step_retry_prompt(
                &turn.assistant_text,
                turn.error.as_deref(),
                protocol,
                marker.as_deref(),
            );
            attempt += 1;
        };

        clear_auto_context(state).await;

        match step_result {
            Ok(()) => {
                plan.update_item_status(current_step_id, PlanItemStatus::Done);
                persist_group_subplan(repo_root, review_id, &group.id, &mut plan)
                    .await
                    .map_err(ReviewStepFailure::infrastructure)?;
                publish_review(state, repo_root, review_id).await;
            }
            Err(failure) => {
                // The interrupted sub-step returns to `Todo` while the completed
                // foundation stays `Done`, so a replanning wave reprojects only the
                // remaining horizon. The reverted plan must reach disk before the
                // caller reads the group it replans from.
                plan.update_item_status(current_step_id, PlanItemStatus::Todo);
                let failure = failure.with_step(current_step_id.as_str());
                report_item_failure(state, &format!("op-{}", operation.id()), failure.reason());
                persist_group_subplan(repo_root, review_id, &group.id, &mut plan)
                    .await
                    .map_err(ReviewStepFailure::infrastructure)?;
                publish_review(state, repo_root, review_id).await;
                return Err(failure);
            }
        }
    }
    clear_auto_context(state).await;

    // #c-3: a finding is `Fixed` only when the whole group plan is complete, so
    // both terminal writes are part of the walk's success contract: a write that
    // cannot be persisted is reported as an infrastructure failure, so the batch
    // outcome never diverges from the durable record.
    let (total, done) = plan.count_stats();
    if total > 0 && done == total {
        let completed = complete_group_plan(state, repo_root, review_id, &group, total).await;
        if let Err(err) = completed {
            report_item_failure(state, &format!("op-{}", operation.id()), &err);
            return Err(ReviewStepFailure::infrastructure(err));
        }
    }
    Ok(())
}

/// Executes an architectural group's plan, reprojecting the remaining horizon
/// around a failed sub-step up to `max_replans` times.
///
/// [`run_group_plan`] confines a failure to the failed sub-step: the sub-step
/// returns to `Todo`, the completed foundation stays `Done`, and the findings keep
/// executing. When the failure is a domain failure attributable to a concrete
/// sub-step and the replan budget allows it, the Architect reprojects the
/// remaining horizon from the immutable `Done` foundation and the failure
/// diagnostics, and the walk resumes from the synthesized horizon. An exhausted
/// budget or a blocked Architect records the interruption and leaves the findings
/// in `Todo` with the diagnostic note, so the durable record states why the root
/// cause was not removed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_group_plan_with_replans(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    session: &ReviewSession,
    group: &ReviewGroup,
    step_id: Option<&str>,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    let max_replans = state.config.lock().await.config.develop.max_replans;
    let mut replans_count = 0usize;
    let mut session = session.clone();
    let mut group = group.clone();
    let mut target = step_id.map(str::to_string);

    loop {
        let failure = match run_group_plan(
            state,
            repo_root,
            review_id,
            &session,
            &group,
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
                group_id = %group.id,
                failed_step_id = failed_step.as_str(),
                max_replans = max_replans,
                "review group plan replanning limit reached"
            );
            let interrupted =
                interrupt_group_plan(state, repo_root, review_id, &group, "", failure.reason())
                    .await;
            return Err(with_status_write(failure, interrupted));
        }
        replans_count += 1;

        tracing::warn!(
            review_id = review_id,
            group_id = %group.id,
            failed_step_id = failed_step.as_str(),
            replan_round = replans_count,
            max_replans = max_replans,
            reason = failure.reason(),
            "review group plan replanning started"
        );

        // The Architect must see the group as persisted after the failed sub-step
        // was reverted, so the group is reloaded before synthesis.
        let reloaded = reload_group(repo_root, review_id, &group.id).await?;
        session = reloaded.0;
        group = reloaded.1;

        match refine_group_with_architect(
            state,
            repo_root,
            &session,
            &group,
            None,
            None,
            Some(ReviewGroupRefineDiagnostics {
                failed_step_id: failed_step.as_str(),
                failure_reason: Some(failure.reason()),
            }),
            &operation,
            // Replanning reprojects the horizon of a running batch member, so it
            // never settles the shared turn itself.
            true,
        )
        .await
        {
            Ok(GroupRefineOutcome::Success { .. }) => {}
            Ok(GroupRefineOutcome::Blocked { reason }) => {
                let interrupted =
                    interrupt_group_plan(state, repo_root, review_id, &group, "", &reason).await;
                return Err(with_status_write(
                    ReviewStepFailure::domain(reason),
                    interrupted,
                ));
            }
            Err(err) => {
                let failure = ReviewStepFailure::infrastructure(err.message);
                let interrupted =
                    interrupt_group_plan(state, repo_root, review_id, &group, "", failure.reason())
                        .await;
                return Err(with_status_write(failure, interrupted));
            }
        }

        // Resume the group from the synthesized horizon: the interruption note of
        // a superseded attempt is cleared and the findings return to `Todo` before
        // the retry.
        resume_group_after_replan(state, repo_root, review_id, &group).await;
        let reloaded = reload_group(repo_root, review_id, &group.id).await?;
        session = reloaded.0;
        group = reloaded.1;

        // A targeted sub-step that the synthesized horizon replaced can no longer
        // be resumed by id; the retry then walks the whole remaining horizon
        // instead of failing on a vanished target.
        let horizon = Plan {
            items: group.subplan.clone(),
            ..Default::default()
        };
        let vanished = target
            .as_deref()
            .is_some_and(|current| horizon.is_leaf_item(current) != Some(true));
        if vanished {
            target = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Builds a finding of the given status that belongs to the named group.
    fn finding_of_group(group: &str, status: IssueStatus) -> ReviewItem {
        ReviewItem {
            id: 1,
            title: "Finding".to_string(),
            uuid: None,
            group: Some(group.to_string()),
            model: None,
            issue_type: tauqe_protocol::IssueType::Quality,
            severity: tauqe_protocol::ReviewSeverity::Suggestion,
            status,
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            resolution_notes: None,
            body: String::new(),
        }
    }

    /// Builds a session holding the given findings.
    fn grouped_session(items: Vec<ReviewItem>) -> ReviewSession {
        ReviewSession {
            id: "rev-groups".to_string(),
            title: "Grouped Audit".to_string(),
            created_at: 7,
            model: tauqe_protocol::ModelRef::openrouter("test/architect"),
            items,
            ..ReviewSession::default()
        }
    }

    /// The step context presents only the findings the horizon still owes:
    /// resolved, skipped, and rejected manifestations stay out of it.
    #[test]
    fn pending_group_findings_keep_only_todo_manifestations() {
        let session = grouped_session(vec![
            finding_of_group("arch-1", IssueStatus::Todo),
            finding_of_group("arch-1", IssueStatus::InProgress),
            finding_of_group("arch-1", IssueStatus::Fixed),
            finding_of_group("arch-1", IssueStatus::Skipped),
            // A finding of another group never leaks into the narrowed set.
            finding_of_group("arch-2", IssueStatus::Todo),
        ]);

        let pending = pending_group_findings(&session, "arch-1");

        assert_eq!(pending.len(), 1);
        assert!(matches!(pending[0].status, IssueStatus::Todo));
    }

    /// Builds a group whose title names the root cause it remediates.
    fn group(title: &str) -> ReviewGroup {
        ReviewGroup {
            id: "arch-1".to_string(),
            title: title.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn completion_note_names_the_root_cause_plan_and_its_steps() {
        let note = group_completion_note(&group("Unified error handling"), 3);
        assert_eq!(
            note,
            "Group root-cause plan 'Unified error handling' completed: all 3 step(s) applied successfully"
        );
    }

    #[test]
    fn completion_note_falls_back_to_the_group_identifier() {
        // A group whose title was never stated is identified by its id, so the
        // note always names the plan it refers to.
        let note = group_completion_note(&group("   "), 1);
        assert!(note.contains("'arch-1'"), "note was: {note}");
    }

    #[test]
    fn interruption_note_keeps_the_prefix_and_the_reason() {
        let note = group_interruption_note(
            "compiler errors persisted",
            "I stopped after the second hunk failed.",
        );
        assert!(note.starts_with(GROUP_PLAN_INTERRUPTED_NOTE_PREFIX));
        assert!(note.contains("compiler errors persisted"));
        assert!(note.contains("I stopped after the second hunk failed."));
        assert!(note.contains("## Model Response:"));
    }

    #[test]
    fn interruption_note_without_a_comment_keeps_the_reason_only() {
        let note = group_interruption_note("retry budget exhausted", "   ");
        assert!(note.starts_with(GROUP_PLAN_INTERRUPTED_NOTE_PREFIX));
        assert!(note.contains("retry budget exhausted"));
        assert!(!note.contains("## Model Response"));
    }

    #[test]
    fn group_step_id_round_trips_through_the_parser() {
        let encoded = group_step_id("rev-42", "arch-1");
        assert_eq!(encoded, "rev-42:g:arch-1");
        assert_eq!(parse_group_step_id(&encoded), Some(("rev-42", "arch-1")));
    }

    #[test]
    fn parser_rejects_a_plain_finding_target_and_a_truncated_member() {
        assert_eq!(parse_group_step_id("7"), None);
        assert_eq!(parse_group_step_id(":g:arch-1"), None);
        assert_eq!(parse_group_step_id("rev-42:g:"), None);
    }

    /// Builds a horizon whose only executable leaf depends on a cancelled step,
    /// so the scope can never become ready.
    fn stalled_plan() -> Plan {
        Plan {
            id: "rev-stall".to_string(),
            title: "Stalled Horizon".to_string(),
            items: vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Abandoned approach".to_string(),
                    status: PlanItemStatus::Cancelled,
                    ..Default::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Follow-up step".to_string(),
                    status: PlanItemStatus::Todo,
                    depends_on: vec!["1".to_string()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    /// A scope stalled on unsatisfiable dependencies is a domain failure of the
    /// stalled sub-step, so the supervisor replans the horizon instead of
    /// aborting the batch as an infrastructure error.
    #[test]
    fn dependency_stall_is_a_domain_failure_of_the_stalled_step() {
        let plan = stalled_plan();
        let stall = plan.evaluate_scope(None);
        assert!(matches!(
            stall,
            PlanScopeEvaluation::BlockedByDependencies(_)
        ));

        let failure = classify_scope_stall(&plan, None, &stall);
        assert!(failure.is_domain());
        assert_eq!(failure.failed_step(), Some("2"));
    }

    /// A stall the harness cannot repair by replanning the horizon stays an
    /// infrastructure failure that aborts the batch.
    #[test]
    fn blocked_scope_remains_an_infrastructure_failure() {
        let mut plan = stalled_plan();
        plan.items[1].status = PlanItemStatus::Blocked;
        let stall = plan.evaluate_scope(None);
        assert!(matches!(stall, PlanScopeEvaluation::BlockedByBlockers(_)));

        let failure = classify_scope_stall(&plan, None, &stall);
        assert!(!failure.is_domain());
        assert_eq!(failure.failed_step(), None);
    }

    /// A ready scope names no stalled sub-step.
    #[test]
    fn ready_scope_names_no_stalled_step() {
        let mut plan = stalled_plan();
        plan.items[0].status = PlanItemStatus::Done;
        assert_eq!(dependency_stalled_step(&plan), None);
    }
}
