//! Plan document RPC adaptation; selection and mutation rules belong to core.
use super::common::{emit, publish_plan_list};
use crate::contracts::Project;
use crate::state::AppState;
use std::sync::Arc;
#[cfg(test)]
#[cfg(test)]
use tauqe_core::plan::storage::PlanStorage;
use tauqe_protocol::{
    errors, events, PlanClearHorizonParams, PlanClearHorizonResult, PlanDeleteParams,
    PlanGetParams, PlanGetResult, PlanListResult, PlanSaveParams, PlanSetActiveParams,
    PlanUpdateItemParams, PlanUpdatedEvent, Request, Response,
};
fn document_error(
    id: tauqe_protocol::RequestId,
    fallback: &str,
    error: tauqe_core::api::DocumentError,
) -> Response {
    use tauqe_core::api::DocumentError;
    let code = match &error {
        DocumentError::InvalidPlanId => errors::INVALID_PLAN_ID,
        DocumentError::InvalidPlanTree(_) => errors::INVALID_PLAN_TREE,
        DocumentError::MissingReviewId => errors::INVALID_PARAMS,
        DocumentError::Busy => errors::OPERATION_IN_PROGRESS,
        DocumentError::Storage(_) => fallback,
    };
    Response::err(id, code, error.to_string())
}
pub async fn handle_plan_list(req: Request, state: &Arc<AppState>) -> Response {
    let root = state.repo_root().await;
    match state.application.documents_at(&root).plan_list().await {
        Ok((plans, active_id)) => Response::ok_typed(
            req.id,
            &PlanListResult {
                plans: plans.into_iter().map(Project::project).collect(),
                active_id,
            },
        ),
        Err(error) => Response::err(req.id, errors::PLAN_LIST_FAILED, error),
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

    let root = state.repo_root().await;
    match state
        .application
        .documents_at(&root)
        .resolve_plan(target_id)
        .await
    {
        Ok(plan) => Response::ok_typed(
            req.id,
            &PlanGetResult {
                plan: plan.map(Project::project),
            },
        ),
        Err(error) => Response::err(req.id, errors::PLAN_GET_FAILED, error),
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

    let root = state.repo_root().await;
    let plan = match state
        .application
        .documents_at(&root)
        .save_plan(params.plan.project())
        .await
    {
        Ok(plan) => plan,
        Err(error) => return document_error(req.id, errors::PLAN_SAVE_FAILED, error),
    };
    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent {
            plan: plan.project(),
            is_save: true,
        })
        .ok(),
    );
    publish_plan_list(state, &root).await;
    Response::ok(req.id, serde_json::json!({"saved":true}))
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

    let root = state.repo_root().await;
    match state
        .application
        .documents_at(&root)
        .update_plan_item(
            &params.plan_id,
            &params.item_id,
            params.status.map(Project::project),
        )
        .await
    {
        Ok(Some(plan)) => {
            emit(
                &state.out,
                events::PLAN_UPDATED,
                serde_json::to_value(PlanUpdatedEvent {
                    plan: plan.clone().project(),
                    is_save: false,
                })
                .ok(),
            );
            publish_plan_list(state, &root).await;
            Response::ok(req.id, serde_json::json!({"plan":plan.project()}))
        }
        Ok(None) => Response::err(
            req.id,
            errors::NOT_FOUND,
            format!(
                "Item '{}' not found in plan '{}'",
                params.item_id, params.plan_id
            ),
        ),
        Err(error) => document_error(req.id, errors::PLAN_UPDATE_FAILED, error),
    }
}
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

    let root = state.repo_root().await;
    match state
        .application
        .documents_at(&root)
        .clear_plan_horizon(&params.plan_id)
        .await
    {
        Ok(Some(plan)) => {
            emit(
                &state.out,
                events::PLAN_UPDATED,
                serde_json::to_value(PlanUpdatedEvent {
                    plan: plan.clone().project(),
                    is_save: false,
                })
                .ok(),
            );
            publish_plan_list(state, &root).await;
            Response::ok_typed(
                req.id,
                &PlanClearHorizonResult {
                    plan: plan.project(),
                },
            )
        }
        Ok(None) => Response::err(
            req.id,
            errors::NOT_FOUND,
            format!("Plan '{}' not found", params.plan_id),
        ),
        Err(error) => document_error(req.id, errors::PLAN_CLEAR_FAILED, error),
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

    let root = state.repo_root().await;
    match state
        .application
        .documents_at(&root)
        .delete_plan(&params.id)
        .await
    {
        Ok(outcome) => {
            if let Some(update) = outcome.focus_update {
                state.publish_focus_update(update);
            }
            if outcome.deleted {
                publish_plan_list(state, &root).await;
            }
            Response::ok(req.id, serde_json::json!({"deleted":outcome.deleted}))
        }
        Err(error) => document_error(req.id, errors::PLAN_DELETE_FAILED, error),
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

    let root = state.repo_root().await;
    if let Err(error) = state
        .application
        .documents_at(&root)
        .set_active_plan(params.id.clone())
        .await
    {
        return document_error(req.id, errors::PLAN_SET_ACTIVE_FAILED, error);
    }
    // Publish the explicitly requested marker, retaining its wire spelling.
    if let Ok(plans) = state.application.documents_at(&root).plans().await {
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(tauqe_protocol::PlanListChangedEvent {
                plans: plans.into_iter().map(Project::project).collect(),
                active_id: params.id.clone(),
            })
            .ok(),
        );
    }
    Response::ok(req.id, serde_json::json!({"active_id":params.id}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::{scratch_root, snapshot_tree};
    use crate::transport::OutChannel;
    use tauqe_core::config::AppConfig;
    use tauqe_core::plan::Plan;
    use tauqe_protocol::RequestId;

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
        let plan = Plan::from_parts(
            "plan-1".to_string(),
            "Read Only".to_string(),
            None,
            0,
            0,
            Vec::new(),
            Vec::new(),
        );
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
            let plan = Plan::from_parts(
                id.to_string(),
                id.to_string(),
                None,
                0,
                0,
                Vec::new(),
                Vec::new(),
            );
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
