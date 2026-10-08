use std::sync::Arc;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_protocol::{
    events, Event, PlanDeleteParams, PlanGetParams, PlanGetResult, PlanListChangedEvent,
    PlanListResult, PlanSaveParams, PlanSetActiveParams, PlanUpdateItemParams, PlanUpdatedEvent,
    Request, Response,
};

use crate::state::AppState;
use crate::transport::OutChannel;

fn emit(out: &OutChannel, method: &str, params: Option<serde_json::Value>) {
    out.send_event(&Event {
        method: method.to_string(),
        params,
    });
}

pub async fn handle_plan_list(req: Request, state: &Arc<AppState>) -> Response {
    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match PlanStorage::list_plans(&repo_root) {
        Ok(plans) => {
            let mut active_id = PlanStorage::load_active_id(&repo_root).ok().flatten();
            if active_id.is_none() {
                if let Some(first) = plans.first() {
                    active_id = Some(first.id.clone());
                    let _ = PlanStorage::save_active_id(&repo_root, Some(first.id.as_str()));
                }
            }
            Response::ok_typed(req.id, &PlanListResult { plans, active_id })
        }
        Err(err) => Response::err(req.id, "PLAN_LIST_FAILED", format!("{:#}", err)),
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
    let plan_id = match &target_id {
        Some(id) => Some(id.clone()),
        None => PlanStorage::load_active_id(&repo_root)
            .ok()
            .flatten()
            .or_else(|| {
                PlanStorage::list_plans(&repo_root)
                    .ok()
                    .and_then(|plans| plans.into_iter().next().map(|p| p.id))
            }),
    };

    if target_id.is_none() {
        if let Ok(None) = PlanStorage::load_active_id(&repo_root) {
            if let Some(ref id) = plan_id {
                let _ = PlanStorage::save_active_id(&repo_root, Some(id.as_str()));
            }
        }
    }

    match plan_id {
        Some(id) => match PlanStorage::load_plan(&repo_root, &id) {
            Ok(plan) => Response::ok_typed(req.id, &PlanGetResult { plan }),
            Err(err) => Response::err(req.id, "PLAN_GET_FAILED", format!("{:#}", err)),
        },
        None => Response::ok_typed(req.id, &PlanGetResult { plan: None }),
    }
}

pub async fn handle_plan_save(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanSaveParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid plan payload"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    if let Err(err) = PlanStorage::save_plan(&repo_root, &params.plan) {
        return Response::err(req.id, "PLAN_SAVE_FAILED", format!("{:#}", err));
    }

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent {
            plan: params.plan.clone(),
        })
        .ok(),
    );

    if let Ok(plans) = PlanStorage::list_plans(&repo_root) {
        let active_id = PlanStorage::load_active_id(&repo_root).ok().flatten();
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
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid update item params"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match PlanStorage::update_item(
        &repo_root,
        &params.plan_id,
        &params.item_id,
        params.status,
        params.checked,
    ) {
        Ok(Some(plan)) => {
            emit(
                &state.out,
                events::PLAN_UPDATED,
                serde_json::to_value(PlanUpdatedEvent {
                    plan: plan.clone(),
                })
                .ok(),
            );
            if let Ok(plans) = PlanStorage::list_plans(&repo_root) {
                let active_id = PlanStorage::load_active_id(&repo_root).ok().flatten();
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
            "NOT_FOUND",
            format!("Item '{}' not found in plan '{}'", params.item_id, params.plan_id),
        ),
        Err(err) => Response::err(req.id, "PLAN_UPDATE_FAILED", format!("{:#}", err)),
    }
}

pub async fn handle_plan_delete(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanDeleteParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => return Response::err(req.id, "INVALID_PARAMS", "Missing or invalid plan ID"),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    match PlanStorage::delete_plan(&repo_root, &params.id) {
        Ok(deleted) => {
            if deleted {
                if let Ok(plans) = PlanStorage::list_plans(&repo_root) {
                    let active_id = PlanStorage::load_active_id(&repo_root).ok().flatten();
                    emit(
                        &state.out,
                        events::PLAN_LIST_CHANGED,
                        serde_json::to_value(PlanListChangedEvent { plans, active_id }).ok(),
                    );
                }
            }
            Response::ok(req.id, serde_json::json!({ "deleted": deleted }))
        }
        Err(err) => Response::err(req.id, "PLAN_DELETE_FAILED", format!("{:#}", err)),
    }
}

pub async fn handle_plan_set_active(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanSetActiveParams = match req.params {
        Some(p) => serde_json::from_value(p).unwrap_or_default(),
        None => PlanSetActiveParams::default(),
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    if let Err(err) = PlanStorage::save_active_id(&repo_root, params.id.as_deref()) {
        return Response::err(req.id, "PLAN_SET_ACTIVE_FAILED", format!("{:#}", err));
    }

    if let Ok(plans) = PlanStorage::list_plans(&repo_root) {
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
