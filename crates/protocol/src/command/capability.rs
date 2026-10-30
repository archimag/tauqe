//! Capability classification model shared by every prompt protocol.
//!
//! A protocol advertises the interaction capabilities it supports; this module
//! declares what each one means: the phase of the bounded turn it belongs to,
//! how its payload reaches the harness, and how the capabilities of one turn
//! regime are placed across the prompt.

use std::fmt;

use serde::{Deserialize, Serialize};

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
/// supports, and its decoder produces only the corresponding
/// [`LlmCommand`](crate::command::LlmCommand) variants. This keeps the execution
/// pipeline independent of any single wire format and makes protocols
/// interchangeable.
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

    /// How the capability's protocol output reaches the harness.
    ///
    /// Declaring delivery beside the capability keeps a single source of
    /// truth: a streaming scanner or a round-level decoder reads this property
    /// instead of special-casing capability names.
    pub const fn delivery(self) -> CapabilityDelivery {
        match self {
            Self::TextMessage | Self::FileEdits => CapabilityDelivery::LiveStreaming,
            _ => CapabilityDelivery::RoundDeferred,
        }
    }
}

impl fmt::Display for LlmCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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

    /// One isolated step: the dialogue head minus plan authoring and
    /// documentation, no delta.
    pub fn isolated_step() -> Self {
        Self::new(
            Self::dialogue_head()
                .without(LlmCapability::PlanCreation)
                .without(LlmCapability::DocRequest),
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

/// Ordered, de-duplicated collection of capabilities a protocol advertises.
///
/// A set preserves first-seen order and never holds a duplicate, so the
/// prompt, the response schema, and the decoder all observe one deterministic
/// surface for the same declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CapabilitySet {
    capabilities: Vec<LlmCapability>,
}

impl CapabilitySet {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self {
            capabilities: Vec::new(),
        }
    }

    /// Builds a set from a slice, dropping duplicates and keeping first-seen
    /// order.
    pub fn from_slice(capabilities: &[LlmCapability]) -> Self {
        let mut set = Self::new();
        for capability in capabilities {
            set.insert(*capability);
        }
        set
    }

    /// Returns the capabilities in the order they entered the set.
    pub fn as_slice(&self) -> &[LlmCapability] {
        &self.capabilities
    }

    /// Iterates the capabilities of the set.
    pub fn iter(&self) -> std::slice::Iter<'_, LlmCapability> {
        self.capabilities.iter()
    }

    /// Reports whether the set holds no capability.
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    /// Reports whether `capability` belongs to the set.
    pub fn contains(&self, capability: LlmCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Inserts `capability` when it is absent.
    pub fn insert(&mut self, capability: LlmCapability) {
        if !self.contains(capability) {
            self.capabilities.push(capability);
        }
    }

    /// Returns a copy extended with `capability`.
    pub fn with(&self, capability: LlmCapability) -> Self {
        let mut set = self.clone();
        set.insert(capability);
        set
    }

    /// Returns a copy without `capability`.
    pub fn without(&self, capability: LlmCapability) -> Self {
        Self {
            capabilities: self
                .capabilities
                .iter()
                .copied()
                .filter(|kept| *kept != capability)
                .collect(),
        }
    }

    /// Keeps only the capabilities of `phase`, preserving their order.
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

    /// Free interactive REPL dialogue capability set.
    ///
    /// The set is the declared surface of [`CapabilityProfile::interactive_repl`]:
    /// the dialogue head every interactive contour renders, without plan
    /// mutation, which the delta of a focused discussion carries instead.
    pub fn interactive_repl() -> Self {
        CapabilityProfile::interactive_repl().surface()
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

    /// Capability set of one isolated step.
    ///
    /// Plan creation and modification are excluded: while an isolated step
    /// runs the plan is immutable and progress is recorded deterministically
    /// by the harness, never by the model.
    ///
    /// Documentation requests are excluded as well: an isolated step executes
    /// an approved design inside the codebase, so knowledge about the harness
    /// itself is noise the step cannot act on.
    ///
    /// Toolchain verification stays declared in the profile and is stripped by
    /// the turn-intent narrowing, so the decision that the harness owns
    /// verification for a step lives in exactly one place instead of being
    /// duplicated across this profile and the narrowing rule.
    pub fn isolated_step() -> Self {
        CapabilityProfile::isolated_step().surface()
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

impl Default for CapabilitySet {
    fn default() -> Self {
        Self::new()
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
        // authoring and documentation; toolchain verification stays declared
        // in the profile and is removed by the turn-intent narrowing.
        let step = CapabilitySet::isolated_step();
        assert_eq!(
            step.as_slice(),
            repl.without(LlmCapability::PlanCreation)
                .without(LlmCapability::DocRequest)
                .as_slice()
        );
        assert!(!step.contains(LlmCapability::PlanCreation));
        assert!(!step.contains(LlmCapability::DocRequest));
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
        assert!(
            !step.core().contains(LlmCapability::DocRequest),
            "an isolated step never consults the harness documentation"
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
