//! Project plan domain model exposed behind a single facade.
//!
//! The plan subsystem is decomposed into focused submodules: the graph leaf
//! types (`item`), the plan aggregate and its document header (`node`), the
//! execution graph that every implementation horizon shares
//! (`implementation`), Markdown rendering (`render`), and the wire DTOs
//! (`dto`). Every public item is re-exported here through an explicit
//! list, so external paths such as `tauqe_protocol::plan::Plan` remain stable
//! and an accidental name collision between two submodules becomes a compile
//! error instead of a silently ambiguous glob. Keeping the submodules private
//! avoids leaking their names onto the crate root.

mod dto;
mod implementation;
mod item;
mod node;
mod render;

#[cfg(test)]
mod tests;

pub use dto::{
    PlanClearHorizonParams, PlanClearHorizonResult, PlanDeleteParams, PlanExecuteParams,
    PlanExecutePipelineParams, PlanGetParams, PlanGetResult, PlanListChangedEvent, PlanListResult,
    PlanRefineParams, PlanRefineResult, PlanReplanParams, PlanSaveParams, PlanSetActiveParams,
    PlanSetActiveResult, PlanUpdateItemParams, PlanUpdateItemResult, PlanUpdatedEvent,
};
pub use implementation::{compute_items_status, PlanImplementation, PlanScopeEvaluation};
pub use item::{ConceptItem, PlanItem, PlanItemStatus};
pub use node::{Plan, PlanStats, PlanSummary, PlanValidationError};
