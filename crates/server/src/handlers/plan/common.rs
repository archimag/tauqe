//! Shared helpers for the plan handler subsystem.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_protocol::{
    events, ModelErrorEvent, Plan, PlanItem, PlanItemStatus, PlanListChangedEvent, PlanUpdatedEvent,
};

use crate::state::AppState;

/// Event emission, auto-context clearing, and turn-outcome inspection are shared
/// with every other handler subsystem, so they are re-exported here instead of
/// being duplicated per subsystem.
pub(super) use crate::handlers::common::{clear_auto_context, emit, turn_applied_edits};

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

/// Emits a `model/error` event for a failed plan step.
pub(super) fn report_step_failure(state: &Arc<AppState>, operation_id: &str, message: &str) {
    emit(
        &state.out,
        events::MODEL_ERROR,
        serde_json::to_value(ModelErrorEvent {
            operation_id: operation_id.to_string(),
            message: message.to_string(),
        })
        .ok(),
    );
}

/// Publishes the updated plan and refreshed plan list to the client.
///
/// The refreshed plan list is read through the blocking pool so the handler
/// never performs disk I/O on an async worker.
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

/// Persists a new status for a plan step and publishes the change.
///
/// `PlanStorage` serializes through a process-global mutex and writes to disk,
/// so persisting and reloading the plan must run on the blocking pool rather
/// than on an async worker.
pub(super) async fn set_step_status(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    status: PlanItemStatus,
) {
    let update_root = repo_root.to_path_buf();
    let plan_id_owned = plan_id.to_string();
    let step_id_owned = step_id.to_string();
    let updated = offload_plan(move || {
        PlanStorage::update_item(&update_root, &plan_id_owned, &step_id_owned, Some(status))
    })
    .await;

    if let Ok(Some(plan)) = updated {
        publish_plan(state, repo_root, plan, plan_id).await;
    }
}

/// Runs a context mutation atomically under the exclusive context write lock.
///
/// The single implementation lives in `crate::handlers`; re-exporting it here
/// keeps plan-side call sites unchanged while removing the duplicated body that
/// cloned the manager and overwrote the shared state afterwards.
pub(super) use crate::handlers::with_context_mutation;

/// Returns the declared dependencies of a step that are not yet DONE.
///
/// A step may execute only once every `depends_on` identifier refers to a step
/// whose status is `Done`; an unknown identifier is treated as unmet so a typo
/// cannot silently unlock a step. This re-check complements the scope
/// evaluation performed when the batch starts.
pub(super) fn unmet_dependencies(plan: &Plan, item: &PlanItem) -> Vec<String> {
    item.depends_on
        .iter()
        .filter(|dep| {
            plan.find_item(dep.as_str())
                .map(|dependency| dependency.status != PlanItemStatus::Done)
                .unwrap_or(true)
        })
        .map(|dep| format!("#{}", dep))
        .collect()
}
