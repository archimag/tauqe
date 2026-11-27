//! Read-only document snapshots published after an interactive completion.
use crate::workflows::repl::documents::DocumentChanges;
use std::path::Path;
use tauqe_core::plan::{storage::PlanStorage, Plan};
use tauqe_core::review::{storage::ReviewStorage, ReviewSession};

pub struct InteractivePlanUpdates {
    pub(crate) saved: Vec<Plan>,
    pub(crate) listing: Option<(Vec<Plan>, Option<String>)>,
}
pub struct InteractiveReviewUpdates {
    pub(crate) session: ReviewSession,
    pub(crate) decisions_applied: usize,
}
/// A failed list read cannot suppress documents confirmed by storage.
pub(crate) async fn interactive_plan_updates(
    root: &Path,
    changes: &DocumentChanges,
) -> Option<InteractivePlanUpdates> {
    let saved = changes.plans.clone();
    if saved.is_empty() {
        return None;
    }
    let root = root.to_path_buf();
    let listing = tokio::task::spawn_blocking(move || {
        let plans = PlanStorage::load_all(&root).ok()?;
        let active_id = PlanStorage::load_active_id(&root).ok().flatten();
        Some((plans, active_id))
    })
    .await
    .ok()
    .flatten();
    Some(InteractivePlanUpdates { saved, listing })
}

/// Review and group decisions refresh their owning session once per applied decision.
pub(crate) async fn interactive_review_updates(
    root: &Path,
    changes: &DocumentChanges,
) -> Vec<InteractiveReviewUpdates> {
    let mut sessions = std::collections::BTreeMap::new();
    for session in &changes.reviews {
        *sessions.entry(session.id.clone()).or_insert(0) += 1;
    }
    if sessions.is_empty() {
        return Vec::new();
    }
    let root = root.to_path_buf();
    match tokio::task::spawn_blocking(move || {
        sessions
            .into_iter()
            .filter_map(
                |(id, decisions_applied)| match ReviewStorage::load_review(&root, &id) {
                    Ok(session) => session.map(|session| InteractiveReviewUpdates {
                        session,
                        decisions_applied,
                    }),
                    Err(error) => {
                        tracing::warn!(review_id=%id, %error, "review session could not be read");
                        None
                    }
                },
            )
            .collect()
    })
    .await
    {
        Ok(updates) => updates,
        Err(error) => {
            tracing::warn!(%error, "review snapshot task failed");
            Vec::new()
        }
    }
}

/// Read repository state after commit publication, before edit-cycle closure.
pub(crate) async fn repository_after_turn_commit(
    root: std::path::PathBuf,
) -> Option<tauqe_core::git::state::RepositoryState> {
    match tokio::task::spawn_blocking(move || tauqe_core::git::get_repository_state(Some(&root)))
        .await
    {
        Ok(repository) => Some(repository),
        Err(error) => {
            let error = format!("blocking task failed: {}", error);
            tracing::warn!(error=%error,"Failed to read repository state after commit");
            None
        }
    }
}
