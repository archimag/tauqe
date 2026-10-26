use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::context::ContextAccess;
use crate::discussion::{DiscussionPlanAction, DiscussionPlanUpdate};
use crate::edit::EditOperation;

/// Phase of the autonomous bounded turn a capability belongs to.
///
/// Grouping capabilities by phase lets the harness reason about *when* a
/// capability is meaningful (gathering context, producing the answer, running
/// verification, or declaring the terminal outcome) without branching on
/// individual command types. A phase is a pure declarative property of a
/// capability, so it stays a single source of truth beside the capability
/// definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityPhase {
    /// Gathering context and documentation before a proposal is produced.
    Discovery,
    /// Producing the answer or the concrete code changes.
    Execution,
    /// Running deterministic toolchain verification.
    Verification,
    /// Declaring a terminal outcome or a semantic blocker.
    Outcome,
}

impl CapabilityPhase {
    /// Canonical lower-case identifier used on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Execution => "execution",
            Self::Verification => "verification",
            Self::Outcome => "outcome",
        }
    }
}

impl fmt::Display for CapabilityPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a capability's protocol output reaches the harness.
///
/// Some capabilities are consumed incrementally while the model is still
/// generating: conversational text and file edits feed the live stream, so the
/// harness can render an answer or a diff as tokens arrive. Every other
/// capability only becomes meaningful once its payload is complete, so it is
/// decoded after the round finishes.
///
/// Declaring the delivery mode beside the capability keeps a single source of
/// truth. A streaming scanner or a round-level decoder reads this property
/// instead of special-casing capability names, which keeps the engine free of
/// string literals that would silently drift from the capability declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDelivery {
    /// Consumed incrementally as tokens arrive from the provider.
    LiveStreaming,
    /// Consumed as a complete payload after the round has finished.
    RoundDeferred,
}

impl CapabilityDelivery {
    /// Canonical lower-case identifier used on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LiveStreaming => "live_streaming",
            Self::RoundDeferred => "round_deferred",
        }
    }

    /// Reports whether the capability is consumed during generation.
    pub const fn is_live(self) -> bool {
        matches!(self, Self::LiveStreaming)
    }
}

impl fmt::Display for CapabilityDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Declarative set of interaction capabilities a prompt protocol may enable.
///
/// Capabilities decouple the concrete protocol syntax (XML markers, JSON schema)
/// from the core business logic. A protocol advertises the capabilities it
/// supports, and its decoder produces only the corresponding [`LlmCommand`]
/// variants. This keeps the execution pipeline independent of any single wire
/// format and makes protocols interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmCapability {
    /// Free-form conversational Markdown text.
    TextMessage,
    /// File creation, replacement, overwrite, deletion, and move operations.
    FileEdits,
    /// Requests to add or drop files from the working context.
    Discovery,
    /// Requests for system documentation by topic.
    DocRequest,
    /// Requests to run deterministic toolchain verification.
    ToolchainVerify,
    /// Creation of a new project plan.
    PlanCreation,
    /// Modification of an existing project plan.
    PlanUpdate,
    /// Ephemeral working-memory notes for multi-round reasoning.
    Scratchpad,
    /// Explicit terminal outcome (`success` or `error`) of a turn.
    Outcome,
    /// Explicit semantic blocker emitted when preconditions or assumptions are violated.
    Blocked,
    /// Explicit unconditional abort emitted when external host environment actions or developer intervention is required.
    Abort,
    /// Detected primary natural language of the user prompt.
    UserLanguage,
    /// Invocation of a project-declared custom tool.
    ///
    /// The project registers a closed allowlist of typed commands in
    /// `tauqe.toml`, and the model may invoke one of them by name with typed
    /// arguments. The capability grants exactly the declared commands and never
    /// an arbitrary shell.
    CustomTools,
}

impl LlmCapability {
    /// Every capability a built-in protocol profile may advertise.
    ///
    /// The order matches the enum declaration so iteration, prompt
    /// composition, and tests stay deterministic. A declared capability stays
    /// absent from this list until the capability module that owns its tags,
    /// prompt text, and decoder is registered: the registry requires a module
    /// for every advertised capability, so advertising one earlier would offer
    /// the model a protocol surface nothing can decode.
    pub const ALL: &'static [LlmCapability] = &[
        Self::TextMessage,
        Self::FileEdits,
        Self::Discovery,
        Self::DocRequest,
        Self::ToolchainVerify,
        Self::PlanCreation,
        Self::PlanUpdate,
        Self::Scratchpad,
        Self::Outcome,
        Self::Blocked,
        Self::Abort,
        Self::UserLanguage,
        Self::CustomTools,
    ];

    /// Canonical lower-case identifier used on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TextMessage => "text_message",
            Self::FileEdits => "file_edits",
            Self::Discovery => "discovery",
            Self::DocRequest => "doc_request",
            Self::ToolchainVerify => "toolchain_verify",
            Self::PlanCreation => "plan_creation",
            Self::PlanUpdate => "plan_update",
            Self::Scratchpad => "scratchpad",
            Self::Outcome => "outcome",
            Self::Blocked => "blocked",
            Self::Abort => "abort",
            Self::UserLanguage => "user_language",
            Self::CustomTools => "custom_tools",
        }
    }

    /// Phase of the bounded turn this capability belongs to.
    pub const fn phase(self) -> CapabilityPhase {
        match self {
            Self::Discovery | Self::DocRequest => CapabilityPhase::Discovery,
            Self::TextMessage
            | Self::FileEdits
            | Self::PlanCreation
            | Self::PlanUpdate
            | Self::Scratchpad
            | Self::UserLanguage
            | Self::CustomTools => CapabilityPhase::Execution,
            Self::ToolchainVerify => CapabilityPhase::Verification,
            Self::Outcome | Self::Blocked | Self::Abort => CapabilityPhase::Outcome,
        }
    }

    /// How this capability's protocol output is delivered to the harness.
    ///
    /// Delivery is orthogonal to [`LlmCapability::phase`]: the execution phase
    /// emits both streamed text and edits, which are consumed on the fly, and
    /// round-deferred payloads such as scratchpad notes, plan updates, and
    /// custom tool invocations, which are only read once generation completes.
    pub const fn delivery(self) -> CapabilityDelivery {
        match self {
            Self::TextMessage | Self::FileEdits => CapabilityDelivery::LiveStreaming,
            Self::Discovery
            | Self::DocRequest
            | Self::ToolchainVerify
            | Self::PlanCreation
            | Self::PlanUpdate
            | Self::Scratchpad
            | Self::Outcome
            | Self::Blocked
            | Self::Abort
            | Self::UserLanguage
            | Self::CustomTools => CapabilityDelivery::RoundDeferred,
        }
    }
}

impl fmt::Display for LlmCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Deterministic, duplicate-free ordered set of [`LlmCapability`] values.
///
/// A `CapabilitySet` is the single declarative description of what a protocol
/// role or operating mode may do. It preserves insertion order so prompts,
/// diagnostics, and tests stay stable, and it never stores the same capability
/// twice so composition through [`CapabilitySet::with`] is idempotent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CapabilitySet {
    capabilities: Vec<LlmCapability>,
}

impl CapabilitySet {
    /// Creates an empty capability set.
    pub fn new() -> Self {
        Self {
            capabilities: Vec::new(),
        }
    }

    /// Builds a set from a slice, preserving first-seen order and dropping
    /// duplicates.
    pub fn from_slice(capabilities: &[LlmCapability]) -> Self {
        let mut set = Self::new();
        for &capability in capabilities {
            set.insert(capability);
        }
        set
    }

    /// Inserts a capability unless it is already present.
    fn insert(&mut self, capability: LlmCapability) {
        if !self.capabilities.contains(&capability) {
            self.capabilities.push(capability);
        }
    }

    /// Reports whether the set contains the given capability.
    pub fn contains(&self, capability: LlmCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Returns a new set with `capability` added.
    ///
    /// Adding a capability that is already present leaves the set unchanged.
    pub fn with(&self, capability: LlmCapability) -> Self {
        let mut next = self.clone();
        next.insert(capability);
        next
    }

    /// Returns a new set without `capability`.
    pub fn without(&self, capability: LlmCapability) -> Self {
        Self {
            capabilities: self
                .capabilities
                .iter()
                .copied()
                .filter(|existing| *existing != capability)
                .collect(),
        }
    }

    /// Iterates over the capabilities in their deterministic order.
    pub fn iter(&self) -> std::slice::Iter<'_, LlmCapability> {
        self.capabilities.iter()
    }

    /// Returns the capabilities as a slice in their deterministic order.
    pub fn as_slice(&self) -> &[LlmCapability] {
        &self.capabilities
    }

    /// Reports whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    /// Returns the subset of capabilities belonging to the given phase,
    /// preserving order.
    pub fn by_phase(&self, phase: CapabilityPhase) -> Self {
        Self {
            capabilities: self
                .capabilities
                .iter()
                .copied()
                .filter(|capability| capability.phase() == phase)
                .collect(),
        }
    }

    /// Format-independent capability set for a free interactive REPL dialogue.
    ///
    /// The profile names a turn regime, not a wire format: the XML and the
    /// structured transports decode this very surface, so switching the
    /// protocol changes only the encoding. It is the full advertised surface
    /// minus plan mutation, because a user may ask for a plan at any moment of
    /// a dialogue while modifying an existing plan requires an active plan
    /// focus (`discussion_focus`).
    pub fn interactive_repl() -> Self {
        CapabilityProfile::interactive_repl().surface()
    }

    /// Format-independent capability set for executing one isolated step.
    ///
    /// Plan creation and modification are excluded: while an isolated step
    /// runs the plan is immutable and progress is recorded deterministically
    /// by the harness, never by the model.
    ///
    /// Toolchain verification stays declared in the profile and is stripped by
    /// the turn-intent narrowing, so the decision that the harness owns
    /// verification for a step lives in exactly one place instead of being
    /// duplicated across this profile and the narrowing rule.
    pub fn isolated_step() -> Self {
        CapabilityProfile::isolated_step().surface()
    }

    /// Discussion and planning capability set.
    ///
    /// The set is the declared surface of [`CapabilityProfile::discussion`], so a
    /// plan discussion carries the whole dialogue head — the very list every
    /// dialogue contour renders into its byte-stable head — plus the plan
    /// mutation the regime authorizes. The head and the delta therefore stay one
    /// declaration, and a capability can never belong to the discussion without
    /// also belonging to its profile.
    pub fn discussion() -> Self {
        CapabilityProfile::discussion().surface()
    }

    /// Architect discovery capability set.
    ///
    /// The architect gathers context and refines plans but never edits files
    /// or runs toolchain verification, so the file-edit and verification
    /// capabilities are excluded.
    pub fn architect_discovery() -> Self {
        CapabilityProfile::architect_discovery().surface()
    }
}

/// Declarative placement of one turn regime's capabilities across the prompt.
///
/// A regime profile is a pair of typed capability lists. The `core` list is
/// rendered into the byte-stable head of the system prompt, which every
/// dialogue contour shares, so switching between the REPL, a plan discussion,
/// and a review discussion keeps the provider prefix cache warm. The `delta`
/// list is rendered into a contour-specific block of the trailing user
/// message, where only the capabilities of the active regime travel.
///
/// Placement is a property of the regime composition, never of a capability's
/// identity: the same capability sits in the head of one regime, in the delta
/// of another, or is absent from a third entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityProfile {
    core: CapabilitySet,
    delta: CapabilitySet,
}

impl CapabilityProfile {
    /// Builds a profile from its head and delta lists.
    pub fn new(core: CapabilitySet, delta: CapabilitySet) -> Self {
        Self { core, delta }
    }

    /// Capabilities rendered into the byte-stable head.
    pub fn core(&self) -> &CapabilitySet {
        &self.core
    }

    /// Capabilities rendered into the contour delta of the trailing message.
    pub fn delta(&self) -> &CapabilitySet {
        &self.delta
    }

    /// Union of both lists, ordered by the canonical capability declaration.
    ///
    /// The canonical order keeps the surface deterministic no matter how a
    /// profile lays its two lists out, so every consumer observes the same
    /// sequence for the same set.
    pub fn surface(&self) -> CapabilitySet {
        let mut surface = CapabilitySet::new();
        for capability in LlmCapability::ALL {
            if self.core.contains(*capability) || self.delta.contains(*capability) {
                surface.insert(*capability);
            }
        }
        surface
    }

    /// Free interactive REPL dialogue: the whole dialogue surface in the head.
    pub fn interactive_repl() -> Self {
        Self::new(Self::dialogue_head(), CapabilitySet::new())
    }

    /// Plan discussion: the shared head plus plan mutation in the delta.
    ///
    /// Plan mutation stays a delta because authorizing it depends on an active
    /// plan focus, which is a property of the regime rather than of the head
    /// every dialogue contour renders identically.
    pub fn discussion() -> Self {
        Self::new(
            Self::dialogue_head(),
            CapabilitySet::from_slice(&[LlmCapability::PlanUpdate]),
        )
    }

    /// One isolated step: the REPL head minus plan authoring, no delta.
    pub fn isolated_step() -> Self {
        Self::new(
            Self::dialogue_head().without(LlmCapability::PlanCreation),
            CapabilitySet::new(),
        )
    }

    /// Architect discovery: plan mutation moves to the delta, surface unchanged.
    pub fn architect_discovery() -> Self {
        Self::new(
            CapabilitySet::from_slice(&[
                LlmCapability::TextMessage,
                LlmCapability::Discovery,
                LlmCapability::DocRequest,
                LlmCapability::PlanCreation,
                LlmCapability::Scratchpad,
                LlmCapability::Outcome,
                LlmCapability::Blocked,
                LlmCapability::UserLanguage,
            ]),
            CapabilitySet::from_slice(&[LlmCapability::PlanUpdate]),
        )
    }

    /// Head shared by every dialogue contour: the REPL surface.
    fn dialogue_head() -> CapabilitySet {
        CapabilitySet::from_slice(LlmCapability::ALL).without(LlmCapability::PlanUpdate)
    }
}

impl<'de> Deserialize<'de> for CapabilitySet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let capabilities = Vec::<LlmCapability>::deserialize(deserializer)?;
        Ok(Self::from_slice(&capabilities))
    }
}

/// Topic of a system documentation request.
///
/// Replaces the former free-form topic string so a request topic is a closed,
/// exhaustively matched set. `All` is the default and the tolerant fallback,
/// honoring Postel's law at the model-output ingest gateway: an unknown, empty,
/// or missing topic resolves to the full documentation set instead of failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", from = "String")]
pub enum DocTopic {
    /// Project introduction and philosophy.
    Overview,
    /// Three-tier context model.
    Context,
    /// Autonomous bounded-turn lifecycle.
    Workflow,
    /// Git safety and transactions.
    Git,
    /// Keyboard-first interface.
    Interface,
    /// Code review workflow.
    Review,
    /// Local plans.
    Plan,
    /// Project and user configuration reference.
    Config,
    /// Every documentation chapter aggregated (default).
    #[default]
    All,
}

impl std::convert::From<String> for DocTopic {
    fn from(s: String) -> Self {
        Self::parse_lossy(&s)
    }
}

impl std::convert::From<&str> for DocTopic {
    fn from(s: &str) -> Self {
        Self::parse_lossy(s)
    }
}

impl DocTopic {
    /// Every documentation topic in canonical prompt order.
    pub const ALL: &'static [DocTopic] = &[
        Self::Overview,
        Self::Context,
        Self::Workflow,
        Self::Git,
        Self::Interface,
        Self::Review,
        Self::Plan,
        Self::Config,
        Self::All,
    ];

    /// Canonical lower-case identifier used on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Context => "context",
            Self::Workflow => "workflow",
            Self::Git => "git",
            Self::Interface => "interface",
            Self::Review => "review",
            Self::Plan => "plan",
            Self::Config => "config",
            Self::All => "all",
        }
    }

    /// Tolerant constructor: trims, matches aliases case-insensitively, and
    /// resolves any unknown or empty topic to [`DocTopic::All`].
    pub fn parse_lossy(topic: &str) -> Self {
        topic.trim().parse().unwrap_or(Self::All)
    }
}

impl fmt::Display for DocTopic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DocTopic {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "overview" => Ok(Self::Overview),
            "context" => Ok(Self::Context),
            "workflow" => Ok(Self::Workflow),
            "git" => Ok(Self::Git),
            "interface" => Ok(Self::Interface),
            "review" => Ok(Self::Review),
            // `plans` is a legacy plural alias kept only for tolerant model
            // output parsing; scheduled for removal in 0.2.0 and tracked by the
            // wire-contract test in `lib.rs`.
            "plan" | "plans" => Ok(Self::Plan),
            "config" => Ok(Self::Config),
            "all" => Ok(Self::All),
            other => Err(format!(
                "Unknown documentation topic '{}'. Expected one of: {}",
                other,
                Self::ALL
                    .iter()
                    .map(DocTopic::as_str)
                    .collect::<Vec<_>>()
                    .join("|")
            )),
        }
    }
}

/// Normalized intent decoded from raw model output.
///
/// The protocol-specific syntax (XML tags, JSON schema) is translated into this
/// uniform command list so the core pipeline operates purely on semantics. Each
/// command corresponds to an enabled [`LlmCapability`].
///
/// `Eq` is deliberately absent: a custom tool invocation carries arbitrary JSON
/// arguments, and a JSON number may hold a float, so the command list is
/// comparable only through `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmCommand {
    /// Conversational text emitted to the user interface.
    Text { content: String },
    /// A single file edit operation.
    Edit { operation: EditOperation },
    /// A request to add a file to the context at a given access level.
    ContextRequest { path: String, access: ContextAccess },
    /// A search request across repository files using Rust/ripgrep regex.
    RepoSearch { path: String, regex: String },
    /// A request for the compact repository directory tree.
    RepoDirs,
    /// A request for the targeted symbol map of one subdirectory.
    ///
    /// The path is a repository-relative prefix matching whole path components;
    /// an empty path names the whole repository.
    RepoMap { path: String },
    /// A request for the tracked-file listing of one directory.
    ///
    /// The path is a repository-relative directory prefix matching whole path
    /// components; an empty path names the repository root. `recursive` widens
    /// the listing from the direct children to every tracked file underneath,
    /// keeping the default request shallow.
    DirList { path: String, recursive: bool },
    /// A request for system documentation by topic.
    DocRequest { topic: DocTopic },
    /// A request to run deterministic toolchain verification.
    Verify {
        target: VerifyTarget,
        on_success: VerifyOnSuccess,
    },
    /// A plan creation or modification payload.
    Plan { update: DiscussionPlanUpdate },
    /// Ephemeral working-memory notes.
    Scratchpad { notes: String },
    /// Explicit terminal outcome of a turn.
    Outcome { outcome: LlmOutcome },
    /// Explicit semantic blocker emitted when plan invariants or preconditions fail.
    Blocked {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<String>,
    },
    /// Explicit unconditional abort emitted when external host environment actions or developer intervention is required.
    Abort {
        reason: String,
        recommendation: String,
    },
    /// Detected primary natural language of the user prompt.
    UserLanguage { language: String },
    /// A typed invocation of one project-declared custom tool.
    ///
    /// `name` addresses a command registered in the project configuration, and
    /// `args` carries the typed argument object the command declares. The
    /// harness validates both against the declared allowlist, so an invocation
    /// can never widen into an arbitrary shell command.
    CustomTool {
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Protocol syntax or validation error encountered while parsing model output.
    ProtocolError { message: String },
}

impl LlmCommand {
    /// Capability that this command realizes, or `None` when the command is a
    /// protocol diagnostic rather than an enabled capability.
    pub fn capability(&self) -> Option<LlmCapability> {
        match self {
            Self::Text { .. } => Some(LlmCapability::TextMessage),
            Self::Edit { .. } => Some(LlmCapability::FileEdits),
            Self::ContextRequest { .. }
            | Self::RepoSearch { .. }
            | Self::RepoDirs
            | Self::RepoMap { .. }
            | Self::DirList { .. } => Some(LlmCapability::Discovery),
            Self::DocRequest { .. } => Some(LlmCapability::DocRequest),
            Self::Verify { .. } => Some(LlmCapability::ToolchainVerify),
            Self::Plan { update } => Some(match update.action {
                DiscussionPlanAction::Save => LlmCapability::PlanCreation,
                DiscussionPlanAction::Update | DiscussionPlanAction::Delete => {
                    LlmCapability::PlanUpdate
                }
            }),
            Self::Scratchpad { .. } => Some(LlmCapability::Scratchpad),
            Self::Outcome { .. } => Some(LlmCapability::Outcome),
            Self::Blocked { .. } => Some(LlmCapability::Blocked),
            Self::Abort { .. } => Some(LlmCapability::Abort),
            Self::UserLanguage { .. } => Some(LlmCapability::UserLanguage),
            Self::CustomTool { .. } => Some(LlmCapability::CustomTools),
            Self::ProtocolError { .. } => None,
        }
    }

    /// Phase of the bounded turn this command belongs to, or `None` for a
    /// protocol diagnostic.
    pub fn phase(&self) -> Option<CapabilityPhase> {
        self.capability().map(LlmCapability::phase)
    }
}

/// Deterministic toolchain verification target requested by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyTarget {
    /// Run every configured verification command.
    All,
    /// Compile and type-check only.
    Check,
    /// Run the test suite only.
    Test,
    /// Run the linter only.
    Clippy,
}

/// Desired behavior when toolchain verification succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyOnSuccess {
    /// Report only a concise confirmation.
    Silent,
    /// Surface the full command output.
    Report,
}

/// Explicit terminal outcome emitted by the model at the end of a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LlmOutcome {
    /// The task completed successfully, with an optional summary.
    Success { summary: Option<String> },
    /// The task is blocked, with an explanatory message.
    Error { message: String },
    /// The task is skipped, with an explanatory reason.
    Skip { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_capability_serde_roundtrip() {
        let cases = [
            (LlmCapability::TextMessage, "\"text_message\""),
            (LlmCapability::FileEdits, "\"file_edits\""),
            (LlmCapability::Discovery, "\"discovery\""),
            (LlmCapability::DocRequest, "\"doc_request\""),
            (LlmCapability::ToolchainVerify, "\"toolchain_verify\""),
            (LlmCapability::PlanCreation, "\"plan_creation\""),
            (LlmCapability::PlanUpdate, "\"plan_update\""),
            (LlmCapability::Scratchpad, "\"scratchpad\""),
            (LlmCapability::Outcome, "\"outcome\""),
            (LlmCapability::Blocked, "\"blocked\""),
            (LlmCapability::Abort, "\"abort\""),
            (LlmCapability::UserLanguage, "\"user_language\""),
            (LlmCapability::CustomTools, "\"custom_tools\""),
        ];

        for (capability, expected) in cases {
            assert_eq!(serde_json::to_string(&capability).unwrap(), expected);
            let decoded: LlmCapability = serde_json::from_str(expected).unwrap();
            assert_eq!(decoded, capability);
        }
    }

    #[test]
    fn test_llm_command_text_and_edit_serde_roundtrip() {
        let text = LlmCommand::Text {
            content: "Refactored the module".to_string(),
        };
        let json = serde_json::to_string(&text).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), text);

        let edit = LlmCommand::Edit {
            operation: EditOperation::Replace {
                path: "src/lib.rs".to_string(),
                old_text: "let a = 1;".to_string(),
                new_text: "let a = 2;".to_string(),
            },
        };
        let json = serde_json::to_string(&edit).unwrap();
        assert!(json.contains("\"type\":\"edit\""));
        assert!(json.contains("\"operation\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), edit);
    }

    #[test]
    fn test_llm_command_discovery_serde_roundtrip() {
        let request = LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::Editable,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"context_request\""));
        assert!(json.contains("\"access\":\"editable\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), request);

        let search = LlmCommand::RepoSearch {
            path: "src".to_string(),
            regex: "fn main".to_string(),
        };
        let json = serde_json::to_string(&search).unwrap();
        assert!(json.contains("\"type\":\"repo_search\""));
        assert!(json.contains("\"path\":\"src\""));
        assert!(json.contains("\"regex\":\"fn main\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), search);

        let doc = LlmCommand::DocRequest {
            topic: DocTopic::Workflow,
        };
        let json = serde_json::to_string(&doc).unwrap();
        assert!(json.contains("\"type\":\"doc_request\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), doc);

        let dirs = LlmCommand::RepoDirs;
        let json = serde_json::to_string(&dirs).unwrap();
        assert!(json.contains("\"type\":\"repo_dirs\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), dirs);

        let map = LlmCommand::RepoMap {
            path: "crates/core".to_string(),
        };
        let json = serde_json::to_string(&map).unwrap();
        assert!(json.contains("\"type\":\"repo_map\""));
        assert!(json.contains("\"path\":\"crates/core\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), map);

        let dir_list = LlmCommand::DirList {
            path: "crates/core".to_string(),
            recursive: true,
        };
        let json = serde_json::to_string(&dir_list).unwrap();
        assert!(json.contains("\"type\":\"dir_list\""));
        assert!(json.contains("\"path\":\"crates/core\""));
        assert!(json.contains("\"recursive\":true"));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), dir_list);
    }

    #[test]
    fn test_doc_topic_serde_aliases_default_and_parsing() {
        assert_eq!(serde_json::to_string(&DocTopic::Plan).unwrap(), "\"plan\"");
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"plans\"").unwrap(),
            DocTopic::Plan
        );
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"  PLANS  \"").unwrap(),
            DocTopic::Plan
        );
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"all\"").unwrap(),
            DocTopic::All
        );
        // Serde deserialization is tolerant on the wire according to Postel's law.
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"bogus\"").unwrap(),
            DocTopic::All
        );
        assert_eq!(DocTopic::default(), DocTopic::All);

        assert_eq!(DocTopic::parse_lossy("bogus"), DocTopic::All);
        assert_eq!(DocTopic::parse_lossy(""), DocTopic::All);
        assert_eq!(DocTopic::parse_lossy("  Interface "), DocTopic::Interface);
        assert_eq!(DocTopic::Interface.to_string(), "interface");
        assert_eq!("GIT".parse::<DocTopic>().unwrap(), DocTopic::Git);

        assert_eq!(
            serde_json::to_string(&DocTopic::Config).unwrap(),
            "\"config\""
        );
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"config\"").unwrap(),
            DocTopic::Config
        );
        assert_eq!(DocTopic::Config.to_string(), "config");
        assert_eq!("config".parse::<DocTopic>().unwrap(), DocTopic::Config);
        assert!("bogus".parse::<DocTopic>().is_err());

        // Unknown doc topic inside an LlmCommand does not fail deserialization.
        let raw = r#"{"type":"doc_request","topic":"nonexistent_topic"}"#;
        let command: LlmCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(
            command,
            LlmCommand::DocRequest {
                topic: DocTopic::All
            }
        );
    }

    #[test]
    fn test_doc_topic_all_roundtrips_through_wire_names() {
        for topic in DocTopic::ALL {
            assert_eq!(topic.as_str().parse::<DocTopic>().unwrap(), *topic);
        }
    }

    #[test]
    fn test_llm_command_verify_serde_roundtrip() {
        let verify = LlmCommand::Verify {
            target: VerifyTarget::Clippy,
            on_success: VerifyOnSuccess::Silent,
        };
        let json = serde_json::to_string(&verify).unwrap();
        assert!(json.contains("\"type\":\"verify\""));
        assert!(json.contains("\"target\":\"clippy\""));
        assert!(json.contains("\"on_success\":\"silent\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), verify);

        assert_eq!(
            serde_json::to_string(&VerifyTarget::All).unwrap(),
            "\"all\""
        );
        assert_eq!(
            serde_json::to_string(&VerifyOnSuccess::Report).unwrap(),
            "\"report\""
        );
    }

    #[test]
    fn test_llm_command_plan_scratchpad_language_serde_roundtrip() {
        let plan = LlmCommand::Plan {
            update: DiscussionPlanUpdate::default(),
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"type\":\"plan\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), plan);

        let scratchpad = LlmCommand::Scratchpad {
            notes: "Verified: parser fixed.".to_string(),
        };
        let json = serde_json::to_string(&scratchpad).unwrap();
        assert!(json.contains("\"type\":\"scratchpad\""));
        assert_eq!(
            serde_json::from_str::<LlmCommand>(&json).unwrap(),
            scratchpad
        );

        let language = LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        };
        let json = serde_json::to_string(&language).unwrap();
        assert!(json.contains("\"type\":\"user_language\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), language);

        let proto_err = LlmCommand::ProtocolError {
            message: "Malformed XML edit block: Unclosed tag".to_string(),
        };
        let json = serde_json::to_string(&proto_err).unwrap();
        assert!(json.contains("\"type\":\"protocol_error\""));
        assert!(json.contains("\"message\":\"Malformed XML edit block: Unclosed tag\""));
        assert_eq!(
            serde_json::from_str::<LlmCommand>(&json).unwrap(),
            proto_err
        );
    }

    #[test]
    fn test_llm_command_outcome_serde_roundtrip() {
        let success = LlmCommand::Outcome {
            outcome: LlmOutcome::Success {
                summary: Some("All checks passed".to_string()),
            },
        };
        let json = serde_json::to_string(&success).unwrap();
        assert!(json.contains("\"type\":\"outcome\""));
        assert!(json.contains("\"status\":\"success\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), success);

        let error = LlmCommand::Outcome {
            outcome: LlmOutcome::Error {
                message: "Missing dependency".to_string(),
            },
        };
        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("\"status\":\"error\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), error);

        let skip = LlmCommand::Outcome {
            outcome: LlmOutcome::Skip {
                reason: "Already implemented".to_string(),
            },
        };
        let json = serde_json::to_string(&skip).unwrap();
        assert!(json.contains("\"status\":\"skip\""));
        assert!(json.contains("\"reason\":\"Already implemented\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), skip);
    }

    #[test]
    fn test_llm_outcome_skip_serde_roundtrip() {
        let skip = LlmOutcome::Skip {
            reason: "Requirements already met".to_string(),
        };
        let json = serde_json::to_string(&skip).unwrap();
        assert_eq!(
            json,
            "{\"status\":\"skip\",\"reason\":\"Requirements already met\"}"
        );
        let decoded: LlmOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, skip);
    }

    #[test]
    fn test_llm_command_blocked_serde_roundtrip() {
        let blocked = LlmCommand::Blocked {
            reason: "Precondition failed".to_string(),
            details: Some("Missing table in db".to_string()),
        };
        let json = serde_json::to_string(&blocked).unwrap();
        assert!(json.contains("\"type\":\"blocked\""));
        assert!(json.contains("\"reason\":\"Precondition failed\""));
        assert!(json.contains("\"details\":\"Missing table in db\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), blocked);
    }

    #[test]
    fn test_llm_command_abort_serde_roundtrip() {
        let abort = LlmCommand::Abort {
            reason: "Approval required".to_string(),
            recommendation: "Run pnpm approve-builds".to_string(),
        };
        let json = serde_json::to_string(&abort).unwrap();
        assert!(json.contains("\"type\":\"abort\""));
        assert!(json.contains("\"reason\":\"Approval required\""));
        assert!(json.contains("\"recommendation\":\"Run pnpm approve-builds\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), abort);
    }

    #[test]
    fn test_capability_phase_serde_and_display() {
        let cases = [
            (CapabilityPhase::Discovery, "\"discovery\""),
            (CapabilityPhase::Execution, "\"execution\""),
            (CapabilityPhase::Verification, "\"verification\""),
            (CapabilityPhase::Outcome, "\"outcome\""),
        ];
        for (phase, encoded) in cases {
            assert_eq!(serde_json::to_string(&phase).unwrap(), encoded);
            let decoded: CapabilityPhase = serde_json::from_str(encoded).unwrap();
            assert_eq!(decoded, phase);
            assert_eq!(phase.as_str(), encoded.trim_matches('"'));
            assert_eq!(phase.to_string(), encoded.trim_matches('"'));
        }
    }

    #[test]
    fn test_llm_capability_as_str_and_phase() {
        assert_eq!(LlmCapability::ALL.len(), 13);
        for capability in LlmCapability::ALL {
            let encoded = format!("\"{}\"", capability.as_str());
            assert_eq!(serde_json::to_string(capability).unwrap(), encoded);
            assert_eq!(capability.to_string(), capability.as_str());
        }
        // A declared capability joins the advertised list once the module that
        // owns its tags, prompt text, and decoder is registered.
        assert!(LlmCapability::ALL.contains(&LlmCapability::CustomTools));

        let expectations = [
            (LlmCapability::Discovery, CapabilityPhase::Discovery),
            (LlmCapability::DocRequest, CapabilityPhase::Discovery),
            (LlmCapability::TextMessage, CapabilityPhase::Execution),
            (LlmCapability::FileEdits, CapabilityPhase::Execution),
            (LlmCapability::PlanCreation, CapabilityPhase::Execution),
            (LlmCapability::PlanUpdate, CapabilityPhase::Execution),
            (LlmCapability::Scratchpad, CapabilityPhase::Execution),
            (LlmCapability::UserLanguage, CapabilityPhase::Execution),
            (LlmCapability::CustomTools, CapabilityPhase::Execution),
            (
                LlmCapability::ToolchainVerify,
                CapabilityPhase::Verification,
            ),
            (LlmCapability::Outcome, CapabilityPhase::Outcome),
            (LlmCapability::Blocked, CapabilityPhase::Outcome),
            (LlmCapability::Abort, CapabilityPhase::Outcome),
        ];
        for (capability, phase) in expectations {
            assert_eq!(capability.phase(), phase);
        }
    }

    #[test]
    fn test_capability_delivery_serde_and_display() {
        let cases = [
            (CapabilityDelivery::LiveStreaming, "\"live_streaming\""),
            (CapabilityDelivery::RoundDeferred, "\"round_deferred\""),
        ];
        for (delivery, encoded) in cases {
            assert_eq!(serde_json::to_string(&delivery).unwrap(), encoded);
            let decoded: CapabilityDelivery = serde_json::from_str(encoded).unwrap();
            assert_eq!(decoded, delivery);
            assert_eq!(delivery.as_str(), encoded.trim_matches('"'));
            assert_eq!(delivery.to_string(), encoded.trim_matches('"'));
        }
        assert!(CapabilityDelivery::LiveStreaming.is_live());
        assert!(!CapabilityDelivery::RoundDeferred.is_live());
    }

    #[test]
    fn test_capability_delivery_is_declared_per_capability() {
        for capability in LlmCapability::ALL {
            let expected = match capability {
                LlmCapability::TextMessage | LlmCapability::FileEdits => {
                    CapabilityDelivery::LiveStreaming
                }
                _ => CapabilityDelivery::RoundDeferred,
            };
            assert_eq!(capability.delivery(), expected);
        }

        // Delivery is orthogonal to phase: the execution phase mixes streamed
        // text and edits with round-deferred plan, scratchpad, and tool
        // payloads, so neither property may be derived from the other.
        let advertised = CapabilitySet::from_slice(LlmCapability::ALL);
        let execution = advertised.by_phase(CapabilityPhase::Execution);
        assert!(execution.iter().any(|c| c.delivery().is_live()));
        assert!(execution.iter().any(|c| !c.delivery().is_live()));

        // Discovery, verification, and outcome capabilities are never streamed.
        for phase in [
            CapabilityPhase::Discovery,
            CapabilityPhase::Verification,
            CapabilityPhase::Outcome,
        ] {
            for capability in advertised.by_phase(phase).iter() {
                assert_eq!(capability.delivery(), CapabilityDelivery::RoundDeferred);
            }
        }
    }

    #[test]
    fn test_capability_set_composition_and_dedup() {
        let empty = CapabilitySet::new();
        assert!(empty.is_empty());
        assert!(!empty.contains(LlmCapability::TextMessage));

        // Duplicates are dropped and first-seen order is preserved.
        let set = CapabilitySet::from_slice(&[
            LlmCapability::FileEdits,
            LlmCapability::TextMessage,
            LlmCapability::FileEdits,
        ]);
        assert_eq!(
            set.as_slice(),
            &[LlmCapability::FileEdits, LlmCapability::TextMessage]
        );
        assert_eq!(set.iter().count(), 2);

        // `with` adds a new capability and is idempotent.
        let extended = set.with(LlmCapability::Discovery);
        assert!(extended.contains(LlmCapability::Discovery));
        assert_eq!(extended.iter().count(), 3);
        assert_eq!(extended.with(LlmCapability::Discovery), extended);

        // `without` removes a capability and tolerates a missing one.
        let reduced = set.without(LlmCapability::FileEdits);
        assert_eq!(reduced.as_slice(), &[LlmCapability::TextMessage]);
        assert_eq!(reduced.without(LlmCapability::Outcome), reduced);

        // `by_phase` keeps only the requested phase, preserving order.
        let discovery = extended.by_phase(CapabilityPhase::Discovery);
        assert_eq!(discovery.as_slice(), &[LlmCapability::Discovery]);
        assert!(extended.by_phase(CapabilityPhase::Outcome).is_empty());
    }

    #[test]
    fn test_capability_set_profiles() {
        // Profiles name a turn regime, never a wire format: both transports
        // decode the same capability surface, so format-specific profile
        // factories no longer exist.
        let discussion = CapabilitySet::discussion();
        assert!(discussion.contains(LlmCapability::TextMessage));
        assert!(discussion.contains(LlmCapability::Discovery));
        assert!(discussion.contains(LlmCapability::PlanCreation));
        assert!(discussion.contains(LlmCapability::PlanUpdate));
        // A plan discussion carries the full dialogue surface: the developer may
        // ask for the fix directly, exactly as in a review discussion, so the
        // edit, verification, abort, and tool capabilities travel with it.
        assert!(discussion.contains(LlmCapability::FileEdits));
        assert!(discussion.contains(LlmCapability::ToolchainVerify));
        assert!(discussion.contains(LlmCapability::Abort));
        assert!(discussion.contains(LlmCapability::CustomTools));
        // The set is the surface of the regime profile, never a second list.
        assert_eq!(discussion, CapabilityProfile::discussion().surface());
        assert_eq!(discussion, CapabilitySet::from_slice(LlmCapability::ALL));

        let architect = CapabilitySet::architect_discovery();
        assert!(architect.contains(LlmCapability::DocRequest));
        assert!(architect.contains(LlmCapability::PlanCreation));
        assert!(architect.contains(LlmCapability::Scratchpad));
        assert!(!architect.contains(LlmCapability::FileEdits));
        assert!(!architect.contains(LlmCapability::ToolchainVerify));
        assert!(!architect.contains(LlmCapability::CustomTools));
    }

    #[test]
    fn test_plan_permission_matrix() {
        // A user may ask for a plan at any moment of a dialogue, but changing
        // an existing plan requires an active plan focus, which the base REPL
        // profile does not assume.
        let repl = CapabilitySet::interactive_repl();
        assert!(repl.contains(LlmCapability::PlanCreation));
        assert!(!repl.contains(LlmCapability::PlanUpdate));

        // A dedicated plan discussion owns both permissions.
        let discussion = CapabilitySet::discussion();
        assert!(discussion.contains(LlmCapability::PlanCreation));
        assert!(discussion.contains(LlmCapability::PlanUpdate));

        // A plan is immutable while an isolated step runs.
        let step = CapabilitySet::isolated_step();
        assert!(!step.contains(LlmCapability::PlanCreation));
        assert!(!step.contains(LlmCapability::PlanUpdate));
    }

    #[test]
    fn test_interactive_repl_and_isolated_step_composition() {
        let repl = CapabilitySet::interactive_repl();
        for capability in [
            LlmCapability::TextMessage,
            LlmCapability::FileEdits,
            LlmCapability::Discovery,
            LlmCapability::DocRequest,
            LlmCapability::ToolchainVerify,
            LlmCapability::PlanCreation,
            LlmCapability::Scratchpad,
            LlmCapability::Outcome,
            LlmCapability::Blocked,
            LlmCapability::Abort,
            LlmCapability::UserLanguage,
            LlmCapability::CustomTools,
        ] {
            assert!(
                repl.contains(capability),
                "interactive_repl must carry {capability}"
            );
        }

        // The profile is the advertised surface minus plan mutation, so the two
        // definitions cannot drift apart when a capability is added.
        let expected =
            CapabilitySet::from_slice(LlmCapability::ALL).without(LlmCapability::PlanUpdate);
        assert_eq!(repl.as_slice(), expected.as_slice());

        // A step keeps the operational surface of a dialogue except plan
        // authoring; toolchain verification stays declared in the profile and
        // is removed by the turn-intent narrowing.
        let step = CapabilitySet::isolated_step();
        assert_eq!(
            step.as_slice(),
            repl.without(LlmCapability::PlanCreation).as_slice()
        );
        assert!(step.contains(LlmCapability::ToolchainVerify));
        assert!(step.contains(LlmCapability::Blocked));
        assert!(step.contains(LlmCapability::Abort));
    }

    #[test]
    fn test_capability_set_serde_normalizes_duplicates() {
        let set = CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::ToolchainVerify,
        ]);
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(json, "[\"text_message\",\"toolchain_verify\"]");

        let raw = "[\"toolchain_verify\",\"text_message\",\"toolchain_verify\"]";
        let decoded: CapabilitySet = serde_json::from_str(raw).unwrap();
        assert_eq!(
            decoded.as_slice(),
            &[LlmCapability::ToolchainVerify, LlmCapability::TextMessage]
        );
    }

    #[test]
    fn test_llm_command_capability_and_phase() {
        let text = LlmCommand::Text {
            content: "hello".to_string(),
        };
        assert_eq!(text.capability(), Some(LlmCapability::TextMessage));
        assert_eq!(text.phase(), Some(CapabilityPhase::Execution));

        let edit = LlmCommand::Edit {
            operation: EditOperation::Replace {
                path: "src/lib.rs".to_string(),
                old_text: "a".to_string(),
                new_text: "b".to_string(),
            },
        };
        assert_eq!(edit.capability(), Some(LlmCapability::FileEdits));
        assert_eq!(edit.phase(), Some(CapabilityPhase::Execution));

        let context = LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        };
        assert_eq!(context.capability(), Some(LlmCapability::Discovery));
        assert_eq!(context.phase(), Some(CapabilityPhase::Discovery));

        let search = LlmCommand::RepoSearch {
            path: "src".to_string(),
            regex: "fn main".to_string(),
        };
        assert_eq!(search.capability(), Some(LlmCapability::Discovery));

        let dirs = LlmCommand::RepoDirs;
        assert_eq!(dirs.capability(), Some(LlmCapability::Discovery));
        assert_eq!(dirs.phase(), Some(CapabilityPhase::Discovery));

        let map = LlmCommand::RepoMap {
            path: "crates/core".to_string(),
        };
        assert_eq!(map.capability(), Some(LlmCapability::Discovery));
        assert_eq!(map.phase(), Some(CapabilityPhase::Discovery));

        let dir_list = LlmCommand::DirList {
            path: "crates/core".to_string(),
            recursive: false,
        };
        assert_eq!(dir_list.capability(), Some(LlmCapability::Discovery));
        assert_eq!(dir_list.phase(), Some(CapabilityPhase::Discovery));

        let doc = LlmCommand::DocRequest {
            topic: DocTopic::Workflow,
        };
        assert_eq!(doc.capability(), Some(LlmCapability::DocRequest));
        assert_eq!(doc.phase(), Some(CapabilityPhase::Discovery));

        let verify = LlmCommand::Verify {
            target: VerifyTarget::All,
            on_success: VerifyOnSuccess::Silent,
        };
        assert_eq!(verify.capability(), Some(LlmCapability::ToolchainVerify));
        assert_eq!(verify.phase(), Some(CapabilityPhase::Verification));

        let scratchpad = LlmCommand::Scratchpad {
            notes: "notes".to_string(),
        };
        assert_eq!(scratchpad.capability(), Some(LlmCapability::Scratchpad));

        let outcome = LlmCommand::Outcome {
            outcome: LlmOutcome::Success { summary: None },
        };
        assert_eq!(outcome.capability(), Some(LlmCapability::Outcome));
        assert_eq!(outcome.phase(), Some(CapabilityPhase::Outcome));

        let blocked = LlmCommand::Blocked {
            reason: "blocked".to_string(),
            details: None,
        };
        assert_eq!(blocked.capability(), Some(LlmCapability::Blocked));

        let abort = LlmCommand::Abort {
            reason: "blocked by host".to_string(),
            recommendation: "run script".to_string(),
        };
        assert_eq!(abort.capability(), Some(LlmCapability::Abort));
        assert_eq!(abort.phase(), Some(CapabilityPhase::Outcome));

        let language = LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        };
        assert_eq!(language.capability(), Some(LlmCapability::UserLanguage));

        let custom_tool = LlmCommand::CustomTool {
            name: "migrate".to_string(),
            args: serde_json::json!({ "database": "primary" }),
        };
        assert_eq!(custom_tool.capability(), Some(LlmCapability::CustomTools));
        assert_eq!(custom_tool.phase(), Some(CapabilityPhase::Execution));

        let error = LlmCommand::ProtocolError {
            message: "malformed".to_string(),
        };
        assert_eq!(error.capability(), None);
        assert_eq!(error.phase(), None);

        let plan_save = LlmCommand::Plan {
            update: DiscussionPlanUpdate {
                action: DiscussionPlanAction::Save,
                ..Default::default()
            },
        };
        let save_capability = plan_save.capability();
        assert_eq!(save_capability, Some(LlmCapability::PlanCreation));

        for action in [DiscussionPlanAction::Update, DiscussionPlanAction::Delete] {
            let plan = LlmCommand::Plan {
                update: DiscussionPlanUpdate {
                    action,
                    ..Default::default()
                },
            };
            assert_eq!(plan.capability(), Some(LlmCapability::PlanUpdate));
        }
    }

    #[test]
    fn test_llm_command_custom_tool_serde_roundtrip() {
        let tool = LlmCommand::CustomTool {
            name: "migrate".to_string(),
            args: serde_json::json!({ "database": "primary", "steps": 2 }),
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"type\":\"custom_tool\""));
        assert!(json.contains("\"name\":\"migrate\""));
        assert!(json.contains("\"database\":\"primary\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), tool);

        // An invocation without arguments still decodes, so a command that
        // declares none never requires the model to invent an empty object.
        let bare: LlmCommand =
            serde_json::from_str(r#"{"type":"custom_tool","name":"status"}"#).unwrap();
        assert_eq!(
            bare,
            LlmCommand::CustomTool {
                name: "status".to_string(),
                args: serde_json::Value::Null,
            }
        );
    }

    #[test]
    fn test_capability_profile_dialogue_heads_are_shared() {
        // Every dialogue contour renders the same head, which is what keeps the
        // provider prefix cache warm across focus switching. The review
        // discussion runs through the REPL profile, so its head is shared by
        // construction rather than by coincidence.
        let head = CapabilitySet::from_slice(LlmCapability::ALL).without(LlmCapability::PlanUpdate);

        for profile in [
            CapabilityProfile::interactive_repl(),
            CapabilityProfile::discussion(),
        ] {
            assert_eq!(profile.core().as_slice(), head.as_slice());
        }
    }

    #[test]
    fn test_capability_profile_placement_per_regime() {
        let discussion = CapabilityProfile::discussion();
        assert_eq!(discussion.delta().as_slice(), &[LlmCapability::PlanUpdate]);
        assert!(!discussion.core().contains(LlmCapability::PlanUpdate));
        assert!(discussion.surface().contains(LlmCapability::PlanUpdate));

        assert!(CapabilityProfile::interactive_repl().delta().is_empty());
        assert!(CapabilityProfile::isolated_step().delta().is_empty());

        let step = CapabilityProfile::isolated_step();
        assert!(
            !step.core().contains(LlmCapability::PlanCreation),
            "an isolated step never authors a plan"
        );
        assert!(step.core().contains(LlmCapability::ToolchainVerify));
    }

    #[test]
    fn test_capability_profile_architect_moves_plan_update_to_delta() {
        let architect = CapabilityProfile::architect_discovery();
        assert_eq!(architect.delta().as_slice(), &[LlmCapability::PlanUpdate]);
        assert!(!architect.core().contains(LlmCapability::PlanUpdate));

        // Only the placement changes: the declared surface stays identical.
        let expected = CapabilitySet::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::Discovery,
            LlmCapability::DocRequest,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::Scratchpad,
            LlmCapability::Outcome,
            LlmCapability::Blocked,
            LlmCapability::UserLanguage,
        ]);
        assert_eq!(architect.surface(), expected);
        assert_eq!(CapabilitySet::architect_discovery(), expected);
    }

    #[test]
    fn test_capability_profile_surface_matches_factories() {
        let cases = [
            (
                CapabilityProfile::interactive_repl(),
                CapabilitySet::interactive_repl(),
            ),
            (CapabilityProfile::discussion(), CapabilitySet::discussion()),
            (
                CapabilityProfile::isolated_step(),
                CapabilitySet::isolated_step(),
            ),
            (
                CapabilityProfile::architect_discovery(),
                CapabilitySet::architect_discovery(),
            ),
        ];

        for (profile, factory) in cases {
            let surface = profile.surface();
            assert_eq!(surface, factory);

            // The union is rendered in the canonical declaration order, so a
            // profile cannot make the surface order depend on how it laid the
            // head and delta lists out.
            let mut previous = None;
            for capability in surface.iter() {
                let index = LlmCapability::ALL
                    .iter()
                    .position(|declared| declared == capability)
                    .expect("surface carries only declared capabilities");
                assert!(
                    previous.is_none_or(|last| last < index),
                    "surface is rendered in the canonical declaration order"
                );
                previous = Some(index);
            }
        }
    }

    #[test]
    fn test_capability_profile_head_loss_is_a_configuration_defect() {
        // A dialogue profile that drops any capability of the shared head
        // invalidates the prefix cache of every other dialogue contour, so the
        // head is audited as a whole rather than per capability.
        let head = CapabilityProfile::interactive_repl().core().clone();
        for profile in [
            CapabilityProfile::interactive_repl(),
            CapabilityProfile::discussion(),
        ] {
            for capability in head.iter() {
                assert!(
                    profile.core().contains(*capability),
                    "{capability} disappeared from the shared head"
                );
            }
        }
    }
}
