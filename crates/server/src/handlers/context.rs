use std::path::Path;
use std::sync::Arc;

use tauqe_protocol::{
    events, ContextAddParams, ContextAddPatternParams, ContextAddPatternResult,
    ContextClearParams, ContextLayer, ContextRemoveParams, ContextSetAccessParams, Event,
    Request, Response, ResponseError,
};

use crate::state::AppState;

pub async fn handle_context_get(req: Request, state: &Arc<AppState>) -> Response {
    let ctx = state.context.read().await;
    let ctx_state = ctx.get_state();
    Response {
        id: req.id,
        result: serde_json::to_value(ctx_state).ok(),
        error: None,
    }
}

pub async fn handle_context_add(req: Request, state: &Arc<AppState>) -> Response {
    let params: ContextAddParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: "INVALID_PARAMS".to_string(),
                    message: "Missing or invalid path/access".to_string(),
                    data: None,
                }),
            };
        }
    };

    let ctx_state = {
        let mut ctx = state.context.write().await;
        match ctx.add_file(&params.path, params.access) {
            Ok(_) => ctx.get_state(),
            Err(err) => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "ADD_FAILED".to_string(),
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

    Response {
        id: req.id,
        result: serde_json::to_value(ctx_state).ok(),
        error: None,
    }
}

pub async fn handle_context_add_pattern(req: Request, state: &Arc<AppState>) -> Response {
    let params: ContextAddPatternParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "INVALID_PARAMS".to_string(),
                        message: "Missing or invalid pattern/access".to_string(),
                        data: None,
                    }),
                };
            }
        };

    let (added_count, added_tokens, ctx_state) = {
        let repo_state = tauqe_core::git::get_repository_state(None);
        let repo_dir = Path::new(&repo_state.root);
        let available_files =
            tauqe_core::git::list_repository_files(Some(repo_dir)).unwrap_or_default();

        let mut ctx = state.context.write().await;
        match ctx.add_files_by_pattern(
            &params.pattern,
            params.access,
            &available_files,
        ) {
            Ok((items, tokens)) => {
                let state = ctx.get_state();
                (items.len(), tokens, state)
            }
            Err(err) => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "ADD_PATTERN_FAILED".to_string(),
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
        state: ctx_state,
    };

    Response {
        id: req.id,
        result: serde_json::to_value(result).ok(),
        error: None,
    }
}

pub async fn handle_context_remove(req: Request, state: &Arc<AppState>) -> Response {
    let params: ContextRemoveParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: "INVALID_PARAMS".to_string(),
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
                        code: "REMOVE_FAILED".to_string(),
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

    Response {
        id: req.id,
        result: serde_json::to_value(ctx_state).ok(),
        error: None,
    }
}

pub async fn handle_context_set_access(req: Request, state: &Arc<AppState>) -> Response {
    let params: ContextSetAccessParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response {
                    id: req.id,
                    result: None,
                    error: Some(ResponseError {
                        code: "INVALID_PARAMS".to_string(),
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
                        code: "SET_ACCESS_FAILED".to_string(),
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

    Response {
        id: req.id,
        result: serde_json::to_value(ctx_state).ok(),
        error: None,
    }
}

pub async fn handle_context_clear(req: Request, state: &Arc<AppState>) -> Response {
    let params: Option<ContextClearParams> =
        req.params.and_then(|p| serde_json::from_value(p).ok());

    let ctx_state = {
        let mut ctx = state.context.write().await;
        if let Some(ContextClearParams {
            layer: Some(ContextLayer::Auto),
        }) = params
        {
            ctx.clear_auto();
        } else {
            ctx.clear();
        }
        ctx.get_state()
    };

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    Response {
        id: req.id,
        result: serde_json::to_value(ctx_state).ok(),
        error: None,
    }
}
