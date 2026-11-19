//! Read-only document snapshots published after an interactive completion.
use crate::api::{Application, InteractiveTurnCompletion};
use std::path::Path;
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::plan::{storage::PlanStorage, Plan};
use tauqe_core::review::{storage::ReviewStorage, ReviewSession};

pub struct InteractivePlanUpdates {
    pub(crate) active_plan: Option<Plan>,
    pub(crate) plans: Vec<Plan>,
    pub(crate) active_id: Option<String>,
}
pub struct InteractiveReviewUpdates {
    pub(crate) session: ReviewSession,
    pub(crate) decisions_applied: usize,
}
impl Application {
    /// Publication is best effort; a failed list read suppresses both plan notices.
    pub(crate) async fn interactive_plan_updates(
        &self,
        root: &Path,
        completion: &InteractiveTurnCompletion,
    ) -> Option<InteractivePlanUpdates> {
        if !completion.plans_applied {
            return None;
        }
        let root = root.to_path_buf();
        let (active_id, plans) = tokio::task::spawn_blocking(move || {
            let active = PlanStorage::load_active_id(&root).ok().flatten();
            let plans = PlanStorage::load_all(&root).ok();
            (active, plans)
        })
        .await
        .unwrap_or((None, None));
        let plans = plans?;
        let active_plan = active_id
            .as_ref()
            .and_then(|id| plans.iter().find(|plan| &plan.id == id))
            .cloned();
        Some(InteractivePlanUpdates {
            active_plan,
            plans,
            active_id,
        })
    }

    /// Review and group decisions refresh their owning session once per applied decision.
    pub(crate) async fn interactive_review_updates(
        &self,
        root: &Path,
        completion: &InteractiveTurnCompletion,
        focus: Option<&DiscussionTarget>,
    ) -> Option<InteractiveReviewUpdates> {
        if completion.review_decisions_applied.is_empty() {
            return None;
        }
        let review_id = focus?.review_session_id()?;
        let root = root.to_path_buf();
        let id = review_id.to_owned();
        let session = match tokio::task::spawn_blocking(move || {
            ReviewStorage::load_review(&root, &id)
        })
        .await
        {
            Ok(Ok(session)) => session,
            Ok(Err(error)) => {
                tracing::warn!(review_id=%review_id,error=%error,"review session could not be read");
                None
            }
            Err(error) => {
                let error = format!("blocking task failed: {}", error);
                tracing::warn!(review_id=%review_id,error=%error,"review session read failed");
                None
            }
        }?;
        Some(InteractiveReviewUpdates {
            session,
            decisions_applied: completion.review_decisions_applied.len(),
        })
    }
}

#[cfg(test)]
mod tests;

impl Application {
    /// Read repository state after commit publication, before edit-cycle closure.
    pub(crate) async fn repository_after_turn_commit(
        &self,
    ) -> Option<tauqe_core::git::state::RepositoryState> {
        let root = self.repository_root().await;
        match tokio::task::spawn_blocking(move || {
            tauqe_core::git::get_repository_state(Some(&root))
        })
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
}
