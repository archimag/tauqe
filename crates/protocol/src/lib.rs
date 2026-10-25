#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

/// Version of the wire contract defined by this crate.
///
/// The `protocol` crate *is* the wire contract: every request, response,
/// event, and shared domain DTO lives here. Its version therefore defines the
/// wire version, so the constant is derived from `CARGO_PKG_VERSION` rather
/// than maintained by hand. This keeps a single source of truth and makes the
/// handshake report a value that cannot drift from the definitions it names.
pub const PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod command;
pub mod config;
pub mod context;
pub mod discussion;
pub mod edit;
pub mod errors;
pub mod events;
pub mod git;
pub mod glob;
pub mod history;
pub mod intent;
pub mod lifecycle;
pub mod locale;
pub mod message;
pub mod methods;
pub mod model;
pub mod plan;
pub mod project;
pub mod review;

// The crate root re-exports the wire contract explicitly rather than through
// wildcard globs. Explicit re-exports keep the public surface auditable and
// turn an accidental name collision between two modules into a compile error
// instead of a silently ambiguous glob re-export. Module paths remain public,
// so both `tauqe_protocol::Item` and `tauqe_protocol::module::Item` resolve.
pub use command::{
    CapabilityDelivery, CapabilityPhase, CapabilitySet, DocTopic, LlmCapability, LlmCommand,
    LlmOutcome, VerifyOnSuccess, VerifyTarget,
};
pub use config::{
    ConfigCreateParams, ConfigSetParams, ConfigState, CredentialsSaveParams, EditProtocolKind,
    SystemStatusResult, WorkflowKind,
};
pub use context::{
    ContextAccess, ContextAddParams, ContextAddPatternParams, ContextAddPatternResult,
    ContextChangedEvent, ContextClearParams, ContextItem, ContextLayer, ContextRemoveParams,
    ContextSetAccessParams, ContextState,
};
pub use discussion::{
    discussion_response_schema, discussion_response_schema_definition, DirListRequest,
    DiscussionClearFocusParams, DiscussionClearFocusResult, DiscussionContextRequest,
    DiscussionFocus, DiscussionFocusChangedEvent, DiscussionGetFocusParams,
    DiscussionGetFocusResult, DiscussionPlanAction, DiscussionPlanUpdate, DiscussionRequest,
    DiscussionResponse, DiscussionSetFocusParams, DiscussionSetFocusResult, DiscussionTarget,
    RepoSearchRequest,
};
pub use edit::{
    EditFileDoneEvent, EditFileOp, EditFileRetryingEvent, EditFileStartedEvent, EditFileStatus,
    EditFinishedEvent, EditHunkEvent, EditOperation, EditOutcome, EditProposal, EditStartedEvent,
    StructuredChangeProposal,
};
pub use errors::{
    ADD_FAILED, ADD_PATTERN_FAILED, CANCELLED, COMMITS_AHEAD_FAILED, CONFIG_CREATE_FAILED,
    CREDENTIALS_SAVE_FAILED, CREDENTIALS_STUB_FAILED, EMPTY_CONTEXT, EMPTY_ITEMS, GET_DIFF_FAILED,
    INIT_FAILED, INTERNAL_ERROR, INVALID_KEY, INVALID_MODEL, INVALID_PARAMS, INVALID_PLAN_ID,
    INVALID_PLAN_TREE, INVALID_PROTOCOL, INVALID_REQUEST, INVALID_WORKFLOW, ITEM_IN_DISCUSSION,
    ITEM_IN_REFINE, ITEM_REJECTED, LIST_FILES_FAILED, METHOD_NOT_FOUND, MODEL_ERROR, NEEDS_CONTEXT,
    NOTHING_TO_EXECUTE, NOT_A_LEAF, NOT_FOUND, NO_API_KEY, OPERATION_IN_PROGRESS, PARSE_ERROR,
    PINNED_PROTECTED, PLAN_CLEAR_FAILED, PLAN_DELETE_FAILED, PLAN_GET_FAILED, PLAN_LIST_FAILED,
    PLAN_LOAD_FAILED, PLAN_SAVE_FAILED, PLAN_SET_ACTIVE_FAILED, PLAN_UPDATE_FAILED,
    PREFLIGHT_VERIFICATION_FAILED, REMOVE_FAILED, REPOSITORY_STATE_FAILED, REVIEW_DELETE_FAILED,
    REVIEW_GET_FAILED, REVIEW_LIST_FAILED, REVIEW_LOAD_FAILED, REVIEW_UPDATE_FAILED, SCHEMA_ERROR,
    SET_ACCESS_FAILED, SQUASH_FAILED, SQUASH_PREVIEW_FAILED, STEP_BLOCKED,
    STEP_BLOCKED_BY_DEPENDENCIES, STEP_CANCELLED, STEP_IMMUTABLE, STEP_IN_DISCUSSION,
    UNEXPECTED_STATUS,
};
pub use git::{
    GitCommitCreatedEvent, GitDiffParams, GitDiffResult, GitSquashApplyParams,
    GitSquashApplyResult, GitSquashCommitItem, GitSquashFileDiff, GitSquashGenerateMessageParams,
    GitSquashGenerateMessageResult, GitSquashPreviewParams, GitSquashPreviewResult,
    RepositoryInitParams, RepositoryInitResult, RepositoryListFilesResult, RepositoryState,
};
pub use glob::{matches_glob_pattern, GlobPattern};
pub use history::{
    HistoryCompactedEvent, HistoryEntryAddedEvent, HistoryEntryRolledBackEvent, HistoryEntryStatus,
    HistoryGetParams, HistoryGetResult, UiHistoryItem, UiHistoryKind,
};
pub use intent::TurnIntent;
pub use lifecycle::{CLIENT_SHUTDOWN_WAIT, SHUTDOWN_DRAIN_BUDGET};
pub use locale::{LocaleCatalogItem, LocaleGenerateParams, LocaleGenerateResult};
pub use message::{
    Event, InitializeParams, InitializeResult, Message, Request, RequestId, Response, ResponseError,
};
pub use model::{
    deserialize_model_ref_lenient, ArchitectTier, BatchFinishStatus, BatchFinishSummary,
    CancelMode, CoderTier, ModelAskParams, ModelCancelParams, ModelCatalogEntry, ModelChangedEvent,
    ModelConnectionRetryingEvent, ModelCurrentResult, ModelDeltaEvent, ModelEntry, ModelErrorEvent,
    ModelEscalatedEvent, ModelFinishedEvent, ModelRef, ModelResult, ModelResultEvent,
    ModelResultProposal, ModelRole, ModelRolesEffort, ModelRolesEntries, ModelRolesSummary,
    ModelSelectParams, ModelSelectResult, ModelSelection, ModelStartedEvent, ModelStreamResetEvent,
    ModelTurnPlanContext, ModelUsageEvent, ModelUsageInfo, ModelUsageTotalsEvent,
    PlanBatchFinishedEvent, ProviderKind, ReasoningEffort, RepoMapInfoEvent,
    ReviewBatchFinishedEvent, StructuredAbortProposal, StructuredBlockedProposal,
    StructuredOutcomeProposal, ToolchainFinishedEvent, ToolchainStartedEvent, TurnPhase,
    TurnPhaseEvent, UsageTotals, UsageTotalsSnapshot,
};
pub use plan::{
    compute_items_status, ConceptItem, Plan, PlanClearHorizonParams, PlanClearHorizonResult,
    PlanDeleteParams, PlanExecuteParams, PlanExecutePipelineParams, PlanGetParams, PlanGetResult,
    PlanItem, PlanItemStatus, PlanListChangedEvent, PlanListResult, PlanRefineParams,
    PlanRefineResult, PlanReplanParams, PlanSaveParams, PlanScopeEvaluation, PlanSetActiveParams,
    PlanSetActiveResult, PlanStats, PlanSummary, PlanUpdateItemParams, PlanUpdateItemResult,
    PlanUpdatedEvent, PlanValidationError,
};
pub use project::{
    format_diagnostics, Diagnostic, DiagnosticParserKind, DiagnosticSeverity, ProjectCapabilities,
    ProjectCheckRequest, ProjectCheckResponse, ProjectCheckStepSpec, ProjectCheckSuiteSpec,
    ProjectCommandSpec, ProjectConfig, ProjectConfigError, ProjectFormatRequest,
    ProjectFormatResponse, ProjectFormatterSpec, ProjectHookOutcome, ProjectManifest,
    ProjectParameterSpec, ProjectPostHookRequest, ProjectPostHookResponse, ProjectPostHookSpec,
    ProjectRequest, ProjectResponse, ProjectStepResult, ResolvedProjectCommand, TaskRunnerKind,
};
pub use review::{
    IssueStatus, IssueType, ReviewCancelParams, ReviewContentDeltaEvent, ReviewDeleteParams,
    ReviewDeleteResult, ReviewErrorEvent, ReviewExecuteParams, ReviewFinishedEvent,
    ReviewGetParams, ReviewGetResult, ReviewIssue, ReviewItem, ReviewItemStatus,
    ReviewListChangedEvent, ReviewListParams, ReviewListResult, ReviewReasoningDeltaEvent,
    ReviewRefineItemParams, ReviewRefineItemResult, ReviewSession, ReviewSeverity,
    ReviewStartParams, ReviewStartedEvent, ReviewSummary, ReviewUpdateItemParams,
    ReviewUpdateItemResult, ReviewUpdatedEvent,
};

#[cfg(test)]
mod wire_contract_tracking {
    use super::PROTOCOL_VERSION;

    /// The reported wire version must never drift from the crate version it
    /// mirrors. Deriving the constant makes this hold by construction; the
    /// assertion pins the intent against a future hand-written regression.
    #[test]
    fn protocol_version_tracks_crate_version() {
        assert_eq!(PROTOCOL_VERSION, env!("CARGO_PKG_VERSION"));
    }

    /// Forcing function for the removal of the legacy compatibility aliases.
    ///
    /// A set of backward-compatibility aliases is retained only so older
    /// clients keep working and is annotated "scheduled for removal in 0.2.0".
    /// A comment alone never forces that cleanup, so this gate turns the intent
    /// into an enforceable check: it passes while the crate is on the 0.1 line
    /// and fails the moment the version reaches 0.2.0, forcing the aliases to
    /// be deleted together with this test.
    ///
    /// Tracked aliases:
    /// - `DocTopic`: `plans`
    /// - `WorkflowKind`: `single`, `direct`
    /// - `EditProtocolKind`: `structured_output`, `json_schema`, `json`
    /// - `IssueStatus`: `done`
    /// - `ReviewUpdateItemParams`: `resolution_notes`
    /// - `PlanReplanParams`: `id`, `item_id`
    /// - `PlanRefineParams`: `id`
    #[test]
    fn legacy_aliases_must_be_removed_by_0_2_0() {
        let mut parts = env!("CARGO_PKG_VERSION").split('.');
        let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        assert!(
            (major, minor) < (0, 2),
            "Crate version {}. The legacy aliases (DocTopic `plans`; WorkflowKind \
             `single`/`direct`; EditProtocolKind `structured_output`/`json_schema`/\
             `json`; IssueStatus `done`; ReviewUpdateItemParams `resolution_notes`; \
             PlanReplanParams `id`/`item_id`; PlanRefineParams `id`) were scheduled \
             for removal in 0.2.0. Remove them and delete this test.",
            PROTOCOL_VERSION
        );
    }
}
