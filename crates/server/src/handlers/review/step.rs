//! Per-finding bounded-turn execution.
//!
//! A finding of a simple review group runs as one isolated bounded turn: its
//! target files are preloaded, one attempt drives the shared
//! `execute_model_turn` entry point, and the coder ladder may be climbed when
//! `auto_level_up` is enabled. Domain failures are confined to the current
//! finding, while infrastructure failures abort the batch. Finding status and
//! the terminal outcome are persisted through `ReviewStorage`, so no plan-tree
//! manipulation happens in the server crate.
//!
//! Two invariants govern this subsystem:
//! - #c-1: a finding of an architectural review group is a symptom of one shared
//!   root cause, so it is never executed on its own. The group is remediated
//!   exclusively through its unified subplan, and this module refuses such a
//!   finding even though the server queue never routes one here.
//! - #c-2: a review finding owns no subplan, so no decomposition precedes its
//!   turn and no sub-step of a finding is ever targeted.
//!
//! An unconditional abort (`<abort>`) is a domain outcome rather than a failure:
//! the host environment needs a developer action — approving a native build,
//! installing a package, starting a daemon. A single-finding turn that reports it
//! is skipped with the exact recommendation recorded and never engages the
//! Architect, so the batch advances to the next independent finding.

use std::path::Path;
use std::sync::Arc;

use crate::handlers::common::append_developer_directives;
use crate::handlers::model::{execute_model_turn, StepTurnOptions};
use crate::state::{AppState, OperationGuard};
use tauqe_core::providers::{format_session_id, SESSION_PREFIX_REVIEW};
use tauqe_core::review::prompt::ReviewStepFailure;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::model::{TurnWorkContext, TurnWorkItem, TurnWorkQueue};
use tauqe_protocol::{
    IssueStatus, ModelRole, ModelSelection, ReviewGroupKind, ReviewItem, ReviewSession, TurnIntent,
    DEFAULT_REVIEW_GROUP_ID,
};
use tokio::sync::watch;

use super::common::{
    clear_auto_context, offload_review, persist_finding_update, preload_item_files,
    report_item_failure, set_item_status, set_item_status_checked, skip_finding_with_diagnostic,
    turn_applied_edits,
};

/// Reloads the review session and the target finding from disk.
///
/// Execution needs the persisted state of the finding — the status a previous
/// attempt wrote and the target files its turn preloads — while the session
/// supplies the effective group that decides whether the finding may run on its
/// own at all.
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

/// Rejects a finding that belongs to an architectural review group.
///
/// #c-1: the findings of an architectural group are symptoms of one shared root
/// cause, so the group is remediated exclusively by its unified subplan. The
/// server queue never schedules such a finding on its own; this check defends
/// the invariant against a direct call instead of quietly coding one symptom.
fn ensure_standalone_execution(
    session: &ReviewSession,
    item: &ReviewItem,
) -> Result<(), ReviewStepFailure> {
    let group_id = item.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID);
    let groups = session.effective_groups();
    let architectural = groups
        .iter()
        .any(|group| group.id == group_id && group.kind == ReviewGroupKind::Architectural);
    if architectural {
        return Err(ReviewStepFailure::infrastructure(format!(
            "Review item #{} belongs to architectural review group '{}' and cannot be executed on its own; the group is remediated through its unified subplan",
            item.id, group_id
        )));
    }
    Ok(())
}

/// Builds the declared work context of one finding executed on its own.
///
/// The queue lists the findings of the session in stored order, so the client
/// renders the position `[i/N]` of the running finding and the findings the
/// session still holds instead of reconstructing them from the stored review.
fn review_step_work_context(session: &ReviewSession, item: &ReviewItem) -> TurnWorkContext {
    let queue = TurnWorkQueue {
        items: session
            .items
            .iter()
            .map(|member| TurnWorkItem {
                id: member.id.to_string(),
                title: member.title.clone(),
            })
            .collect(),
        active_index: session
            .items
            .iter()
            .position(|member| member.id == item.id)
            .unwrap_or_default(),
    };
    TurnWorkContext::ReviewStep {
        review_id: session.id.clone(),
        review_title: session.title.clone(),
        queue,
    }
}

/// Executes one finding in an isolated turn.
///
/// The finding becomes `Fixed` only when the turn satisfies the success
/// contract; on any failure it returns to `Todo`. The transient auto context
/// layer is always cleared once the step finishes, so a retried attempt starts
/// from a clean, deterministic basis.
///
/// `model` overrides the Coder model and `instructions` are appended to the
/// execution prompt as developer directives.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_review_step(
    state: &Arc<AppState>,
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
    model: Option<ModelSelection>,
    instructions: Option<String>,
    cancel_rx: &watch::Receiver<bool>,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    let res = run_review_step_inner(
        state,
        repo_root,
        review_id,
        item_id,
        model,
        instructions,
        cancel_rx,
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
    model: Option<ModelSelection>,
    instructions: Option<String>,
    cancel_rx: &watch::Receiver<bool>,
    operation: Arc<OperationGuard>,
) -> Result<(), ReviewStepFailure> {
    // Every finding turn of one audit extends the same provider prefix cache, so
    // the session identifier is derived from the review, not from the dialogue.
    let session_id = Some(format_session_id(SESSION_PREFIX_REVIEW, review_id));
    let (session, item) = reload_finding(repo_root, review_id, item_id).await?;

    // #c-1: a symptom of an architectural group is never executed on its own.
    ensure_standalone_execution(&session, &item)?;

    match item.status {
        // A finished finding is not re-executed.
        IssueStatus::Fixed | IssueStatus::Skipped => return Ok(()),
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
        IssueStatus::Todo | IssueStatus::InProgress => {}
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
    let protocol = state.config.lock().await.config.develop.protocol;

    let mut prompt = match tauqe_core::review::prompt::format_review_step_execution_prompt(
        &session,
        item_id,
        protocol,
        marker.as_deref(),
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

    // The selected coder is the starting rung of this finding: an explicit
    // selection runs at its own reasoning depth, and a later escalation
    // continues above it instead of restarting the ladder at its base.
    let mut selection = model
        .or_else(|| item.model.clone())
        .unwrap_or(ModelSelection::Role(ModelRole::Coder));
    let mut ladder_idx = models_config.coder_tier_index(Some(&selection));
    let mut reasoning_effort = models_config.resolve_selection_effort(ModelRole::Coder, &selection);
    // Declare the work every attempt of this step executes: the running finding
    // and its position among the findings of the session, so the client renders
    // the work marker from typed fields instead of stored reviews.
    let work_context = review_step_work_context(&session, &item);
    let mut attempt = 0usize;

    loop {
        // Phase boundary: every attempt is a whole bounded turn, so a cancel
        // observed here returns the finding to TODO before another turn is
        // requested. The batch executor reports the resulting failure as a
        // cancelled step.
        let cancelled = *cancel_rx.borrow();
        if cancelled {
            set_item_status(state, repo_root, review_id, item_id, IssueStatus::Todo).await;
            return Err(ReviewStepFailure::infrastructure(
                "Operation cancelled before the finding turn started".to_string(),
            ));
        }
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
                work_context: Some(work_context.clone()),
                reasoning_effort,
                marker: marker.clone(),
                shared_operation: Some(Arc::clone(&operation)),
                session_id: session_id.clone(),
                // A review finding streams the turn's live phases into the
                // client channel, so verifying one finding stays visible.
                phase_out: Some(state.out.clone()),
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

        // A rung without a stronger successor is saturated, and a manual run on a
        // model the ladder excludes cannot escalate at all: the retry keeps the
        // current model instead of silently entering another one.
        let next_rung = if auto_level_up {
            models_config.next_coder_tier(ladder_idx)
        } else {
            None
        };
        if let Some((next_idx, tier)) = next_rung {
            tracing::warn!(
                review_id = review_id,
                item_id = item_id,
                from_tier = ladder_idx,
                to_tier = next_idx,
                to_model = %tier.model,
                effort = ?tier.effort,
                reason = %reason,
                "review step coder escalation"
            );
            ladder_idx = next_idx;
            selection = ModelSelection::Specific(tier.model);
            reasoning_effort = tier.effort;
        }

        prompt = tauqe_core::review::prompt::format_review_step_retry_prompt(
            &turn.assistant_text,
            turn.error.as_deref(),
            protocol,
            marker.as_deref(),
        );
        attempt += 1;
    }
}
