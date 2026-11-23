//! Application scenarios and the types consumed by their callers.

pub mod types;
pub use crate::application::focus::FocusUpdate;
pub(crate) use crate::application::operation::next_operation_id;
#[cfg(feature = "test-support")]
pub use crate::application::operation::OperationGuard;
#[cfg(not(feature = "test-support"))]
pub(crate) use crate::application::operation::OperationGuard;
pub use crate::application::operation::OperationInProgress;
pub use crate::application::session::Application;
pub use tauqe_core::discussion::{DiscussionFocus, DiscussionTarget};

pub(crate) use crate::application::configuration::ConfigurationChange;
pub use crate::application::configuration::{
    ConfigurationError, ConfigurationPatch, ConfigurationSnapshot, ReloadSelection,
};

pub use crate::application::configuration_files::ConfigurationFileError;
pub(crate) use crate::application::configuration_files::ConfigurationFileResult;

pub use crate::application::system_status::{ConfigurationLocations, SystemStatus};

pub use crate::application::history::{HistoryError, HistoryPage};

pub use crate::application::context::{ContextClearScope, ContextError};

pub(crate) use crate::application::context::normalize_preload_path;
pub use crate::application::context_operation::ContextPatternAddition;

pub(crate) use crate::application::outer_docs_cache::OuterDocsCacheRemoval;
pub use tauqe_core::outer_docs::{CacheError, CacheRemoval, CachedDocument};

pub(crate) use crate::application::outer_docs_preload::OuterDocsRefusal;

#[cfg(feature = "test-support")]
pub use crate::workflows::plan::StepPreparationError;

pub use crate::application::turn_setup::TurnProviderError;

pub use crate::application::interactive_completion::InteractiveTurnCompletion;

pub(crate) use crate::application::interactive_task::{
    InteractiveTask, InteractiveTaskError, InteractiveTaskOutcome,
};

pub use crate::application::usage::UsageGuard;

pub use crate::application::usage_totals::{UsageTotals, UsageTotalsSnapshot};

pub use crate::application::task_exit::{
    abort_and_reap_with_deadline, await_with_deadline, Awaited, TaskExit,
};

pub use crate::workflows::review::analysis::{
    AnalyzeReviewError, ReviewAnalysisCompletionError, ReviewAnalysisExecutionError,
    ReviewAnalysisLoadError, ReviewAnalysisPersistenceError, ReviewAnalysisPreparationError,
};

pub use crate::application::locale_generation::LocaleGenerationError;

pub use crate::application::git_squash::{SquashMessage, SquashMessageError};

pub use crate::application::git_squash::{AppliedSquash, SquashApplyError};

#[cfg(feature = "test-support")]
pub use crate::workflows::architect::{
    test_relay_architect_events, ArchitectScope, ContourRelaySummary, PlanHorizonContour,
};
#[cfg(feature = "test-support")]
pub use crate::workflows::plan::synthesis::collect_plan_context_files;

pub use crate::application::preflight::{run_preflight_verification, PreflightVerdict};

pub use crate::application::plan_batch::{
    BatchOutcome, PlanBatchFinished, PlanBatchOptions, PlanBatchStarted, PlanPipelineStarted,
    PlanQueueError, PlanQueueFailure,
};
pub use crate::workflows::plan::{PlanExecutionError, PlanExecutionFailure, PlanLaunchError};

pub use crate::workflows::plan::synthesis::{
    PlanHorizonError, PlanHorizonFailure, PlanSynthesisError, PlanSynthesisInput,
};

pub use crate::workflows::review::{
    group_member_id as review_group_member_id, BatchOutcome as ReviewBatchOutcome,
    BatchTelemetry as ReviewBatchTelemetry, ExecutedReviewBatch,
    FailedMember as FailedReviewMember, ReviewBatchStarted, ReviewLaunchError, ReviewQueueError,
    ReviewQueueFailure,
};

pub use crate::application::discussion_prompt::{DiscussionPromptError, ModelAskInput};

#[cfg(feature = "test-support")]
pub use crate::workflows::step::run::turn_prompt_protocol;

pub use crate::application::documents::{DocumentError, PlanPreparationError};
pub use crate::application::repository::{RepositoryError, SquashPreview};

pub use tauqe_core::git::state::RepositoryState;

pub use crate::application::repository::{discover_repository, repository_configuration};
pub use tauqe_core::config::cli_arg_value_from;

pub use crate::application::ask::{AskError, AskStarted};
pub use crate::application::edit_cycle::EditCycleEvent;
#[cfg(feature = "test-support")]
pub use crate::application::edit_cycle::{EditCycle, EditTermination};
pub use crate::application::events::{ApplicationEvent, ApplicationObserver};
pub use crate::application::task_exit::TASK_EXIT_DEADLINE;

pub use crate::application::settings::{
    ClientInitialization, ConfigurationCommand, ConfigurationView, InitializationError,
    ReviewPresetView, SettingsError,
};
pub use crate::application::shutdown::CancellationMode;
pub use crate::workflows::review::audit::{
    PresetReviewPreparationError, ReviewEvent, ReviewPreview, ReviewPreviewError,
    ReviewPreviewGroup, ReviewStartError, ReviewStartInput, ReviewSynthesisPreparationError,
};
