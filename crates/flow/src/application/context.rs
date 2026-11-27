//! Shared context ownership and prompt-prefix lifecycle.

use super::context_publication::ContextPublication;
use super::dialogue::DialogueSession;
use super::operation::{OperationGuard, OperationRegistry};
use crate::api::{Application, ApplicationObserver};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauqe_core::context::{ContextAccess, ContextListener, ContextState};
use tauqe_core::context::{ContextManager, ManualContext};

use tokio::sync::RwLock;

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("Context cannot be modified while a turn or batch is running")]
    Busy,
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

#[derive(Clone)]
pub(crate) struct ContextSession {
    state: Arc<RwLock<ManualContext>>,
    publication: ContextPublication,
    dialogue: Arc<DialogueSession>,
    #[cfg(test)]
    settlement_failures: Arc<std::sync::Mutex<(bool, bool)>>,
}

impl Default for ContextSession {
    fn default() -> Self {
        Self::with_manager(
            ManualContext::default(),
            Arc::new(OperationRegistry::default()),
        )
    }
}

impl ContextSession {
    #[cfg(test)]
    pub(crate) fn new(root: PathBuf) -> Self {
        let manager = tauqe_core::blocking::run_blocking(move || ManualContext::new(root));
        Self::with_manager(manager, Arc::new(OperationRegistry::default()))
    }

    pub(crate) fn with_operations(root: PathBuf, operations: Arc<OperationRegistry>) -> Self {
        let manager = tauqe_core::blocking::run_blocking(move || ManualContext::new(root));
        Self::with_manager(manager, operations)
    }

    fn with_manager(mut manager: ManualContext, operations: Arc<OperationRegistry>) -> Self {
        let publication = ContextPublication::new(manager.state(), operations);
        manager.set_listener(Some(publication.listener()));
        Self {
            state: Arc::new(RwLock::new(manager)),
            publication,
            dialogue: Arc::new(DialogueSession::default()),
            #[cfg(test)]
            settlement_failures: Arc::new(std::sync::Mutex::new((false, false))),
        }
    }
    pub(crate) fn session_uuid(&self) -> String {
        self.dialogue.id()
    }

    pub(crate) fn reset_session_uuid(&self) -> String {
        self.dialogue.reset()
    }

    pub(crate) fn rotate_on_model_change(
        &self,
        old: &tauqe_core::models::ModelRef,
        new: &tauqe_core::models::ModelRef,
    ) -> bool {
        self.dialogue.rotate_on_model_change(old, new)
    }
    /// Detached turn-local state; mutations become shared only through an
    /// explicit merge. No application lock escapes this boundary.
    pub(crate) async fn context_for_turn(&self) -> ContextManager {
        self.state.read().await.scenario()
    }

    pub(crate) async fn context_state(&self) -> ContextState {
        self.publication.state()
    }

    pub(crate) fn activate_manual_publication(
        &self,
        operation: &OperationGuard,
        observer: Arc<dyn ApplicationObserver>,
    ) -> bool {
        self.publication.activate_shared(operation, observer)
    }

    pub(crate) fn close_manual_publication(&self, operation: &OperationGuard) {
        self.publication.close(operation);
    }

    pub(crate) fn activate_research_publication(
        &self,
        operation: &OperationGuard,
        stream_id: String,
        observer: Arc<dyn ApplicationObserver>,
        context: &ContextManager,
    ) -> Option<ContextListener> {
        self.publication
            .activate(operation, stream_id, observer, context.get_state())
    }

    pub(crate) fn close_research_publication(&self, operation_id: u64) {
        self.publication.close_source(operation_id);
    }

    pub(crate) fn rebind_research_publication(
        &self,
        operation_id: u64,
        context: &mut ContextManager,
    ) {
        context.set_listener(
            self.publication
                .rebind_source(operation_id, context.get_state()),
        );
    }

    pub(crate) async fn repository_root(&self) -> PathBuf {
        self.state.read().await.repo_root().to_path_buf()
    }

    /// Keep the write guard in the worker even if its caller is cancelled.
    /// Resolve subprocess inputs before entering this closure.
    pub(crate) async fn mutate_context<T, F>(&self, mutate: F) -> Result<T, ContextError>
    where
        F: FnOnce(&mut ContextManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        let mut guard = Arc::clone(&self.state).write_owned().await;
        tauqe_core::blocking::run_blocking_async(move || guard.scenario_mut(mutate))
            .await
            .map_err(|error| ContextError::Offload(error.to_string()))
    }

    async fn mutate_manual<T, F>(&self, mutate: F) -> Result<T, ContextError>
    where
        F: FnOnce(&mut ManualContext) -> T + Send + 'static,
        T: Send + 'static,
    {
        let mut guard = Arc::clone(&self.state).write_owned().await;
        tauqe_core::blocking::run_blocking_async(move || mutate(&mut guard))
            .await
            .map_err(|error| ContextError::Offload(error.to_string()))
    }

    pub(crate) async fn merge_turn_context(
        &self,
        context: ContextManager,
    ) -> Result<ContextState, ContextError> {
        self.mutate_context(move |shared| {
            shared.merge_turn_context(&context);
        })
        .await?;
        Ok(self.publication.state())
    }

    /// Adopt an available computation context and finish a confirmed edit
    /// independently. A lost task response cannot discard a disk receipt.
    pub(crate) async fn settle_turn_context(
        &self,
        context: Option<ContextManager>,
        applied: bool,
    ) -> (ContextState, Vec<String>) {
        let mut errors = Vec::new();
        if let Some(context) = context {
            #[cfg(test)]
            let fail = {
                let mut failures = self.settlement_failures.lock().unwrap();
                std::mem::take(&mut failures.0)
            };
            #[cfg(not(test))]
            let fail = false;
            let merged = if fail {
                Err(ContextError::Offload("injected merge failure".into()))
            } else {
                self.merge_turn_context(context).await.map(|_| ())
            };
            if let Err(error) = merged {
                errors.push(format!("Failed to merge the turn context: {error}"));
            }
        }
        if applied {
            #[cfg(test)]
            let fail = {
                let mut failures = self.settlement_failures.lock().unwrap();
                std::mem::take(&mut failures.1)
            };
            #[cfg(not(test))]
            let fail = false;
            // Even if the worker reports an error, it owns the lock until its
            // transition finishes and guards against a second UUID rotation.
            let cleaned = self.finish_applied_edit().await;
            if let Err(error) = cleaned {
                errors.push(format!("Applied edit context cleanup failed: {error}"));
            } else if fail {
                errors.push("Applied edit context cleanup failed: injected cleanup failure".into());
            }
        }
        (self.context_state().await, errors)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_settlement(&self, merge: bool, cleanup: bool) {
        *self.settlement_failures.lock().unwrap() = (merge, cleanup);
    }

    /// An applied edit invalidates the dialogue even when transient context
    /// was already cleared by the turn. The worker owns the guard on cancellation.
    pub(crate) async fn finish_applied_edit(&self) -> Result<ContextState, ContextError> {
        self.finish_applied_edit_with(|| {}).await
    }

    async fn finish_applied_edit_with(
        &self,
        after_rotation: impl FnOnce() + Send + 'static,
    ) -> Result<ContextState, ContextError> {
        let mut guard = Arc::clone(&self.state).write_owned().await;
        let dialogue = self.dialogue.clone();
        let rotated = Arc::new(AtomicBool::new(false));
        let worker_rotated = rotated.clone();
        tauqe_core::blocking::run_blocking_async(move || {
            dialogue.reset();
            worker_rotated.store(true, Ordering::Release);
            after_rotation();
            guard.clear_transient();
            guard.state()
        })
        .await
        .map(|_| self.publication.state())
        .map_err(|error| {
            // A worker that failed before rotating cannot leave an applied edit
            // using its old identity. A worker that did rotate is never retried.
            if !rotated.load(Ordering::Acquire) {
                self.dialogue.reset();
            }
            ContextError::Offload(error.to_string())
        })
    }
}

impl Application {
    pub async fn context_for_turn(&self) -> ContextManager {
        self.context_session().context_for_turn().await
    }

    pub async fn context_state(&self) -> ContextState {
        self.context_session().context_state().await
    }

    pub async fn repository_root(&self) -> PathBuf {
        self.context_session().repository_root().await
    }

    pub(crate) async fn mutate_context<T, F>(&self, mutate: F) -> Result<T, ContextError>
    where
        F: FnOnce(&mut ContextManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.context_session().mutate_context(mutate).await
    }

    pub(crate) async fn mutate_manual_context<T, F>(&self, mutate: F) -> Result<T, ContextError>
    where
        F: FnOnce(&mut ManualContext) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.context_session().mutate_manual(mutate).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn test_mutate_manual_context<T, F>(&self, mutate: F) -> Result<T, ContextError>
    where
        F: FnOnce(&mut ManualContext) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.mutate_manual_context(mutate).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn test_mutate_context<T, F>(&self, mutate: F) -> Result<T, ContextError>
    where
        F: FnOnce(&mut ContextManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.mutate_context(mutate).await
    }
}

impl ContextSession {
    pub(crate) async fn preload_declared_context(
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
                (!path.is_empty()).then(|| tauqe_core::edits::protocol::ContextRequest {
                    path: path.to_string(),
                    access,
                })
            })
            .collect();
        if requests.is_empty() {
            return Ok(ContextAdmission {
                outcome: Default::default(),
                state: self.publication.state(),
            });
        }
        let available = repository_files(root).await;
        self.admit_context(requests, available).await
    }

    pub(crate) async fn admit_outer_documents(
        &self,
        paths: Vec<String>,
    ) -> Result<ContextAdmission, ContextError> {
        let requests = paths
            .into_iter()
            .map(|path| tauqe_core::edits::protocol::ContextRequest {
                path,
                access: tauqe_core::context::ContextAccess::ReadOnly,
            })
            .collect();
        self.admit_context(requests, Vec::new()).await
    }

    async fn admit_context(
        &self,
        requests: Vec<tauqe_core::edits::protocol::ContextRequest>,
        available: Vec<String>,
    ) -> Result<ContextAdmission, ContextError> {
        let outcome = self
            .mutate_context(move |context| {
                tauqe_core::workflow::research::apply_context_requests_detailed(
                    context, &requests, &available,
                )
            })
            .await?;
        Ok(ContextAdmission {
            outcome,
            state: self.publication.state(),
        })
    }

    pub(crate) async fn clear_transient_context(&self) -> Result<ContextState, ContextError> {
        self.clear_context(ContextClearScope::Transient).await
    }

    async fn clear_context(&self, scope: ContextClearScope) -> Result<ContextState, ContextError> {
        let dialogue = self.dialogue.clone();
        self.mutate_manual(move |context| {
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
            if removed {
                dialogue.reset();
            }
            context.state()
        })
        .await
        .map(|_| self.publication.state())
    }

    pub(crate) async fn release_cached_documents(
        &self,
        logical: String,
    ) -> Result<(bool, ContextState), ContextError> {
        let dialogue = self.dialogue.clone();
        let removed = self
            .mutate_context(move |context| {
                let documents = tauqe_core::outer_docs::affected_documents(
                    &logical,
                    &context.outer_docs_files(),
                );
                let mut removed = false;
                for document in documents {
                    removed |= context.remove_outer_docs_file(&document).unwrap_or(false);
                }
                if removed {
                    dialogue.reset();
                }
                removed
            })
            .await?;
        Ok((removed, self.publication.state()))
    }
}

impl Application {
    pub(crate) async fn rebind_context(&self, root: PathBuf) -> Result<(), ContextError> {
        self.mutate_context(move |context| context.set_repo_root(root))
            .await
    }

    pub async fn clear_context(
        &self,
        scope: ContextClearScope,
    ) -> Result<ContextState, ContextError> {
        self.context_session().clear_context(scope).await
    }

    pub(crate) async fn release_cached_documents(
        &self,
        logical: String,
    ) -> Result<(bool, ContextState), ContextError> {
        self.context_session()
            .release_cached_documents(logical)
            .await
    }
}

/// Snapshot paired with the admission decision made under the same context lock.
pub struct ContextAdmission {
    pub outcome: tauqe_core::workflow::research::ContextApplicationOutcome,
    pub state: ContextState,
}

/// Normalize declared paths consistently for preloading and horizon expansion.
pub fn normalize_preload_path(path: &str) -> &str {
    path.trim().trim_start_matches("./").trim_start_matches('/')
}

pub(super) async fn repository_files(root: PathBuf) -> Vec<String> {
    tauqe_core::blocking::run_blocking_async(move || {
        tauqe_core::git::list_repository_files(Some(&root))
    })
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
            tauqe_core::config::AppConfig::default(),
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
        peer.mutate_manual_context(|context| {
            context.add_file(
                "shared.rs",
                ContextAccess::Editable,
                tauqe_core::context::ContextLayer::User,
            )
        })
        .await
        .unwrap()
        .unwrap();
        let merged = app
            .context_session()
            .merge_turn_context(local)
            .await
            .unwrap();
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
        app.mutate_manual_context(|context| {
            context
                .add_file(
                    "user.rs",
                    ContextAccess::ReadOnly,
                    tauqe_core::context::ContextLayer::User,
                )
                .unwrap();
            context
                .scenario_mut(|scenario| scenario.add_auto_file("auto.rs", ContextAccess::ReadOnly))
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
                        .add_auto_file("first.rs", ContextAccess::ReadOnly)
                        .unwrap();
                })
                .await
        });
        ready.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let peer = app.clone();
        let next = tokio::spawn(async move {
            peer.mutate_context(|context| {
                context.add_auto_file("second.rs", ContextAccess::ReadOnly)
            })
            .await
            .unwrap()
            .unwrap();
        });
        release.send(()).unwrap();
        next.await.unwrap();
        assert_eq!(app.context_state().await.items.len(), 2);
    }

    #[tokio::test]
    async fn applied_edit_rotates_once_for_full_and_empty_transient_context() {
        let root = tempfile::tempdir().unwrap();
        for file in ["auto.rs", "user.rs"] {
            std::fs::write(root.path().join(file), "source").unwrap();
        }
        let app = application(root.path());
        app.mutate_manual_context(|context| {
            context
                .add_file(
                    "user.rs",
                    ContextAccess::ReadOnly,
                    tauqe_core::context::ContextLayer::User,
                )
                .unwrap();
            context
                .scenario_mut(|scenario| scenario.add_auto_file("auto.rs", ContextAccess::ReadOnly))
                .unwrap();
        })
        .await
        .unwrap();
        let before = app.session_uuid().await;
        let revision = app.context_state().await.revision;
        let state = app.context_session().finish_applied_edit().await.unwrap();
        assert_eq!(state.revision, revision + 1);
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].path, "user.rs");
        let after = app.session_uuid().await;
        assert_ne!(before, after);

        let state = app.context_session().finish_applied_edit().await.unwrap();
        assert_eq!(state.revision, revision + 1);
        assert_eq!(state.items.len(), 1);
        assert_ne!(after, app.session_uuid().await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_cleanup_caller_keeps_the_worker_transition_serialized() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("auto.rs"), "source").unwrap();
        let app = application(root.path());
        app.mutate_context(|context| context.add_auto_file("auto.rs", ContextAccess::ReadOnly))
            .await
            .unwrap()
            .unwrap();
        let before = app.session_uuid().await;
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = app.context_session().clone();
        let caller = tokio::spawn(async move {
            worker
                .finish_applied_edit_with(move || {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                })
                .await
        });
        ready.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let peer = app.clone();
        let next = tokio::spawn(async move { peer.context_for_turn().await.get_state() });
        release.send(()).unwrap();
        let state = next.await.unwrap();
        assert!(state.items.is_empty());
        assert_ne!(before, app.session_uuid().await);
    }

    #[tokio::test]
    async fn settlement_attempts_cleanup_after_merge_failure_and_without_returned_context() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("auto.rs"), "source").unwrap();
        let app = application(root.path());
        let session = app.context_session();
        app.mutate_context(|context| context.add_auto_file("auto.rs", ContextAccess::ReadOnly))
            .await
            .unwrap()
            .unwrap();
        let before = app.session_uuid().await;
        session.fail_next_settlement(true, true);
        let (state, errors) = session
            .settle_turn_context(Some(app.context_for_turn().await), true)
            .await;
        assert_eq!(errors.len(), 2);
        assert!(errors[0].contains("merge"));
        assert!(errors[1].contains("cleanup"));
        assert!(state.items.is_empty());
        let rotated = app.session_uuid().await;
        assert_ne!(before, rotated);

        app.mutate_context(|context| context.add_auto_file("auto.rs", ContextAccess::ReadOnly))
            .await
            .unwrap()
            .unwrap();
        let (state, errors) = session.settle_turn_context(None, true).await;
        assert!(errors.is_empty());
        assert!(state.items.is_empty());
        assert_ne!(rotated, app.session_uuid().await);
    }

    #[tokio::test]
    async fn applied_settlement_merges_full_turn_context_before_clearing_transient_layers() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Conventions.md"), "root").unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        for path in ["src/Conventions.md", "src/auto.rs", "user.rs"] {
            std::fs::write(root.path().join(path), "source").unwrap();
        }
        let document = root
            .path()
            .join(tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join("notes.md");
        std::fs::create_dir_all(document.parent().unwrap()).unwrap();
        std::fs::write(document, "external notes").unwrap();
        let session = ContextSession::new(root.path().into());
        session
            .mutate_context(|context| context.load_pinned(&["Conventions.md".into()]))
            .await
            .unwrap();
        let before = session.session_uuid();
        let initial_revision = session.context_state().await.revision;
        session
            .mutate_manual(|context| {
                context.add_file(
                    "user.rs",
                    ContextAccess::Editable,
                    tauqe_core::context::ContextLayer::User,
                )
            })
            .await
            .unwrap()
            .unwrap();
        let mut turn = session.context_for_turn().await;
        turn.add_auto_file("src/auto.rs", ContextAccess::ReadOnly)
            .unwrap();
        turn.add_outer_docs_file("notes.md").unwrap();
        let (state, errors) = session.settle_turn_context(Some(turn), true).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(state.revision >= initial_revision);
        assert_eq!(state, session.context_state().await);
        assert_ne!(before, session.session_uuid());
        assert_eq!(state.items.len(), 2);
        assert!(state.items.iter().any(|item| item.path == "Conventions.md"));
        assert!(state
            .items
            .iter()
            .any(|item| item.path == "user.rs" && item.access == ContextAccess::Editable));
        assert!(!state.items.iter().any(|item| item.path == "src/auto.rs"
            || item.path == "notes.md"
            || item.path == "src/Conventions.md"));
    }

    #[tokio::test]
    async fn transient_cleanup_preserves_manual_access_and_rotates_only_on_change() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("user.rs"), "source").unwrap();
        std::fs::write(root.path().join("auto.rs"), "source").unwrap();
        let document = root
            .path()
            .join(tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join("notes.md");
        std::fs::create_dir_all(document.parent().unwrap()).unwrap();
        std::fs::write(document, "notes").unwrap();
        let session = ContextSession::new(root.path().into());
        session
            .mutate_manual(|manual| {
                manual.add_file(
                    "user.rs",
                    ContextAccess::Editable,
                    tauqe_core::context::ContextLayer::User,
                )
            })
            .await
            .unwrap()
            .unwrap();
        session
            .mutate_context(|context| {
                context
                    .add_auto_file("auto.rs", ContextAccess::ReadOnly)
                    .unwrap();
                context.add_outer_docs_file("notes.md").unwrap();
            })
            .await
            .unwrap();
        let before = session.session_uuid();
        let state = session.clear_transient_context().await.unwrap();
        assert_ne!(before, session.session_uuid());
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].path, "user.rs");
        assert_eq!(state.items[0].access, ContextAccess::Editable);
        let identity = session.session_uuid();
        assert_eq!(session.clear_transient_context().await.unwrap(), state);
        assert_eq!(identity, session.session_uuid());
    }
}
