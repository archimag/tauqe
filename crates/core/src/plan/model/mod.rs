//! Owned plan documents and authoritative graph rules.
mod implementation;
mod item;
mod node;
mod rejection;
mod render;
pub use implementation::{PlanImplementation, PlanScopeEvaluation};
pub use item::{ConceptItem, PlanItem, PlanItemStatus};
pub use node::{Plan, PlanSummary, PlanValidationError};
pub use rejection::{PlanRejection, PlanRejectionKind};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanSynthesisResult {
    Success {
        plan: Plan,
        explanation: Option<String>,
    },
    Blocked {
        reason: String,
    },
}

mod update;
pub use update::{DiscussionPlanAction, DiscussionPlanUpdate};

#[cfg(test)]
mod lifecycle_tests;
