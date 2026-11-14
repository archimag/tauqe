//! Shared helpers for the plan handler subsystem.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_protocol::{events, Plan, PlanItem, PlanListChangedEvent, PlanUpdatedEvent};

use crate::state::AppState;

/// Event emission and transient-context cleanup are shared with every other
/// handler subsystem, so they are re-exported here instead of being duplicated
/// per subsystem.
pub(super) use crate::handlers::common::emit;

/// Runs a blocking plan-storage operation on the Tokio blocking pool.
///
/// The storage engine holds a synchronous process-wide mutex and performs disk
/// I/O, both of which must stay off the async worker threads.
pub(super) async fn offload_plan<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match crate::handlers::offload(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(format!("{:#}", err)),
        Err(err) => Err(err),
    }
}

/// Returns the first duplicate or empty step ID found in a plan item tree, if any.
///
/// Architect and client payloads are not trusted: a repeated ID would make
/// `Plan::find_item` ambiguous and let one step silently shadow another, while an
/// empty ID cannot be addressed at all. Such a tree is rejected before persistence.
pub(crate) fn find_duplicate_item_ids(items: &[PlanItem]) -> Option<String> {
    fn walk(items: &[PlanItem], seen: &mut HashSet<String>) -> Option<String> {
        for item in items {
            let key = item.id.trim().to_ascii_lowercase();
            if key.is_empty() || !seen.insert(key) {
                return Some(item.id.clone());
            }
            if let Some(duplicate) = walk(&item.children, seen) {
                return Some(duplicate);
            }
        }
        None
    }
    walk(items, &mut HashSet::new())
}

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
            plan,
            is_save: false,
        })
        .ok(),
    );
    let list_root = repo_root.to_path_buf();
    if let Ok(plans) = offload_plan(move || PlanStorage::load_all(&list_root)).await {
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent {
                plans,
                active_id: Some(active_id.to_string()),
            })
            .ok(),
        );
    }
}
