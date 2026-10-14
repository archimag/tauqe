use std::sync::Arc;

use tauqe_protocol::{
    errors, events, ContextAddParams, ContextAddPatternParams, ContextAddPatternResult,
    ContextClearParams, ContextLayer, ContextRemoveParams, ContextSetAccessParams, Event, Request,
    RequestId, Response, ResponseError,
};

use crate::state::AppState;

/// Rejects a context mutation while a turn, plan batch, or review batch is active.
///
/// Every turn reasons over a clone of the context and merges that clone back when
/// it finishes. A mutation applied in between would be overwritten (or resurrected
/// from the turn's frozen snapshot), so mutations are refused for the duration of
/// the operation instead of being silently lost.
fn reject_if_busy(req_id: &RequestId, state: &Arc<AppState>) -> Option<Response> {
    if state.is_operation_active() {
        Some(Response::err(
            req_id.clone(),
            errors::OPERATION_IN_PROGRESS,
            "Context cannot be modified while a turn or batch is running",
        ))
    } else {
        None
    }
}

pub async fn handle_context_get(req: Request, state: &Arc<AppState>) -> Response {
    let ctx = state.context.read().await;
    let ctx_state = ctx.get_state();
    Response::ok_typed(req.id, &ctx_state)
}

pub async fn handle_context_add(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = reject_if_busy(&req.id, state) {
        return resp;
    }

    let params: ContextAddParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: errors::INVALID_PARAMS.to_string(),
                    message: "Missing or invalid path/access".to_string(),
                    data: None,
                }),
            };
        }
    };

    // Honor the requested layer instead of always defaulting to the User layer.
    // The Pinned layer stays configuration-only, so it is refused here.
    let layer = params.layer.unwrap_or(ContextLayer::User);
    if layer == ContextLayer::Pinned {
        return Response::err(
            req.id,
            errors::PINNED_PROTECTED,
            "Pinned files are managed by project configuration and cannot be added manually",
        );
    }

    let ctx_state = {
        let mut ctx = state.context.write().await;
        match ctx.add_file_to_layer(&params.path, params.access, layer) {
            Ok(_) => ctx.get_state(),
            Err(err) => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: errors::ADD_FAILED.to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                };
            }
        }
    };

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    Response::ok_typed(req.id, &ctx_state)
}

pub async fn handle_context_add_pattern(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = reject_if_busy(&req.id, state) {
        return resp;
    }

    let params: ContextAddPatternParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: errors::INVALID_PARAMS.to_string(),
                        message: "Missing or invalid pattern/access".to_string(),
                        data: None,
                    }),
                };
            }
        };

    // Read the configurable batch limit before taking the context write lock so
    // the two locks are never held at the same time.
    let max_batch = {
        let guard = state.config.lock().await;
        guard.config.context.max_pattern_batch
    };

    let repo_root = state.repo_root().await;
    let available_files = super::offload(move || {
        tauqe_core::git::list_repository_files(Some(repo_root.as_path()))
    })
    .await
    .map(|result| result.unwrap_or_default())
    .unwrap_or_default();

    let (added_count, added_tokens, truncated, ctx_state) = {
        let mut ctx = state.context.write().await;
        match ctx.add_files_by_pattern(
            &params.pattern,
            params.access,
            &available_files,
            max_batch,
        ) {
            Ok(outcome) => {
                let state = ctx.get_state();
                (
                    outcome.added_items.len(),
                    outcome.added_tokens,
                    outcome.truncated,
                    state,
                )
            }
            Err(err) => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: errors::ADD_PATTERN_FAILED.to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                };
            }
        }
    };

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    let result = ContextAddPatternResult {
        added_count,
        added_tokens,
        truncated,
        state: ctx_state,
    };

    Response::ok_typed(req.id, &result)
}

pub async fn handle_context_remove(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = reject_if_busy(&req.id, state) {
        return resp;
    }

    let params: ContextRemoveParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: errors::INVALID_PARAMS.to_string(),
                    message: "Missing or invalid path".to_string(),
                    data: None,
                }),
            };
        }
    };

    let ctx_state = {
        let mut ctx = state.context.write().await;
        match ctx.remove_file(&params.path) {
            Ok(_) => ctx.get_state(),
            Err(err) => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: errors::REMOVE_FAILED.to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                };
            }
        }
    };

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    Response::ok_typed(req.id, &ctx_state)
}

pub async fn handle_context_set_access(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = reject_if_busy(&req.id, state) {
        return resp;
    }

    let params: ContextSetAccessParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: errors::INVALID_PARAMS.to_string(),
                        message: "Missing or invalid parameters".to_string(),
                        data: None,
                    }),
                };
            }
        };

    let ctx_state = {
        let mut ctx = state.context.write().await;
        match ctx.set_access(&params.path, params.access) {
            Ok(_) => ctx.get_state(),
            Err(err) => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: errors::SET_ACCESS_FAILED.to_string(),
                        message: err.to_string(),
                        data: None,
                    }),
                };
            }
        }
    };

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    Response::ok_typed(req.id, &ctx_state)
}

pub async fn handle_context_clear(req: Request, state: &Arc<AppState>) -> Response {
    if let Some(resp) = reject_if_busy(&req.id, state) {
        return resp;
    }

    // Params are optional (a bare clear clears the User and Auto layers), but a
    // present payload must parse cleanly: a malformed payload must never silently
    // degrade into a destructive "clear all". The Pinned layer is protected and is
    // refused explicitly instead of falling through to the default branch.
    let params: ContextClearParams = match req.params {
        Some(p) => match serde_json::from_value(p) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid clear parameters: {}", err),
                );
            }
        },
        None => ContextClearParams::default(),
    };

    if params.layer == Some(ContextLayer::Pinned) {
        return Response::err(
            req.id,
            errors::PINNED_PROTECTED,
            "Pinned files are protected and cannot be cleared",
        );
    }

    let ctx_state = {
        let mut ctx = state.context.write().await;
        match params.layer {
            Some(ContextLayer::Auto) => ctx.clear_auto(),
            Some(ContextLayer::User) => ctx.clear_user(),
            _ => ctx.clear(),
        }
        ctx.get_state()
    };

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    Response::ok_typed(req.id, &ctx_state)
}
