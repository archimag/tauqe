//! Application scenarios and the types consumed by their callers.

pub mod types;
pub(crate) use crate::application::operation::next_operation_id;
#[cfg(feature = "test-support")]
pub use crate::application::operation::OperationGuard;
#[cfg(not(feature = "test-support"))]
pub(crate) use crate::application::operation::OperationGuard;
pub use crate::application::operation::OperationInProgress;
pub use crate::application::session::Application;
pub use crate::application::session::FocusUpdate;
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

pub(crate) use crate::application::outer_docs_preload::{OuterDocsPreload, OuterDocsRefusal};

pub(crate) use crate::application::implementation_refinement::PreparedImplementationRefinement;

#[cfg(feature = "test-support")]
pub use crate::application::implementation_step::StepPreparationError;

pub use crate::application::turn_setup::TurnProviderError;
pub(crate) use crate::application::turn_setup::{ResolvedTurnModel, TurnMode};

pub(crate) use crate::application::interactive_completion::classify_interactive_completion;
pub use crate::application::interactive_completion::InteractiveTurnCompletion;

pub(crate) use crate::application::interactive_interruption::InteractiveInterruption;

pub(crate) use crate::application::interactive_task::{
    InteractiveTask, InteractiveTaskError, InteractiveTaskOutcome,
};

pub use crate::application::usage::UsageGuard;

pub use crate::application::usage_totals::{UsageTotals, UsageTotalsSnapshot};

pub(crate) use crate::application::review_analysis::PreparedReviewPostAnalysis;
pub use crate::application::review_analysis::ReviewAnalysisPreparationError;

pub use crate::application::task_exit::{
    abort_and_reap_with_deadline, await_with_deadline, Awaited, TaskExit,
};

pub use crate::application::review_analysis_response::ReviewAnalysisCompletionError;
pub(crate) use crate::application::review_analysis_response::{
    ReviewAnalysisResponse, ReviewAnalysisUpdate,
};

pub use crate::application::review_analysis_completion::ReviewAnalysisPersistenceError;

pub use crate::application::review_analysis_execution::ReviewAnalysisExecutionError;

pub use crate::application::review_analysis_load::ReviewAnalysisLoadError;

pub(crate) use crate::application::review_synthesis_completion::ReviewSynthesisPersistenceError;

pub(crate) use crate::application::review_synthesis_preparation::ResolvedReviewSynthesis;
pub use crate::application::review_synthesis_preparation::ReviewSynthesisPreparationError;

pub(crate) use crate::application::provider_task::{ProviderTask, ProviderTaskError};
pub(crate) use crate::application::review_synthesis_request::PreparedReviewSynthesisRequest;

pub(crate) use crate::application::review_synthesis_response::{
    ReviewSynthesisCompletionError, ReviewSynthesisStream,
};

pub(crate) use crate::application::review_observer::ReviewObserver;
pub(crate) use crate::application::review_synthesis_execution::{
    ReviewSynthesisExecutionConfig, ReviewSynthesisObserver,
};

pub(crate) use crate::application::review_groups::{run_group_audits, GroupAuditObserver};

pub(crate) use crate::application::review_preset::PresetReviewExecutionError;
pub use crate::application::review_preset::PresetReviewPreparationError;

pub use crate::application::locale_generation::LocaleGenerationError;

pub use crate::application::git_squash::{SquashMessage, SquashMessageError};

pub use crate::application::git_squash::{AppliedSquash, SquashApplyError};

#[cfg(feature = "test-support")]
pub use crate::application::plan_horizon::{
    collect_plan_context_files, ContourRelaySummary, PlanHorizonContour,
};
#[cfg(not(feature = "test-support"))]
pub(crate) use crate::application::plan_horizon::{ContourRelaySummary, PlanHorizonContour};
pub(crate) use crate::application::plan_horizon::{
    PlanHorizonDiagnosticsOwned, PlanHorizonRequest,
};
pub use crate::application::plan_horizon::{PlanHorizonError, PlanHorizonFailure};

pub use crate::application::preflight::{run_preflight_verification, PreflightVerdict};

pub use crate::application::plan_batch::{
    BatchOutcome, PlanBatchFinished, PlanBatchOptions, PlanBatchStarted, PlanLaunchError,
    PlanPipelineStarted, PlanQueueError, PlanQueueFailure,
};

pub use crate::application::plan_two_phase::{PlanSynthesisError, PlanSynthesisInput};

#[cfg(feature = "test-support")]
pub use crate::application::architect::test_relay_architect_events;

pub use crate::application::review_batch::{
    group_member_id as review_group_member_id, BatchOutcome as ReviewBatchOutcome,
    BatchTelemetry as ReviewBatchTelemetry, ExecutedReviewBatch,
    FailedMember as FailedReviewMember, ReviewBatchStarted, ReviewLaunchError, ReviewQueueError,
    ReviewQueueFailure,
};

pub use crate::application::review_preview::{
    ReviewPreview, ReviewPreviewError, ReviewPreviewGroup,
};

pub use crate::application::plan_execution::{PlanExecutionError, PlanExecutionFailure};

pub use crate::application::discussion_prompt::{DiscussionPromptError, ModelAskInput};

pub(crate) use crate::application::interactive_updates::{
    InteractivePlanUpdates, InteractiveReviewUpdates,
};
pub(crate) use crate::application::step_completion::{
    classify_step_edit_completion, StepEditDisposition,
};

#[cfg(feature = "test-support")]
pub use crate::application::turn_execution::turn_prompt_protocol;
pub(crate) use crate::application::turn_execution::{
    InteractiveExecutionRequest, StepExecutionRequest,
};

pub(crate) use crate::application::interactive_finalization::{
    InteractiveFinalizationInput, InteractiveFinalizationObserver, InteractiveFinalizationOutcome,
    InteractiveFinalizationUpdate, InteractiveStreamState,
};

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
pub(crate) use crate::application::step_turn::StepLoopOptions;
pub use crate::application::task_exit::TASK_EXIT_DEADLINE;

pub use crate::application::review_commands::{
    AnalyzeReviewError, ReviewEvent, ReviewStartError, ReviewStartInput,
};
pub use crate::application::settings::{
    ClientInitialization, ConfigurationCommand, ConfigurationView, InitializationError,
    ReviewPresetView, SettingsError,
};
pub use crate::application::shutdown::CancellationMode;
