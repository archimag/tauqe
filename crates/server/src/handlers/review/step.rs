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
//! The subsystem is decomposed into focused submodules: `subplan` walks the
//! embedded subplan of a decomposed finding one leaf sub-step at a time, and
//! `replan` reprojects the remaining horizon around a failed sub-step. This
//! facade keeps the single-finding turn with its retry and escalation ladder.
//!
//! Three invariants govern this subsystem:
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
//!
//! An unconditional abort (`<abort>`) is a domain outcome rather than a failure:
//! the host environment needs a developer action — approving a native build,
//! installing a package, starting a daemon. A single-finding turn that reports it
//! is skipped with the exact recommendation recorded and never engages the
//! Architect, so the batch advances to the next independent finding.

mod replan;
mod subplan;

use std::path::Path;
use std::sync::Arc;

use tauqe_core::providers::{format_session_id, SESSION_PREFIX_REVIEW};
use tauqe_core::review::prompt::ReviewStepFailure;
use tauqe_protocol::{IssueStatus, IssueType, ModelRole, ModelSelection, TurnIntent};

use crate::handlers::model::{execute_model_turn, StepTurnOptions};
use crate::state::{AppState, OperationGuard};

use self::replan::{reload_finding, run_review_subplan_with_replans};
use self::subplan::append_developer_directives;
use super::common::{
    clear_auto_context, persist_finding_update, preload_item_files, report_item_failure,
    set_item_status, set_item_status_checked, skip_finding_with_diagnostic, turn_applied_edits,
};
use super::refine::{refine_finding_with_architect, RefineAdmission, RefineOutcome};

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
            RefineAdmission::Harness,
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
                // When the skip record cannot be persisted, the storage failure
                // becomes the outcome so the batch never advances past a finding
                // the durable record still shows as InProgress.
                return Err(
                    match skip_finding_with_diagnostic(
                        state, repo_root, review_id, item_id, "", &reason,
                    )
                    .await
                    {
                        Ok(()) => ReviewStepFailure::domain(reason),
                        Err(storage_err) => ReviewStepFailure::infrastructure(storage_err),
                    },
                );
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
    set_item_status(
        state,
        repo_root,
        review_id,
        item_id,
        IssueStatus::InProgress,
    )
    .await;
    tracing::info!(
        review_id = review_id,
        item_id = item_id,
        "review step started"
    );

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
                // The failed turn names the operation it belonged to, so the
                // failure event is attributable to this batch instead of a fresh
                // identifier that matches no registered operation.
                let op_id = err
                    .operation_id
                    .clone()
                    .unwrap_or_else(|| format!("op-{}", operation.id()));
                report_item_failure(state, &op_id, &err.error.message);
                return Err(ReviewStepFailure::infrastructure(err.error.message));
            }
        };

        // Semantic block: the coder reports that the finding preconditions
        // cannot be satisfied as stated. This is a domain failure: the finding
        // is skipped with diagnostics and the batch continues without a retry
        // cycle. When the skip record cannot be persisted, the storage failure
        // becomes the outcome so the batch never advances past a finding the
        // durable record still shows as InProgress.
        if let Some(reason) = turn.blocked.clone() {
            report_item_failure(state, &turn.operation_id, &reason);
            return Err(
                match skip_finding_with_diagnostic(
                    state,
                    repo_root,
                    review_id,
                    item_id,
                    &turn.assistant_text,
                    &reason,
                )
                .await
                {
                    Ok(()) => ReviewStepFailure::domain(reason),
                    Err(storage_err) => ReviewStepFailure::infrastructure(storage_err),
                },
            );
        }

        // Unconditional abort: the work cannot be performed by the harness at
        // all — the host environment needs a developer action first, such as
        // approving a native build, installing a package, or starting a daemon.
        // Unlike a semantic block, an abort never starts an Architect replanning
        // cycle: the finding is skipped with the exact recommendation recorded,
        // and the queue advances to the next independent finding. When the skip
        // record cannot be persisted, the storage failure becomes the outcome
        // so the batch never advances past a finding the durable record still
        // shows as InProgress.
        if let Some(abort) = turn.abort.clone() {
            let message = abort.to_message();
            tracing::warn!(
                review_id = review_id,
                item_id = item_id,
                reason = %abort.reason,
                "review finding aborted; developer action in the host environment is required"
            );
            report_item_failure(state, &turn.operation_id, &message);
            return Err(
                match skip_finding_with_diagnostic(
                    state,
                    repo_root,
                    review_id,
                    item_id,
                    &turn.assistant_text,
                    &message,
                )
                .await
                {
                    Ok(()) => ReviewStepFailure::domain(message),
                    Err(storage_err) => ReviewStepFailure::infrastructure(storage_err),
                },
            );
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
            // The FIXED transition is part of the success contract: when the
            // write cannot be persisted the finding is reported as an
            // infrastructure failure, so the batch outcome never claims a
            // resolution the durable record does not hold.
            let persisted = match notes {
                Some(notes) => {
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
                }
                None => {
                    set_item_status_checked(
                        state,
                        repo_root,
                        review_id,
                        item_id,
                        IssueStatus::Fixed,
                    )
                    .await
                }
            };
            if let Err(err) = persisted {
                report_item_failure(state, &turn.operation_id, &err);
                return Err(ReviewStepFailure::infrastructure(err));
            }
            return Ok(());
        }

        if !applied && turn.error.is_none() && !turn.is_cancelled {
            // The registry-bound skip parser is the single source of truth for
            // the `<skip>` tag, so the finding is skipped through the typed
            // protocol outcome instead of a second ad-hoc scanner. A skip that
            // cannot be persisted is an infrastructure failure, so the queue
            // never advances past a finding the durable record still shows as
            // InProgress.
            if let Some(skip_reason) =
                tauqe_core::edits::protocol::xml::extract_skip(&turn.assistant_text)
            {
                let persisted = persist_finding_update(
                    state,
                    repo_root,
                    review_id,
                    item_id,
                    "Skipped status",
                    move |it| {
                        it.status = IssueStatus::Skipped;
                        it.note = Some(skip_reason);
                    },
                )
                .await;
                if let Err(err) = persisted {
                    report_item_failure(state, &turn.operation_id, &err);
                    return Err(ReviewStepFailure::infrastructure(err));
                }
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
        // diagnostics so the queue advances to the next one. When the skip
        // record cannot be persisted, the storage failure becomes the outcome
        // so the batch never advances past a finding the durable record still
        // shows as InProgress.
        if attempt >= max_retries {
            if turn.error.is_none() {
                report_item_failure(state, &turn.operation_id, &reason);
            }
            return Err(
                match skip_finding_with_diagnostic(
                    state,
                    repo_root,
                    review_id,
                    item_id,
                    &turn.assistant_text,
                    &reason,
                )
                .await
                {
                    Ok(()) => ReviewStepFailure::domain(reason),
                    Err(storage_err) => ReviewStepFailure::infrastructure(storage_err),
                },
            );
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
