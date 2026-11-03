use std::sync::Arc;

use tauqe_protocol::{
    errors, events, ContextAddParams, ContextAddPatternParams, ContextAddPatternResult,
    ContextClearParams, ContextLayer, ContextRemoveParams, ContextSetAccessParams, Event, Request,
    RequestId, Response,
};

use crate::state::{AppState, OperationGuard};

/// Claims the operation slot for a context mutation.
///
/// Every turn reasons over a clone of the context and merges that clone back when
/// it finishes, so a mutation applied in between would be overwritten, or
/// resurrected from the turn's frozen snapshot. Sampling the slot once cannot
/// prevent that: a turn registering between the check and the mutation races it.
/// The mutation therefore claims the slot instead of observing it, which makes the
/// refusal and the mutation one atomic decision.
fn claim_context_mutation(
    req_id: &RequestId,
    state: &Arc<AppState>,
) -> Result<OperationGuard, Box<Response>> {
    super::begin_exclusive_operation(
        req_id,
        state,
        "Context cannot be modified while a turn or batch is running",
    )
}

pub async fn handle_context_get(req: Request, state: &Arc<AppState>) -> Response {
    let ctx = state.context.read().await;
    let ctx_state = ctx.get_state();
    Response::ok_typed(req.id, &ctx_state)
}

pub async fn handle_context_add(req: Request, state: &Arc<AppState>) -> Response {
    let _operation = match claim_context_mutation(&req.id, state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let params: ContextAddParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing or invalid path/access",
            );
        }
    };

    // Honor the requested layer instead of always defaulting to the User layer.
    // The Pinned layer stays configuration-only and the OuterDocs layer is
    // owned by the external documentation registry, so both are refused here.
    let layer = params.layer.unwrap_or(ContextLayer::User);
    if layer == ContextLayer::Pinned {
        return Response::err(
            req.id,
            errors::PINNED_PROTECTED,
            "Pinned files are managed by project configuration and cannot be added manually",
        );
    }
    if layer == ContextLayer::OuterDocs {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "The OuterDocs layer is registry-managed and cannot be added manually",
        );
    }

    // Metadata reads and path verification touch the disk, so the mutation runs
    // on the blocking pool while the exclusive context guard is held for the
    // whole read-modify-write sequence.
    let outcome = super::with_context_mutation(state, move |ctx| {
        ctx.add_file_to_layer(&params.path, params.access, layer)
            .map(|_| ctx.get_state())
            .map_err(|err| err.to_string())
    })
    .await;

    let ctx_state = match outcome {
        Ok(Ok(state)) => state,
        Ok(Err(err)) | Err(err) => {
            return Response::err(req.id, errors::ADD_FAILED, err);
        }
    };

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    Response::ok_typed(req.id, &ctx_state)
}

pub async fn handle_context_add_pattern(req: Request, state: &Arc<AppState>) -> Response {
    let _operation = match claim_context_mutation(&req.id, state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let params: ContextAddPatternParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Missing or invalid pattern/access",
                );
            }
        };

    // Read the aggregate token budget before taking the context write lock so the
    // two locks are never held at the same time.
    let token_budget = {
        let guard = state.config.lock().await;
        guard.config.context.token_budget
    };

    let repo_root = state.repo_root().await;
    let available_files =
        super::offload(move || tauqe_core::git::list_repository_files(Some(repo_root.as_path())))
            .await
            .map(|result| result.unwrap_or_default())
            .unwrap_or_default();

    // Matching the pattern against the repository listing and pricing every
    // admitted file from its metadata touches the disk, so the whole
    // application runs inside the blocking closure.
    let outcome = super::with_context_mutation(state, move |ctx| {
        ctx.add_files_by_pattern(
            &params.pattern,
            params.access,
            &available_files,
            token_budget,
        )
        .map(|outcome| {
            let state = ctx.get_state();
            (
                outcome.added_items.len(),
                outcome.added_tokens,
                outcome.truncated,
                state,
            )
        })
        .map_err(|err| err.to_string())
    })
    .await;

    let (added_count, added_tokens, truncated, ctx_state) = match outcome {
        Ok(Ok(value)) => value,
        Ok(Err(err)) | Err(err) => {
            return Response::err(req.id, errors::ADD_PATTERN_FAILED, err);
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
    let _operation = match claim_context_mutation(&req.id, state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let params: ContextRemoveParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid path");
        }
    };

    let outcome = super::with_context_mutation(state, move |ctx| {
        ctx.remove_file(&params.path)
            .map(|_| ctx.get_state())
            .map_err(|err| err.to_string())
    })
    .await;

    let ctx_state = match outcome {
        Ok(Ok(state)) => state,
        Ok(Err(err)) | Err(err) => {
            return Response::err(req.id, errors::REMOVE_FAILED, err);
        }
    };

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    Response::ok_typed(req.id, &ctx_state)
}

pub async fn handle_context_set_access(req: Request, state: &Arc<AppState>) -> Response {
    let _operation = match claim_context_mutation(&req.id, state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    let params: ContextSetAccessParams =
        match req.params.and_then(|p| serde_json::from_value(p).ok()) {
            Some(p) => p,
            None => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    "Missing or invalid parameters",
                );
            }
        };

    let outcome = super::with_context_mutation(state, move |ctx| {
        ctx.set_access(&params.path, params.access)
            .map(|_| ctx.get_state())
            .map_err(|err| err.to_string())
    })
    .await;

    let ctx_state = match outcome {
        Ok(Ok(state)) => state,
        Ok(Err(err)) | Err(err) => {
            return Response::err(req.id, errors::SET_ACCESS_FAILED, err);
        }
    };

    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    Response::ok_typed(req.id, &ctx_state)
}

pub async fn handle_context_clear(req: Request, state: &Arc<AppState>) -> Response {
    let _operation = match claim_context_mutation(&req.id, state) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

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

    // Clearing recomputes hierarchical pinned files, which stats the disk, so it
    // runs on the blocking pool like every other context mutation. Clearing the
    // Auto layer, the OuterDocs layer, or the whole context really shrinks the
    // stable prompt prefix, so those paths go through the shared `AppState`
    // facade, which rotates the dialogue session exactly when files were
    // removed. A user-layer clear leaves the Auto layer intact, so it keeps the
    // warm session and stays a plain context mutation.
    let cleared = match params.layer {
        Some(ContextLayer::Auto) => state.clear_auto_context().await,
        Some(ContextLayer::User) => {
            super::with_context_mutation(state, |ctx| {
                ctx.clear_user();
                ctx.get_state()
            })
            .await
        }
        // External documents form a persistent layer of their own, so clearing
        // it drops exactly that layer and leaves the other three intact.
        Some(ContextLayer::OuterDocs) => state.clear_outer_docs_context().await,
        // The Pinned layer is refused above, so nothing else remains to clear.
        _ => state.clear_all_context().await,
    };

    let ctx_state = match cleared {
        Ok(state) => state,
        Err(err) => return Response::err(req.id, errors::INTERNAL_ERROR, err),
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
        let result: ContextAddPatternResult = serde_json::from_value(res.result.unwrap()).unwrap();
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

        let warm_session = state.session_uuid().await;

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
        assert_ne!(
            warm_session,
            state.session_uuid().await,
            "removing auto files must rotate the provider session identifier"
        );

        // Repopulate auto
        state
            .context
            .write()
            .await
            .add_auto_file("auto.rs", ContextAccess::ReadOnly)
            .unwrap();

        // Clear User only
        let user_session = state.session_uuid().await;
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
        assert_eq!(
            user_session,
            state.session_uuid().await,
            "clearing the user layer alone leaves the auto layer and its warm session intact"
        );

        // Clear all (None)
        let res = handle_context_clear(make_req(5, None), &state).await;
        assert!(res.error.is_none());
        let ctx_state: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert!(ctx_state.items.is_empty());
        assert_ne!(
            user_session,
            state.session_uuid().await,
            "a full context reset must rotate the provider session identifier"
        );

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

    /// A mutation that a running turn would overwrite or resurrect is refused for
    /// the operation's whole duration, and the refusal is the mutation's own slot
    /// claim rather than a sample of the slot: the running operation keeps the
    /// slot, the context is never touched, and the same request succeeds as soon
    /// as the slot is free.
    #[tokio::test]
    async fn test_operation_slot_refuses_context_mutations() {
        let dir = TestDir::new();
        let (state, _rx) = test_state(&dir);

        let operation = state.try_begin_operation().expect("begin op");

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
        let res =
            handle_context_add_pattern(make_req(methods::CONTEXT_ADD_PATTERN, pat_params), &state)
                .await;
        assert_eq!(res.error.unwrap().code, errors::OPERATION_IN_PROGRESS);

        let rem_params = serde_json::to_value(ContextRemoveParams {
            path: "foo.rs".to_string(),
        })
        .ok();
        let res =
            handle_context_remove(make_req(methods::CONTEXT_REMOVE, rem_params), &state).await;
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

        // Every refusal came from the mutation's own claim, so the running
        // operation still owns the slot and no mutation reached the manager.
        assert!(
            state.is_operation_active(),
            "the running operation must keep owning the slot"
        );
        assert!(
            state.context.read().await.get_state().items.is_empty(),
            "a refused context mutation must not reach the manager"
        );

        // Releasing the slot lets the same request through: the refusal is the
        // claim, not a permanent exclusion.
        drop(operation);
        std::fs::write(dir.path().join("fresh.rs"), "fn fresh() {}\n").unwrap();
        let fresh_params = serde_json::to_value(ContextAddParams {
            path: "fresh.rs".to_string(),
            access: ContextAccess::ReadOnly,
            layer: None,
        })
        .ok();
        let res = handle_context_add(make_req(methods::CONTEXT_ADD, fresh_params), &state).await;
        assert!(
            res.error.is_none(),
            "the mutation must succeed once the slot is free: {:?}",
            res.error
        );
    }
    #[tokio::test]
    async fn test_handle_context_add_rejects_the_outer_docs_layer() {
        let dir = TestDir::new();
        let (state, _rx) = test_state(&dir);
        std::fs::write(dir.path().join("ref.md"), "# External\n").unwrap();

        let req = Request {
            id: RequestId::Number(1),
            method: methods::CONTEXT_ADD.to_string(),
            params: serde_json::to_value(ContextAddParams {
                path: "ref.md".to_string(),
                access: ContextAccess::ReadOnly,
                layer: Some(ContextLayer::OuterDocs),
            })
            .ok(),
        };
        let res = handle_context_add(req, &state).await;

        assert_eq!(res.error.unwrap().code, errors::INVALID_PARAMS);
        assert!(
            !state.context.read().await.contains("ref.md").unwrap(),
            "the registry owns the OuterDocs layer"
        );
    }

    #[tokio::test]
    async fn test_handle_context_clear_drops_only_the_outer_docs_layer() {
        let dir = TestDir::new();
        let (state, mut rx) = test_state(&dir);
        std::fs::write(dir.path().join("ref.md"), "# External snapshot\n").unwrap();
        std::fs::write(dir.path().join("user.rs"), "fn user() {}\n").unwrap();

        state
            .context
            .write()
            .await
            .add_outer_docs_file("ref.md")
            .unwrap();
        state
            .context
            .write()
            .await
            .add_file("user.rs", ContextAccess::ReadOnly)
            .unwrap();
        let warm_session = state.session_uuid().await;

        let req = Request {
            id: RequestId::Number(1),
            method: methods::CONTEXT_CLEAR.to_string(),
            params: serde_json::to_value(ContextClearParams {
                layer: Some(ContextLayer::OuterDocs),
            })
            .ok(),
        };
        let res = handle_context_clear(req, &state).await;
        assert!(res.error.is_none());

        let cleared: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        assert!(
            cleared
                .items
                .iter()
                .all(|item| item.layer != ContextLayer::OuterDocs),
            "the OuterDocs layer must be empty"
        );
        assert!(
            cleared.items.iter().any(|item| item.path == "user.rs"),
            "clearing external documents must leave the other layers intact"
        );
        assert_ne!(
            warm_session,
            state.session_uuid().await,
            "removing external documents must rotate the provider session"
        );

        let evt = rx.try_recv().expect("event received");
        assert!(evt.contains(events::CONTEXT_CHANGED));
    }
}
