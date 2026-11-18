//! Shared context ownership and prompt-prefix lifecycle.

use crate::api::Application;
use crate::context::ContextManager;
use crate::context::{ContextAccess, ContextState};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("Pinned files are managed by project configuration and cannot be added manually")]
    PinnedAddition,
    #[error("The OuterDocs layer is registry-managed and cannot be added manually")]
    RegistryManaged,
    #[error("Pinned files are protected and cannot be cleared")]
    PinnedClear,
    #[error("{0}")]
    Mutation(String),
    #[error("blocking task failed: {0}")]
    Offload(String),
}

#[derive(Clone, Copy)]
pub enum ContextClearScope {
    Auto,
    User,
    OuterDocs,
    Transient,
    All,
}

#[derive(Default)]
pub(crate) struct ContextSession {
    state: Arc<RwLock<ContextManager>>,
}

impl ContextSession {
    pub(crate) fn new(root: PathBuf) -> Self {
        let manager = crate::blocking::run_blocking(move || ContextManager::new(root));
        Self {
            state: Arc::new(RwLock::new(manager)),
        }
    }
}

impl Application {
    /// Detached turn-local state; mutations become shared only through an
    /// explicit merge. No application lock escapes this boundary.
    pub async fn context_for_turn(&self) -> ContextManager {
        self.context_session().state.read().await.clone()
    }

    pub async fn context_state(&self) -> ContextState {
        self.context_session().state.read().await.get_state()
    }

    pub async fn repository_root(&self) -> PathBuf {
        self.context_session()
            .state
            .read()
            .await
            .repo_root()
            .to_path_buf()
    }

    /// Keep the write guard in the worker even if its caller is cancelled.
    /// Resolve subprocess inputs before entering this closure.
    pub(crate) async fn mutate_context<T, F>(&self, mutate: F) -> Result<T, ContextError>
    where
        F: FnOnce(&mut ContextManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        let mut guard = Arc::clone(&self.context_session().state)
            .write_owned()
            .await;
        crate::blocking::run_blocking_async(move || mutate(&mut guard))
            .await
            .map_err(|error| ContextError::Offload(error.to_string()))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn test_mutate_context<T, F>(&self, mutate: F) -> Result<T, ContextError>
    where
        F: FnOnce(&mut ContextManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.mutate_context(mutate).await
    }

    pub async fn preload_declared_context(
        &self,
        root: PathBuf,
        files: Vec<String>,
        readonly_files: Vec<String>,
    ) -> Result<ContextAdmission, ContextError> {
        let requests: Vec<_> = files
            .iter()
            .map(|path| (path, ContextAccess::Editable))
            .chain(
                readonly_files
                    .iter()
                    .map(|path| (path, ContextAccess::ReadOnly)),
            )
            .filter_map(|(path, access)| {
                let path = normalize_preload_path(path);
                (!path.is_empty()).then(|| crate::edits::protocol::ContextRequest {
                    path: path.to_string(),
                    access,
                })
            })
            .collect();
        if requests.is_empty() {
            return Ok(ContextAdmission {
                outcome: Default::default(),
                state: self.context_state().await,
            });
        }
        let available = repository_files(root).await;
        self.admit_context(requests, available, usize::MAX).await
    }

    pub(crate) async fn admit_outer_documents(
        &self,
        paths: Vec<String>,
        token_budget: usize,
    ) -> Result<ContextAdmission, ContextError> {
        let requests = paths
            .into_iter()
            .map(|path| crate::edits::protocol::ContextRequest {
                path,
                access: crate::context::ContextAccess::ReadOnly,
            })
            .collect();
        self.admit_context(requests, Vec::new(), token_budget).await
    }

    async fn admit_context(
        &self,
        requests: Vec<crate::edits::protocol::ContextRequest>,
        available: Vec<String>,
        token_budget: usize,
    ) -> Result<ContextAdmission, ContextError> {
        self.mutate_context(move |context| {
            let outcome = crate::workflow::research::apply_context_requests_detailed(
                context,
                &requests,
                &available,
                token_budget,
            );
            ContextAdmission {
                outcome,
                state: context.get_state(),
            }
        })
        .await
    }

    pub async fn merge_turn_context(
        &self,
        context: ContextManager,
    ) -> Result<ContextState, ContextError> {
        self.mutate_context(move |shared| {
            shared.merge_turn_context(&context);
            shared.get_state()
        })
        .await
    }

    pub(crate) async fn rebind_context(&self, root: PathBuf) -> Result<(), ContextError> {
        self.mutate_context(move |context| context.set_repo_root(root))
            .await
    }

    pub async fn clear_context(
        &self,
        scope: ContextClearScope,
    ) -> Result<ContextState, ContextError> {
        let (removed, snapshot) = self
            .mutate_context(move |context| {
                let removed = match scope {
                    ContextClearScope::Auto => context.clear_auto(),
                    ContextClearScope::User => {
                        context.clear_user();
                        false
                    }
                    ContextClearScope::OuterDocs => context.clear_outer_docs(),
                    ContextClearScope::Transient => context.clear_transient(),
                    ContextClearScope::All => context.clear(),
                };
                (removed, context.get_state())
            })
            .await?;
        if removed {
            self.reset_session_uuid().await;
        }
        Ok(snapshot)
    }

    pub(crate) async fn release_cached_documents(
        &self,
        logical: String,
    ) -> Result<(bool, ContextState), ContextError> {
        let (removed, snapshot) = self
            .mutate_context(move |context| {
                let documents =
                    crate::outer_docs::affected_documents(&logical, &context.outer_docs_files());
                let mut removed = false;
                for document in documents {
                    removed |= context.remove_file(&document).unwrap_or(false);
                }
                (removed, context.get_state())
            })
            .await?;
        if removed {
            self.reset_session_uuid().await;
        }
        Ok((removed, snapshot))
    }
}

/// Snapshot paired with the admission decision made under the same context lock.
pub struct ContextAdmission {
    pub outcome: crate::workflow::research::ContextApplicationOutcome,
    pub state: ContextState,
}

/// Normalize declared paths consistently for preloading and horizon expansion.
pub fn normalize_preload_path(path: &str) -> &str {
    path.trim().trim_start_matches("./").trim_start_matches('/')
}

pub(super) async fn repository_files(root: PathBuf) -> Vec<String> {
    crate::blocking::run_blocking_async(move || crate::git::list_repository_files(Some(&root)))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn application(root: &std::path::Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(
            crate::config::AppConfig::default(),
            root.to_path_buf(),
            sender,
        )
    }

    #[tokio::test]
    async fn unbound_application_has_an_empty_in_memory_context() {
        let app = Application::default();
        assert!(app.context_state().await.items.is_empty());
        assert!(app.repository_root().await.as_os_str().is_empty());
        assert!(app.context_for_turn().await.get_state().items.is_empty());
    }

    #[tokio::test]
    async fn turn_copies_are_detached_and_merge_without_losing_shared_entries() {
        let root = tempfile::tempdir().unwrap();
        for file in ["local.rs", "shared.rs"] {
            std::fs::write(root.path().join(file), "fn f() {}\n").unwrap();
        }
        let app = application(root.path());
        let peer = app.clone();
        let mut local = app.context_for_turn().await;
        local
            .add_auto_file("local.rs", ContextAccess::ReadOnly)
            .unwrap();
        assert!(peer.context_state().await.items.is_empty());
        peer.mutate_context(|context| context.add_file("shared.rs", ContextAccess::Editable))
            .await
            .unwrap()
            .unwrap();
        let merged = app.merge_turn_context(local).await.unwrap();
        assert!(merged.items.iter().any(|item| item.path == "local.rs"));
        assert!(merged.items.iter().any(|item| item.path == "shared.rs"));
        assert_eq!(peer.context_state().await.revision, merged.revision);
    }

    #[tokio::test]
    async fn clearing_scopes_preserves_user_policy_and_rotates_only_after_removal() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("user.rs"), "user").unwrap();
        std::fs::write(root.path().join("auto.rs"), "auto").unwrap();
        let app = application(root.path());
        app.mutate_context(|context| {
            context
                .add_file("user.rs", ContextAccess::ReadOnly)
                .unwrap();
            context
                .add_auto_file("auto.rs", ContextAccess::ReadOnly)
                .unwrap();
        })
        .await
        .unwrap();
        let warm = app.session_uuid().await;
        let user_cleared = app.clear_context(ContextClearScope::User).await.unwrap();
        assert_eq!(warm, app.session_uuid().await);
        assert_eq!(user_cleared.items.len(), 1);
        assert_eq!(user_cleared.items[0].path, "auto.rs");
        app.clear_context(ContextClearScope::Transient)
            .await
            .unwrap();
        let rotated = app.session_uuid().await;
        assert_ne!(warm, rotated);
        app.clear_context(ContextClearScope::Transient)
            .await
            .unwrap();
        assert_eq!(rotated, app.session_uuid().await);
    }

    #[tokio::test]
    async fn cancelling_caller_keeps_mutation_serialized_until_worker_finishes() {
        let root = tempfile::tempdir().unwrap();
        for file in ["first.rs", "second.rs"] {
            std::fs::write(root.path().join(file), "source").unwrap();
        }
        let app = application(root.path());
        let worker = app.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            worker
                .mutate_context(move |context| {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                    context
                        .add_file("first.rs", ContextAccess::ReadOnly)
                        .unwrap();
                })
                .await
        });
        ready.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let peer = app.clone();
        let next = tokio::spawn(async move {
            peer.mutate_context(|context| context.add_file("second.rs", ContextAccess::ReadOnly))
                .await
                .unwrap()
                .unwrap();
        });
        release.send(()).unwrap();
        next.await.unwrap();
        assert_eq!(app.context_state().await.items.len(), 2);
    }
}
