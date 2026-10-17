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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::OutChannel;
    use tauqe_protocol::{methods, ContextAccess, ContextLayer, Request, RequestId};

    struct TestDir(std::path::PathBuf);
    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tauqe-test-context-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::create_dir_all(&path);
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_state(dir: &TestDir) -> (Arc<AppState>, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (out, rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.path().to_path_buf(),
            hist_tx,
            out,
        );
        (state, rx)
    }

    #[tokio::test]
    async fn test_handle_context_get() {
        let dir = TestDir::new();
        let (state, _rx) = test_state(&dir);

        let file_path = dir.path().join("lib.rs");
        std::fs::write(&file_path, "pub fn hello() {}\n").unwrap();
        state
            .context
            .write()
            .await
            .add_file("lib.rs", ContextAccess::ReadOnly)
            .unwrap();

        let req = Request {
            id: RequestId::Number(1),
            method: methods::CONTEXT_GET.to_string(),
            params: None,
        };
        let res = handle_context_get(req, &state).await;
        assert!(res.error.is_none());
        let ctx_state: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert_eq!(ctx_state.items.len(), 1);
        assert_eq!(ctx_state.items[0].path, "lib.rs");
    }

    #[tokio::test]
    async fn test_handle_context_add_and_validation() {
        let dir = TestDir::new();
        let (state, mut rx) = test_state(&dir);

        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "fn test() {}\n").unwrap();

        let make_req = |id: u64, params: Option<serde_json::Value>| Request {
            id: RequestId::Number(id),
            method: methods::CONTEXT_ADD.to_string(),
            params,
        };

        // Missing params -> INVALID_PARAMS
        let res = handle_context_add(make_req(1, None), &state).await;
        assert_eq!(res.error.unwrap().code, errors::INVALID_PARAMS);

        // Pinned layer -> PINNED_PROTECTED
        let pinned_params = serde_json::to_value(ContextAddParams {
            path: "test.rs".to_string(),
            access: ContextAccess::ReadOnly,
            layer: Some(ContextLayer::Pinned),
        })
        .ok();
        let res = handle_context_add(make_req(2, pinned_params), &state).await;
        assert_eq!(res.error.unwrap().code, errors::PINNED_PROTECTED);

        // Non-existent file -> ADD_FAILED
        let missing_params = serde_json::to_value(ContextAddParams {
            path: "missing.rs".to_string(),
            access: ContextAccess::ReadOnly,
            layer: None,
        })
        .ok();
        let res = handle_context_add(make_req(3, missing_params), &state).await;
        assert_eq!(res.error.unwrap().code, errors::ADD_FAILED);

        // Valid add
        let ok_params = serde_json::to_value(ContextAddParams {
            path: "test.rs".to_string(),
            access: ContextAccess::Editable,
            layer: Some(ContextLayer::User),
        })
        .ok();
        let res = handle_context_add(make_req(4, ok_params), &state).await;
        assert!(res.error.is_none());
        let ctx_state: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert_eq!(ctx_state.items.len(), 1);
        assert_eq!(ctx_state.items[0].path, "test.rs");
        assert_eq!(ctx_state.items[0].access, ContextAccess::Editable);

        // Verify CONTEXT_CHANGED event
        let evt = rx.try_recv().expect("event received");
        assert!(evt.contains(events::CONTEXT_CHANGED));
    }

    #[tokio::test]
    async fn test_handle_context_add_pattern() {
        let dir = TestDir::new();
        tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "fn b() {}\n").unwrap();
        tauqe_core::git::cmd::stage_files(
            dir.path(),
            &["src/a.rs".to_string(), "src/b.rs".to_string()],
        )
        .unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "init", false).unwrap();

        let (state, mut rx) = test_state(&dir);

        let make_req = |id: u64, params: Option<serde_json::Value>| Request {
            id: RequestId::Number(id),
            method: methods::CONTEXT_ADD_PATTERN.to_string(),
            params,
        };

        // Missing params -> INVALID_PARAMS
        let res = handle_context_add_pattern(make_req(1, None), &state).await;
        assert_eq!(res.error.unwrap().code, errors::INVALID_PARAMS);

        // Empty pattern -> ADD_PATTERN_FAILED
        let empty_params = serde_json::to_value(ContextAddPatternParams {
            pattern: "   ".to_string(),
            access: ContextAccess::ReadOnly,
        })
        .ok();
        let res = handle_context_add_pattern(make_req(2, empty_params), &state).await;
        assert_eq!(res.error.unwrap().code, errors::ADD_PATTERN_FAILED);

        // Valid pattern
        let pat_params = serde_json::to_value(ContextAddPatternParams {
            pattern: "src/*.rs".to_string(),
            access: ContextAccess::ReadOnly,
        })
        .ok();
        let res = handle_context_add_pattern(make_req(3, pat_params), &state).await;
        assert!(res.error.is_none());
        let result: ContextAddPatternResult =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert_eq!(result.added_count, 2);
        assert!(!result.truncated);
        assert_eq!(result.state.items.len(), 2);

        let evt = rx.try_recv().expect("event received");
        assert!(evt.contains(events::CONTEXT_CHANGED));
    }

    #[tokio::test]
    async fn test_handle_context_remove_and_set_access() {
        let dir = TestDir::new();
        let (state, mut rx) = test_state(&dir);

        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "fn test() {}\n").unwrap();

        state
            .context
            .write()
            .await
            .add_file("test.rs", ContextAccess::ReadOnly)
            .unwrap();

        let make_set_req = |id: u64, params: Option<serde_json::Value>| Request {
            id: RequestId::Number(id),
            method: methods::CONTEXT_SET_ACCESS.to_string(),
            params,
        };

        // Set access missing params -> INVALID_PARAMS
        let res = handle_context_set_access(make_set_req(1, None), &state).await;
        assert_eq!(res.error.unwrap().code, errors::INVALID_PARAMS);

        // Valid set access to Editable
        let set_params = serde_json::to_value(ContextSetAccessParams {
            path: "test.rs".to_string(),
            access: ContextAccess::Editable,
        })
        .ok();
        let res = handle_context_set_access(make_set_req(2, set_params), &state).await;
        assert!(res.error.is_none());
        let ctx_state: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert_eq!(ctx_state.items[0].access, ContextAccess::Editable);

        let evt = rx.try_recv().expect("event received");
        assert!(evt.contains(events::CONTEXT_CHANGED));

        let make_rem_req = |id: u64, params: Option<serde_json::Value>| Request {
            id: RequestId::Number(id),
            method: methods::CONTEXT_REMOVE.to_string(),
            params,
        };

        // Remove missing params -> INVALID_PARAMS
        let res = handle_context_remove(make_rem_req(3, None), &state).await;
        assert_eq!(res.error.unwrap().code, errors::INVALID_PARAMS);

        // Valid remove
        let rem_params = serde_json::to_value(ContextRemoveParams {
            path: "test.rs".to_string(),
        })
        .ok();
        let res = handle_context_remove(make_rem_req(4, rem_params), &state).await;
        assert!(res.error.is_none());
        let ctx_state: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert!(ctx_state.items.is_empty());

        let evt = rx.try_recv().expect("event received");
        assert!(evt.contains(events::CONTEXT_CHANGED));
    }

    #[tokio::test]
    async fn test_handle_context_clear() {
        let dir = TestDir::new();
        let (state, mut rx) = test_state(&dir);

        let user_file = dir.path().join("user.rs");
        let auto_file = dir.path().join("auto.rs");
        std::fs::write(&user_file, "fn user() {}\n").unwrap();
        std::fs::write(&auto_file, "fn auto() {}\n").unwrap();

        let make_req = |id: u64, params: Option<serde_json::Value>| Request {
            id: RequestId::Number(id),
            method: methods::CONTEXT_CLEAR.to_string(),
            params,
        };

        // Clearing pinned -> PINNED_PROTECTED
        let pinned_clear = serde_json::to_value(ContextClearParams {
            layer: Some(ContextLayer::Pinned),
        })
        .ok();
        let res = handle_context_clear(make_req(1, pinned_clear), &state).await;
        assert_eq!(res.error.unwrap().code, errors::PINNED_PROTECTED);

        // Malformed params -> INVALID_PARAMS
        let malformed = Some(serde_json::json!({ "layer": "nonexistent" }));
        let res = handle_context_clear(make_req(2, malformed), &state).await;
        assert_eq!(res.error.unwrap().code, errors::INVALID_PARAMS);

        // Populate user and auto
        state
            .context
            .write()
            .await
            .add_file("user.rs", ContextAccess::ReadOnly)
            .unwrap();
        state
            .context
            .write()
            .await
            .add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert_eq!(state.context.read().await.get_state().items.len(), 2);

        // Clear Auto only
        let auto_clear = serde_json::to_value(ContextClearParams {
            layer: Some(ContextLayer::Auto),
        })
        .ok();
        let res = handle_context_clear(make_req(3, auto_clear), &state).await;
        assert!(res.error.is_none());
        let ctx_state: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert_eq!(ctx_state.items.len(), 1);
        assert_eq!(ctx_state.items[0].path, "user.rs");

        // Repopulate auto
        state
            .context
            .write()
            .await
            .add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();

        // Clear User only
        let user_clear = serde_json::to_value(ContextClearParams {
            layer: Some(ContextLayer::User),
        })
        .ok();
        let res = handle_context_clear(make_req(4, user_clear), &state).await;
        assert!(res.error.is_none());
        let ctx_state: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert_eq!(ctx_state.items.len(), 1);
        assert_eq!(ctx_state.items[0].path, "auto.rs");

        // Clear all (None)
        let res = handle_context_clear(make_req(5, None), &state).await;
        assert!(res.error.is_none());
        let ctx_state: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert!(ctx_state.items.is_empty());

        let mut events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            events.push(evt);
        }
        assert!(events.iter().any(|e| e.contains(events::CONTEXT_CHANGED)));
    }

    #[tokio::test]
    async fn test_handle_context_set_access_pinned_rejected() {
        let dir = TestDir::new();
        let (state, _rx) = test_state(&dir);

        let pinned_file = dir.path().join("pinned.rs");
        std::fs::write(&pinned_file, "fn pinned() {}\n").unwrap();

        state
            .context
            .write()
            .await
            .load_pinned(&["pinned.rs".to_string()]);

        let req = Request {
            id: RequestId::Number(1),
            method: methods::CONTEXT_SET_ACCESS.to_string(),
            params: serde_json::to_value(ContextSetAccessParams {
                path: "pinned.rs".to_string(),
                access: ContextAccess::Editable,
            })
            .ok(),
        };
        let res = handle_context_set_access(req, &state).await;
        assert_eq!(res.error.unwrap().code, errors::SET_ACCESS_FAILED);
    }

    #[tokio::test]
    async fn test_reject_if_busy_blocks_mutations() {
        let dir = TestDir::new();
        let (state, _rx) = test_state(&dir);

        let _op = state.try_begin_operation().expect("begin op");

        let make_req = |method: &str, params: Option<serde_json::Value>| Request {
            id: RequestId::Number(1),
            method: method.to_string(),
            params,
        };

        let add_params = serde_json::to_value(ContextAddParams {
            path: "foo.rs".to_string(),
            access: ContextAccess::ReadOnly,
            layer: None,
        })
        .ok();
        let res = handle_context_add(make_req(methods::CONTEXT_ADD, add_params), &state).await;
        assert_eq!(res.error.unwrap().code, errors::OPERATION_IN_PROGRESS);

        let pat_params = serde_json::to_value(ContextAddPatternParams {
            pattern: "*.rs".to_string(),
            access: ContextAccess::ReadOnly,
        })
        .ok();
        let res = handle_context_add_pattern(
            make_req(methods::CONTEXT_ADD_PATTERN, pat_params),
            &state,
        )
        .await;
        assert_eq!(res.error.unwrap().code, errors::OPERATION_IN_PROGRESS);

        let rem_params = serde_json::to_value(ContextRemoveParams {
            path: "foo.rs".to_string(),
        })
        .ok();
        let res = handle_context_remove(make_req(methods::CONTEXT_REMOVE, rem_params), &state).await;
        assert_eq!(res.error.unwrap().code, errors::OPERATION_IN_PROGRESS);

        let set_params = serde_json::to_value(ContextSetAccessParams {
            path: "foo.rs".to_string(),
            access: ContextAccess::Editable,
        })
        .ok();
        let res =
            handle_context_set_access(make_req(methods::CONTEXT_SET_ACCESS, set_params), &state)
                .await;
        assert_eq!(res.error.unwrap().code, errors::OPERATION_IN_PROGRESS);

        let clear_params = serde_json::to_value(ContextClearParams::default()).ok();
        let res =
            handle_context_clear(make_req(methods::CONTEXT_CLEAR, clear_params), &state).await;
        assert_eq!(res.error.unwrap().code, errors::OPERATION_IN_PROGRESS);

        // Read query still succeeds while busy
        let res = handle_context_get(make_req(methods::CONTEXT_GET, None), &state).await;
        assert!(res.error.is_none());
    }
}
