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
    /// Researching context and documentation before a proposal is produced.
    Research,
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
            Self::Research => "research",
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
    /// Replace text or an anchored line range in an existing file.
    ///
    /// Projected through `PatchStrategy`: `EditReplace` under
    /// `SearchReplace`, or `EditLineAnchor` under `LineAnchor`.
    EditReplace,
    /// Create a new file.
    EditCreate,
    /// Delete an existing file.
    EditDelete,
    /// Move or rename an existing file.
    EditMove,
    /// Move or rename a directory subtree.
    EditMoveDir,
    /// Overwrite an existing file.
    EditOverwrite,
    /// Request to add a file to the working context.
    ContextRequest,
    /// Search repository files using Rust/ripgrep regex.
    RepoSearch,
    /// Request the compact repository directory tree.
    RepoDirs,
    /// Request a targeted symbol map of one subdirectory.
    RepoMap,
    /// Request a tracked-file listing of one directory.
    DirList,
    /// Requests for system documentation of this harness by topic.
    DocRequest,
    /// Requests for external project documentation by its logical identifier.
    ///
    /// The identifier names a document the project declared in
    /// `[[outer_docs]]`, never a cache path, so only declared material is
    /// reachable and the harness alone decides where a snapshot lives. The
    /// request belongs to the research phase of every contour that investigates
    /// the project domain, an isolated step included, because a step reads the
    /// contracts it was pointed at; the internal documentation of the harness
    /// itself stays closed to a step.
    OuterDocRequest,
    /// Requests to run deterministic toolchain verification.
    ToolchainVerify,
    /// Creation of a new project plan.
    PlanCreation,
    /// Modification of an existing project plan.
    PlanUpdate,
    /// A decision recorded for one finding of a discussed review session.
    ///
    /// The decision belongs to the review discussion regime: it states the
    /// status the engineer settled on for one finding together with the
    /// reasoning behind it, and it travels exclusively through the structured
    /// response schema, because the capability owns no control tag. The harness
    /// applies it against the session the turn names and never against whichever
    /// session happens to be active.
    ReviewUpdate,
    /// Ephemeral working-memory notes for multi-round reasoning.
    Scratchpad,
    /// Terminal commit message stated by the model when a turn succeeds.
    CommitMessage,
    /// Terminal error stated by the model when a turn fails.
    TurnError,
    /// Terminal skip reason stated by the model when the work is already satisfied.
    TurnSkip,
    /// Explicit semantic blocker emitted when preconditions or assumptions are violated.
    Blocked,
    /// Explicit unconditional abort emitted when external host environment actions or developer intervention is required.
    Abort,
    CustomTools,
    /// Concise explanation accompanying code modifications in turn/step execution.
    ShortDescription,
}

impl LlmCapability {
    /// Every capability a built-in protocol profile may advertise.
    ///
    /// The order matches the enum declaration so iteration, prompt
    /// composition, and tests stay deterministic.
    pub const ALL: &'static [LlmCapability] = &[
        Self::TextMessage,
        Self::EditReplace,
        Self::EditCreate,
        Self::EditDelete,
        Self::EditMove,
        Self::EditMoveDir,
        Self::EditOverwrite,
        Self::ContextRequest,
        Self::RepoSearch,
        Self::RepoDirs,
        Self::RepoMap,
        Self::DirList,
        Self::DocRequest,
        Self::OuterDocRequest,
        Self::ToolchainVerify,
        Self::PlanCreation,
        Self::PlanUpdate,
        Self::ReviewUpdate,
        Self::Scratchpad,
        Self::CommitMessage,
        Self::TurnError,
        Self::TurnSkip,
        Self::Blocked,
        Self::Abort,
        Self::CustomTools,
        Self::ShortDescription,
    ];

    /// Canonical lower-case identifier used on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TextMessage => "text_message",
            Self::EditReplace => "edit_replace",
            Self::EditCreate => "edit_create",
            Self::EditDelete => "edit_delete",
            Self::EditMove => "edit_move",
            Self::EditMoveDir => "edit_move_dir",
            Self::EditOverwrite => "edit_overwrite",
            Self::ContextRequest => "context_request",
            Self::RepoSearch => "repo_search",
            Self::RepoDirs => "repo_dirs",
            Self::RepoMap => "repo_map",
            Self::DirList => "dir_list",
            Self::DocRequest => "doc_request",
            Self::OuterDocRequest => "outer_doc_request",
            Self::ToolchainVerify => "toolchain_verify",
            Self::PlanCreation => "plan_creation",
            Self::PlanUpdate => "plan_update",
            Self::ReviewUpdate => "review_update",
            Self::Scratchpad => "scratchpad",
            Self::CommitMessage => "commit_message",
            Self::TurnError => "turn_error",
            Self::TurnSkip => "turn_skip",
            Self::Blocked => "blocked",
            Self::Abort => "abort",
            Self::CustomTools => "custom_tools",
            Self::ShortDescription => "short_description",
        }
    }

    /// Phase of the bounded turn this capability belongs to.
    pub const fn phase(self) -> CapabilityPhase {
        match self {
            Self::ContextRequest
            | Self::RepoSearch
            | Self::RepoDirs
            | Self::RepoMap
            | Self::DirList
            | Self::DocRequest
            | Self::OuterDocRequest => CapabilityPhase::Research,
            Self::TextMessage
            | Self::EditReplace
            | Self::EditCreate
            | Self::EditDelete
            | Self::EditMove
            | Self::EditMoveDir
            | Self::EditOverwrite
            | Self::PlanCreation
            | Self::PlanUpdate
            | Self::ReviewUpdate
            | Self::Scratchpad
            | Self::CustomTools
            | Self::ShortDescription => CapabilityPhase::Execution,
            Self::ToolchainVerify => CapabilityPhase::Verification,
            Self::CommitMessage
            | Self::TurnError
            | Self::TurnSkip
            | Self::Blocked
            | Self::Abort => CapabilityPhase::Outcome,
        }
    }

    /// How the capability's protocol output reaches the harness.
    ///
    /// Declaring delivery beside the capability keeps a single source of
    /// truth: a streaming scanner or a round-level decoder reads this property
    /// instead of special-casing capability names.
    pub const fn delivery(self) -> CapabilityDelivery {
        match self {
            Self::TextMessage
            | Self::EditReplace
            | Self::EditCreate
            | Self::EditDelete
            | Self::EditMove
            | Self::EditMoveDir
            | Self::EditOverwrite
            | Self::ShortDescription => CapabilityDelivery::LiveStreaming,
            _ => CapabilityDelivery::RoundDeferred,
        }
    }
}

impl fmt::Display for LlmCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A mutually exclusive operational branch of capabilities.
///
/// In a single round, an agent or model can operate within exactly one branch
/// (for example, either performing discovery or applying modifications),
/// enforcing mutual exclusion at the protocol and schema level. The branch
/// label is data, not a derived property: the contour that assembles the tree
/// declares `kind` explicitly, so no consumer sniffs the leaves to recover it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityBranch {
    /// Machine-readable identifier of the branch (e.g. "research", "edit").
    pub kind: String,
    /// Capability expression carried by this branch.
    pub expr: CapabilityExpr,
}

impl CapabilityBranch {
    /// Creates a new capability branch.
    pub fn new(kind: impl Into<String>, expr: CapabilityExpr) -> Self {
        Self {
            kind: kind.into(),
            expr,
        }
    }
}

/// Algebraic composition of capabilities into an operational branch tree.
///
/// A protocol surface is rarely a flat list when incompatible actions must be
/// excluded from the same round. A capability expression makes that structure
/// explicit: [`Leaf`](Self::Leaf) names one atomic capability,
/// [`Set`](Self::Set) conjoins children into one operational branch, and
/// [`AnyOf`](Self::AnyOf) declares mutually exclusive branches of a single
/// round, each carried as a [`CapabilityBranch`] with its own label beside the
/// capabilities it holds, so the JSON constrained decoder can select one branch
/// and physically exclude the properties of every other branch from the
/// produced object. The flat projection a consumer acts upon is recovered
/// through [`leaves`](Self::leaves), so a consumer that needs the flat list
/// reads it here instead of walking the tree itself.
///
/// The expression is a pure algebraic value: it carries no behavior of its own
/// and drives no turn. It states only which capabilities belong to which
/// branch, so the prompt, the response schema, and the XML decoder can read
/// one declaration instead of restating the grouping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityExpr {
    /// A single atomic capability.
    Leaf(LlmCapability),
    /// Conjunction: every child belongs to the same operational branch.
    Set(Vec<CapabilityExpr>),
    /// Disjunction of mutually exclusive branches.
    ///
    /// The label names the discriminated union the branches collapse into
    /// (for example `"action"`), while every branch carries its own
    /// [`CapabilityBranch::kind`] beside the capabilities it holds, so the
    /// JSON constrained decoder can select one branch and physically exclude
    /// the properties of every other branch from the produced object.
    AnyOf(String, Vec<CapabilityBranch>),
}

impl CapabilityExpr {
    /// Builds a leaf expression from a single capability.
    pub fn leaf(capability: LlmCapability) -> Self {
        Self::Leaf(capability)
    }

    /// Builds a conjunction over the given child expressions.
    pub fn set(items: impl IntoIterator<Item = CapabilityExpr>) -> Self {
        Self::Set(items.into_iter().collect())
    }

    /// Builds a conjunction over the given atomic capabilities.
    ///
    /// A caller that holds a flat list of capabilities — a narrowed surface, a
    /// test fixture, an ad-hoc contour — declares it as a tree without
    /// re-deriving the leaf shape by hand. The result is the same conjunction
    /// [`set`](Self::set) builds, so every consumer observes one tree shape
    /// whichever constructor produced it.
    pub fn from_leaves(capabilities: impl IntoIterator<Item = LlmCapability>) -> Self {
        Self::Set(capabilities.into_iter().map(Self::Leaf).collect())
    }

    /// Builds a disjunction of labeled branches.
    pub fn any_of(
        name: impl Into<String>,
        branches: impl IntoIterator<Item = CapabilityBranch>,
    ) -> Self {
        Self::AnyOf(name.into(), branches.into_iter().collect())
    }

    /// Reports whether the expression is a leaf.
    pub fn is_leaf(&self) -> bool {
        matches!(self, Self::Leaf(_))
    }

    /// Returns the atomic capability of a leaf expression.
    pub fn as_leaf(&self) -> Option<LlmCapability> {
        match self {
            Self::Leaf(capability) => Some(*capability),
            _ => None,
        }
    }

    /// Reports whether the tree reaches `capability` through any branch.
    pub fn contains(&self, capability: LlmCapability) -> bool {
        match self {
            Self::Leaf(leaf) => *leaf == capability,
            Self::Set(items) => items.iter().any(|item| item.contains(capability)),
            Self::AnyOf(_, branches) => branches
                .iter()
                .any(|branch| branch.expr.contains(capability)),
        }
    }

    /// Flat projection of the expression: every atomic capability the tree
    /// reaches, in first-seen depth-first order and without duplicates.
    ///
    /// This is the surface a round actually consumes, so a consumer that needs
    /// the flat list — schema validation, prompt-head composition, envelope
    /// filtering — reads it here instead of walking the tree itself. The
    /// projection is total for every shape of the tree, so an empty `Set` or an
    /// empty `AnyOf` drops out of the surface rather than fabricating a
    /// capability the tree never declared.
    pub fn leaves(&self) -> Vec<LlmCapability> {
        let mut leaves = Vec::new();
        self.walk_leaves(&mut leaves);
        leaves
    }

    pub fn has_operational_capability(&self) -> bool {
        match self {
            // A marker branch holds no leaf, yet it stays an operational
            // branch of its own, so narrowing keeps it in the tree.
            Self::Set(items) if items.is_empty() => true,
            _ => self.leaves().iter().any(|c| match c {
                LlmCapability::EditReplace
                | LlmCapability::EditCreate
                | LlmCapability::EditDelete
                | LlmCapability::EditMove
                | LlmCapability::EditMoveDir
                | LlmCapability::EditOverwrite
                | LlmCapability::ToolchainVerify
                | LlmCapability::ContextRequest
                | LlmCapability::RepoSearch
                | LlmCapability::RepoDirs
                | LlmCapability::RepoMap
                | LlmCapability::DirList
                | LlmCapability::DocRequest
                | LlmCapability::OuterDocRequest
                | LlmCapability::CustomTools
                | LlmCapability::PlanCreation
                | LlmCapability::PlanUpdate
                | LlmCapability::ReviewUpdate
                // A terminal signal owns its branch, so the branch survives
                // narrowing instead of being dropped as foliage beside the
                // capabilities that act.
                | LlmCapability::Blocked
                | LlmCapability::Abort
                // The conversational channel is operational: a branch that
                // carries only the text field still acts on the response, so
                // it survives narrowing and filtering.
                | LlmCapability::TextMessage
                | LlmCapability::ShortDescription => true,
                LlmCapability::Scratchpad
                | LlmCapability::CommitMessage
                | LlmCapability::TurnError
                | LlmCapability::TurnSkip => false,
            }),
        }
    }

    /// Returns a filtered expression retaining only capabilities that satisfy
    /// `predicate`. A retained branch keeps its kind, so a filtered tree
    /// carries the same labels as the tree it was filtered from.
    pub fn filter<F>(&self, predicate: &F) -> Option<Self>
    where
        F: Fn(LlmCapability) -> bool,
    {
        match self {
            Self::Leaf(capability) => {
                if predicate(*capability) {
                    Some(Self::Leaf(*capability))
                } else {
                    None
                }
            }
            Self::Set(items) => {
                let filtered: Vec<_> = items
                    .iter()
                    .filter_map(|item| item.filter(predicate))
                    .collect();
                // A Set that was empty before filtering stays: the answer
                // marker holds no leaf, so narrowing keeps the conversational
                // branch instead of dropping it as exhausted foliage.
                if filtered.is_empty() && !items.is_empty() {
                    None
                } else {
                    Some(Self::Set(filtered))
                }
            }
            Self::AnyOf(label, branches) => {
                let filtered: Vec<CapabilityBranch> = branches
                    .iter()
                    .filter_map(|branch| {
                        branch
                            .expr
                            .filter(predicate)
                            .map(|expr| CapabilityBranch::new(branch.kind.clone(), expr))
                    })
                    .filter(|branch| branch.expr.has_operational_capability())
                    .collect();
                if filtered.is_empty() {
                    None
                } else {
                    Some(Self::AnyOf(label.clone(), filtered))
                }
            }
        }
    }

    /// Depth-first walk that appends every leaf to `out`. Duplicates are
    /// dropped by the contains-check, so a capability reached through two
    /// branches appears only once in the projection.
    fn walk_leaves(&self, out: &mut Vec<LlmCapability>) {
        match self {
            Self::Leaf(capability) => {
                if !out.contains(capability) {
                    out.push(*capability);
                }
            }
            Self::Set(items) => {
                for item in items {
                    item.walk_leaves(out);
                }
            }
            Self::AnyOf(_, branches) => {
                for branch in branches {
                    branch.expr.walk_leaves(out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_llm_capability_serde_roundtrip() {
        let cases = [
            (LlmCapability::TextMessage, "\"text_message\""),
            (LlmCapability::EditReplace, "\"edit_replace\""),
            (LlmCapability::EditCreate, "\"edit_create\""),
            (LlmCapability::EditDelete, "\"edit_delete\""),
            (LlmCapability::EditMove, "\"edit_move\""),
            (LlmCapability::EditMoveDir, "\"edit_move_dir\""),
            (LlmCapability::EditOverwrite, "\"edit_overwrite\""),
            (LlmCapability::ContextRequest, "\"context_request\""),
            (LlmCapability::RepoSearch, "\"repo_search\""),
            (LlmCapability::RepoDirs, "\"repo_dirs\""),
            (LlmCapability::RepoMap, "\"repo_map\""),
            (LlmCapability::DirList, "\"dir_list\""),
            (LlmCapability::DocRequest, "\"doc_request\""),
            (LlmCapability::OuterDocRequest, "\"outer_doc_request\""),
            (LlmCapability::ToolchainVerify, "\"toolchain_verify\""),
            (LlmCapability::PlanCreation, "\"plan_creation\""),
            (LlmCapability::PlanUpdate, "\"plan_update\""),
            (LlmCapability::ReviewUpdate, "\"review_update\""),
            (LlmCapability::Scratchpad, "\"scratchpad\""),
            (LlmCapability::CommitMessage, "\"commit_message\""),
            (LlmCapability::TurnError, "\"turn_error\""),
            (LlmCapability::TurnSkip, "\"turn_skip\""),
            (LlmCapability::Blocked, "\"blocked\""),
            (LlmCapability::Abort, "\"abort\""),
            (LlmCapability::CustomTools, "\"custom_tools\""),
            (LlmCapability::ShortDescription, "\"short_description\""),
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
            (CapabilityPhase::Research, "\"research\""),
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
        assert_eq!(LlmCapability::ALL.len(), 26);
        for capability in LlmCapability::ALL {
            let encoded = format!("\"{}\"", capability.as_str());
            assert_eq!(serde_json::to_string(capability).unwrap(), encoded);
            assert_eq!(capability.to_string(), capability.as_str());
        }
        // A declared capability joins the advertised list once the module that
        // owns its tags, prompt text, and decoder is registered.
        assert!(LlmCapability::ALL.contains(&LlmCapability::CustomTools));

        let expectations = [
            (LlmCapability::ContextRequest, CapabilityPhase::Research),
            (LlmCapability::RepoSearch, CapabilityPhase::Research),
            (LlmCapability::RepoDirs, CapabilityPhase::Research),
            (LlmCapability::RepoMap, CapabilityPhase::Research),
            (LlmCapability::DirList, CapabilityPhase::Research),
            (LlmCapability::DocRequest, CapabilityPhase::Research),
            (LlmCapability::OuterDocRequest, CapabilityPhase::Research),
            (LlmCapability::TextMessage, CapabilityPhase::Execution),
            (LlmCapability::EditReplace, CapabilityPhase::Execution),
            (LlmCapability::EditCreate, CapabilityPhase::Execution),
            (LlmCapability::EditDelete, CapabilityPhase::Execution),
            (LlmCapability::EditMove, CapabilityPhase::Execution),
            (LlmCapability::EditMoveDir, CapabilityPhase::Execution),
            (LlmCapability::EditOverwrite, CapabilityPhase::Execution),
            (LlmCapability::PlanCreation, CapabilityPhase::Execution),
            (LlmCapability::PlanUpdate, CapabilityPhase::Execution),
            (LlmCapability::ReviewUpdate, CapabilityPhase::Execution),
            (LlmCapability::Scratchpad, CapabilityPhase::Execution),
            (LlmCapability::ShortDescription, CapabilityPhase::Execution),
            (
                LlmCapability::ToolchainVerify,
                CapabilityPhase::Verification,
            ),
            (LlmCapability::CommitMessage, CapabilityPhase::Outcome),
            (LlmCapability::TurnError, CapabilityPhase::Outcome),
            (LlmCapability::TurnSkip, CapabilityPhase::Outcome),
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
                LlmCapability::TextMessage
                | LlmCapability::EditReplace
                | LlmCapability::EditCreate
                | LlmCapability::EditDelete
                | LlmCapability::EditMove
                | LlmCapability::EditMoveDir
                | LlmCapability::EditOverwrite
                | LlmCapability::ShortDescription => CapabilityDelivery::LiveStreaming,
                _ => CapabilityDelivery::RoundDeferred,
            };
            assert_eq!(capability.delivery(), expected);
        }

        // Delivery is orthogonal to phase: the execution phase mixes streamed
        // text and edits with round-deferred plan, scratchpad, and tool
        // payloads, so neither property may be derived from the other.
        let execution: Vec<LlmCapability> = LlmCapability::ALL
            .iter()
            .copied()
            .filter(|capability| capability.phase() == CapabilityPhase::Execution)
            .collect();
        assert!(execution.iter().any(|c| c.delivery().is_live()));
        assert!(execution.iter().any(|c| !c.delivery().is_live()));

        // Discovery, verification, and outcome capabilities are never streamed.
        for capability in LlmCapability::ALL.iter().copied() {
            if capability.phase() == CapabilityPhase::Execution {
                continue;
            }
            assert_eq!(capability.delivery(), CapabilityDelivery::RoundDeferred);
        }
    }

    #[test]
    fn test_capability_expr_leaf_projection() {
        let expr = CapabilityExpr::leaf(LlmCapability::EditReplace);
        assert!(expr.is_leaf());
        assert_eq!(expr.as_leaf(), Some(LlmCapability::EditReplace));
        assert_eq!(expr.leaves().as_slice(), &[LlmCapability::EditReplace]);
        assert!(expr.contains(LlmCapability::EditReplace));
        assert!(!expr.contains(LlmCapability::TextMessage));
    }

    #[test]
    fn test_capability_expr_set_unions_children_in_first_seen_order() {
        let expr = CapabilityExpr::set([
            CapabilityExpr::leaf(LlmCapability::ContextRequest),
            CapabilityExpr::leaf(LlmCapability::EditReplace),
            CapabilityExpr::leaf(LlmCapability::CommitMessage),
        ]);
        assert_eq!(
            expr.leaves().as_slice(),
            &[
                LlmCapability::ContextRequest,
                LlmCapability::EditReplace,
                LlmCapability::CommitMessage,
            ]
        );
    }

    #[test]
    fn test_capability_expr_any_of_unions_branches_and_dedups() {
        let expr = CapabilityExpr::any_of(
            "action",
            [
                CapabilityBranch::new(
                    "research",
                    CapabilityExpr::set([
                        CapabilityExpr::leaf(LlmCapability::ContextRequest),
                        CapabilityExpr::leaf(LlmCapability::CommitMessage),
                    ]),
                ),
                CapabilityBranch::new(
                    "edit",
                    CapabilityExpr::set([
                        CapabilityExpr::leaf(LlmCapability::EditReplace),
                        CapabilityExpr::leaf(LlmCapability::CommitMessage),
                    ]),
                ),
            ],
        );
        assert_eq!(
            expr.leaves().as_slice(),
            &[
                LlmCapability::ContextRequest,
                LlmCapability::CommitMessage,
                LlmCapability::EditReplace,
            ]
        );
    }

    #[test]
    fn test_capability_expr_nested_tree_yields_flat_surface() {
        let expr = CapabilityExpr::set([
            CapabilityExpr::leaf(LlmCapability::TextMessage),
            CapabilityExpr::any_of(
                "action",
                [
                    CapabilityBranch::new(
                        "edit",
                        CapabilityExpr::set([
                            CapabilityExpr::leaf(LlmCapability::EditReplace),
                            CapabilityExpr::leaf(LlmCapability::CommitMessage),
                        ]),
                    ),
                    CapabilityBranch::new(
                        "plan",
                        CapabilityExpr::set([
                            CapabilityExpr::leaf(LlmCapability::PlanCreation),
                            CapabilityExpr::leaf(LlmCapability::CommitMessage),
                        ]),
                    ),
                ],
            ),
        ]);
        let leaves = expr.leaves();
        assert!(leaves.contains(&LlmCapability::TextMessage));
        assert!(leaves.contains(&LlmCapability::EditReplace));
        assert!(leaves.contains(&LlmCapability::PlanCreation));
        // A capability reached through two branches appears once in the
        // projection, so a consumer cannot double-count the atom.
        assert_eq!(
            leaves
                .iter()
                .filter(|c| **c == LlmCapability::CommitMessage)
                .count(),
            1
        );
    }

    #[test]
    fn test_capability_expr_empty_children_project_to_empty_surface() {
        let expr = CapabilityExpr::set(std::iter::empty::<CapabilityExpr>());
        assert!(expr.leaves().is_empty());
        let expr = CapabilityExpr::any_of("action", std::iter::empty::<CapabilityBranch>());
        assert!(expr.leaves().is_empty());
    }

    #[test]
    fn test_capability_expr_serde_roundtrip() {
        let expr = CapabilityExpr::any_of(
            "action",
            [CapabilityBranch::new(
                "research",
                CapabilityExpr::set([
                    CapabilityExpr::leaf(LlmCapability::ContextRequest),
                    CapabilityExpr::leaf(LlmCapability::CommitMessage),
                ]),
            )],
        );
        let json = serde_json::to_string(&expr).expect("expression serializes");
        let decoded: CapabilityExpr = serde_json::from_str(&json).expect("expression deserializes");
        assert_eq!(decoded, expr);
    }
}
