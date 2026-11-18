//! Public application entry point. Session state stays private to the engine.

use std::sync::Arc;
use tokio::sync::Mutex;

pub use crate::discussion::{DiscussionFocus, DiscussionTarget, FocusUpdate};

#[derive(Clone, Default)]
pub struct Application {
    session: Arc<Session>,
}

#[derive(Default)]
struct Session {
    usage: crate::application::usage::UsageSession,
    interactive_execution: crate::application::interactive_task::InteractiveExecution,
    context: crate::application::context::ContextSession,
    history: crate::application::history::HistorySession,
    configuration: crate::application::configuration::Configuration,
    dialogue: crate::application::dialogue::DialogueSession,
    transaction: Arc<std::sync::atomic::AtomicBool>,
    operations: Arc<crate::application::operation::OperationRegistry>,
    focus: Mutex<DiscussionFocus>,
}

impl Application {
    pub async fn discussion_focus(&self) -> DiscussionFocus {
        self.session.focus.lock().await.clone()
    }

    pub async fn set_discussion_focus(&self, focus: DiscussionFocus) -> FocusUpdate {
        let mut current = self.session.focus.lock().await;
        let changed = *current != focus;
        *current = focus.clone();
        FocusUpdate { focus, changed }
    }

    pub async fn clear_discussion_focus(&self) -> FocusUpdate {
        self.set_discussion_focus(DiscussionFocus::None).await
    }

    /// Compare and clear under one lock so releasing a deleted entity cannot
    /// clear a focus installed on another entity concurrently.
    pub async fn release_discussion_focus(&self, anchor: &DiscussionTarget) -> FocusUpdate {
        let mut current = self.session.focus.lock().await;
        let changed =
            matches!(&*current, DiscussionFocus::Focused { target, .. } if target == anchor);
        if changed {
            *current = DiscussionFocus::None;
        }
        FocusUpdate {
            focus: current.clone(),
            changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focus(target: DiscussionTarget) -> DiscussionFocus {
        DiscussionFocus::Focused {
            target,
            focused_items: vec!["item-1".into()],
        }
    }

    #[tokio::test]
    async fn cloned_application_shares_state_and_reports_only_changes() {
        let app = Application::default();
        let client = app.clone();
        assert!(!app.clear_discussion_focus().await.changed);
        let requested = focus(DiscussionTarget::Plan {
            id: "plan-1".into(),
        });
        let update = app.set_discussion_focus(requested.clone()).await;
        assert!(update.changed);
        assert_eq!(update.focus, requested);
        assert_eq!(client.discussion_focus().await, requested);
        assert!(!client.set_discussion_focus(requested).await.changed);
        assert!(client.clear_discussion_focus().await.changed);
        assert_eq!(app.discussion_focus().await, DiscussionFocus::None);
    }

    #[tokio::test]
    async fn release_preserves_other_targets_and_clears_matching_target_once() {
        let app = Application::default();
        let deleted = DiscussionTarget::ReviewGroup {
            session_id: "review-1".into(),
            group_id: "group-1".into(),
        };
        for survivor in [
            DiscussionTarget::Plan {
                id: "plan-1".into(),
            },
            DiscussionTarget::Review {
                id: "review-1".into(),
            },
            DiscussionTarget::ReviewGroup {
                session_id: "review-2".into(),
                group_id: "group-1".into(),
            },
            DiscussionTarget::ReviewGroup {
                session_id: "review-1".into(),
                group_id: "group-2".into(),
            },
        ] {
            let expected = focus(survivor);
            app.set_discussion_focus(expected.clone()).await;
            let update = app.release_discussion_focus(&deleted).await;
            assert!(!update.changed);
            assert_eq!(update.focus, expected);
        }
        app.set_discussion_focus(focus(deleted.clone())).await;
        let update = app.release_discussion_focus(&deleted).await;
        assert!(update.changed);
        assert_eq!(update.focus, DiscussionFocus::None);
        assert!(!app.release_discussion_focus(&deleted).await.changed);
    }

    #[tokio::test]
    async fn applications_have_independent_sessions() {
        let first = Application::default();
        let second = Application::default();
        first
            .set_discussion_focus(focus(DiscussionTarget::Plan {
                id: "plan-1".into(),
            }))
            .await;
        assert_eq!(second.discussion_focus().await, DiscussionFocus::None);
    }
}

pub use crate::application::operation::{next_operation_id, OperationGuard, OperationInProgress};

impl Application {
    pub fn try_begin_operation(&self) -> Result<OperationGuard, OperationInProgress> {
        self.session.operations.begin()
    }
    pub fn active_operation_id(&self) -> Option<u64> {
        self.session.operations.current_id()
    }
    pub fn cancel_active_operation(&self) -> Option<u64> {
        self.session.operations.hard_cancel()
    }
    pub fn soft_stop_active_operation(&self) {
        self.session.operations.soft_stop();
    }
    /// Abort only the task belonging to this operation identity.
    pub fn abort_operation(&self, id: u64) -> bool {
        self.session.operations.abort(id)
    }
    /// Release an orphan's registration without disturbing a newer operation.
    pub(crate) fn release_operation_registration(&self, id: u64) -> bool {
        self.session.operations.release(id)
    }
}

#[cfg(test)]
mod operation_tests {
    use super::*;

    #[test]
    fn registration_is_exclusive_and_guard_drop_releases_it() {
        let app = Application::default();
        let clone = app.clone();
        let guard = app.try_begin_operation().unwrap();
        assert_eq!(clone.active_operation_id(), Some(guard.id()));
        assert!(clone.try_begin_operation().is_err());
        drop(guard);
        assert_eq!(app.active_operation_id(), None);
        assert!(clone.try_begin_operation().is_ok());
    }

    #[test]
    fn cancellation_keeps_registration_and_soft_stop_is_advisory() {
        let app = Application::default();
        let guard = app.try_begin_operation().unwrap();
        let cancel = guard.cancel_receiver();
        let stop = guard.soft_stop_receiver();
        app.soft_stop_active_operation();
        assert!(*stop.borrow());
        assert!(!*cancel.borrow());
        assert_eq!(app.cancel_active_operation(), Some(guard.id()));
        assert!(*cancel.borrow());
        assert_eq!(app.cancel_active_operation(), Some(guard.id()));
        assert!(app.try_begin_operation().is_err());
        drop(guard);
        assert_eq!(app.cancel_active_operation(), None);
    }

    #[tokio::test]
    async fn stale_guard_and_watchdog_cannot_touch_a_new_operation() {
        let app = Application::default();
        let stale = app.try_begin_operation().unwrap();
        let stale_id = stale.id();
        assert!(app.release_operation_registration(stale_id));
        let current = app.try_begin_operation().unwrap();
        let id = current.id();
        let task = tokio::spawn(std::future::pending::<()>());
        current.set_abort_handle(task.abort_handle());
        stale.clear_abort_handle();
        stale.set_abort_handle(task.abort_handle());
        assert!(!app.abort_operation(stale_id));
        assert!(!app.release_operation_registration(stale_id));
        drop(stale);
        assert_eq!(app.active_operation_id(), Some(id));
        assert!(app.abort_operation(id));
        assert!(task.await.unwrap_err().is_cancelled());
        drop(current);
        assert_eq!(app.active_operation_id(), None);
    }

    #[tokio::test]
    async fn clearing_a_finished_task_handle_keeps_the_slot() {
        let app = Application::default();
        let guard = app.try_begin_operation().unwrap();
        let task = tokio::spawn(std::future::pending::<()>());
        guard.set_abort_handle(task.abort_handle());
        guard.clear_abort_handle();
        assert!(!app.abort_operation(guard.id()));
        assert_eq!(app.active_operation_id(), Some(guard.id()));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
}

impl Application {
    /// Build the turn policy against this session's transaction activity.
    pub fn git_transaction_policy(
        &self,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> crate::workflow::git_policy::GitTransactionPolicy {
        crate::workflow::git_policy::GitTransactionPolicy::with_guard_and_cancel(
            Arc::clone(&self.session.transaction),
            cancel,
        )
    }

    pub(crate) fn transaction_is_active(&self) -> bool {
        self.session
            .transaction
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Test seam for shutdown tests that simulate a stalled Git transaction.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_transaction_activity(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.session.transaction)
    }
}

impl Application {
    pub async fn session_uuid(&self) -> String {
        self.session.dialogue.id().await
    }

    pub async fn reset_session_uuid(&self) -> String {
        self.session.dialogue.reset().await
    }

    pub async fn maybe_rotate_session_on_model_change(
        &self,
        old: &crate::models::ModelRef,
        new: &crate::models::ModelRef,
    ) -> bool {
        self.session.dialogue.rotate_on_model_change(old, new).await
    }
}

#[cfg(test)]
mod dialogue_tests {
    use super::*;
    use crate::models::ModelRef;

    #[tokio::test]
    async fn dialogue_identity_is_shared_by_clones_and_isolated_between_sessions() {
        let app = Application::default();
        let clone = app.clone();
        let other = Application::default();
        let original = app.session_uuid().await;
        assert_eq!(
            uuid::Uuid::parse_str(&original).unwrap().get_version_num(),
            7
        );
        assert_eq!(clone.session_uuid().await, original);
        assert_ne!(other.session_uuid().await, original);
        let reset = clone.reset_session_uuid().await;
        assert_ne!(reset, original);
        assert_eq!(app.session_uuid().await, reset);
    }

    #[tokio::test]
    async fn model_reselection_keeps_the_cache_and_a_model_change_rotates_it() {
        let app = Application::default();
        let old = ModelRef::openrouter("alpha/model");
        let new = ModelRef::openrouter("beta/model");
        let warm = app.session_uuid().await;
        assert!(!app.maybe_rotate_session_on_model_change(&old, &old).await);
        assert_eq!(app.session_uuid().await, warm);
        assert!(app.maybe_rotate_session_on_model_change(&old, &new).await);
        let fresh = app.session_uuid().await;
        assert_ne!(fresh, warm);
        assert!(!app.maybe_rotate_session_on_model_change(&new, &new).await);
        assert_eq!(app.session_uuid().await, fresh);
    }
}

pub use crate::application::configuration::{
    ConfigurationChange, ConfigurationError, ConfigurationPatch, ConfigurationSnapshot,
    ReloadSelection,
};

impl Application {
    pub fn new(config: crate::config::AppConfig) -> Self {
        Self {
            session: Arc::new(Session {
                configuration: crate::application::configuration::Configuration::new(config),
                ..Session::default()
            }),
        }
    }
    pub async fn configuration(&self) -> ConfigurationSnapshot {
        self.session.configuration.snapshot().await
    }
    pub(crate) async fn replace_configuration(
        &self,
        config: crate::config::AppConfig,
        selection: ReloadSelection,
    ) -> ConfigurationChange {
        self.session.configuration.replace(config, selection).await
    }
    pub(crate) async fn update_configuration(
        &self,
        patch: ConfigurationPatch,
    ) -> Result<ConfigurationChange, ConfigurationError> {
        let change = self.session.configuration.update(patch).await?;
        let model = change
            .snapshot
            .config
            .resolve_model(&change.snapshot.selection);
        self.maybe_rotate_session_on_model_change(&change.previous_model, &model)
            .await;
        Ok(change)
    }
    pub(crate) async fn select_model(
        &self,
        selection: crate::models::ModelSelection,
    ) -> Result<ConfigurationChange, ConfigurationError> {
        self.update_configuration(ConfigurationPatch {
            selection: Some(selection),
            ..Default::default()
        })
        .await
    }
}

#[cfg(test)]
mod configuration_tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::models::{ModelRef, ModelSelection};
    use crate::workflow::WorkflowKind;

    fn config() -> AppConfig {
        let mut config = AppConfig::default();
        config.models.repl = vec![
            ModelRef::openrouter("alpha/model").into(),
            ModelRef::openrouter("beta/model").into(),
        ];
        config
    }

    #[tokio::test]
    async fn snapshots_are_detached_and_clones_observe_model_selection() {
        let app = Application::new(config());
        let clone = app.clone();
        let before = app.configuration().await;
        let warm = app.session_uuid().await;
        let requested = ModelSelection::Specific(ModelRef::openrouter("beta/model"));
        app.select_model(requested.clone()).await.unwrap();
        assert_eq!(clone.configuration().await.selection, requested);
        assert_ne!(before.selection, requested);
        let fresh = app.session_uuid().await;
        assert_ne!(fresh, warm);
        clone.select_model(requested).await.unwrap();
        assert_eq!(app.session_uuid().await, fresh);
    }

    #[tokio::test]
    async fn invalid_patch_changes_neither_config_selection_nor_dialogue() {
        let app = Application::new(config());
        let before = app.configuration().await;
        let warm = app.session_uuid().await;
        let result = app
            .update_configuration(ConfigurationPatch {
                selection: Some(ModelSelection::Specific(ModelRef::openrouter("beta/model"))),
                history_model: Some(ModelRef::openrouter("missing/model")),
                workflow: Some(WorkflowKind::Naive),
                ..Default::default()
            })
            .await;
        assert!(matches!(
            result,
            Err(ConfigurationError::UnknownHistoryModel { .. })
        ));
        let after = app.configuration().await;
        assert_eq!(after.selection, before.selection);
        assert_eq!(after.config.models.scribe, before.config.models.scribe);
        assert_eq!(after.config.repl.workflow, before.config.repl.workflow);
        assert_eq!(app.session_uuid().await, warm);
    }

    #[tokio::test]
    async fn selection_precedes_model_and_unknown_models_are_rejected() {
        let app = Application::new(config());
        let selection = ModelSelection::Specific(ModelRef::openrouter("beta/model"));
        app.update_configuration(ConfigurationPatch {
            selection: Some(selection.clone()),
            model: Some(ModelRef::openrouter("missing/model")),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(matches!(
            app.select_model(ModelSelection::Specific(ModelRef::openrouter(
                "missing/model"
            )))
            .await,
            Err(ConfigurationError::UnknownModel { .. })
        ));
        assert_eq!(app.configuration().await.selection, selection);
    }

    #[tokio::test]
    async fn reload_explicitly_preserves_or_resets_selection() {
        let app = Application::new(config());
        let selected = ModelSelection::Specific(ModelRef::openrouter("beta/model"));
        app.select_model(selected.clone()).await.unwrap();
        let reloaded = app
            .replace_configuration(config(), ReloadSelection::Preserve)
            .await;
        assert_eq!(reloaded.snapshot.selection, selected);
        let reset = app
            .replace_configuration(config(), ReloadSelection::Default)
            .await;
        assert_eq!(
            reset.snapshot.selection,
            reset.snapshot.config.default_selection()
        );
    }
}

pub use crate::application::configuration_files::{
    ConfigurationFileError, ConfigurationFileResult,
};

pub use crate::application::system_status::{ConfigurationLocations, SystemStatus};

pub use crate::application::configuration_operation::ConfigurationOperation;

pub use crate::application::history::{HistoryError, HistoryPage};

impl Application {
    /// Opens a repository session after best-effort crash recovery and legacy
    /// review migration. Startup runs once per newly constructed session;
    /// cloning an application shares the initialized session.
    pub fn for_repository(
        config: crate::config::AppConfig,
        root: std::path::PathBuf,
        mutations: tokio::sync::mpsc::UnboundedSender<crate::history::HistoryMutation>,
    ) -> Self {
        crate::application::repository_startup::prepare(&root);
        Self {
            session: Arc::new(Session {
                configuration: crate::application::configuration::Configuration::new(config),
                usage: crate::application::usage::UsageSession::new(&root),
                context: crate::application::context::ContextSession::new(root.clone()),
                history: crate::application::history::HistorySession::new(root, mutations),
                ..Session::default()
            }),
        }
    }
    pub(crate) fn usage_session(&self) -> &crate::application::usage::UsageSession {
        &self.session.usage
    }

    pub(crate) fn interactive_execution(
        &self,
    ) -> &crate::application::interactive_task::InteractiveExecution {
        &self.session.interactive_execution
    }

    pub(crate) fn context_session(&self) -> &crate::application::context::ContextSession {
        &self.session.context
    }

    pub(crate) fn history_session(&self) -> &crate::application::history::HistorySession {
        &self.session.history
    }
}

impl Application {
    pub(crate) fn owns_operation(&self, operation: &OperationGuard) -> bool {
        operation.belongs_to(&self.session.operations)
    }
}

pub use crate::application::history_operation::{HistoryClearOutcome, HistoryOperation};

pub use crate::application::context::{ContextClearScope, ContextError};

pub use crate::application::context::{normalize_preload_path, ContextAdmission};
pub use crate::application::context_operation::{ContextOperation, ContextPatternAddition};

pub use crate::application::outer_docs_cache::OuterDocsCacheRemoval;
pub use crate::outer_docs::{CacheError, CacheRemoval, CachedDocument};

pub use crate::application::outer_docs_preload::{OuterDocsPreload, OuterDocsRefusal};

pub use crate::application::implementation_refinement::{
    PreparedImplementationRefinement, RefinementPreparationError,
};

pub use crate::application::implementation_step::StepPreparationError;

pub use crate::application::turn_setup::{ResolvedTurnModel, TurnMode, TurnProviderError};

pub use crate::application::interactive_history::InteractiveHistoryStart;

pub use crate::application::interactive_completion::{
    classify_interactive_completion, InteractiveTurnCompletion,
};

pub use crate::application::interactive_interruption::InteractiveInterruption;

pub use crate::application::interactive_task::{
    InteractiveTask, InteractiveTaskError, InteractiveTaskOutcome,
};

pub use crate::application::usage::UsageGuard;

pub use crate::application::usage_totals::{UsageTotals, UsageTotalsSnapshot};

pub use crate::application::review_analysis::{
    PreparedReviewPostAnalysis, ReviewAnalysisPreparationError,
};

pub use crate::application::task_exit::{
    abort_and_reap_with_deadline, await_with_deadline, Awaited, TaskExit,
};

pub use crate::application::review_analysis::ReviewAnalysisTask;

pub use crate::application::review_analysis_response::{
    ReviewAnalysisCompletionError, ReviewAnalysisResponse, ReviewAnalysisUpdate,
};

pub use crate::application::review_analysis_completion::ReviewAnalysisPersistenceError;

pub use crate::application::review_analysis_execution::{
    ReviewAnalysisExecutionError, ReviewAnalysisObserver,
};

pub use crate::application::review_analysis_load::{
    parse_review_analysis_target, ReviewAnalysisLoadError,
};

pub use crate::application::review_analysis::ReviewAnalysisProviderError;

pub use crate::application::review_synthesis_completion::ReviewSynthesisPersistenceError;

pub use crate::application::review_synthesis_preparation::{
    PreparedReviewSynthesisWindow, ResolvedReviewSynthesis, ReviewSynthesisPreparationError,
    ReviewSynthesisProviderError,
};

pub use crate::application::provider_task::{ProviderTask, ProviderTaskError};
pub use crate::application::review_synthesis_request::PreparedReviewSynthesisRequest;

pub use crate::application::review_synthesis_response::{
    ReviewSynthesisCompletionError, ReviewSynthesisStream,
};

pub use crate::application::review_observer::ReviewObserver;
pub use crate::application::review_synthesis_execution::{
    ReviewSynthesisExecutionConfig, ReviewSynthesisExecutionError, ReviewSynthesisObserver,
    ReviewSynthesisResult,
};

pub use crate::application::review_groups::{run_group_audits, GroupAuditObserver};

pub use crate::application::review_preset::{
    PresetReviewExecutionError, PresetReviewObserver, PresetReviewPreparationError,
    PresetReviewResult, ResolvedPresetReview,
};

pub use crate::application::locale_generation::{
    LocaleExecutionConfig, LocaleGenerationError, PreparedLocaleGeneration, UsageObserver,
};

pub use crate::application::git_squash::{
    PreparedSquashMessage, SquashMessage, SquashMessageError, SquashMessageExecutionConfig,
};

pub use crate::application::git_squash::{AppliedSquash, SquashApplyError};

pub use crate::application::plan_horizon::{
    collect_plan_context_files, ContourRelaySummary, PlanHorizonContour,
    PlanHorizonDiagnosticsOwned, PlanHorizonError, PlanHorizonFailure, PlanHorizonObserver,
    PlanHorizonRequest,
};

pub use crate::application::preflight::{run_preflight_verification, PreflightVerdict};

pub use crate::application::plan_batch::{BatchOutcome, PlanBatchObserver, PlanBatchOptions};

pub use crate::application::plan_two_phase::{TwoPhaseObserver, TwoPhaseRequest};

pub use crate::application::architect::{relay_architect_events, ArchitectObserver};

pub use crate::application::review_batch::{
    group_member_id as review_group_member_id, BatchOutcome as ReviewBatchOutcome,
    BatchTelemetry as ReviewBatchTelemetry, ExecutedReviewBatch,
    FailedMember as FailedReviewMember, PreparedReviewBatch, ReviewBatchObserver,
    ReviewMemberRequest, ReviewQueueError, ReviewQueueFailure,
};

pub use crate::application::plan_batch::{
    PlanQueueError, PlanQueueFailure, PlanQueueObserver, PreparedPlanQueue,
};

pub use crate::application::review_preview::{
    ReviewPreview, ReviewPreviewError, ReviewPreviewGroup,
};

pub use crate::application::plan_execution::{PlanExecutionError, PlanExecutionFailure};

pub use crate::application::discussion_prompt::{
    DiscussionPromptError, ModelAskInput, PreparedModelAsk,
};

pub use crate::application::interactive_updates::{
    InteractivePlanUpdates, InteractiveReviewUpdates,
};
pub use crate::application::step_completion::{
    classify_step_edit_completion, StepEditCompletion, StepEditDisposition,
};

pub use crate::application::turn_execution::{
    turn_prompt_protocol, InteractiveExecutionRequest, PreparedStepExecution,
    StartedInteractiveExecution, StepExecutionPreparationError, StepExecutionRequest,
};

pub use crate::application::interactive_finalization::{
    FinalizedInteractiveTurn, InteractiveFinalizationInput, InteractiveFinalizationObserver,
    InteractiveFinalizationOutcome, InteractiveFinalizationUpdate, InteractiveStreamState,
};

pub use crate::application::documents::{
    DocumentDeletion, DocumentError, Documents, PlanPreparationError, PreparedPlanReplan,
};
pub use crate::application::repository::{RepositoryError, SquashPreview};

pub use crate::git::state::RepositoryState;

pub use crate::application::repository::{discover_repository, repository_configuration};
pub use crate::config::cli_arg_value_from;
