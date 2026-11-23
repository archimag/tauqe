//! Declarative model-facing dialogue capabilities shared with legacy callers.

use crate::llm::{CapabilityTree, LlmCapability};

/// Builds the algebraic capability expression for an interactive dialogue.
pub(crate) fn repl_capability_expr(
    focus: Option<&crate::discussion::DiscussionTarget>,
) -> CapabilityTree {
    let (plan_update, review_decision) = match focus {
        Some(crate::discussion::DiscussionTarget::Plan { .. }) => (true, false),
        Some(crate::discussion::DiscussionTarget::Review { .. })
        | Some(crate::discussion::DiscussionTarget::ReviewGroup { .. }) => (false, true),
        None => (false, false),
    };
    dialogue_capability_expr(plan_update, review_decision)
}

/// Composes the mutually exclusive action branches beside ambient text and scratchpad.
pub(crate) fn dialogue_capability_expr(plan_update: bool, review_decision: bool) -> CapabilityTree {
    let ambient = CapabilityTree::root([
        CapabilityTree::leaf(LlmCapability::Scratchpad),
        CapabilityTree::leaf(LlmCapability::TextMessage),
    ]);
    let plan_capabilities = if plan_update {
        CapabilityTree::any_of(
            "plan_action",
            [
                CapabilityTree::named_set(
                    "create",
                    [CapabilityTree::leaf(LlmCapability::PlanCreation)],
                ),
                CapabilityTree::named_set(
                    "modify",
                    [CapabilityTree::leaf(LlmCapability::PlanUpdate)],
                ),
            ],
        )
    } else {
        CapabilityTree::leaf(LlmCapability::PlanCreation)
    };
    let plan = CapabilityTree::named_set(
        "plan",
        [
            plan_capabilities,
            CapabilityTree::leaf(LlmCapability::CommitMessage),
            CapabilityTree::leaf(LlmCapability::TurnError),
            CapabilityTree::leaf(LlmCapability::TurnSkip),
        ],
    );
    let mut branches = vec![
        research_branch(true),
        edit_branch(),
        plan,
        CapabilityTree::named_set("answer", std::iter::empty::<CapabilityTree>()),
        blocked_branch(),
        abort_branch(),
    ];
    if review_decision {
        branches.push(CapabilityTree::named_set(
            "review",
            [
                CapabilityTree::leaf(LlmCapability::ReviewUpdate),
                CapabilityTree::leaf(LlmCapability::CommitMessage),
                CapabilityTree::leaf(LlmCapability::TurnError),
                CapabilityTree::leaf(LlmCapability::TurnSkip),
            ],
        ));
    }
    CapabilityTree::root([ambient, CapabilityTree::any_of("action", branches)])
}

/// Discovery capabilities shared by dialogue and isolated step contours.
pub(crate) fn research_branch(with_docs: bool) -> CapabilityTree {
    let mut capabilities = vec![
        LlmCapability::ContextRequest,
        LlmCapability::RepoSearch,
        LlmCapability::RepoDirs,
        LlmCapability::RepoMap,
        LlmCapability::DirList,
    ];
    if with_docs {
        capabilities.push(LlmCapability::DocRequest);
    }
    capabilities.push(LlmCapability::OuterDocRequest);
    capabilities.push(LlmCapability::CustomTools);
    CapabilityTree::named_set(
        "research",
        capabilities.into_iter().map(CapabilityTree::leaf),
    )
}

/// Edit branch shared by dialogue and isolated step contours.
pub(crate) fn edit_branch() -> CapabilityTree {
    CapabilityTree::named_set(
        "edit",
        [
            LlmCapability::EditReplace,
            LlmCapability::EditCreate,
            LlmCapability::EditDelete,
            LlmCapability::EditMove,
            LlmCapability::EditMoveDir,
            LlmCapability::EditOverwrite,
            LlmCapability::CommitMessage,
            LlmCapability::TurnError,
            LlmCapability::TurnSkip,
        ]
        .map(CapabilityTree::leaf),
    )
}

pub(crate) fn blocked_branch() -> CapabilityTree {
    CapabilityTree::named_set("blocked", [CapabilityTree::leaf(LlmCapability::Blocked)])
}

pub(crate) fn abort_branch() -> CapabilityTree {
    CapabilityTree::named_set("abort", [CapabilityTree::leaf(LlmCapability::Abort)])
}

pub fn repl_capabilities(focus: Option<&crate::discussion::DiscussionTarget>) -> CapabilityTree {
    repl_capability_expr(focus)
}

/// A refused document may repair only the operation it originally declared.
pub fn plan_correction_capabilities(operation: LlmCapability) -> CapabilityTree {
    assert!(matches!(
        operation,
        LlmCapability::PlanCreation | LlmCapability::PlanUpdate
    ));
    CapabilityTree::root([
        CapabilityTree::leaf(LlmCapability::Scratchpad),
        CapabilityTree::leaf(LlmCapability::TextMessage),
        CapabilityTree::any_of(
            "action",
            [
                CapabilityTree::named_set("plan", [CapabilityTree::leaf(operation)]),
                blocked_branch(),
                abort_branch(),
            ],
        ),
    ])
}
