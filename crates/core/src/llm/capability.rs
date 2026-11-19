//! Operational capability trees for schema composition and decoding.

use crate::llm::LlmCapability;
use serde::{Deserialize, Serialize};

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
/// recovered through [`leaves`](Self::leaves).
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
    pub(crate) fn root(items: impl IntoIterator<Item = CapabilityTree>) -> Self {
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
    pub(crate) fn named_set(
        label: impl Into<String>,
        items: impl IntoIterator<Item = CapabilityTree>,
    ) -> Self {
        Self::Set(label.into(), items.into_iter().collect())
    }

    /// Builds a disjunction of labeled branches.
    pub(crate) fn any_of(
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
    pub(crate) fn from_leaves(capabilities: impl IntoIterator<Item = LlmCapability>) -> Self {
        Self::root(capabilities.into_iter().map(Self::Leaf))
    }

    /// Label of the node.
    ///
    /// A [`Set`](Self::Set) reports the name of its block, which is empty for the
    /// transparent root conjunction, and an [`AnyOf`](Self::AnyOf) reports the
    /// name of its union. A leaf carries no label, so the branch name is read
    /// here instead of being sniffed from the leaves.
    pub(crate) fn label(&self) -> Option<&str> {
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
    pub(crate) fn leaves(&self) -> Vec<LlmCapability> {
        let mut leaves = Vec::new();
        self.walk_leaves(&mut leaves);
        leaves
    }

    #[cfg(test)]
    fn has_operational_capability(&self) -> bool {
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
    #[cfg(test)]
    pub(crate) fn filter<F>(&self, predicate: &F) -> Option<Self>
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
    fn test_capability_expr_leaf_projection() {
        let expr = CapabilityTree::leaf(LlmCapability::EditReplace);
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
}
