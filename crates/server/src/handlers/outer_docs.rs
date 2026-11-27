//! Inspection and removal of the materialized external documentation cache.
//!
//! The cache directory is an engine detail: the wire contract addresses a
//! document by its logical path, so both handlers hand the path to the domain
//! layer and never build a cache path of their own. Removal also reconciles the
//! resident OuterDocs layer, because a document the cache no longer holds must
//! not stay in the context it widened.

use std::sync::Arc;

use tauqe_flow::api::CacheError;
use tauqe_protocol::{
    errors, OuterDocsCacheEntry, OuterDocsCacheListResult, OuterDocsCacheRemoveParams,
    OuterDocsCacheRemoveResult, Request, Response,
};

use crate::state::AppState;

/// Lists every document the isolated external documentation cache holds.
///
/// The answer is the durable inventory of the cache: one logical path, one
/// size, and one token estimate per document, never the disk prefix the
/// snapshot is stored under.
pub async fn handle_outer_docs_cache_list(req: Request, state: &Arc<AppState>) -> Response {
    match state.application.outer_docs_cache().await {
        Ok(entries) => {
            let entries = entries
                .into_iter()
                .map(|entry| OuterDocsCacheEntry {
                    path: entry.path,
                    size_bytes: entry.size_bytes,
                    estimated_tokens: entry.estimated_tokens,
                })
                .collect();
            Response::ok_typed(req.id, &OuterDocsCacheListResult { entries })
        }
        Err(error) => Response::err(req.id, errors::INTERNAL_ERROR, error.to_string()),
    }
}

/// Removes one document or one directory of the cache.
///
/// The requested path is logical: the engine validates it, unlinks the files
/// atomically, and prunes the directories the removal emptied. The resident
/// OuterDocs layer is reconciled in the same request, so a document the cache
/// no longer holds leaves the layer that still names it.
pub async fn handle_outer_docs_cache_remove(req: Request, state: &Arc<AppState>) -> Response {
    let params: OuterDocsCacheRemoveParams = match req
        .params
        .and_then(|params| serde_json::from_value(params).ok())
    {
        Some(params) => params,
        None => {
            return Response::err(req.id, errors::INVALID_PARAMS, "Missing or invalid path");
        }
    };
    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    let removed = match state
        .application
        .remove_cached_documents(params.path, observer)
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return Response::err(req.id, cache_error_code(&error), error.to_string()),
    };
    let result = OuterDocsCacheRemoveResult {
        removed_files: removed.removed_files,
        removed_bytes: removed.removed_bytes,
    };
    Response::ok_typed(req.id, &result)
}

/// The wire error code of one cache removal refusal.
fn cache_error_code(error: &CacheError) -> &'static str {
    match error {
        CacheError::Busy => errors::OPERATION_IN_PROGRESS,
        CacheError::InvalidPath(_) => errors::INVALID_PARAMS,
        CacheError::NotFound(_) => errors::NOT_FOUND,
        CacheError::Failed(_) => errors::INTERNAL_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::OutChannel;
    use tauqe_protocol::events;
    use tauqe_protocol::{methods, RequestId};

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tauqe-test-outer-docs-cache-{}-{}",
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

    fn test_state(dir: &TestDir) -> Arc<AppState> {
        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.path().to_path_buf(),
            hist_tx,
            out,
        )
    }

    /// Writes one cached document and answers the repository-relative path the
    /// context layer stores it under.
    fn write_cache_file(dir: &TestDir, logical: &str, contents: &str) -> String {
        let resident = format!(
            "{}/{}",
            tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR,
            logical
        );
        let path = dir.path().join(&resident);
        std::fs::create_dir_all(path.parent().expect("the path has a directory")).unwrap();
        std::fs::write(&path, contents).unwrap();
        resident
    }

    fn request(id: u64, method: &str, params: Option<serde_json::Value>) -> Request {
        Request {
            id: RequestId::Number(id),
            method: method.to_string(),
            params,
        }
    }

    fn remove_request(id: u64, path: &str) -> Request {
        request(
            id,
            methods::OUTER_DOCS_CACHE_REMOVE,
            Some(serde_json::json!({ "path": path })),
        )
    }

    #[tokio::test]
    async fn test_the_cache_list_and_remove_handlers_reconcile_the_context() {
        let dir = TestDir::new();
        let state = test_state(&dir);
        let resident = write_cache_file(&dir, "openrouter/api.md", "endpoints");
        write_cache_file(&dir, "openrouter/models.md", "models");
        let mounted = resident.clone();
        state
            .mutate_context(move |context| context.add_outer_docs_file(&mounted))
            .await
            .expect("fixture context mutation runs")
            .unwrap();
        let warm_session = state.session_uuid().await;

        let listed =
            handle_outer_docs_cache_list(request(1, methods::OUTER_DOCS_CACHE_LIST, None), &state)
                .await;
        assert!(listed.error.is_none(), "{:?}", listed.error);
        let listed: OuterDocsCacheListResult =
            serde_json::from_value(listed.result.unwrap()).unwrap();
        let paths: Vec<&str> = listed
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, vec!["openrouter/api.md", "openrouter/models.md"]);

        let removed =
            handle_outer_docs_cache_remove(remove_request(2, "openrouter/api.md"), &state).await;
        assert!(removed.error.is_none(), "{:?}", removed.error);
        let removed: OuterDocsCacheRemoveResult =
            serde_json::from_value(removed.result.unwrap()).unwrap();
        assert_eq!(removed.removed_files, 1);
        assert!(!dir.path().join(&resident).exists());
        assert!(state.context_snapshot().await.outer_docs_files().is_empty());
        assert_ne!(
            warm_session,
            state.session_uuid().await,
            "removing a resident document must rotate the provider session"
        );
    }

    #[tokio::test]
    async fn test_the_cache_remove_handler_reports_an_unknown_document() {
        let dir = TestDir::new();
        let state = test_state(&dir);

        let response =
            handle_outer_docs_cache_remove(remove_request(1, "absent/api.md"), &state).await;

        assert_eq!(
            response.error.map(|error| error.code),
            Some(errors::NOT_FOUND.to_string())
        );
        assert!(
            !state.is_operation_active(),
            "a refused removal must release the operation slot"
        );
    }

    #[tokio::test]
    async fn test_the_cache_remove_handler_is_refused_while_an_operation_is_registered() {
        let dir = TestDir::new();
        let state = test_state(&dir);
        let operation = state.try_begin_operation().expect("the slot is free");

        let response =
            handle_outer_docs_cache_remove(remove_request(1, "absent/api.md"), &state).await;

        assert_eq!(
            response.error.map(|error| error.code),
            Some(errors::OPERATION_IN_PROGRESS.to_string())
        );
        assert!(state.is_operation_active());
        drop(operation);
    }
    #[tokio::test]
    async fn cache_removal_publishes_context_only_when_resident_material_changes() {
        let dir = TestDir::new();
        let (out, mut events_rx) = OutChannel::new();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.path().to_path_buf(),
            history,
            out,
        );
        write_cache_file(&dir, "vendor/unmounted.md", "unmounted");
        let mounted = write_cache_file(&dir, "vendor/mounted.md", "mounted");
        state
            .mutate_context(move |context| context.add_outer_docs_file(&mounted))
            .await
            .unwrap()
            .unwrap();
        let unchanged =
            handle_outer_docs_cache_remove(remove_request(1, "vendor/unmounted.md"), &state).await;
        assert!(unchanged.error.is_none());
        assert!(events_rx.try_recv().is_err());
        let invalid = handle_outer_docs_cache_remove(remove_request(2, "../vendor"), &state).await;
        assert_eq!(invalid.error.unwrap().code, errors::INVALID_PARAMS);
        assert!(events_rx.try_recv().is_err());
        let changed =
            handle_outer_docs_cache_remove(remove_request(3, "vendor/mounted.md"), &state).await;
        assert!(changed.error.is_none());
        let event: serde_json::Value =
            serde_json::from_str(&events_rx.try_recv().unwrap()).unwrap();
        assert_eq!(event["method"], events::CONTEXT_CHANGED);
        assert_eq!(
            event["params"]["state"],
            serde_json::to_value(crate::handlers::context::context_state_dto(
                &state.context_state().await
            ))
            .unwrap()
        );
        assert!(events_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cache_removal_decodes_before_admission() {
        let dir = TestDir::new();
        let state = test_state(&dir);
        let operation = state.try_begin_operation().unwrap();
        let response = handle_outer_docs_cache_remove(
            request(1, methods::OUTER_DOCS_CACHE_REMOVE, None),
            &state,
        )
        .await;
        assert_eq!(response.error.unwrap().code, errors::INVALID_PARAMS);
        drop(operation);
        let response = handle_outer_docs_cache_remove(
            request(2, methods::OUTER_DOCS_CACHE_REMOVE, None),
            &state,
        )
        .await;
        assert_eq!(response.error.unwrap().code, errors::INVALID_PARAMS);
        assert!(!state.is_operation_active());
    }
}
