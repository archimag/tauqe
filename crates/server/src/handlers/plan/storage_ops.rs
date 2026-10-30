//! Plan CRUD RPC handlers.

use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_protocol::discussion::DiscussionTarget;
use tauqe_protocol::{
    errors, events, PlanClearHorizonParams, PlanClearHorizonResult, PlanDeleteParams,
    PlanGetParams, PlanGetResult, PlanListChangedEvent, PlanListResult, PlanSaveParams,
    PlanSetActiveParams, PlanUpdateItemParams, PlanUpdatedEvent, Request, Response,
};

use crate::handlers::common::release_focus_anchored_on;
use crate::state::AppState;

use super::common::{emit, find_duplicate_item_ids, offload_plan};

/// Rejects plan IDs that are empty or contain path separators.
///
/// `PlanStorage` maps a plan ID onto `<repo>/.tauqe/plans/<id>.json` and sanitizes it
/// before building the path, so directory traversal is already blocked. Rejecting
/// separator-bearing and empty IDs at the boundary keeps the persisted ID and its file
/// name consistent and gives a deterministic error.
fn is_valid_plan_id(id: &str) -> bool {
    !id.trim().is_empty() && !id.contains('/') && !id.contains('\\')
}

/// Lists every stored plan together with the persisted active pointer.
pub async fn handle_plan_list(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match offload_plan(move || {
        let plans = PlanStorage::load_all(&repo_root)?;
        let active_id = PlanStorage::load_active_id(&repo_root).ok().flatten();
        Ok((plans, active_id))
    })
    .await
    {
        Ok((plans, active_id)) => Response::ok_typed(req.id, &PlanListResult { plans, active_id }),
        Err(err) => Response::err(req.id, errors::PLAN_LIST_FAILED, err),
    }
}

/// Resolves a plan for display: the explicitly named one, the active one, or
/// the first stored plan.
///
/// Resolution is a pure read. It never persists the plan it fell back to, so
/// reading never rewrites which plan the client renders as active; promoting
/// a plan is an explicit mutation through `plan/setActive`.
pub async fn handle_plan_get(req: Request, state: &Arc<AppState>) -> Response {
    // Strict wire-contract parse: a present payload must decode through the
    // typed DTO, so a malformed `id` answers INVALID_PARAMS instead of
    // silently resolving through the active plan (Conventions §2.5). An
    // absent payload or an empty id still means "the active plan", which is
    // how the client requests the current plan.
    let target_id = match req.params {
        None => None,
        Some(payload) => match serde_json::from_value::<PlanGetParams>(payload) {
            Ok(params) if !params.id.trim().is_empty() => Some(params.id),
            Ok(_) => None,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid plan get parameters: {}", err),
                );
            }
        },
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();

    let active_id = {
        let root = repo_root.clone();
        offload_plan(move || PlanStorage::load_active_id(&root))
            .await
            .unwrap_or(None)
    };
    // Resolution never persists the fallback it picked: promoting a plan is an
    // explicit mutation, so a read RPC leaves the stored active pointer as it
    // found it.
    let plan_id = match &target_id {
        Some(id) => Some(id.clone()),
        None => match active_id {
            Some(id) => Some(id),
            None => {
                let root = repo_root.clone();
                offload_plan(move || {
                    PlanStorage::list_plans(&root)
                        .map(|plans| plans.into_iter().next().map(|p| p.id))
                })
                .await
                .unwrap_or(None)
            }
        },
    };

    match plan_id {
        Some(id) => {
            let root = repo_root.clone();
            match offload_plan(move || PlanStorage::load_plan(&root, &id)).await {
                Ok(plan) => Response::ok_typed(req.id, &PlanGetResult { plan }),
                Err(err) => Response::err(req.id, errors::PLAN_GET_FAILED, err),
            }
        }
        None => Response::ok_typed(req.id, &PlanGetResult { plan: None }),
    }
}

pub async fn handle_plan_save(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot save a plan while a model operation is in progress",
    ) {
        return resp;
    }

    let params: PlanSaveParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid plan payload",
            )
        }
    };

    if !is_valid_plan_id(&params.plan.id) {
        return Response::err(
            req.id,
            errors::INVALID_PLAN_ID,
            "Plan ID must be non-empty and must not contain path separators",
        );
    }
    if let Some(duplicate) = find_duplicate_item_ids(&params.plan.items) {
        return Response::err(
            req.id,
            errors::INVALID_PLAN_TREE,
            format!(
                "Plan contains a duplicate or empty step ID: '{}'",
                duplicate
            ),
        );
    }

    // Normalize before persisting so the emitted event matches the stored plan.
    let mut plan = params.plan;
    plan.normalize_leaf_files();

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let save_root = repo_root.clone();
    let plan_to_save = plan.clone();
    if let Err(err) = offload_plan(move || PlanStorage::save_plan(&save_root, &plan_to_save)).await
    {
        return Response::err(req.id, errors::PLAN_SAVE_FAILED, err);
    }

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent {
            plan: plan.clone(),
            is_save: true,
        })
        .ok(),
    );

    let list_root = repo_root.clone();
    if let Ok((plans, active_id)) = offload_plan(move || {
        let plans = PlanStorage::load_all(&list_root)?;
        let active_id = PlanStorage::load_active_id(&list_root).ok().flatten();
        Ok((plans, active_id))
    })
    .await
    {
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent { plans, active_id }).ok(),
        );
    }

    Response::ok(req.id, serde_json::json!({ "saved": true }))
}

pub async fn handle_plan_update_item(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot update a plan item while a model operation is in progress",
    ) {
        return resp;
    }

    let params: PlanUpdateItemParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid update item params",
            )
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let update_root = repo_root.clone();
    let plan_id = params.plan_id.clone();
    let item_id = params.item_id.clone();
    let status = params.status;
    let updated = offload_plan(move || {
        PlanStorage::update_item(&update_root, &plan_id, &item_id, status, None)
    })
    .await;
    match updated {
        Ok(Some(plan)) => {
            emit(
                &state.out,
                events::PLAN_UPDATED,
                serde_json::to_value(PlanUpdatedEvent {
                    plan: plan.clone(),
                    is_save: false,
                })
                .ok(),
            );
            let list_root = repo_root.clone();
            if let Ok((plans, active_id)) = offload_plan(move || {
                let plans = PlanStorage::load_all(&list_root)?;
                let active_id = PlanStorage::load_active_id(&list_root).ok().flatten();
                Ok((plans, active_id))
            })
            .await
            {
                emit(
                    &state.out,
                    events::PLAN_LIST_CHANGED,
                    serde_json::to_value(PlanListChangedEvent { plans, active_id }).ok(),
                );
            }
            Response::ok(req.id, serde_json::json!({ "plan": plan }))
        }
        Ok(None) => Response::err(
            req.id,
            errors::NOT_FOUND,
            format!(
                "Item '{}' not found in plan '{}'",
                params.item_id, params.plan_id
            ),
        ),
        Err(err) => Response::err(req.id, errors::PLAN_UPDATE_FAILED, err),
    }
}

/// Discards the whole implementation horizon while preserving the concept.
///
/// Only the step list is cleared; the `concept` tree of requirements and
/// invariants survives untouched. The `Done` immutability guard is intentionally
/// bypassed here, so the client is responsible for demanding the strict
/// two-tier confirmation before dispatching this request; the server never
/// silently downgrades an authorized reset.
pub async fn handle_plan_clear_horizon(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot clear a plan horizon while a model operation is in progress",
    ) {
        return resp;
    }

    let params: PlanClearHorizonParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Missing or invalid clear horizon payload",
                );
            }
        };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let clear_root = repo_root.clone();
    let plan_id = params.plan_id.clone();
    match offload_plan(move || PlanStorage::clear_horizon(&clear_root, &plan_id)).await {
        Ok(Some(plan)) => {
            emit(
                &state.out,
                events::PLAN_UPDATED,
                serde_json::to_value(PlanUpdatedEvent {
                    plan: plan.clone(),
                    is_save: false,
                })
                .ok(),
            );
            let list_root = repo_root.clone();
            if let Ok((plans, active_id)) = offload_plan(move || {
                let plans = PlanStorage::load_all(&list_root)?;
                let active_id = PlanStorage::load_active_id(&list_root).ok().flatten();
                Ok((plans, active_id))
            })
            .await
            {
                emit(
                    &state.out,
                    events::PLAN_LIST_CHANGED,
                    serde_json::to_value(PlanListChangedEvent { plans, active_id }).ok(),
                );
            }
            Response::ok_typed(req.id, &PlanClearHorizonResult { plan })
        }
        Ok(None) => Response::err(
            req.id,
            errors::NOT_FOUND,
            format!("Plan '{}' not found", params.plan_id),
        ),
        Err(err) => Response::err(req.id, errors::PLAN_CLEAR_FAILED, err),
    }
}

pub async fn handle_plan_delete(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot delete a plan while a model operation is in progress",
    ) {
        return resp;
    }

    let params: PlanDeleteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid plan ID"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let delete_root = repo_root.clone();
    let plan_id = params.id.clone();
    match offload_plan(move || PlanStorage::delete_plan(&delete_root, &plan_id)).await {
        Ok(deleted) => {
            if deleted {
                // Discussion Focus invariant: an anchor may never address a
                // deleted entity. A focus bound to the removed plan is
                // released together with it through the shared release helper,
                // which broadcasts the release so every client drops the dead
                // anchor instead of rendering it.
                release_focus_anchored_on(
                    state,
                    &DiscussionTarget::Plan {
                        id: params.id.clone(),
                    },
                )
                .await;

                let list_root = repo_root.clone();
                if let Ok((plans, active_id)) = offload_plan(move || {
                    let plans = PlanStorage::load_all(&list_root)?;
                    let active_id = PlanStorage::load_active_id(&list_root).ok().flatten();
                    Ok((plans, active_id))
                })
                .await
                {
                    emit(
                        &state.out,
                        events::PLAN_LIST_CHANGED,
                        serde_json::to_value(PlanListChangedEvent { plans, active_id }).ok(),
                    );
                }
            }
            Response::ok(req.id, serde_json::json!({ "deleted": deleted }))
        }
        Err(err) => Response::err(req.id, errors::PLAN_DELETE_FAILED, err),
    }
}

pub async fn handle_plan_set_active(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot change the active plan while a model operation is in progress",
    ) {
        return resp;
    }

    // A present payload must parse cleanly: a silent fallback to the default would
    // clear the active plan when the payload is malformed.
    let params: PlanSetActiveParams = match req.params {
        Some(p) => match serde_json::from_value(p) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid set active parameters: {}", err),
                );
            }
        },
        None => PlanSetActiveParams::default(),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let save_root = repo_root.clone();
    let active = params.id.clone();
    if let Err(err) =
        offload_plan(move || PlanStorage::save_active_id(&save_root, active.as_deref())).await
    {
        return Response::err(req.id, errors::PLAN_SET_ACTIVE_FAILED, err);
    }

    let list_root = repo_root.clone();
    if let Ok(plans) = offload_plan(move || PlanStorage::load_all(&list_root)).await {
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent {
                plans,
                active_id: params.id.clone(),
            })
            .ok(),
        );
    }

    Response::ok(req.id, serde_json::json!({ "active_id": params.id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::{scratch_root, snapshot_tree};
    use crate::transport::OutChannel;
    use tauqe_core::config::AppConfig;
    use tauqe_protocol::{Plan, RequestId};

    /// Builds the harness state a read-path regression test runs against.
    fn state_over(root: std::path::PathBuf) -> Arc<AppState> {
        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(AppConfig::default(), root, history_sender, out)
    }

    /// Builds an id-less `plan/get`, the request that used to promote an active
    /// plan as a side effect.
    fn plan_get_request() -> Request {
        Request {
            id: RequestId::Number(1),
            method: tauqe_protocol::methods::PLAN_GET.to_string(),
            params: None,
        }
    }

    /// Resolves a stored plan while nothing is active, and persists nothing.
    #[tokio::test]
    async fn plan_get_resolves_without_persisting_an_active_pointer() {
        let root = scratch_root("plan-get-read-only");
        let plan = Plan {
            id: "plan-1".to_string(),
            title: "Read Only".to_string(),
            ..Default::default()
        };
        PlanStorage::save_plan(&root, &plan).expect("save the stored plan");
        // Persisting a plan promotes the first stored one so a client always
        // has a marker to render. The read path is what must never promote,
        // so the pointer is cleared first and has to stay cleared.
        PlanStorage::save_active_id(&root, None).expect("clear the active pointer");
        // The scratch repository and the harness state are materialized before
        // the snapshot, so the comparison captures exactly what the request
        // itself changed.
        let state = state_over(root.clone());
        let before = snapshot_tree(&root);

        let response = handle_plan_get(plan_get_request(), &state).await;
        assert!(
            response.error.is_none(),
            "plan/get failed: {:?}",
            response.error
        );
        let result: PlanGetResult =
            serde_json::from_value(response.result.expect("plan/get result"))
                .expect("plan/get result deserializes");
        assert_eq!(
            result.plan.as_ref().map(|plan| plan.id.as_str()),
            Some("plan-1"),
            "a stored plan is still resolved without an active pointer"
        );
        assert_eq!(
            snapshot_tree(&root),
            before,
            "a read RPC must leave every persisted file byte-identical"
        );
        let stored = PlanStorage::load_active_id(&root).expect("read the active pointer");
        assert!(
            stored.is_none(),
            "plan/get must never promote an active plan as a read side effect"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Resolves the active plan and keeps the stored pointer untouched, even
    /// though another stored plan is enumerated first.
    #[tokio::test]
    async fn plan_get_preserves_an_existing_active_pointer() {
        let root = scratch_root("plan-get-preserve");
        for id in ["plan-a", "plan-b"] {
            let plan = Plan {
                id: id.to_string(),
                title: id.to_string(),
                ..Default::default()
            };
            PlanStorage::save_plan(&root, &plan).expect("save a stored plan");
        }
        PlanStorage::save_active_id(&root, Some("plan-b")).expect("store the active id");
        let state = state_over(root.clone());
        let before = snapshot_tree(&root);

        let response = handle_plan_get(plan_get_request(), &state).await;
        assert!(
            response.error.is_none(),
            "plan/get failed: {:?}",
            response.error
        );
        let result: PlanGetResult =
            serde_json::from_value(response.result.expect("plan/get result"))
                .expect("plan/get result deserializes");
        assert_eq!(
            result.plan.as_ref().map(|plan| plan.id.as_str()),
            Some("plan-b"),
            "the active plan wins over the first enumerated plan"
        );
        assert_eq!(
            snapshot_tree(&root),
            before,
            "a read RPC must not rewrite the persisted active pointer"
        );
        let stored = PlanStorage::load_active_id(&root).expect("read the active pointer");
        assert_eq!(
            stored,
            Some("plan-b".to_string()),
            "the stored pointer still names the active plan"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
