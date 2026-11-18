//! Shared helpers for the plan handler subsystem.

use crate::contracts::Project;
use std::path::Path;
use std::sync::Arc;

use tauqe_core::plan::Plan;
use tauqe_protocol::{events, PlanListChangedEvent, PlanUpdatedEvent};

use crate::state::AppState;

/// Event emission and transient-context cleanup are shared with every other
/// handler subsystem, so they are re-exported here instead of being duplicated
/// per subsystem.
pub(super) use crate::handlers::common::emit;

/// Publishes the updated plan and refreshed plan list to the client.
///
/// The refreshed plan list is read through the blocking pool so the handler never
/// performs disk I/O on an async worker.
pub(super) async fn publish_plan(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan: Plan,
    active_id: &str,
) {
    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent {
            plan: plan.project(),
            is_save: false,
        })
        .ok(),
    );
    let list_root = repo_root.to_path_buf();
    if let Ok(plans) = state.application.documents_at(&list_root).plans().await {
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent {
                plans: plans
                    .into_iter()
                    .map(crate::contracts::Project::project)
                    .collect(),
                active_id: Some(active_id.to_string()),
            })
            .ok(),
        );
    }
}

pub(super) async fn publish_plan_list(state: &Arc<AppState>, root: &Path) {
    if let Ok((plans, active_id)) = state.application.documents_at(root).plan_list().await {
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent {
                plans: plans.into_iter().map(Project::project).collect(),
                active_id,
            })
            .ok(),
        );
    }
}
