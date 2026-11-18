mod projection;
pub(crate) use projection::{
    context_access_domain, context_layer_domain, context_state_dto, publish_context,
};
use std::sync::Arc;

use tauqe_protocol::{
    errors, ContextAddParams, ContextAddPatternParams, ContextAddPatternResult, ContextClearParams,
    ContextRemoveParams, ContextSetAccessParams, Request, RequestId, Response,
};

use crate::state::AppState;
use tauqe_core::api::{ContextError, ContextOperation};

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
) -> Result<ContextOperation, Box<Response>> {
    state.application.begin_context_change().map_err(|_| {
        tracing::warn!(
            code = errors::OPERATION_IN_PROGRESS,
            "Refused to register a new operation: a slot is already active"
        );
        Box::new(Response::err(
            req_id.clone(),
            errors::OPERATION_IN_PROGRESS,
            "Context cannot be modified while a turn or batch is running",
        ))
    })
}

pub async fn handle_context_get(req: Request, state: &Arc<AppState>) -> Response {
    let ctx_state = state.context_state().await;
    Response::ok_typed(req.id, &context_state_dto(&ctx_state))
}

pub async fn handle_context_add(req: Request, state: &Arc<AppState>) -> Response {
    let operation = match claim_context_mutation(&req.id, state) {
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

    let ctx_state = match operation
        .add_file(
            params.path,
            context_access_domain(params.access),
            params.layer.map(context_layer_domain),
        )
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let code = match error {
                ContextError::PinnedAddition => errors::PINNED_PROTECTED,
                ContextError::RegistryManaged => errors::INVALID_PARAMS,
                _ => errors::ADD_FAILED,
            };
            return Response::err(req.id, code, error.to_string());
        }
    };

    crate::handlers::context::publish_context(&state.out, &ctx_state);

    Response::ok_typed(req.id, &context_state_dto(&ctx_state))
}

pub async fn handle_context_add_pattern(req: Request, state: &Arc<AppState>) -> Response {
    let operation = match claim_context_mutation(&req.id, state) {
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

    let addition = match operation
        .add_pattern(params.pattern, context_access_domain(params.access))
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return Response::err(req.id, errors::ADD_PATTERN_FAILED, error.to_string()),
    };
    let ctx_state = addition.state;

    crate::handlers::context::publish_context(&state.out, &ctx_state);

    let result = ContextAddPatternResult {
        added_count: addition.added_count,
        added_tokens: addition.added_tokens,
        truncated: addition.truncated,
        state: context_state_dto(&ctx_state),
    };

    Response::ok_typed(req.id, &result)
}

pub async fn handle_context_remove(req: Request, state: &Arc<AppState>) -> Response {
    let operation = match claim_context_mutation(&req.id, state) {
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

    let ctx_state = match operation.remove_file(params.path).await {
        Ok(snapshot) => snapshot,
        Err(error) => return Response::err(req.id, errors::REMOVE_FAILED, error.to_string()),
    };

    crate::handlers::context::publish_context(&state.out, &ctx_state);

    Response::ok_typed(req.id, &context_state_dto(&ctx_state))
}

pub async fn handle_context_set_access(req: Request, state: &Arc<AppState>) -> Response {
    let operation = match claim_context_mutation(&req.id, state) {
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

    let ctx_state = match operation
        .set_access(params.path, context_access_domain(params.access))
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => return Response::err(req.id, errors::SET_ACCESS_FAILED, error.to_string()),
    };

    crate::handlers::context::publish_context(&state.out, &ctx_state);

    Response::ok_typed(req.id, &context_state_dto(&ctx_state))
}

pub async fn handle_context_clear(req: Request, state: &Arc<AppState>) -> Response {
    let operation = match claim_context_mutation(&req.id, state) {
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

    let ctx_state = match operation
        .clear(params.layer.map(context_layer_domain))
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let code = match error {
                ContextError::PinnedClear => errors::PINNED_PROTECTED,
                _ => errors::INTERNAL_ERROR,
            };
            return Response::err(req.id, code, error.to_string());
        }
    };

    crate::handlers::context::publish_context(&state.out, &ctx_state);

    Response::ok_typed(req.id, &context_state_dto(&ctx_state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
    use crate::transport::OutChannel;
    use tauqe_protocol::{events, methods, ContextAccess, ContextLayer, Request, RequestId};

    /// Scratch repository root of one context handler test, removed on drop.
    struct TestDir(std::path::PathBuf);
    impl TestDir {
        fn new() -> Self {
            Self(scratch_root("context"))
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
            .mutate_context(|context| {
                context.add_file("lib.rs", tauqe_core::context::ContextAccess::ReadOnly)
            })
            .await
            .expect("fixture context mutation runs")
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
            .mutate_context(|context| {
                context.add_file("test.rs", tauqe_core::context::ContextAccess::ReadOnly)
            })
            .await
            .expect("fixture context mutation runs")
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
            .mutate_context(|context| {
                context.add_file("user.rs", tauqe_core::context::ContextAccess::ReadOnly)
            })
            .await
            .expect("fixture context mutation runs")
            .unwrap();
        state
            .mutate_context(|context| {
                context.add_auto_file("auto.rs", tauqe_core::context::ContextAccess::ReadOnly)
            })
            .await
            .expect("fixture context mutation runs")
            .unwrap();
        assert_eq!(state.context_state().await.items.len(), 2);

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
            .mutate_context(|context| {
                context.add_auto_file("auto.rs", tauqe_core::context::ContextAccess::ReadOnly)
            })
            .await
            .expect("fixture context mutation runs")
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
            .mutate_context(|context| context.load_pinned(&["pinned.rs".to_string()]))
            .await
            .expect("fixture context mutation runs");

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
            state.context_state().await.items.is_empty(),
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
            !state.context_snapshot().await.contains("ref.md").unwrap(),
            "the registry owns the OuterDocs layer"
        );
    }

    #[tokio::test]
    async fn test_handle_context_clear_drops_only_the_outer_docs_layer() {
        let dir = TestDir::new();
        let (state, mut rx) = test_state(&dir);
        // The external document lives in the isolated cache the registry
        // materializes into. The layer admits it by the cache path the
        // materialization reports and addresses it by its logical path.
        let cached = format!("{}/ref.md", tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR);
        let snapshot = dir.path().join(&cached);
        std::fs::create_dir_all(snapshot.parent().expect("the snapshot has a parent")).unwrap();
        std::fs::write(&snapshot, "# External snapshot\n").unwrap();
        std::fs::write(dir.path().join("user.rs"), "fn user() {}\n").unwrap();
        state
            .mutate_context(move |context| context.add_outer_docs_file(&cached))
            .await
            .expect("fixture context mutation runs")
            .unwrap();
        state
            .mutate_context(|context| {
                context.add_file("user.rs", tauqe_core::context::ContextAccess::ReadOnly)
            })
            .await
            .expect("fixture context mutation runs")
            .unwrap();
        let resident = state.context_state().await;
        let document = resident
            .items
            .iter()
            .find(|item| item.layer == tauqe_core::context::ContextLayer::OuterDocs)
            .expect("the document is resident");
        assert_eq!(
            document.path, "ref.md",
            "the layer reports the logical path, never the cache path"
        );

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

    /// The context surface addresses an external document by its logical path, so
    /// the interface renders the very path `context/get` answers and the cache
    /// directory the snapshot occupies never reaches the wire contract.
    #[tokio::test]
    async fn test_the_outer_docs_layer_reports_logical_paths() {
        let dir = TestDir::new();
        let (state, _rx) = test_state(&dir);
        let cached = format!(
            "{}/rurqe/SUMMARY.md",
            tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR
        );
        let snapshot = dir.path().join(&cached);
        std::fs::create_dir_all(snapshot.parent().expect("the snapshot has a parent")).unwrap();
        std::fs::write(&snapshot, "# Summary\n").unwrap();

        state
            .mutate_context(move |context| context.add_outer_docs_file(&cached))
            .await
            .expect("fixture context mutation runs")
            .expect("the snapshot enters the context");

        let req = Request {
            id: RequestId::Number(1),
            method: methods::CONTEXT_GET.to_string(),
            params: None,
        };
        let res = handle_context_get(req, &state).await;
        assert!(res.error.is_none());

        let ctx_state: tauqe_protocol::ContextState =
            serde_json::from_value(res.result.unwrap()).unwrap();
        let document = ctx_state
            .items
            .iter()
            .find(|item| item.layer == ContextLayer::OuterDocs)
            .expect("the document is resident");
        assert_eq!(document.path, "rurqe/SUMMARY.md");
        assert!(
            !document
                .path
                .contains(tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR),
            "the cache directory must never reach the interface"
        );
    }
}
