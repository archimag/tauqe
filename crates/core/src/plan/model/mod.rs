//! Owned plan documents and authoritative graph rules.
mod implementation;
mod node;
mod rejection;
pub use crate::plan_item::{ConceptItem, PlanItem, PlanItemStatus};
pub use implementation::{PlanImplementation, PlanScopeEvaluation};
pub(crate) use node::PlanValidationError;
pub use node::{Plan, PlanSummary};
pub use rejection::{PlanRejection, PlanRejectionKind};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanSynthesisResult {
    Success {
        plan: Box<Plan>,
        explanation: Option<String>,
    },
    Blocked {
        reason: String,
    },
}

#[cfg(test)]
mod lifecycle_tests;
