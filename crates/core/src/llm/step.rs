//! Model-facing capabilities for one isolated Coder step.

use super::repl::{abort_branch, blocked_branch, edit_branch, research_branch};
use super::{CapabilityTree, LlmCapability};

/// An isolated step can report progress and results through the ambient message
/// but cannot complete by answering in prose or authoring a plan.
pub fn turn_capability_expr() -> CapabilityTree {
    let ambient = CapabilityTree::root([
        CapabilityTree::leaf(LlmCapability::Scratchpad),
        CapabilityTree::leaf(LlmCapability::TextMessage),
    ]);
    let branches = [
        research_branch(false),
        edit_branch(),
        blocked_branch(),
        abort_branch(),
    ];
    CapabilityTree::root([ambient, CapabilityTree::any_of("action", branches)])
}
