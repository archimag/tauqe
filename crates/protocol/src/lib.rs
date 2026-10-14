#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub const PROTOCOL_VERSION: &str = "0.1.0";

pub mod command;
pub mod config;
pub mod context;
pub mod discussion;
pub mod edit;
pub mod errors;
pub(crate) mod escape;
pub mod events;
pub mod git;
pub mod glob;
pub mod history;
pub mod message;
pub mod methods;
pub mod model;
pub mod plan;
pub mod review;

// The crate root re-exports the wire contract explicitly rather than through
// wildcard globs. Explicit re-exports keep the public surface auditable and
// turn an accidental name collision between two modules into a compile error
// instead of a silently ambiguous glob re-export. Module paths remain public,
// so both `tauqe_protocol::Item` and `tauqe_protocol::module::Item` resolve.
pub use command::{LlmCapability, LlmCommand, LlmOutcome, VerifyOnSuccess, VerifyTarget};
pub use config::{
    ConfigCreateParams, ConfigSetParams, ConfigState, CredentialsSaveParams, SystemStatusResult,
};
pub use context::{
    ContextAccess, ContextAddParams, ContextAddPatternParams, ContextAddPatternResult,
    ContextChangedEvent, ContextClearParams, ContextItem, ContextLayer, ContextRemoveParams,
    ContextSetAccessParams, ContextState,
};
pub use discussion::{
    discussion_response_schema, discussion_response_schema_definition, DiscussionClearFocusParams,
    DiscussionClearFocusResult, DiscussionContextRequest, DiscussionFocus,
    DiscussionFocusChangedEvent, DiscussionGetFocusParams, DiscussionGetFocusResult,
    DiscussionPlanAction, DiscussionPlanUpdate, DiscussionRequest, DiscussionResponse,
    DiscussionSetFocusParams, DiscussionSetFocusResult, DiscussionTarget,
};
pub use edit::{
    EditFileDoneEvent, EditFileRetryingEvent, EditFileStartedEvent, EditFinishedEvent,
    EditHunkEvent, EditOperation, EditProposal, EditStartedEvent, StructuredChangeProposal,
};
pub use errors::{
    ADD_FAILED, ADD_PATTERN_FAILED, CANCELLED, COMMITS_AHEAD_FAILED, CONFIG_CREATE_FAILED,
    CREDENTIALS_SAVE_FAILED, CREDENTIALS_STUB_FAILED, EMPTY_CONTEXT, EMPTY_ITEMS, GET_DIFF_FAILED,
    INIT_FAILED, INTERNAL_ERROR, INVALID_KEY, INVALID_MODEL, INVALID_PARAMS, INVALID_PLAN_ID,
    INVALID_PLAN_TREE, INVALID_PROTOCOL, INVALID_REQUEST, INVALID_WORKFLOW, ITEM_IN_DISCUSSION,
    ITEM_REJECTED, LIST_FILES_FAILED, METHOD_NOT_FOUND, MODEL_ERROR, NEEDS_CONTEXT, NOT_FOUND,
    NOTHING_TO_EXECUTE, NOT_A_LEAF, NO_API_KEY, OPERATION_IN_PROGRESS,
    PARSE_ERROR, PINNED_PROTECTED, PLAN_DELETE_FAILED, PLAN_GET_FAILED, PLAN_GRAFT_FAILED,
    PLAN_LIST_FAILED, PLAN_LOAD_FAILED, PLAN_SAVE_FAILED, PLAN_SET_ACTIVE_FAILED,
    PLAN_UPDATE_FAILED, PREFLIGHT_VERIFICATION_FAILED, REMOVE_FAILED, REPOSITORY_STATE_FAILED,
    REVIEW_DELETE_FAILED, REVIEW_GET_FAILED, REVIEW_LIST_FAILED, REVIEW_LOAD_FAILED,
    REVIEW_UPDATE_FAILED, SCHEMA_ERROR, SET_ACCESS_FAILED, SQUASH_FAILED, SQUASH_PREVIEW_FAILED,
    STEP_BLOCKED, STEP_BLOCKED_BY_DEPENDENCIES, STEP_CANCELLED, STEP_IMMUTABLE,
    STEP_IN_DISCUSSION, UNDO_FAILED, UNEXPECTED_STATUS,
};
pub use git::{
    GitCommitCreatedEvent, GitDiffParams, GitDiffResult, GitSquashApplyParams,
    GitSquashApplyResult, GitSquashCommitItem, GitSquashFileDiff, GitSquashGenerateMessageParams,
    GitSquashGenerateMessageResult, GitSquashPreviewParams, GitSquashPreviewResult, GitUndoResult,
    RepositoryInitParams, RepositoryInitResult, RepositoryListFilesResult, RepositoryState,
};
pub use glob::matches_glob_pattern;
pub use history::{
    HistoryEntryAddedEvent, HistoryGetParams, HistoryGetResult, UiHistoryItem, UiHistoryKind,
};
pub use message::{
    Event, InitializeParams, InitializeResult, Message, Request, RequestId, Response,
    ResponseError,
};
pub use model::{
    BatchFinishStatus, BatchFinishSummary, CoderTier, ModelAskParams,
    ModelChangedEvent, ModelCurrentResult, ModelDeltaEvent, ModelEntry, ModelErrorEvent,
    ModelFinishedEvent, ModelRef, ModelResult, ModelResultEvent, ModelResultProposal, ModelRole,
    ModelRolesEffort, ModelRolesEntries, ModelRolesSummary, ModelSelectParams, ModelSelectResult,
    ModelSelection, ModelStartedEvent, ModelTurnPlanContext, ModelUsageEvent, ModelUsageInfo,
    PlanBatchFinishedEvent, ProviderKind, ReasoningEffort, RepoMapInfoEvent,
    ReviewBatchFinishedEvent, ToolchainFinishedEvent, ToolchainStartedEvent, TurnPhase,
    TurnPhaseEvent,
};
pub use plan::{
    compute_items_status, extract_plan_discussion_id, format_plan_discussion_prompt,
    is_plan_discussion_prompt, ConceptItem, Plan, PlanDeleteParams, PlanExecuteParams,
    PlanGetParams, PlanGetResult, PlanItem, PlanItemStatus, PlanListChangedEvent, PlanListResult,
    PlanRefineParams, PlanRefineResult, PlanSaveParams, PlanScopeEvaluation, PlanSetActiveParams,
    PlanSetActiveResult, PlanStats, PlanSummary, PlanUpdateItemParams, PlanUpdateItemResult,
    PlanUpdatedEvent, PLAN_DISCUSSION_PREFIX,
};
pub use review::{
    format_review_discussion_prompt, format_review_discussion_prompt_items,
    ReviewContentDeltaEvent, ReviewDeleteParams, ReviewDeleteResult, ReviewErrorEvent,
    ReviewExecuteParams, ReviewFinishedEvent, ReviewGetParams, ReviewGetResult, ReviewItem,
    ReviewItemStatus, ReviewListChangedEvent, ReviewListParams, ReviewListResult,
    ReviewReasoningDeltaEvent, ReviewSession, ReviewSeverity, ReviewStartParams,
    ReviewStartedEvent, ReviewStatus, ReviewSummary, ReviewUpdateItemParams,
    ReviewUpdateItemResult, ReviewUpdatedEvent, REVIEW_DISCUSSION_PREFIX,
};
