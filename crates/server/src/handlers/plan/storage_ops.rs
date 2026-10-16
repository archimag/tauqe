//! Plan CRUD RPC handlers.

use std::sync::Arc;

use tauqe_core::plan::storage::PlanStorage;
use tauqe_protocol::{
    errors, events, PlanDeleteParams, PlanGetParams, PlanGetResult, PlanListChangedEvent,
    PlanListResult, PlanSaveParams, PlanSetActiveParams, PlanUpdateItemParams, PlanUpdatedEvent,
    Request, Response,
};

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
        Ok((plans, active_id)) => {
            Response::ok_typed(req.id, &PlanListResult { plans, active_id })
        }
        Err(err) => Response::err(req.id, errors::PLAN_LIST_FAILED, err),
    }
}

pub async fn handle_plan_get(req: Request, state: &Arc<AppState>) -> Response {
    let target_id = req.params.and_then(|p| {
        if let Ok(params) = serde_json::from_value::<PlanGetParams>(p.clone()) {
            if !params.id.trim().is_empty() {
                return Some(params.id);
            }
        }
        if let Ok(val) = serde_json::from_value::<serde_json::Value>(p) {
            if let Some(id_str) = val.get("id").and_then(|v| v.as_str()) {
                if !id_str.trim().is_empty() {
                    return Some(id_str.to_string());
                }
            }
        }
        None
    });

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
            let _ = offload_plan(move || PlanStorage::save_active_id(&root, Some(id.as_str()))).await;
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
    let params: PlanSaveParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid plan payload"),
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
            format!("Plan contains a duplicate or empty step ID: '{}'", duplicate),
        );
    }

    // Normalize before persisting so the emitted event matches the stored plan.
    let mut plan = params.plan;
    plan.normalize_leaf_files();

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let save_root = repo_root.clone();
    let plan_to_save = plan.clone();
    if let Err(err) = offload_plan(move || PlanStorage::save_plan(&save_root, &plan_to_save)).await {
        return Response::err(req.id, errors::PLAN_SAVE_FAILED, err);
    }

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent { plan: plan.clone() }).ok(),
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
    let params: PlanUpdateItemParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid update item params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let update_root = repo_root.clone();
    let plan_id = params.plan_id.clone();
    let item_id = params.item_id.clone();
    let status = params.status;
    let updated =
        offload_plan(move || PlanStorage::update_item(&update_root, &plan_id, &item_id, status))
            .await;
    match updated {
        Ok(Some(plan)) => {
            emit(
                &state.out,
                events::PLAN_UPDATED,
                serde_json::to_value(PlanUpdatedEvent {
                    plan: plan.clone(),
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
            format!("Item '{}' not found in plan '{}'", params.item_id, params.plan_id),
        ),
        Err(err) => Response::err(req.id, errors::PLAN_UPDATE_FAILED, err),
    }
}

pub async fn handle_plan_delete(req: Request, state: &Arc<AppState>) -> Response {
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
