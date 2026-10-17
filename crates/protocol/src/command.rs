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
    /// Detected primary natural language of the user prompt.
    UserLanguage,
}

impl LlmCapability {
    /// Every capability in canonical declaration order.
    ///
    /// The order matches the enum declaration so iteration, prompt
    /// composition, and tests stay deterministic.
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
        Self::UserLanguage,
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
            Self::UserLanguage => "user_language",
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
            | Self::UserLanguage => CapabilityPhase::Execution,
            Self::ToolchainVerify => CapabilityPhase::Verification,
            Self::Outcome | Self::Blocked => CapabilityPhase::Outcome,
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

    /// Full interactive XML development capability set.
    pub fn develop_xml() -> Self {
        Self::from_slice(LlmCapability::ALL)
    }

    /// XML capability set for executing one isolated plan step.
    ///
    /// Plan creation and modification are excluded: while an isolated step
    /// runs the plan is immutable and progress is recorded deterministically
    /// by the harness, never by the model.
    pub fn isolated_step_xml() -> Self {
        Self::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::FileEdits,
            LlmCapability::Discovery,
            LlmCapability::DocRequest,
            LlmCapability::ToolchainVerify,
            LlmCapability::Scratchpad,
            LlmCapability::Outcome,
            LlmCapability::Blocked,
            LlmCapability::UserLanguage,
        ])
    }

    /// Structured-output development capability set.
    ///
    /// Structured output transports the same operational surface as the XML
    /// development set, minus plan authoring: documentation requests,
    /// scratchpad notes, terminal outcomes, and semantic blockers travel as
    /// dedicated schema fields instead of control tags. Only plan creation and
    /// plan modification remain XML-exclusive, because a JSON edit proposal is
    /// not a planning channel.
    pub fn develop_structured() -> Self {
        Self::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::FileEdits,
            LlmCapability::Discovery,
            LlmCapability::DocRequest,
            LlmCapability::ToolchainVerify,
            LlmCapability::Scratchpad,
            LlmCapability::Outcome,
            LlmCapability::Blocked,
            LlmCapability::UserLanguage,
        ])
    }

    /// Structured-output capability set for executing one isolated plan step.
    ///
    /// Plan creation and modification are excluded: while an isolated step runs
    /// the plan is immutable and progress is recorded deterministically by the
    /// harness, never by the model. The resulting set therefore matches
    /// [`CapabilitySet::develop_structured`].
    pub fn isolated_step_structured() -> Self {
        Self::develop_structured()
    }

    /// Discussion and planning capability set.
    pub fn discussion() -> Self {
        Self::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::Discovery,
            LlmCapability::UserLanguage,
        ])
    }

    /// Architect discovery capability set.
    ///
    /// The architect gathers context and refines plans but never edits files
    /// or runs toolchain verification, so the file-edit and verification
    /// capabilities are excluded.
    pub fn architect_discovery() -> Self {
        Self::from_slice(&[
            LlmCapability::TextMessage,
            LlmCapability::Discovery,
            LlmCapability::DocRequest,
            LlmCapability::PlanCreation,
            LlmCapability::PlanUpdate,
            LlmCapability::Scratchpad,
            LlmCapability::Outcome,
            LlmCapability::Blocked,
            LlmCapability::UserLanguage,
        ])
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Detected primary natural language of the user prompt.
    UserLanguage { language: String },
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
            Self::ContextRequest { .. } | Self::RepoSearch { .. } => {
                Some(LlmCapability::Discovery)
            }
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
            Self::UserLanguage { .. } => Some(LlmCapability::UserLanguage),
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
            (LlmCapability::UserLanguage, "\"user_language\""),
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
        assert!("bogus".parse::<DocTopic>().is_err());

        // Unknown doc topic inside an LlmCommand does not fail deserialization.
        let raw = r#"{"type":"doc_request","topic":"nonexistent_topic"}"#;
        let command: LlmCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(command, LlmCommand::DocRequest { topic: DocTopic::All });
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

        assert_eq!(serde_json::to_string(&VerifyTarget::All).unwrap(), "\"all\"");
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
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), scratchpad);

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
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), proto_err);
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
        assert_eq!(LlmCapability::ALL.len(), 11);
        for capability in LlmCapability::ALL {
            let encoded = format!("\"{}\"", capability.as_str());
            assert_eq!(serde_json::to_string(capability).unwrap(), encoded);
            assert_eq!(capability.to_string(), capability.as_str());
        }

        let expectations = [
            (LlmCapability::Discovery, CapabilityPhase::Discovery),
            (LlmCapability::DocRequest, CapabilityPhase::Discovery),
            (LlmCapability::TextMessage, CapabilityPhase::Execution),
            (LlmCapability::FileEdits, CapabilityPhase::Execution),
            (LlmCapability::PlanCreation, CapabilityPhase::Execution),
            (LlmCapability::PlanUpdate, CapabilityPhase::Execution),
            (LlmCapability::Scratchpad, CapabilityPhase::Execution),
            (LlmCapability::UserLanguage, CapabilityPhase::Execution),
            (LlmCapability::ToolchainVerify, CapabilityPhase::Verification),
            (LlmCapability::Outcome, CapabilityPhase::Outcome),
            (LlmCapability::Blocked, CapabilityPhase::Outcome),
        ];
        for (capability, phase) in expectations {
            assert_eq!(capability.phase(), phase);
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
        let develop_xml = CapabilitySet::develop_xml();
        assert_eq!(develop_xml.as_slice(), LlmCapability::ALL);

        let isolated_xml = CapabilitySet::isolated_step_xml();
        assert!(isolated_xml.contains(LlmCapability::FileEdits));
        assert!(isolated_xml.contains(LlmCapability::Blocked));
        assert!(isolated_xml.contains(LlmCapability::ToolchainVerify));
        assert!(!isolated_xml.contains(LlmCapability::PlanCreation));
        assert!(!isolated_xml.contains(LlmCapability::PlanUpdate));

        let develop_structured = CapabilitySet::develop_structured();
        let isolated_structured = CapabilitySet::isolated_step_structured();
        assert_eq!(develop_structured, isolated_structured);
        assert!(develop_structured.contains(LlmCapability::FileEdits));
        assert!(develop_structured.contains(LlmCapability::DocRequest));
        assert!(develop_structured.contains(LlmCapability::Scratchpad));
        assert!(develop_structured.contains(LlmCapability::Outcome));
        assert!(develop_structured.contains(LlmCapability::Blocked));
        assert!(!develop_structured.contains(LlmCapability::PlanCreation));
        assert!(!develop_structured.contains(LlmCapability::PlanUpdate));
        // The structured and isolated-step XML profiles share one operational
        // surface: only plan authoring distinguishes them from `develop_xml`.
        assert_eq!(
            develop_structured.as_slice(),
            CapabilitySet::isolated_step_xml().as_slice()
        );

        let discussion = CapabilitySet::discussion();
        assert!(discussion.contains(LlmCapability::PlanCreation));
        assert!(discussion.contains(LlmCapability::PlanUpdate));
        assert!(!discussion.contains(LlmCapability::FileEdits));
        assert!(!discussion.contains(LlmCapability::ToolchainVerify));

        let architect = CapabilitySet::architect_discovery();
        assert!(architect.contains(LlmCapability::DocRequest));
        assert!(architect.contains(LlmCapability::PlanCreation));
        assert!(!architect.contains(LlmCapability::FileEdits));
        assert!(!architect.contains(LlmCapability::ToolchainVerify));
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

        let language = LlmCommand::UserLanguage {
            language: "Russian".to_string(),
        };
        assert_eq!(language.capability(), Some(LlmCapability::UserLanguage));

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
}
