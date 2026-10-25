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
use tauqe_core::review::prompt::{apply_finding_architectural_context, ReviewStepFailure};
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    IssueStatus, ModelRole, ModelSelection, Plan, PlanItem, PlanItemStatus, PlanScopeEvaluation,
    ReviewItem, ReviewSession, TurnIntent,
};

use crate::handlers::model::{execute_model_turn, StepTurnOptions};
use crate::handlers::plan::preload_step_files;
use crate::state::{AppState, OperationGuard};

use crate::handlers::common::append_developer_directives;

use super::super::common::{
    append_model_comment, clear_auto_context, offload_review, persist_finding_update,
    publish_review, report_item_failure, set_item_status, set_item_status_checked,
    turn_applied_edits,
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

/// Classifies a stalled subplan scope for the review batch supervisor.
///
/// A scope stalls on dependencies when its leaf sub-steps wait for work that will
/// never become `Done`: the subplan structure itself is wrong, so the horizon has
/// to be reprojected around the stalled dependencies. That outcome is therefore a
/// domain failure attributed to the stalled sub-step, and the supervisor replans
/// the remaining horizon instead of aborting the batch. Every other stall is a
/// lifecycle or structural precondition the harness cannot repair by replanning.
fn classify_scope_stall(
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
///
/// Returns `Err` when the interruption cannot be persisted, so the caller
/// reports an infrastructure failure instead of letting the walk claim a skip
/// the durable record does not hold.
async fn finish_subplan_interrupted(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    plan: &mut Plan,
    assistant_text: &str,
    reason: &str,
) -> Result<(), String> {
    plan.refresh_parent_statuses();
    let subplan = plan.items.clone();
    let notes = append_model_comment(
        format!("{}: {}", SUBPLAN_INTERRUPTED_NOTE_PREFIX, reason),
        assistant_text,
    );

    persist_finding_update(
        state,
        repo_root,
        review_id,
        item_id,
        "Skipped interruption record",
        move |it| {
            it.status = IssueStatus::Skipped;
            it.plan = subplan;
            it.note = Some(notes);
        },
    )
    .await
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
/// returns to `Todo`. A scope that stalls because its leaf sub-steps depend on
/// work that will never become `Done` is reported as a domain failure attributed
/// to the stalled sub-step, so the supervisor replans the remaining horizon
/// instead of aborting the batch.
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
    let protocol = state.config.lock().await.config.develop.protocol;

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
    // The subplan must inherit the architectural decision of its finding, so the
    // step prompt renders the rationale and the contract the reviewer fixed.
    apply_finding_architectural_context(&mut plan, item);

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
            return Err(classify_scope_stall(&plan, step_id, &stall));
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
            protocol,
            marker.as_deref(),
        ) {
            Ok(p) => p,
            Err(err) => {
                plan.update_item_status(current_step_id, PlanItemStatus::Todo);
                let interrupt = finish_subplan_interrupted(
                    state,
                    repo_root,
                    review_id,
                    item_id,
                    &mut plan,
                    &step_assistant_text,
                    &err,
                )
                .await;
                return Err(match interrupt {
                    Ok(()) => ReviewStepFailure::infrastructure(err),
                    Err(storage_err) => ReviewStepFailure::infrastructure(format!(
                        "{err}; additionally, the subplan interruption could not be persisted: {storage_err}"
                    )),
                });
            }
        };
        append_developer_directives(&mut prompt, instructions.as_deref());

        let mut selection = explicit_selection
            .clone()
            .unwrap_or(ModelSelection::Role(ModelRole::Coder));
        let mut ladder_idx = models_config.coder_tier_index(Some(&selection));
        let mut reasoning_effort =
            models_config.resolve_selection_effort(ModelRole::Coder, &selection);
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
                Err(err) => break Err(ReviewStepFailure::infrastructure(err.error.message)),
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

            if auto_level_up {
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
                protocol,
                marker.as_deref(),
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
                // The failed sub-step is reported under the operation that runs
                // this subplan, so the failure is attributable to its batch.
                let op_id = format!("op-{}", operation.id());
                report_item_failure(state, &op_id, &reason);
                let interrupt = finish_subplan_interrupted(
                    state,
                    repo_root,
                    review_id,
                    item_id,
                    &mut plan,
                    &step_assistant_text,
                    &reason,
                )
                .await;
                match interrupt {
                    Ok(()) => return Err(failure),
                    // The Skipped record could not be persisted, so the walk
                    // must not report the finding as skipped: the storage
                    // failure becomes the outcome and the batch stops instead
                    // of advancing past a finding the durable record still
                    // shows as InProgress.
                    Err(storage_err) => {
                        report_item_failure(state, &op_id, &storage_err);
                        return Err(ReviewStepFailure::infrastructure(storage_err));
                    }
                }
            }
        }
    }
    clear_auto_context(state).await;

    // #c-2: a finding is Fixed only when every remaining leaf sub-step is Done.
    // Both terminal writes are part of the walk's success contract: a write
    // that cannot be persisted is reported as an infrastructure failure, so
    // the batch outcome never diverges from the durable record.
    let (total, done) = plan.count_stats();
    let persisted = if total > 0 && done == total {
        let notes = format!(
            "Subplan completed: all {} sub-step(s) applied successfully",
            total
        );
        persist_finding_update(
            state,
            repo_root,
            review_id,
            item_id,
            "Fixed status",
            move |it| {
                it.status = IssueStatus::Fixed;
                if it.note.is_none() {
                    it.note = Some(notes);
                }
            },
        )
        .await
    } else {
        set_item_status_checked(state, repo_root, review_id, item_id, IssueStatus::Todo).await
    };
    if let Err(err) = persisted {
        let op_id = format!("op-{}", operation.id());
        report_item_failure(state, &op_id, &err);
        return Err(ReviewStepFailure::infrastructure(err));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a finding whose embedded subplan carries one executable sub-step.
    fn finding_with_subplan(body: &str) -> ReviewItem {
        let mut item = ReviewItem::from(tauqe_protocol::review::ReviewIssue {
            id: 7,
            title: "Guard the parser".to_string(),
            model: None,
            issue_type: tauqe_protocol::review::IssueType::Bug,
            status: IssueStatus::Todo,
            description: body.to_string(),
            recommendations: vec!["Keep it lazy".to_string()],
            files: Vec::new(),
            readonly_files: Vec::new(),
            plan: Vec::new(),
            note: None,
        });
        item.subplan = vec![tauqe_protocol::plan::PlanItem {
            id: "1".to_string(),
            title: "Return a structured error".to_string(),
            details: Some("Reuse the existing scanner.".to_string()),
            status: PlanItemStatus::Todo,
            blocked_reason: None,
            files: vec!["crates/x/src/parser.rs".to_string()],
            readonly_files: vec!["crates/x/src/api.rs".to_string()],
            concept_refs: Vec::new(),
            depends_on: Vec::new(),
            children: Vec::new(),
        }];
        item
    }

    /// An interrupted subplan whose `Skipped` record cannot be persisted must
    /// surface the storage failure instead of silently claiming the finding
    /// was skipped.
    #[tokio::test]
    async fn finish_subplan_interrupted_surfaces_persistence_failure() {
        // Same hermetic fixture contract as the sibling review-persistence
        // test: this test corrupts its own session file, so the directory is
        // process-unique and cleared before use rather than inherited from an
        // interrupted earlier run.
        let root = std::env::temp_dir().join(format!(
            "tauqe-subplan-interrupt-{}-{}",
            std::process::id(),
            crate::state::next_operation_id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            root.clone(),
            history_sender,
            out,
        );

        let item = finding_with_subplan("The parser panics on truncated input.");
        let session = tauqe_protocol::ReviewSession {
            id: "rev-interrupt".to_string(),
            title: "Interrupt Audit".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: vec![],
            items: vec![item.clone()],
            raw_markdown: String::new(),
        };
        ReviewStorage::save_session(&root, &session).unwrap();

        let mut plan = Plan {
            id: "rev-interrupt".to_string(),
            title: "Interrupt Audit".to_string(),
            items: item.subplan.clone(),
            ..Default::default()
        };

        // A healthy write persists the interruption record.
        finish_subplan_interrupted(
            &state,
            &root,
            "rev-interrupt",
            7,
            &mut plan,
            "I stopped after the second hunk failed.",
            "sub-step failed",
        )
        .await
        .expect("healthy interruption write succeeds");
        let stored = ReviewStorage::load_review(&root, "rev-interrupt")
            .unwrap()
            .unwrap();
        let issue = stored.issues().into_iter().next().unwrap();
        assert_eq!(issue.status, IssueStatus::Skipped);
        assert!(issue
            .note
            .as_deref()
            .is_some_and(|note| note.starts_with(SUBPLAN_INTERRUPTED_NOTE_PREFIX)));

        // A corrupted session file makes the session unresolvable, so the next
        // interruption write cannot land on disk: the failure must be
        // returned, not discarded. Review storage skips an unparseable session
        // file and reports the session as missing, so the surfaced diagnostic
        // is the not-persisted message rather than a raw parse error.
        let review_file = ReviewStorage::reviews_dir(&root).join("rev-interrupt.json");
        std::fs::write(&review_file, b"{ not a review session }").unwrap();
        let err = finish_subplan_interrupted(
            &state,
            &root,
            "rev-interrupt",
            7,
            &mut plan,
            "",
            "sub-step failed",
        )
        .await
        .expect_err("persistence failure must surface");
        assert!(err.contains("was not persisted"));
        assert!(err.contains("rev-interrupt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Builds a subplan whose only executable leaf depends on a cancelled step,
    /// so the scope can never become ready.
    fn stalled_plan() -> Plan {
        let leaf =
            |id: &str, title: &str, status: PlanItemStatus, depends_on: Vec<String>| PlanItem {
                id: id.to_string(),
                title: title.to_string(),
                details: None,
                status,
                blocked_reason: None,
                files: Vec::new(),
                readonly_files: Vec::new(),
                concept_refs: Vec::new(),
                depends_on,
                children: Vec::new(),
            };
        Plan {
            id: "rev-stall".to_string(),
            title: "Stalled Subplan".to_string(),
            items: vec![
                leaf(
                    "1",
                    "Abandoned approach",
                    PlanItemStatus::Cancelled,
                    Vec::new(),
                ),
                leaf(
                    "2",
                    "Follow-up step",
                    PlanItemStatus::Todo,
                    vec!["1".to_string()],
                ),
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
