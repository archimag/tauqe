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
//
// The wire contract is everything a facade re-exports: DTOs, events, request
// and result types, enums, the error and event name constants, and the schema
// constructors. Every wire-contract name must also resolve as
// `tauqe_protocol::<Name>`, so the lists below are pinned mechanically by the
// `wire_contract_tracking` facade-parity test rather than by author discipline
// alone.
//
// A small fixed set of pure helpers is deliberately module-scoped and stays
// reachable only through its module paths: `edit::normalize_repo_path`,
// `edit::ChangeConversionError`, and `discussion::normalize_repo_map_path`.
// That test's module-scoped helper table is the authoritative record of the
// exceptions, so a helper is never lifted to the root and a contract name is
// never left behind at its module.
pub use command::{
    CapabilityDelivery, CapabilityPhase, CapabilityProfile, CapabilitySet, DocTopic, LlmCapability,
    LlmCommand, LlmOutcome, VerifyOnSuccess, VerifyTarget,
};
pub use config::{
    ConfigCreateParams, ConfigSetParams, ConfigState, CredentialsSaveParams, EditProtocolKind,
    PatchStrategy, PromptLayoutKind, ReviewPresetSummary, SystemStatusResult, WorkflowKind,
};
pub use context::{
    ContextAccess, ContextAddParams, ContextAddPatternParams, ContextAddPatternResult,
    ContextChangedEvent, ContextClearParams, ContextItem, ContextLayer, ContextRemoveParams,
    ContextSetAccessParams, ContextState,
};
pub use discussion::{
    custom_tool_call_schema, discussion_response_schema, discussion_response_schema_definition,
    DirListRequest, DiscussionClearFocusParams, DiscussionClearFocusResult,
    DiscussionContextRequest, DiscussionFocus, DiscussionFocusChangedEvent,
    DiscussionGetFocusParams, DiscussionGetFocusResult, DiscussionPlanAction, DiscussionPlanUpdate,
    DiscussionRequest, DiscussionResponse, DiscussionSetFocusParams, DiscussionSetFocusResult,
    DiscussionTarget, RepoSearchRequest,
};
pub use edit::{
    EditFileDoneEvent, EditFileOp, EditFileRetryingEvent, EditFileStartedEvent, EditFileStatus,
    EditFinishedEvent, EditHunkEvent, EditOperation, EditOutcome, EditProposal, EditStartedEvent,
    StructuredChangeProposal, VerificationVerdict,
};
pub use errors::{
    ADD_FAILED, ADD_PATTERN_FAILED, ARCHITECTURAL_GROUP_EXECUTION_REQUIRED, CANCELLED,
    COMMITS_AHEAD_FAILED, CONFIG_CREATE_FAILED, CREDENTIALS_SAVE_FAILED, CREDENTIALS_STUB_FAILED,
    EMPTY_CONTEXT, EMPTY_ITEMS, GET_DIFF_FAILED, INIT_FAILED, INTERNAL_ERROR, INVALID_KEY,
    INVALID_MODEL, INVALID_PARAMS, INVALID_PLAN_ID, INVALID_PLAN_TREE, INVALID_PROTOCOL,
    INVALID_REQUEST, INVALID_WORKFLOW, ITEM_IN_DISCUSSION, ITEM_REJECTED, LIST_FILES_FAILED,
    METHOD_NOT_FOUND, MODEL_ERROR, NEEDS_CONTEXT, NOTHING_TO_EXECUTE, NOT_A_LEAF, NOT_FOUND,
    NO_API_KEY, OPERATION_IN_PROGRESS, PARSE_ERROR, PINNED_PROTECTED, PLAN_CLEAR_FAILED,
    PLAN_DELETE_FAILED, PLAN_GET_FAILED, PLAN_LIST_FAILED, PLAN_LOAD_FAILED, PLAN_SAVE_FAILED,
    PLAN_SET_ACTIVE_FAILED, PLAN_UPDATE_FAILED, PREFLIGHT_VERIFICATION_FAILED, REMOVE_FAILED,
    REPOSITORY_STATE_FAILED, REVIEW_DELETE_FAILED, REVIEW_GET_FAILED, REVIEW_LIST_FAILED,
    REVIEW_LOAD_FAILED, REVIEW_UPDATE_FAILED, SCHEMA_ERROR, SET_ACCESS_FAILED, SQUASH_FAILED,
    SQUASH_PREVIEW_FAILED, STEP_BLOCKED, STEP_BLOCKED_BY_DEPENDENCIES, STEP_CANCELLED,
    STEP_IMMUTABLE, STEP_IN_DISCUSSION, UNEXPECTED_STATUS,
};

// The event and error namespaces both declare `MODEL_ERROR`: the RPC error code
// `MODEL_ERROR` and the `model/error` event method share one identifier. The
// root therefore keeps the error code under the bare name every other error
// constant uses and re-exports the event as `MODEL_ERROR_EVENT`, so neither
// contract group loses a member of its surface.
pub use events::{
    CONFIG_CHANGED, CONTEXT_CHANGED, DISCUSSION_FOCUS_CHANGED, EDIT_FILE_DONE, EDIT_FILE_RETRYING,
    EDIT_FILE_STARTED, EDIT_FINISHED, EDIT_HUNK, EDIT_STARTED, GIT_COMMIT_CREATED,
    GIT_SQUASH_COMPLETED, GIT_STATE_CHANGED, HISTORY_COMPACTED, HISTORY_ENTRY_ADDED,
    HISTORY_ENTRY_ROLLED_BACK, MODEL_CANCELLED, MODEL_CHANGED, MODEL_CONNECTION_RETRYING,
    MODEL_ERROR as MODEL_ERROR_EVENT, MODEL_ESCALATED, MODEL_FINISHED, MODEL_REASONING_DELTA,
    MODEL_RESULT, MODEL_STARTED, MODEL_STREAM_RESET, MODEL_TEXT_DELTA, MODEL_USAGE,
    MODEL_USAGE_TOTALS, PLAN_BATCH_FINISHED, PLAN_LIST_CHANGED, PLAN_UPDATED,
    REVIEW_ANALYZE_FINISHED, REVIEW_ANALYZE_STARTED, REVIEW_BATCH_FINISHED, REVIEW_CANCELLED,
    REVIEW_CONTENT_DELTA, REVIEW_ERROR, REVIEW_FINISHED, REVIEW_GROUP_FINISHED,
    REVIEW_GROUP_STARTED, REVIEW_LIST_CHANGED, REVIEW_REASONING_DELTA, REVIEW_STARTED,
    REVIEW_UPDATED, TOOLCHAIN_FINISHED, TOOLCHAIN_STARTED, TURN_PHASE, TURN_REPOMAP,
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
    HistoryFileChange, HistoryGetParams, HistoryGetResult, UiHistoryItem, UiHistoryKind,
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
    ReviewBatchFinishedEvent, ReviewWorkPhase, StructuredAbortProposal, StructuredBlockedProposal,
    StructuredOutcomeProposal, ToolchainFinishedEvent, ToolchainStartedEvent, TurnPhase,
    TurnPhaseEvent, TurnWorkContext, TurnWorkItem, TurnWorkQueue, UsageTotals, UsageTotalsSnapshot,
};
pub use plan::{
    compute_items_status, ConceptItem, Plan, PlanClearHorizonParams, PlanClearHorizonResult,
    PlanDeleteParams, PlanExecuteParams, PlanExecutePipelineParams, PlanGetParams, PlanGetResult,
    PlanImplementation, PlanItem, PlanItemStatus, PlanListChangedEvent, PlanListResult,
    PlanRefineParams, PlanRefineResult, PlanReplanParams, PlanSaveParams, PlanScopeEvaluation,
    PlanSetActiveParams, PlanSetActiveResult, PlanStats, PlanSummary, PlanUpdateItemParams,
    PlanUpdateItemResult, PlanUpdatedEvent, PlanValidationError,
};
pub use project::{
    format_diagnostics, Diagnostic, DiagnosticSeverity, ProjectCommandSpec, ProjectConfig,
    ProjectConfigError, ProjectHookOutcome, ProjectParameterSpec, ProjectPostHookResponse,
    ResolvedProjectCommand, TaskRunnerKind,
};
pub use review::{
    ArchitecturalIssueGroup, IssueStatus, IssueType, PostAnalysisGroupOutput, PostAnalysisMode,
    PostAnalysisOutput, PostAnalysisTriageOutput, ReviewAnalyzeFinishedEvent, ReviewAnalyzeParams,
    ReviewAnalyzeResult, ReviewAnalyzeStartedEvent, ReviewCancelParams, ReviewContentDeltaEvent,
    ReviewDecisionStatus, ReviewDeleteParams, ReviewDeleteResult, ReviewErrorEvent,
    ReviewExecuteParams, ReviewFinishedEvent, ReviewGetParams, ReviewGetResult, ReviewGroup,
    ReviewGroupFinishedEvent, ReviewGroupKind, ReviewGroupStartedEvent, ReviewIssue, ReviewItem,
    ReviewItemStatus, ReviewLaunchConfig, ReviewListChangedEvent, ReviewListParams,
    ReviewListResult, ReviewPostAnalysis, ReviewPreviewGroup, ReviewPreviewParams,
    ReviewPreviewResult, ReviewPreviewWindow, ReviewReasoningDeltaEvent, ReviewSession,
    ReviewSeverity, ReviewStartParams, ReviewStartedEvent, ReviewSummary, ReviewUpdateItemParams,
    ReviewUpdateItemResult, ReviewUpdateProposal, ReviewUpdatedEvent, DEFAULT_REVIEW_GROUP_ID,
    LOCAL_REVIEW_GROUP_ID, LOCAL_REVIEW_GROUP_TITLE,
};

#[cfg(test)]
mod wire_contract_tracking {
    use std::collections::{BTreeMap, BTreeSet};

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
    /// - `ReviewIssue`: `resolution_notes`
    /// - `ReviewItem`: `resolution_notes`    /// - `PlanReplanParams`: `id`, `item_id`
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
             ReviewIssue and ReviewItem `resolution_notes`; PlanReplanParams \
             `id`/`item_id`; PlanRefineParams `id`) were scheduled for removal in \
             0.2.0. Remove them and delete this test.",
            PROTOCOL_VERSION
        );
    }
    /// Source text of every facade module whose `pub use` statements form the
    /// module half of the wire contract.
    ///
    /// The text is embedded at compile time, so the parity check reads exactly
    /// the bytes the compiler does: no runtime I/O and no dependency added to
    /// this leaf crate.
    const FACADE_SOURCES: &[(&str, &str)] = &[
        ("command", include_str!("command.rs")),
        ("config", include_str!("config.rs")),
        ("context", include_str!("context.rs")),
        ("discussion", include_str!("discussion.rs")),
        ("edit", include_str!("edit.rs")),
        ("errors", include_str!("errors.rs")),
        ("events", include_str!("events.rs")),
        ("git", include_str!("git.rs")),
        ("glob", include_str!("glob.rs")),
        ("history", include_str!("history.rs")),
        ("intent", include_str!("intent.rs")),
        ("lifecycle", include_str!("lifecycle.rs")),
        ("locale", include_str!("locale.rs")),
        ("message", include_str!("message.rs")),
        ("model", include_str!("model.rs")),
        ("plan", include_str!("plan.rs")),
        ("project", include_str!("project.rs")),
        ("review", include_str!("review.rs")),
    ];

    /// Authoritative record of the module-scoped helper policy.
    ///
    /// A name recorded here stays reachable only through its module path,
    /// `tauqe_protocol::<module>::<Name>`, because it is a pure helper of that
    /// module rather than wire contract. Every other name a facade re-exports is
    /// wire contract and must also resolve as `tauqe_protocol::<Name>`; the
    /// exemption covers helpers alone and never widens into a blanket allowance
    /// for a root omission.
    ///
    /// `module_scoped_helper_table_stays_honest` keeps the record exact: an
    /// entry that is lifted to the root or deleted fails that test, so the table
    /// can only shrink.
    const MODULE_SCOPED_HELPERS: &[(&str, &str)] = &[
        ("discussion", "normalize_repo_map_path"),
        ("edit", "ChangeConversionError"),
        ("edit", "normalize_repo_path"),
    ];

    /// Line that opens this tracking module at the tail of `lib.rs`.
    const ROOT_TEST_MODULE_MARKER: &str = "#[cfg(test)]";

    /// One `pub use` statement reduced to its path head and exported names.
    ///
    /// The head is the first `::`-separated segment of the re-exported path, so
    /// a root statement such as `pub use model::{ ... };` reports `model`.
    type FacadeReexport = (String, BTreeSet<String>);

    /// Removes a trailing `//` line comment from one source line.
    ///
    /// A re-export statement may carry a comment on any of its lines, and the
    /// comment text must never be read as a re-exported name.
    fn strip_line_comment(line: &str) -> &str {
        line.split_once("//").map_or(line, |(head, _)| head)
    }

    /// First `::`-separated segment of a re-exported path.
    fn facade_head(path: &str) -> String {
        path.split("::").next().unwrap_or(path).trim().to_string()
    }

    /// Reports whether one re-exported name is a plain identifier.
    fn is_plain_identifier(name: &str) -> bool {
        !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
    }

    /// Visible name of one member of a `pub use` statement.
    ///
    /// An `original as renamed` pair contributes the renamed identifier, because
    /// that is the name the facade exports. A member that is not a plain
    /// identifier is skipped defensively instead of being guessed.
    fn visible_name(token: &str) -> Option<String> {
        let member = token.trim();
        let visible = member
            .rsplit_once(" as ")
            .map_or(member, |(_, renamed)| renamed.trim());
        if !is_plain_identifier(visible) {
            return None;
        }
        Some(visible.to_string())
    }

    /// Splits one complete `pub use` statement into its path head and names.
    ///
    /// Both statement shapes the facades use are handled: `pub use path::Name;`
    /// contributes `Name`, while the braced form contributes every
    /// comma-separated member of the brace interior.
    fn split_reexport(statement: &str) -> FacadeReexport {
        let body = statement.trim();
        let body = body.strip_prefix("pub use ").unwrap_or(body);
        let body = body.strip_suffix(';').unwrap_or(body);
        let body = body.trim();

        if let Some((head, rest)) = body.split_once('{') {
            let interior = rest.rsplit_once('}').map_or(rest, |(inner, _)| inner);
            let names = interior
                .split(',')
                .filter_map(visible_name)
                .collect::<BTreeSet<String>>();
            return (facade_head(head), names);
        }

        match body.rsplit_once("::") {
            Some((head, name)) => {
                let names = visible_name(name).into_iter().collect();
                (facade_head(head), names)
            }
            // A `pub use` statement always names a path, so a body without one
            // records no name instead of guessing a head.
            None => (facade_head(body), BTreeSet::new()),
        }
    }

    /// Extracts every `pub use` re-export statement from one module's source.
    ///
    /// The scan is line-oriented: a statement starts at a line beginning with
    /// `pub use `, which excludes `pub(crate) use` and plain `use`
    /// declarations, and a multi-line braced statement is accumulated until the
    /// line that closes it. Comments are stripped per line before accumulating.
    fn pub_use_statements(source: &str) -> Vec<FacadeReexport> {
        let mut statements = Vec::new();
        let mut open: Option<String> = None;

        for raw in source.lines() {
            let trimmed = strip_line_comment(raw).trim();
            if trimmed.is_empty() {
                continue;
            }

            match open.take() {
                Some(mut statement) => {
                    statement.push(' ');
                    statement.push_str(trimmed);
                    if trimmed.ends_with(';') {
                        statements.push(split_reexport(&statement));
                    } else {
                        open = Some(statement);
                    }
                }
                None => {
                    if !trimmed.starts_with("pub use ") {
                        continue;
                    }
                    if trimmed.ends_with(';') {
                        statements.push(split_reexport(trimmed));
                    } else {
                        open = Some(trimmed.to_string());
                    }
                }
            }
        }

        statements
    }

    /// Text of `lib.rs` before this tracking module.
    ///
    /// The root facade lists live in the file head, so the cut keeps this
    /// module's own code from ever being read as a root list. The marker is
    /// matched at a line start and the boundary is the sum of whole line
    /// lengths, so the slice never falls inside a character.
    fn root_facade_source() -> &'static str {
        let source = include_str!("lib.rs");
        let mut head_end = 0;
        for line in source.split_inclusive('\n') {
            if line.starts_with(ROOT_TEST_MODULE_MARKER) {
                return &source[..head_end];
            }
            head_end += line.len();
        }
        source
    }

    /// Root facade lists keyed by the module each statement lifts names from.
    fn root_facade_lists() -> BTreeMap<String, BTreeSet<String>> {
        let mut lists: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (module, names) in pub_use_statements(root_facade_source()) {
            lists.entry(module).or_default().extend(names);
        }
        lists
    }

    /// Every name one facade module re-exports through `pub use`.
    fn facade_reexports(source: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for (_, statement_names) in pub_use_statements(source) {
            names.extend(statement_names);
        }
        names
    }

    /// Reports whether the exemption table records `module::name`.
    fn is_module_scoped_helper(module: &str, name: &str) -> bool {
        MODULE_SCOPED_HELPERS.contains(&(module, name))
    }

    /// Pins the facade policy: every facade re-export resolves at the crate root.
    ///
    /// This is the regression gate for the drift class the policy exists to
    /// prevent. A wire-contract type added to a module facade without being
    /// lifted to the root list fails here instead of silently leaving the
    /// canonical `tauqe_protocol::<Name>` path unresolved.
    #[test]
    fn root_facade_covers_every_module_facade_reexport() {
        let root = root_facade_lists();
        let mut missing: Vec<String> = Vec::new();

        for &(module, source) in FACADE_SOURCES {
            let root_list = root.get(module);
            for name in facade_reexports(source) {
                if root_list.is_some_and(|names| names.contains(&name)) {
                    continue;
                }
                if is_module_scoped_helper(module, name.as_str()) {
                    continue;
                }
                missing.push(format!("{module}::{name}"));
            }
        }

        assert!(
            missing.is_empty(),
            "these facade re-exports do not resolve at the crate root: {missing:?}. \
             Add each name to the root `pub use <module>::{{ ... }}` list in `lib.rs`, or \
             record it in `MODULE_SCOPED_HELPERS` when it is a pure helper of its module \
             rather than wire contract."
        );
    }

    /// Keeps the exemption table exact: every entry still exists and stays out
    /// of the root.
    ///
    /// An entry that was lifted to the root, renamed, or deleted fails this
    /// test, so the table cannot outlive the code it excuses and can never
    /// quietly turn into a blanket allowance for root omissions.
    #[test]
    fn module_scoped_helper_table_stays_honest() {
        let root = root_facade_lists();

        for &(module, name) in MODULE_SCOPED_HELPERS {
            let source = FACADE_SOURCES
                .iter()
                .find(|(facade, _)| *facade == module)
                .map(|(_, source)| *source);
            let Some(source) = source else {
                panic!("no facade source is recorded for helper {module}::{name}");
            };

            assert!(
                facade_reexports(source).contains(name),
                "{module}::{name} is recorded in MODULE_SCOPED_HELPERS, but the {module} \
                 facade no longer re-exports it; delete the entry"
            );
            let lifted = root.get(module).is_some_and(|names| names.contains(name));
            assert!(
                !lifted,
                "{module}::{name} is recorded in MODULE_SCOPED_HELPERS, but it also resolves \
                 at the crate root; delete the entry so the table records only genuine \
                 exemptions"
            );
        }
    }
}
