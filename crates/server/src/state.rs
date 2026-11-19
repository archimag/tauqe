use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use tauqe_flow::api::types::ContextState;
#[cfg(test)]
use tauqe_protocol::DiscussionTarget;

#[cfg(test)]
use tauqe_core::context::ContextManager;
use tauqe_flow::api::types::AppConfig;
use tauqe_flow::api::types::HistoryMutation;
#[cfg(test)]
use tauqe_flow::api::UsageTotalsSnapshot;
#[cfg(test)]
use tauqe_protocol::DiscussionFocus;

#[cfg(test)]
use crate::reap::task_exit_deadline;
use crate::transport::OutChannel;

#[cfg(test)]
pub use tauqe_flow::api::OperationGuard;

pub struct AppState {
    /// Application facade owning the migrated session scenarios.
    /// Discussion focus state and transition rules are private to core.
    pub application: tauqe_flow::api::Application,
    pub out: OutChannel,
    pub login: Arc<crate::handlers::login::SignIn>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        repo_path: PathBuf,
        history_sender: tokio::sync::mpsc::UnboundedSender<HistoryMutation>,
        out: OutChannel,
    ) -> Arc<Self> {
        let application =
            tauqe_flow::api::Application::for_repository(config, repo_path, history_sender);

        let login = Arc::new(crate::handlers::login::SignIn::new(out.clone()));
        let application = application.with_chatgpt_sign_in(login.clone());
        Arc::new(Self {
            application,
            out,
            login,
        })
    }

    /// Registers an exclusive operation in the application runtime.
    #[cfg(test)]
    pub fn try_begin_operation(self: &Arc<Self>) -> Result<OperationGuard, ()> {
        self.application.test_begin_operation().map_err(|_| {
            tracing::warn!(
                code = tauqe_protocol::errors::OPERATION_IN_PROGRESS,
                "Refused to register a new operation: a slot is already active"
            );
        })
    }

    pub fn is_operation_active(&self) -> bool {
        self.application.active_operation_id().is_some()
    }

    /// Resolve caller-scoped budgets and delegate cancellation policy to core.
    #[cfg(test)]
    pub fn hard_cancel_active_operation(self: &Arc<Self>) {
        self.application.cancel(
            tauqe_flow::api::CancellationMode::Hard,
            task_exit_deadline(),
        );
    }

    #[cfg(test)]
    pub(crate) async fn abort_stalled_operation_with_deadline(
        &self,
        op_id: u64,
        exit_deadline: std::time::Duration,
    ) {
        self.application
            .abort_stalled_operation_with_deadline(op_id, exit_deadline)
            .await;
    }

    pub async fn force_abort_active_operation(&self, budget: std::time::Duration) {
        self.application.force_abort_active_operation(budget).await;
    }

    /// Delivers a cancellation signal to the active operation without clearing
    /// its registration, so callers can still observe when the operation
    /// finishes and unwinds its transaction.
    ///
    /// The signal is raised at most once per operation: the shutdown drain
    /// re-signals on every poll, and logging each repeat would flood the
    /// journal.
    pub fn cancel_active_operation(&self) {
        let _ = self.signal_hard_cancel();
    }

    /// Raises the hard-cancel signal on the registered operation and returns its
    /// id, or `None` when no operation is registered. Shared by the shutdown
    /// drain and the interactive hard-cancel path.
    fn signal_hard_cancel(&self) -> Option<u64> {
        self.application.cancel_active_operation()
    }

    /// Requests a graceful stop of the active operation without clearing its
    /// registration.
    ///
    /// A batch supervisor observes this signal between members: the current
    /// member finishes its bounded turn, compiler verification, and Git commit,
    /// and only then does the queue stop before launching the next member.
    /// Unlike [`AppState::cancel_active_operation`], the request is advisory and
    /// never interrupts work that is already running.
    #[cfg(test)]
    pub fn soft_stop_active_operation(&self) {
        self.application.cancel(
            tauqe_flow::api::CancellationMode::Soft,
            task_exit_deadline(),
        );
    }

    /// Returns the authoritative repository root shared by every handler.
    ///
    /// Git, context, plan, and review handlers must all resolve the repository
    /// from this single source instead of re-deriving it from the process CWD,
    /// which can drift from the initialized repository.
    #[cfg(test)]
    pub async fn repo_root(&self) -> PathBuf {
        self.application.repository_root().await
    }

    /// Returns the authoritative usage snapshot of the daemon.
    ///
    /// The accounting state describes the snapshot together with the identity
    /// of the window that owns its in-flight scope, so every reader goes through
    /// this accessor instead of reaching for the bare snapshot and silently
    /// dropping the identity that guards it.
    #[cfg(test)]
    pub async fn usage_snapshot(&self) -> UsageTotalsSnapshot {
        self.application.usage_snapshot().await
    }

    /// Applies a mutation to the shared context atomically.
    ///
    /// The exclusive write guard is acquired before the blocking closure runs
    /// and is moved into the blocking pool together with it, so the entire
    /// read-modify-write sequence stays serialized. Cloning the manager under a
    /// short read guard and overwriting the slot afterwards would let a
    /// concurrent mutation be silently lost and would reset the monotonic
    /// context revision.
    ///
    /// A guarded closure must never perform subprocess work. The guard excludes
    /// every context reader for its whole body, so spawning a `git` process
    /// inside it would stall `context/get` and every `repo_root` lookup behind
    /// that spawn. Resolve such inputs on the blocking pool first and move the
    /// result into the closure.
    #[cfg(test)]
    pub async fn mutate_context<T, F>(self: &Arc<Self>, mutate: F) -> Result<T, String>
    where
        F: FnOnce(&mut ContextManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.application
            .test_mutate_context(mutate)
            .await
            .map_err(|error| error.to_string())
    }

    /// Adapts the application focus scenario for existing RPC callers.
    #[cfg(test)]
    pub async fn discussion_focus(&self) -> DiscussionFocus {
        crate::discussion::focus_dto(self.application.discussion_focus().await)
    }

    #[cfg(test)]
    pub async fn set_discussion_focus(&self, focus: DiscussionFocus) -> DiscussionFocus {
        let observer = crate::publication::Publisher {
            out: self.out.clone(),
        };
        crate::discussion::focus_dto(
            self.application
                .set_focus(crate::discussion::focus_from_dto(focus), &observer)
                .await,
        )
    }

    #[cfg(test)]
    pub async fn release_discussion_focus_anchored_on(&self, anchor: &DiscussionTarget) -> bool {
        let observer = crate::publication::Publisher {
            out: self.out.clone(),
        };
        self.application
            .release_document_focus(
                &crate::discussion::target_from_dto(anchor.clone()),
                &observer,
            )
            .await
    }

    #[cfg(test)]
    pub async fn session_uuid(&self) -> String {
        self.application.session_uuid().await
    }

    /// Clears the transient auto context layer and starts a fresh dialogue
    /// session when files were actually removed.
    #[cfg(test)]
    pub async fn clear_auto_context(self: &Arc<Self>) -> Result<ContextState, String> {
        self.clear_context(tauqe_flow::api::ContextClearScope::Auto)
            .await
    }

    #[cfg(test)]
    pub async fn clear_transient_context(self: &Arc<Self>) -> Result<ContextState, String> {
        self.clear_context(tauqe_flow::api::ContextClearScope::Transient)
            .await
    }

    #[cfg(test)]
    pub async fn clear_all_context(self: &Arc<Self>) -> Result<ContextState, String> {
        self.clear_context(tauqe_flow::api::ContextClearScope::All)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn clear_context(
        &self,
        scope: tauqe_flow::api::ContextClearScope,
    ) -> Result<ContextState, String> {
        self.application
            .clear_context(scope)
            .await
            .map_err(|error| error.to_string())
    }

    #[cfg(test)]
    pub(crate) async fn context_state(&self) -> ContextState {
        self.application.context_state().await
    }

    /// Returns a detached copy for a turn-local workflow.
    #[cfg(test)]
    pub(crate) async fn context_snapshot(&self) -> ContextManager {
        self.application.context_for_turn().await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
    use tauqe_core::context::ContextAccess;
    use tauqe_core::models::ModelRef;

    /// Concurrent context mutations must all survive: a clone-then-overwrite
    /// sequence would drop the mutations that finished earlier and reset the
    /// monotonic revision.
    #[tokio::test]
    async fn mutate_context_serializes_concurrent_mutations() {
        let root = scratch_root("state-context");

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let mut tasks = Vec::new();
        for index in 0..8 {
            let path = format!("file{}.rs", index);
            std::fs::write(root.join(&path), "fn stub() {}\n").unwrap();
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                state
                    .mutate_context(move |ctx| {
                        ctx.add_auto_file(&path, ContextAccess::ReadOnly)
                            .map(|_| ())
                    })
                    .await
            }));
        }

        for task in tasks {
            let outcome = task.await.expect("mutation task joins");
            let applied = outcome.expect("mutation runs on the blocking pool");
            assert!(
                applied.is_ok(),
                "context mutation failed: {:?}",
                applied.err()
            );
        }

        let snapshot = state.context_state().await;
        assert_eq!(
            snapshot.items.len(),
            8,
            "every concurrent mutation must survive"
        );
        assert!(
            snapshot.revision > 1,
            "the context revision must stay monotonic"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Concurrent history mutations must all survive: a clone-then-overwrite
    /// sequence would drop the records that finished earlier.
    #[tokio::test]
    async fn mutate_history_serializes_concurrent_mutations() {
        let root = scratch_root("state-history");

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let mut tasks = Vec::new();
        for index in 0..8 {
            let prompt = format!("Task {}", index);
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                state
                    .application
                    .record_history_turn(prompt, Vec::new())
                    .await
            }));
        }

        for task in tasks {
            let outcome = task.await.expect("mutation task joins");
            outcome.expect("history mutation persists on the blocking pool");
        }

        let entries = state
            .application
            .test_mutate_history(|history| history.get_entries())
            .await
            .unwrap()
            .expect("history reads back");
        assert_eq!(entries.len(), 8, "every concurrent mutation must survive");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Clearing an Auto layer that really held files starts a fresh dialogue
    /// session, because the prompt prefix those files warmed is gone.
    #[tokio::test]
    async fn clear_auto_context_rotates_the_session_when_files_were_removed() {
        let root = scratch_root("state-clear");
        std::fs::write(root.join("auto.rs"), "fn auto() {}\n").unwrap();

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        state
            .mutate_context(|context| context.add_auto_file("auto.rs", ContextAccess::ReadOnly))
            .await
            .expect("fixture context mutation runs")
            .unwrap();
        let warm_session = state.session_uuid().await;

        let snapshot = state.clear_auto_context().await.expect("clear runs");

        assert!(
            snapshot.items.iter().all(|item| item.path != "auto.rs"),
            "the auto file must leave the context"
        );
        assert_ne!(
            warm_session,
            state.session_uuid().await,
            "removing auto files must start a fresh dialogue session"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Clearing an already empty Auto layer removes nothing, so the cached
    /// prompt prefix is intact and the warm session must be preserved.
    #[tokio::test]
    async fn clear_auto_context_keeps_the_session_when_the_layer_is_empty() {
        let root = scratch_root("state-clear");

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let warm_session = state.session_uuid().await;
        let snapshot = state.clear_auto_context().await.expect("clear runs");

        assert!(snapshot.items.is_empty(), "nothing was loaded to clear");
        assert_eq!(
            warm_session,
            state.session_uuid().await,
            "an empty auto layer must not rotate the session"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A full reset drops the mutable layers, so it starts a fresh dialogue
    /// session for exactly the same reason the auto-layer clear does.
    #[tokio::test]
    async fn clear_all_context_rotates_the_session_when_files_were_removed() {
        let root = scratch_root("state-clear");
        std::fs::write(root.join("user.rs"), "fn user() {}\n").unwrap();

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        state
            .mutate_context(|context| context.add_file("user.rs", ContextAccess::Editable))
            .await
            .expect("fixture context mutation runs")
            .unwrap();
        let warm_session = state.session_uuid().await;

        let snapshot = state.clear_all_context().await.expect("clear runs");

        assert!(
            snapshot.items.is_empty(),
            "every mutable layer must be gone"
        );
        assert_ne!(
            warm_session,
            state.session_uuid().await,
            "a full reset must start a fresh dialogue session"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The Auto layer and the external documentation layer are one transient
    /// context: releasing it drops both layers and starts a fresh dialogue
    /// session, while a repeated clear on an already empty transient context
    /// keeps the warm session.
    #[tokio::test]
    async fn clear_transient_context_drops_both_transient_layers_and_rotates_the_session() {
        let root = scratch_root("state-transient");
        std::fs::write(root.join("auto.rs"), "fn auto() {}\n").unwrap();
        // The external document lives in the isolated cache the registry
        // materializes into, never in the working tree. The layer addresses
        // it by its logical path and resolves the bytes in that cache.
        let snapshot = root
            .join(tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join("ref.md");
        std::fs::create_dir_all(snapshot.parent().expect("the snapshot has a parent")).unwrap();
        std::fs::write(&snapshot, "# External snapshot\n").unwrap();

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        state
            .mutate_context(|context| context.add_auto_file("auto.rs", ContextAccess::ReadOnly))
            .await
            .expect("fixture context mutation runs")
            .unwrap();
        state
            .mutate_context(|context| context.add_outer_docs_file("ref.md"))
            .await
            .expect("fixture context mutation runs")
            .unwrap();
        let warm_session = state.session_uuid().await;

        let transient = state.clear_transient_context().await.expect("clear runs");

        assert!(
            transient.items.is_empty(),
            "both transient layers must be released"
        );
        assert_ne!(
            warm_session,
            state.session_uuid().await,
            "releasing the transient context must start a fresh dialogue session"
        );

        // A repeated clear removes nothing, so the warm session is preserved.
        let repeated_session = state.session_uuid().await;
        state
            .clear_transient_context()
            .await
            .expect("the repeated clear runs");
        assert_eq!(
            repeated_session,
            state.session_uuid().await,
            "an empty transient context must not rotate the session"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A deletion releases exactly the anchor bound to the deleted entity; a
    /// focus installed on any other entity survives the release untouched.
    #[tokio::test]
    async fn release_discussion_focus_anchored_on_clears_only_the_named_anchor() {
        let root = scratch_root("state-focus-release");

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let deleted = DiscussionTarget::Plan {
            id: "plan-1".to_string(),
        };
        let neighbour = DiscussionTarget::Plan {
            id: "plan-2".to_string(),
        };
        let review = DiscussionTarget::Review {
            id: "review-1".to_string(),
        };

        // A neighbour of the same kind and an anchor of the other kind both
        // outlive a release that does not name them.
        for survivor in [&neighbour, &review] {
            state
                .set_discussion_focus(DiscussionFocus::from_target(
                    (*survivor).clone(),
                    Vec::new(),
                ))
                .await;
            assert!(
                !state.release_discussion_focus_anchored_on(&deleted).await,
                "a focus anchored elsewhere must never be released"
            );
            assert_eq!(
                state.discussion_focus().await.target(),
                Some(survivor),
                "the surviving anchor must stay installed"
            );
        }

        // The anchor bound to the deleted entity is released exactly once.
        state
            .set_discussion_focus(DiscussionFocus::from_target(deleted.clone(), Vec::new()))
            .await;
        assert!(
            state.release_discussion_focus_anchored_on(&deleted).await,
            "the anchor bound to the deleted entity must be released"
        );
        assert_eq!(
            state.discussion_focus().await,
            DiscussionFocus::None,
            "the released anchor must leave no focus behind"
        );
        assert!(
            !state.release_discussion_focus_anchored_on(&deleted).await,
            "a repeated release finds nothing to clear"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A finished streaming round must leave no abort handle behind, or a later
    /// hard-cancel would abort a completed task instead of the work in flight.
    #[tokio::test]
    async fn clear_abort_handle_drops_the_installed_handle() {
        let root = scratch_root("state-abort");

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let guard = state.try_begin_operation().expect("slot is free");
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        guard.set_abort_handle(task.abort_handle());

        guard.clear_abort_handle();
        assert!(!state.application.abort_operation(guard.id()));
        assert!(state.is_operation_active());

        task.abort();
        let _ = task.await;
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A provider prefix cache belongs to one model, so switching the effective
    /// model starts a fresh dialogue session while reselecting the same model
    /// keeps the warm one.
    #[tokio::test]
    async fn maybe_rotate_session_on_model_change_tracks_the_effective_model() {
        let root = scratch_root("state-model");

        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);

        let alpha = ModelRef::openrouter("alpha/model");
        let beta = ModelRef::openrouter("beta/model");
        let warm = state.session_uuid().await;

        assert!(
            !state
                .application
                .maybe_rotate_session_on_model_change(&alpha, &alpha)
                .await,
            "an unchanged model must not rotate the session"
        );
        assert_eq!(warm, state.session_uuid().await);

        assert!(
            state
                .application
                .maybe_rotate_session_on_model_change(&alpha, &beta)
                .await,
            "a model change must rotate the session"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
