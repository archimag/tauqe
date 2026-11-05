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
pub mod outer_docs;
pub mod plan;
pub mod project;
pub mod review;

// The crate root re-exports the wire contract explicitly rather than through
// wildcard globs. Explicit re-exports keep the public surface auditable and
// turn an accidental name collision between two modules into a compile error
// instead of a silently ambiguous glob re-export. Module paths remain public,
// so both `tauqe_protocol::Item` and `tauqe_protocol::module::Item` resolve.
// The wire contract is everything a facade re-exports or declares: DTOs,
// events, request and result types, enums, the error and event name constants,
// and the schema constructors. Every wire-contract name must also resolve as
// `tauqe_protocol::<Name>`, so the lists below are pinned mechanically by the
// `wire_contract_tracking` tests rather than by author discipline alone. Both
// coverage classes are gated: one test checks the `pub use` re-export parity of
// every facade module, and another checks the string constants a facade module
// declares directly, so a name declared in `errors.rs` or `events.rs` without
// its root entry fails the suite instead of drifting out of the facade.
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
    EMPTY_CONTEXT, EMPTY_ITEMS, GET_DIFF_FAILED, GROUP_UNRESOLVED_QUESTIONS, HISTORY_GET_FAILED,
    INIT_FAILED, INTERNAL_ERROR, INVALID_KEY, INVALID_MODEL, INVALID_PARAMS, INVALID_PLAN_ID,
    INVALID_PLAN_TREE, INVALID_PROTOCOL, INVALID_REQUEST, INVALID_WORKFLOW, ITEM_IN_DISCUSSION,
    ITEM_REJECTED, LIST_FILES_FAILED, METHOD_NOT_FOUND, MODEL_ERROR, MODULE_CONFLICT,
    NEEDS_CONTEXT, NOTHING_TO_EXECUTE, NOT_A_LEAF, NOT_FOUND, NO_API_KEY, OPERATION_IN_PROGRESS,
    PARSE_ERROR, PINNED_PROTECTED, PLAN_CLEAR_FAILED, PLAN_DELETE_FAILED, PLAN_GET_FAILED,
    PLAN_LIST_FAILED, PLAN_LOAD_FAILED, PLAN_SAVE_FAILED, PLAN_SET_ACTIVE_FAILED,
    PLAN_UPDATE_FAILED, PREFLIGHT_VERIFICATION_FAILED, REMOVE_FAILED, REPOSITORY_STATE_FAILED,
    REVIEW_DELETE_FAILED, REVIEW_GET_FAILED, REVIEW_LIST_FAILED, REVIEW_LOAD_FAILED,
    REVIEW_PREVIEW_FAILED, REVIEW_UPDATE_FAILED, SCHEMA_ERROR, SET_ACCESS_FAILED, SQUASH_FAILED,
    SQUASH_PREVIEW_FAILED, STEP_BLOCKED, STEP_BLOCKED_BY_DEPENDENCIES, STEP_CANCELLED,
    STEP_IMMUTABLE, STEP_IN_DISCUSSION, UNDO_FAILED, UNEXPECTED_STATUS,
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

pub use outer_docs::{
    OuterDocsCacheEntry, OuterDocsCacheListResult, OuterDocsCacheRemoveParams,
    OuterDocsCacheRemoveResult,
};
pub use plan::{
    compute_items_status, ConceptItem, Plan, PlanClearHorizonParams, PlanClearHorizonResult,
    PlanCreatePipelineParams, PlanDeleteParams, PlanExecuteParams, PlanExecutePipelineParams,
    PlanGetParams, PlanGetResult, PlanImplementation, PlanItem, PlanItemStatus,
    PlanListChangedEvent, PlanListResult, PlanRefineParams, PlanRefineResult, PlanRejection,
    PlanRejectionKind, PlanReplanParams, PlanSaveParams, PlanScopeEvaluation, PlanSetActiveParams,
    PlanSetActiveResult, PlanStats, PlanSummary, PlanUpdateItemParams, PlanUpdateItemResult,
    PlanUpdatedEvent, PlanValidationError,
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
    /// One line of the machine-readable alias ledger.
    ///
    /// The record is `(file, anchor, spelling)`. `file` is the path relative to
    /// `crates/protocol/src`, `anchor` names the type, variant, or field that
    /// carries the attribute, and `spelling` is the alias value itself. The pair
    /// `(file, spelling)` is the machine-checked identity, so it must match one
    /// `alias = "..."` attribute of the scanned source exactly; `anchor` stays
    /// documentation, because renaming a field changes prose rather than the
    /// compatibility surface.
    type AliasRecord = (&'static str, &'static str, &'static str);

    /// Every `alias = "..."` attribute the crate sources declare.
    ///
    /// The ledger is the authoritative record of the compatibility surface, and
    /// `alias_registry_matches_source_attributes` checks it in both directions:
    /// an attribute without a recorded entry fails completeness, and a record
    /// whose attribute is gone fails honesty. The scan reads every source file of
    /// the crate, and `alias_scan_surface_covers_every_declared_module` keeps
    /// that surface complete as files are added, so a new spelling cannot enter
    /// the crate without a decision recorded here.
    const ALIAS_REGISTRY: &[AliasRecord] = &[
        // review/item.rs: a category and a severity are read tolerantly, so a
        // payload produced under either vocabulary keeps decoding.
        ("review/item.rs", "IssueType::Bug", "critical"),
        ("review/item.rs", "IssueType::Architecture", "warning"),
        ("review/item.rs", "IssueType::Architecture", "warn"),
        ("review/item.rs", "IssueType::Quality", "style"),
        ("review/item.rs", "IssueType::Quality", "suggestion"),
        ("review/item.rs", "IssueType::Quality", "info"),
        ("review/item.rs", "ReviewSeverity::Critical", "bug"),
        ("review/item.rs", "ReviewSeverity::Critical", "security"),
        ("review/item.rs", "ReviewSeverity::Warning", "architecture"),
        ("review/item.rs", "ReviewSeverity::Warning", "performance"),
        ("review/item.rs", "ReviewSeverity::Suggestion", "quality"),
        ("review/item.rs", "ReviewSeverity::Suggestion", "style"),
        // review/item.rs: the historical wire keys of the category field.
        ("review/item.rs", "ReviewIssue::issue_type", "severity"),
        ("review/item.rs", "ReviewIssue::issue_type", "issue_type"),
        ("review/item.rs", "ReviewItemWire::issue_type", "severity"),
        ("review/item.rs", "ReviewItemWire::issue_type", "issue_type"),
        // review/item.rs: the shadow reads the canonical key the item writes.
        ("review/item.rs", "ReviewItemWire::body", "description"),
        // model/turn.rs
        (
            "model/turn.rs",
            "ModelResultProposal::doc_requests",
            "doc_requests",
        ),
        // command.rs
        ("command.rs", "LlmCommand::DocRequest", "doc_request"),
        // context.rs
        ("context.rs", "ContextLayer::OuterDocs", "refs"),
    ];

    /// Source text of every crate file the facade list does not name.
    ///
    /// `FACADE_SOURCES` names one file per wire-contract surface, but a serde
    /// attribute sits in the module that declares the type, not in the facade
    /// that re-exports it, and the crate declares serde types far beyond the
    /// handful of submodules this list once held. The scan therefore reads every
    /// remaining source file, so an attribute is documented where it is written
    /// rather than where it is re-exported.
    ///
    /// The list is exhaustive by construction rather than by discipline:
    /// `alias_scan_surface_covers_every_declared_module` walks every tracked
    /// file, maps each file module it declares onto the path the compiler reads,
    /// and fails when that path is not tracked here. A new source file therefore
    /// cannot enter the crate without entering the scan surface.
    const CRATE_SOURCES: &[(&str, &str)] = &[
        ("lib.rs", include_str!("lib.rs")),
        ("methods.rs", include_str!("methods.rs")),
        (
            "command/capability.rs",
            include_str!("command/capability.rs"),
        ),
        ("command/topic.rs", include_str!("command/topic.rs")),
        (
            "discussion/normalize.rs",
            include_str!("discussion/normalize.rs"),
        ),
        ("discussion/schema.rs", include_str!("discussion/schema.rs")),
        ("edit/proposal.rs", include_str!("edit/proposal.rs")),
        ("model/batch.rs", include_str!("model/batch.rs")),
        ("model/catalog.rs", include_str!("model/catalog.rs")),
        ("model/effort.rs", include_str!("model/effort.rs")),
        ("model/entry.rs", include_str!("model/entry.rs")),
        ("model/reference.rs", include_str!("model/reference.rs")),
        ("model/role.rs", include_str!("model/role.rs")),
        ("model/selection.rs", include_str!("model/selection.rs")),
        ("model/turn.rs", include_str!("model/turn.rs")),
        ("model/usage.rs", include_str!("model/usage.rs")),
        ("plan/dto.rs", include_str!("plan/dto.rs")),
        (
            "plan/implementation.rs",
            include_str!("plan/implementation.rs"),
        ),
        ("plan/item.rs", include_str!("plan/item.rs")),
        ("plan/node.rs", include_str!("plan/node.rs")),
        ("plan/rejection.rs", include_str!("plan/rejection.rs")),
        ("plan/render.rs", include_str!("plan/render.rs")),
        ("plan/tests.rs", include_str!("plan/tests.rs")),
        ("project/config.rs", include_str!("project/config.rs")),
        ("review/analysis.rs", include_str!("review/analysis.rs")),
        ("review/dto.rs", include_str!("review/dto.rs")),
        ("review/events.rs", include_str!("review/events.rs")),
        ("review/group.rs", include_str!("review/group.rs")),
        ("review/item.rs", include_str!("review/item.rs")),
        ("review/session.rs", include_str!("review/session.rs")),
        ("review/tests.rs", include_str!("review/tests.rs")),
    ];

    /// Every source file the alias scan reads, as `(path, text)` pairs.
    ///
    /// Facade entries are spelled `<module>.rs`, so one path vocabulary covers
    /// both halves of the surface and a registry record names the same file the
    /// scan read. The two halves together are the whole crate: the facade files
    /// plus every file `FACADE_SOURCES` does not name.
    fn alias_scan_surface() -> Vec<(String, &'static str)> {
        let mut surface = Vec::new();
        for (module, source) in FACADE_SOURCES {
            surface.push((format!("{module}.rs"), *source));
        }
        for (path, source) in CRATE_SOURCES {
            surface.push(((*path).to_string(), *source));
        }
        surface
    }

    /// Removes a leading `pub` visibility from one module declaration line.
    ///
    /// The crate spells module visibility as `pub`, `pub(crate)`, `pub(super)`,
    /// or `pub(in path)`. The helper drops the keyword together with its
    /// parenthesized group, so each of those forms reduces to the bare
    /// declaration the scan looks for. A line without the keyword is returned
    /// unchanged, which keeps every `pub use`, `pub const`, and `pub struct` line
    /// out of the module scan.
    fn strip_module_visibility(line: &str) -> &str {
        let Some(rest) = line.strip_prefix("pub") else {
            return line;
        };
        let rest = rest.trim_start();
        let Some(group) = rest.strip_prefix('(') else {
            return rest;
        };
        group
            .split_once(')')
            .map_or(line, |(_, remainder)| remainder.trim_start())
    }

    /// Every file module one source text declares, as bare module names.
    ///
    /// A declaration that ends with a semicolon names a file the compiler
    /// includes, while an inline `mod name { ... }` block declares its body in
    /// the same file. Only the first shape introduces a source file, so the scan
    /// strips the visibility, requires the semicolon, and records the bare name.
    /// Comments are stripped first, so a declaration quoted in prose is never
    /// read as code.
    fn declared_file_modules(source: &str) -> BTreeSet<String> {
        let text = strip_comments(source);
        let mut names = BTreeSet::new();

        for raw in text.lines() {
            let line = strip_module_visibility(raw.trim());
            let Some(rest) = line.strip_prefix("mod ") else {
                continue;
            };
            let Some(name) = rest.trim().strip_suffix(';') else {
                continue;
            };
            let name = name.trim();
            if is_plain_identifier(name) {
                names.insert(name.to_string());
            }
        }

        names
    }

    /// Source path the compiler reads for one file module of `path`.
    ///
    /// Rust resolves a module declared in `foo.rs` to `foo/<name>.rs` and a
    /// module declared in the crate root to `<name>.rs`. The mapping reproduces
    /// that rule, so the completeness gate compares a declaration against the
    /// path the compiler itself would open.
    fn child_module_path(path: &str, name: &str) -> String {
        match path.strip_suffix(".rs") {
            Some("lib") | None => format!("{name}.rs"),
            Some(directory) => format!("{directory}/{name}.rs"),
        }
    }

    /// Removes every line and block comment from one source text.
    ///
    /// The scan reads textual markers rather than a syntax tree, so it also
    /// tracks double-quoted literals: a `//` inside a string is data, and
    /// treating it as a comment would hide the attribute that follows it on the
    /// same line. A comment is dropped in place while its line breaks are kept,
    /// so two spellings on neighbouring lines never join into one.
    fn strip_comments(source: &str) -> String {
        let mut stripped = String::with_capacity(source.len());
        let mut chars = source.chars().peekable();
        let mut in_string = false;
        let mut block_depth = 0usize;

        while let Some(current) = chars.next() {
            if block_depth > 0 {
                if current == '\n' {
                    stripped.push('\n');
                }
                if current == '/' && chars.peek() == Some(&'*') {
                    chars.next();
                    block_depth += 1;
                } else if current == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    block_depth -= 1;
                }
                continue;
            }

            if in_string {
                stripped.push(current);
                if current == '\\' {
                    stripped.extend(chars.next());
                } else if current == '"' {
                    in_string = false;
                }
                continue;
            }

            if current == '"' {
                in_string = true;
                stripped.push(current);
            } else if current == '/' && chars.peek() == Some(&'/') {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            } else if current == '/' && chars.peek() == Some(&'*') {
                chars.next();
                block_depth = 1;
            } else {
                stripped.push(current);
            }
        }

        stripped
    }

    /// Every `alias = "..."` spelling one source text declares, in source order.
    ///
    /// Several aliases may share one attribute line, so the scan resumes after
    /// each spelling it reads instead of consuming the rest of the line.
    fn alias_spellings(source: &str) -> Vec<String> {
        let text = strip_comments(source);
        let mut spellings = Vec::new();
        let mut rest = text.as_str();

        while let Some(index) = rest.find("alias") {
            let Some(after_alias) = rest.get(index + "alias".len()..) else {
                break;
            };
            rest = after_alias;
            let Some(attribute) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let Some(value) = attribute.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some((spelling, remainder)) = value.split_once('"') else {
                continue;
            };
            spellings.push(spelling.to_string());
            rest = remainder;
        }

        spellings
    }

    /// Pins the alias ledger against the source attributes it records.
    ///
    /// The ledger is only as authoritative as its agreement with the sources, so
    /// the check runs in both directions: a source attribute without a record
    /// fails completeness, and a record whose attribute is gone fails honesty.
    /// The comparison is a multiset one, because several distinct
    /// declarations deliberately carry the same spelling and each of them needs
    /// a decision of its own.
    #[test]
    fn alias_registry_matches_source_attributes() {
        let mut declared: Vec<(String, String)> = Vec::new();
        for (file, source) in alias_scan_surface() {
            for spelling in alias_spellings(source) {
                declared.push((file.clone(), spelling));
            }
        }

        let mut recorded: Vec<(String, String)> = ALIAS_REGISTRY
            .iter()
            .map(|(file, _, spelling)| ((*file).to_string(), (*spelling).to_string()))
            .collect();
        let mut unrecorded = Vec::new();

        for entry in declared {
            match recorded.iter().position(|candidate| candidate == &entry) {
                Some(index) => {
                    recorded.remove(index);
                }
                None => unrecorded.push(entry),
            }
        }

        assert!(
            unrecorded.is_empty(),
            "these source `alias` attributes carry no registry record: {unrecorded:?}. \
             Record each spelling in `ALIAS_REGISTRY`."
        );
        assert!(
            recorded.is_empty(),
            "these `ALIAS_REGISTRY` records match no source attribute: {recorded:?}. \
             Delete each stale record, or restore the attribute it documents."
        );
    }

    /// Pins the completeness of the alias scan surface itself.
    ///
    /// `alias_registry_matches_source_attributes` reports an unregistered
    /// attribute only in a file it read, so the ledger is complete exactly when
    /// the surface covers every compiled file. The gate walks each tracked file,
    /// maps every file module it declares onto the path the compiler reads, and
    /// requires that path to be tracked too. Because the crate root is tracked
    /// and every other source file is reachable from it through such
    /// declarations, a new file cannot join the crate without joining the
    /// surface: either its declaration names an untracked file and this test
    /// fails, or nothing declares it and the compiler never reads it.
    #[test]
    fn alias_scan_surface_covers_every_declared_module() {
        let surface = alias_scan_surface();
        let tracked = surface
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<BTreeSet<&str>>();
        let mut missing: Vec<String> = Vec::new();

        assert_eq!(
            tracked.len(),
            surface.len(),
            "the alias scan surface lists one path twice"
        );
        for (path, source) in &surface {
            for name in declared_file_modules(source) {
                let child = child_module_path(path, &name);
                if tracked.contains(child.as_str()) {
                    continue;
                }
                missing.push(format!(
                    "{path} declares {name}, so {child} must be scanned"
                ));
            }
        }

        assert!(
            missing.is_empty(),
            "these declared modules fall outside the alias scan surface: {missing:?}. \
             Add each file to `CRATE_SOURCES` so the scan reads the attributes it declares."
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
        ("outer_docs", include_str!("outer_docs.rs")),
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

    /// Sanctioned rename of a declared constant at the crate root.
    ///
    /// Each entry pairs the module that declares the constant and the declared
    /// name with the name the root facade exports it under. A declaration
    /// reaches the crate root under its own name unless an entry records
    /// otherwise, so the table is the authoritative record of the exceptions
    /// rather than a list that can silently absorb an omission.
    type FacadeRename = (&'static str, &'static str, &'static str);

    /// The single sanctioned rename of the wire contract.
    ///
    /// `events::MODEL_ERROR` is exported as `MODEL_ERROR_EVENT` because the RPC
    /// error code and the event method share one identifier, and the error code
    /// keeps the bare name every other error constant uses.
    const FACADE_RENAMES: &[FacadeRename] = &[("events", "MODEL_ERROR", "MODEL_ERROR_EVENT")];

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

    /// Reports whether one declared constant type is a string type.
    ///
    /// The wire contract declares its name constants as `&str`, so a borrowed
    /// string type is the coverage class the declaration test enumerates. Any
    /// other type — the `Duration` budgets of `lifecycle.rs`, an integer, a
    /// slice — belongs to a different class and is skipped instead of guessed.
    fn is_string_type(declared_type: &str) -> bool {
        let declared_type = declared_type.trim();
        declared_type.starts_with('&') && declared_type.ends_with("str")
    }

    /// Every string constant one facade module declares directly.
    ///
    /// The scan mirrors the re-export scan: a declaration starts at a line
    /// beginning with `pub const`, the identifier before the type colon is the
    /// declared name, and the declared type decides whether the constant
    /// belongs to this coverage class. A declaration whose left side is not a
    /// plain identifier — the signature of a `pub const fn` helper, for
    /// instance — is skipped the same way.
    fn declared_string_constants(source: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();

        for raw in source.lines() {
            let trimmed = strip_line_comment(raw).trim();
            let Some(rest) = trimmed.strip_prefix("pub const ") else {
                continue;
            };
            let Some((name, type_and_value)) = rest.split_once(':') else {
                continue;
            };
            let declared_type = type_and_value
                .split_once('=')
                .map_or(type_and_value, |(head, _)| head)
                .trim();
            if !is_string_type(declared_type) {
                continue;
            }
            let name = name.trim();
            if is_plain_identifier(name) {
                names.insert(name.to_string());
            }
        }

        names
    }

    /// Root name a declared constant must carry in the facade lists.
    ///
    /// A declaration reaches the crate root under its own name unless an entry
    /// of `FACADE_RENAMES` records a sanctioned rename, so a caller asks this
    /// function instead of branching on the table itself.
    fn root_name_of<'a>(module: &str, declared: &'a str) -> &'a str {
        FACADE_RENAMES
            .iter()
            .find(|(facade, name, _)| *facade == module && *name == declared)
            .map_or(declared, |(_, _, renamed)| *renamed)
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

    /// Pins the second coverage class the root facade owes: every string
    /// constant a facade module declares directly.
    ///
    /// `root_facade_covers_every_module_facade_reexport` enumerates `pub use`
    /// statements, so it cannot see a constant the module declares itself.
    /// `errors.rs` declares its codes directly, which is how five of them left
    /// the root list while that test stayed green. A declaration resolves at the
    /// root under its own name unless `FACADE_RENAMES` records a rename.
    #[test]
    fn root_facade_covers_every_declared_string_constant() {
        let root = root_facade_lists();
        let mut missing: Vec<String> = Vec::new();

        for &(module, source) in FACADE_SOURCES {
            let root_list = root.get(module);
            for name in declared_string_constants(source) {
                let root_name = root_name_of(module, name.as_str());
                if root_list.is_some_and(|names| names.contains(root_name)) {
                    continue;
                }
                missing.push(format!("{module}::{name}"));
            }
        }

        assert!(
            missing.is_empty(),
            "these facade declarations do not resolve at the crate root: {missing:?}. \
             Add each name to the root `pub use <module>::{{ ... }}` list in `lib.rs`, or \
             record it in `FACADE_RENAMES` when the root exports it under another name."
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

    /// Keeps the rename table exact: every entry still matches its declaration,
    /// its root name, and the absence of the declared name at the root.
    ///
    /// A stale entry — a constant that was renamed back, deleted, or never
    /// renamed at all — fails here, so the table records only genuine renames
    /// and never widens into a blanket allowance for root omissions.
    #[test]
    fn facade_rename_table_stays_honest() {
        let root = root_facade_lists();

        for &(module, declared, renamed) in FACADE_RENAMES {
            let source = FACADE_SOURCES
                .iter()
                .find(|(facade, _)| *facade == module)
                .map(|(_, source)| *source);
            let Some(source) = source else {
                panic!("no facade source is recorded for rename {module}::{declared}");
            };

            assert!(
                declared_string_constants(source).contains(declared),
                "{module}::{declared} is recorded in FACADE_RENAMES, but the {module} \
                 module no longer declares it; delete the entry"
            );
            let root_list = root.get(module);
            assert!(
                root_list.is_some_and(|names| names.contains(renamed)),
                "{module}::{renamed} is recorded in FACADE_RENAMES as the root name of \
                 {module}::{declared}, but it does not resolve at the crate root; restore \
                 the re-export or delete the entry"
            );
            assert!(
                !root_list.is_some_and(|names| names.contains(declared)),
                "{module}::{declared} is recorded in FACADE_RENAMES, but it also resolves at \
                 the crate root; delete the entry so the table records only genuine renames"
            );
        }
    }
}
