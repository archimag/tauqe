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
            | Self::CustomTools => CapabilityPhase::Execution,
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
            | Self::EditOverwrite => CapabilityDelivery::LiveStreaming,
            _ => CapabilityDelivery::RoundDeferred,
        }
    }
}

impl fmt::Display for LlmCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Algebraic composition of capabilities into an operational branch tree.
///
/// A protocol surface is rarely a flat list when incompatible actions must be
/// excluded from the same round. A capability tree makes that structure
/// explicit with three nodes and nothing else: [`Leaf`](Self::Leaf) names one
/// atomic capability, [`Set`](Self::Set) conjoins children into one operational
/// block, and [`AnyOf`](Self::AnyOf) declares mutually exclusive branches of a
/// single round. Every node is labeled, so the JSON constrained decoder selects
/// one branch and physically excludes the properties of every other branch from
/// the produced object.
///
/// The tree is a pure algebraic value: it carries no behavior of its own and
/// drives no turn. It states only which capabilities belong to which branch, so
/// the prompt, the response schema, and the decoder read one declaration instead
/// of restating the grouping. The flat projection a consumer acts upon is
/// recovered through [`leaves`](Self::leaves), and a surface assembled from more
/// than one declaration is reconciled through [`merge`](Self::merge).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityTree {
    /// A single atomic capability.
    Leaf(LlmCapability),
    /// A block of capabilities: the conjunction of its children.
    ///
    /// The label names the block. An empty label is the transparent root
    /// conjunction built by [`root`](Self::root): its children take part in the
    /// level that holds them. A named label is the block built by
    /// [`named_set`](Self::named_set): placed inside an [`AnyOf`](Self::AnyOf)
    /// it becomes the variant that label discriminates, so a branch states its
    /// own name and no consumer recovers it from the leaves.
    Set(String, Vec<CapabilityTree>),
    /// Disjunction of mutually exclusive branches.
    ///
    /// The label names the discriminated union the branches collapse into (for
    /// example `"action"`), while every branch carries its own label, so the
    /// JSON constrained decoder selects one branch and physically excludes the
    /// properties of every other branch from the produced object.
    AnyOf(String, Vec<CapabilityTree>),
}
impl CapabilityTree {
    /// Builds a leaf expression from a single capability.
    pub fn leaf(capability: LlmCapability) -> Self {
        Self::Leaf(capability)
    }

    /// Builds the transparent root conjunction over the given children.
    ///
    /// The block carries the empty label, so its children take part in the level
    /// that holds them instead of forming a branch of their own.
    pub fn root(items: impl IntoIterator<Item = CapabilityTree>) -> Self {
        Self::Set(String::new(), items.into_iter().collect())
    }

    /// Builds an unnamed conjunction over the given child expressions.
    ///
    /// The block is the same transparent root [`root`](Self::root) declares, kept
    /// under the spelling the contours use while their phase trees are assembled
    /// from the shared branches.
    pub fn set(items: impl IntoIterator<Item = CapabilityTree>) -> Self {
        Self::root(items)
    }

    /// Builds a labeled block of capabilities.
    ///
    /// The label names the block, so placing it inside an
    /// [`AnyOf`](Self::AnyOf) turns it into the variant the label
    /// discriminates.
    pub fn named_set(
        label: impl Into<String>,
        items: impl IntoIterator<Item = CapabilityTree>,
    ) -> Self {
        Self::Set(label.into(), items.into_iter().collect())
    }

    /// Builds a disjunction of labeled branches.
    pub fn any_of(
        label: impl Into<String>,
        branches: impl IntoIterator<Item = CapabilityTree>,
    ) -> Self {
        Self::AnyOf(label.into(), branches.into_iter().collect())
    }

    /// Builds a conjunction over the given atomic capabilities.
    ///
    /// A caller that holds a flat list of capabilities — a narrowed surface, a
    /// test fixture, an ad-hoc contour — declares it as a tree without
    /// re-deriving the leaf shape by hand. The result is the same conjunction
    /// [`root`](Self::root) builds, so every consumer observes one tree shape
    /// whichever constructor produced it.
    pub fn from_leaves(capabilities: impl IntoIterator<Item = LlmCapability>) -> Self {
        Self::root(capabilities.into_iter().map(Self::Leaf))
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

    /// Label of the node.
    ///
    /// A [`Set`](Self::Set) reports the name of its block, which is empty for the
    /// transparent root conjunction, and an [`AnyOf`](Self::AnyOf) reports the
    /// name of its union. A leaf carries no label, so the branch name is read
    /// here instead of being sniffed from the leaves.
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Leaf(_) => None,
            Self::Set(label, _) | Self::AnyOf(label, _) => Some(label.as_str()),
        }
    }

    /// Reports whether the tree reaches `capability` through any branch.
    pub fn contains(&self, capability: LlmCapability) -> bool {
        match self {
            Self::Leaf(leaf) => *leaf == capability,
            Self::Set(_, items) => items.iter().any(|item| item.contains(capability)),
            Self::AnyOf(_, branches) => branches.iter().any(|branch| branch.contains(capability)),
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
            Self::Set(_, items) if items.is_empty() => true,
            _ => self.leaves().iter().any(|c| match c {
                LlmCapability::EditReplace
                | LlmCapability::EditCreate
                | LlmCapability::EditDelete
                | LlmCapability::EditMove
                | LlmCapability::EditMoveDir
                | LlmCapability::EditOverwrite
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
                | LlmCapability::TextMessage => true,
                LlmCapability::Scratchpad
                | LlmCapability::CommitMessage
                | LlmCapability::TurnError
                | LlmCapability::TurnSkip => false,
            }),
        }
    }

    /// Returns a filtered expression retaining only capabilities that satisfy
    /// `predicate`. A retained node keeps its label, so a filtered tree carries
    /// the same names as the tree it was filtered from.
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
            Self::Set(label, items) => {
                let filtered: Vec<_> = items
                    .iter()
                    .filter_map(|item| item.filter(predicate))
                    .collect();
                // A block that was empty before filtering stays: the answer
                // marker holds no leaf, so narrowing keeps the conversational
                // branch instead of dropping it as exhausted foliage.
                if filtered.is_empty() && !items.is_empty() {
                    None
                } else {
                    Some(Self::Set(label.clone(), filtered))
                }
            }
            Self::AnyOf(label, branches) => {
                let filtered: Vec<CapabilityTree> = branches
                    .iter()
                    .filter_map(|branch| branch.filter(predicate))
                    .filter(|branch| branch.has_operational_capability())
                    .collect();
                if filtered.is_empty() {
                    None
                } else {
                    Some(Self::AnyOf(label.clone(), filtered))
                }
            }
        }
    }

    /// Merges `other` into this tree, preserving every capability both declare.
    ///
    /// The operation reconciles the two directions a surface is assembled from: a
    /// contour declares its phase as a tree, and the loop that runs the phase
    /// enriches that declaration with the surface it owns. The merge is
    /// idempotent — merging the same tree into a surface that already carries it
    /// yields the surface unchanged — so a surface composed more than once
    /// renders the same bytes, the prompt head stays stable inside a turn, and
    /// the provider prefix cache of the following round is never invalidated.
    pub fn merge(&self, other: &Self) -> Self {
        if self == other {
            return self.clone();
        }
        match (self, other) {
            (Self::Set(left, ours), Self::Set(right, theirs)) if left == right => Self::Set(
                left.clone(),
                Self::merge_children(ours.clone(), theirs.iter().cloned()),
            ),
            (Self::AnyOf(left, ours), Self::AnyOf(right, theirs)) if left == right => Self::AnyOf(
                left.clone(),
                Self::merge_children(ours.clone(), theirs.iter().cloned()),
            ),
            (Self::Set(label, ours), _) => Self::Set(
                label.clone(),
                Self::merge_children(ours.clone(), std::iter::once(other.clone())),
            ),
            _ => Self::root([self.clone(), other.clone()]),
        }
    }

    /// Folds `incoming` into `children`, merging a node that names the same
    /// block as one already present instead of appending it twice.
    fn merge_children(
        mut children: Vec<Self>,
        incoming: impl IntoIterator<Item = Self>,
    ) -> Vec<Self> {
        for child in incoming {
            if children.contains(&child) {
                continue;
            }
            match children
                .iter()
                .position(|held| held.identity() == child.identity())
            {
                Some(index) => {
                    let merged = children[index].merge(&child);
                    children[index] = merged;
                }
                None => children.push(child),
            }
        }
        children
    }

    /// Identity of the node within one level: two nodes with the same identity
    /// name the same block, capability, or union and are therefore merged
    /// instead of appended side by side.
    fn identity(&self) -> (&'static str, &str) {
        match self {
            Self::Leaf(capability) => ("leaf", capability.as_str()),
            Self::Set(label, _) => ("set", label.as_str()),
            Self::AnyOf(label, _) => ("any_of", label.as_str()),
        }
    }

    /// Depth-first walk that appends every leaf to `out`. Duplicates are dropped
    /// by the contains-check, so a capability reached through two branches
    /// appears only once in the projection.
    fn walk_leaves(&self, out: &mut Vec<LlmCapability>) {
        match self {
            Self::Leaf(capability) => {
                if !out.contains(capability) {
                    out.push(*capability);
                }
            }
            Self::Set(_, items) => {
                for item in items {
                    item.walk_leaves(out);
                }
            }
            Self::AnyOf(_, branches) => {
                for branch in branches {
                    branch.walk_leaves(out);
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
        assert_eq!(LlmCapability::ALL.len(), 24);
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
                | LlmCapability::EditOverwrite => CapabilityDelivery::LiveStreaming,
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
        let expr = CapabilityTree::leaf(LlmCapability::EditReplace);
        assert!(expr.is_leaf());
        assert_eq!(expr.as_leaf(), Some(LlmCapability::EditReplace));
        assert_eq!(expr.leaves().as_slice(), &[LlmCapability::EditReplace]);
        assert!(expr.contains(LlmCapability::EditReplace));
        assert!(!expr.contains(LlmCapability::TextMessage));
    }

    #[test]
    fn test_capability_expr_set_unions_children_in_first_seen_order() {
        let expr = CapabilityTree::root([
            CapabilityTree::leaf(LlmCapability::ContextRequest),
            CapabilityTree::leaf(LlmCapability::EditReplace),
            CapabilityTree::leaf(LlmCapability::CommitMessage),
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
        let expr = CapabilityTree::any_of(
            "action",
            [
                CapabilityTree::named_set(
                    "research",
                    [
                        CapabilityTree::leaf(LlmCapability::ContextRequest),
                        CapabilityTree::leaf(LlmCapability::CommitMessage),
                    ],
                ),
                CapabilityTree::named_set(
                    "edit",
                    [
                        CapabilityTree::leaf(LlmCapability::EditReplace),
                        CapabilityTree::leaf(LlmCapability::CommitMessage),
                    ],
                ),
            ],
        );
        assert_eq!(expr.label(), Some("action"));
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
        let expr = CapabilityTree::root([
            CapabilityTree::leaf(LlmCapability::TextMessage),
            CapabilityTree::any_of(
                "action",
                [
                    CapabilityTree::named_set(
                        "edit",
                        [
                            CapabilityTree::leaf(LlmCapability::EditReplace),
                            CapabilityTree::leaf(LlmCapability::CommitMessage),
                        ],
                    ),
                    CapabilityTree::named_set(
                        "plan",
                        [
                            CapabilityTree::leaf(LlmCapability::PlanCreation),
                            CapabilityTree::leaf(LlmCapability::CommitMessage),
                        ],
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
        let expr = CapabilityTree::root(std::iter::empty::<CapabilityTree>());
        assert!(expr.leaves().is_empty());
        let expr = CapabilityTree::any_of("action", std::iter::empty::<CapabilityTree>());
        assert!(expr.leaves().is_empty());
    }
    #[test]
    fn test_capability_expr_serde_roundtrip() {
        let expr = CapabilityTree::any_of(
            "action",
            [CapabilityTree::named_set(
                "research",
                [
                    CapabilityTree::leaf(LlmCapability::ContextRequest),
                    CapabilityTree::leaf(LlmCapability::CommitMessage),
                ],
            )],
        );
        let json = serde_json::to_string(&expr).expect("expression serializes");
        let decoded: CapabilityTree = serde_json::from_str(&json).expect("expression deserializes");
        assert_eq!(decoded, expr);
    }

    #[test]
    fn test_capability_expr_labels_and_idempotent_merge() {
        let root = CapabilityTree::root([CapabilityTree::leaf(LlmCapability::ContextRequest)]);
        assert_eq!(root.label(), Some(""));
        let block = CapabilityTree::named_set(
            "research",
            [CapabilityTree::leaf(LlmCapability::RepoSearch)],
        );
        assert_eq!(block.label(), Some("research"));
        assert_eq!(
            CapabilityTree::leaf(LlmCapability::TextMessage).label(),
            None
        );

        // A merge never repeats a capability the parent already carries, so a
        // surface merged more than once renders the bytes it rendered before.
        let merged = root.merge(&CapabilityTree::leaf(LlmCapability::CommitMessage));
        assert_eq!(
            merged.merge(&CapabilityTree::leaf(LlmCapability::CommitMessage)),
            merged
        );
        assert!(merged.contains(LlmCapability::ContextRequest));
        assert!(merged.contains(LlmCapability::CommitMessage));

        // A labeled branch keeps its name, and merging it into a tree that
        // already holds it is a no-op.
        let with_branch = root.merge(&block);
        assert_eq!(with_branch.merge(&block), with_branch);
        assert_eq!(
            with_branch.leaves(),
            vec![LlmCapability::ContextRequest, LlmCapability::RepoSearch]
        );
    }
}
