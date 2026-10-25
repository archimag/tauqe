//! Automated replanning of a review finding's remaining subplan horizon.
//!
//! `run_review_subplan` confines a failure to the current finding: the failed
//! sub-step returns to `Todo`, the completed foundation stays `Done`, and the
//! finding is marked `Skipped` with an interruption note. When the failure is a
//! domain failure attributable to a concrete sub-step and the replan budget
//! allows it, the Architect reprojects the remaining horizon from the immutable
//! `Done` foundation and the failure diagnostics — exactly as plan replanning
//! does — and the finding is resumed from the synthesized horizon. An exhausted
//! budget or a blocked Architect leaves the finding `Skipped`, so the batch
//! advances instead of retrying a hopeless approach.

use std::path::Path;
use std::sync::Arc;

use tauqe_core::review::prompt::refine::ReviewRefineDiagnostics;
use tauqe_core::review::prompt::ReviewStepFailure;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{IssueStatus, ModelSelection, PlanItem, ReviewItem, ReviewSession};

use crate::state::{AppState, OperationGuard};

use super::super::common::{offload_review, publish_review, skip_finding_with_diagnostic};
use super::super::refine::{refine_finding_with_architect, RefineAdmission, RefineOutcome};
use super::subplan::{run_review_subplan, SUBPLAN_INTERRUPTED_NOTE_PREFIX};

/// Reloads the review session and the target finding from disk.
///
/// Execution and replanning both need the persisted state of the finding: the
/// subplan mutated by a failed sub-step, the status written by an interruption,
/// and the horizon synthesized by the Architect.
pub(super) async fn reload_finding(
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
fn subplan_contains(items: &[PlanItem], step_id: &str) -> bool {
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
pub(super) async fn run_review_subplan_with_replans(
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
            RefineAdmission::Harness,
        )
        .await
        {
            Ok(RefineOutcome::Success { .. }) => {}
            Ok(RefineOutcome::Blocked { reason }) => {
                return Err(
                    match skip_finding_with_diagnostic(
                        state, repo_root, review_id, item.id, "", &reason,
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
