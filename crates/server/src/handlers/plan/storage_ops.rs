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
    let active_missing = active_id.is_none();

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

    if target_id.is_none() && active_missing {
        if let Some(id) = plan_id.clone() {
            let root = repo_root.clone();
            let _ =
                offload_plan(move || PlanStorage::save_active_id(&root, Some(id.as_str()))).await;
        }
    }

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
