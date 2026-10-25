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
/// `blocked_reason` carries the diagnostic report of the semantic blocker that
/// stopped the step. `PlanStorage` persists it only for
/// [`PlanItemStatus::Blocked`] and drops it on every other transition, so a
/// resumed step never keeps describing a blocker that no longer holds.
///
/// `PlanStorage` serializes through a process-global mutex and writes to disk,
/// so persisting and reloading the plan must run on the blocking pool rather
/// than on an async worker.
///
/// Best-effort variant: a persistence failure is journaled inside
/// [`set_step_status_checked`] and swallowed, which suits the `InProgress`
/// publication and the `Todo` reverts that accompany an already-reported
/// failure. Terminal transitions whose outcome must match the durable record
/// call the checked variant instead.
pub(super) async fn set_step_status(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    status: PlanItemStatus,
    blocked_reason: Option<&str>,
) {
    // This wrapper is the single sanctioned best-effort boundary: the failure
    // is already journaled by the checked variant, so discarding the result
    // here keeps the write observable without inventing a fresh operation id.
    let _ =
        set_step_status_checked(state, repo_root, plan_id, step_id, status, blocked_reason).await;
}

/// Persists a terminal plan step status and publishes the change.
///
/// A `blocked_reason` supplied with [`PlanItemStatus::Blocked`] is stored
/// beside the status, so the diagnostic survives the interrupted turn on disk
/// instead of being lost with the transient session.
///
/// Returns `Err` when the write does not land on disk — the storage error, or a
/// not-found diagnostic — so the caller reports the step as failed instead of
/// letting the batch outcome diverge from the persisted state.
pub(super) async fn set_step_status_checked(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    status: PlanItemStatus,
    blocked_reason: Option<&str>,
) -> Result<(), String> {
    let update_root = repo_root.to_path_buf();
    let plan_id_owned = plan_id.to_string();
    let step_id_owned = step_id.to_string();
    // The storage call runs on the blocking pool, so the report must outlive
    // the borrow of the caller's string.
    let blocked_reason_owned = blocked_reason.map(str::to_string);
    let updated = offload_plan(move || {
        PlanStorage::update_item(
            &update_root,
            &plan_id_owned,
            &step_id_owned,
            Some(status),
            blocked_reason_owned.as_deref(),
        )
    })
    .await;

    match updated {
        Ok(Some(plan)) => {
            publish_plan(state, repo_root, plan, plan_id).await;
            Ok(())
        }
        Ok(None) => {
            let message = format!(
                "Step '{}' not found in plan '{}'; status '{}' was not persisted",
                step_id, plan_id, status
            );
            tracing::warn!(
                plan_id = plan_id,
                step_id = step_id,
                status = %status,
                "{}",
                message
            );
            Err(message)
        }
        Err(err) => {
            tracing::warn!(
                plan_id = plan_id,
                step_id = step_id,
                status = %status,
                error = %err,
                "plan step status was not persisted"
            );
            Err(err)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::AppConfig;

    /// A terminal plan-step status write must surface a persistence failure
    /// instead of discarding it, so the batch outcome can never claim a `Done`
    /// step the durable record does not hold.
    #[tokio::test]
    async fn checked_step_status_surfaces_persistence_failure() {
        let root = std::env::temp_dir().join(format!(
            "tauqe-plan-persist-{}",
            crate::state::next_operation_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let (out, _rx) = crate::transport::OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let plan = Plan {
            id: "persist-plan".to_string(),
            title: "Persistence".to_string(),
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                status: PlanItemStatus::Todo,
                ..Default::default()
            }],
            ..Default::default()
        };
        PlanStorage::save_plan(&root, &plan).unwrap();

        // A healthy terminal write persists the status and reports success.
        set_step_status_checked(
            &state,
            &root,
            "persist-plan",
            "1",
            PlanItemStatus::Done,
            None,
        )
        .await
        .expect("healthy write succeeds");
        let stored = PlanStorage::load_plan(&root, "persist-plan")
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item("1").unwrap().status, PlanItemStatus::Done);

        // A corrupted plan file makes the next status write fail: the failure
        // must be returned so the caller reports a failed batch step instead
        // of a silent success.
        let plan_file = PlanStorage::plans_dir(&root).join("persist-plan.json");
        std::fs::write(&plan_file, b"{ not a valid plan }").unwrap();
        let err = set_step_status_checked(
            &state,
            &root,
            "persist-plan",
            "1",
            PlanItemStatus::Todo,
            None,
        )
        .await
        .expect_err("persistence failure must surface");
        assert!(err.contains("Failed to parse plan JSON"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
