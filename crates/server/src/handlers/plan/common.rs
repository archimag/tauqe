//! Shared helpers for the plan handler subsystem.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_protocol::{
    events, Event, ModelErrorEvent, Plan, PlanItem, PlanItemStatus, PlanListChangedEvent,
    PlanUpdatedEvent,
};

use crate::state::AppState;
use crate::transport::OutChannel;

/// Emits a server event with an optional serialized parameter payload.
pub(super) fn emit(out: &OutChannel, method: &str, params: Option<serde_json::Value>) {
    out.send_event(&Event {
        method: method.to_string(),
        params,
    });
}

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
pub(super) fn publish_plan(state: &Arc<AppState>, repo_root: &Path, plan: Plan, active_id: &str) {
    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent { plan }).ok(),
    );
    if let Ok(plans) = PlanStorage::load_all(repo_root) {
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
pub(super) fn set_step_status(
    state: &Arc<AppState>,
    repo_root: &Path,
    plan_id: &str,
    step_id: &str,
    status: PlanItemStatus,
) {
    if let Ok(Some(plan)) = PlanStorage::update_item(repo_root, plan_id, step_id, Some(status)) {
        publish_plan(state, repo_root, plan, plan_id);
    }
}

/// Clears the transient auto context layer so a retried attempt starts from a
/// clean, deterministic basis, then notifies clients of the new snapshot.
pub(super) async fn clear_auto_context(state: &Arc<AppState>) {
    let new_ctx_state = {
        let mut ctx = state.context.write().await;
        ctx.clear_auto();
        ctx.get_state()
    };
    emit(
        &state.out,
        events::CONTEXT_CHANGED,
        Some(serde_json::json!({ "state": new_ctx_state })),
    );
}

/// Whether a completed turn actually applied code edits to disk.
///
/// Plan steps ask the model to modify code: explanatory text alone, even when
/// accompanied by a terminal success tag, must not satisfy the step contract.
pub(super) fn turn_applied_edits(turn: &crate::handlers::model::ModelTurnResult) -> bool {
    match &turn.result {
        Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) => {
            outcome.applied && outcome.error.is_none()
        }
        _ => false,
    }
}

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
