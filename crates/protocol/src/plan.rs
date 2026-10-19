//! Project plan domain model exposed behind a single facade.
//!
//! The plan subsystem is decomposed into focused submodules: the graph leaf
//! types (`item`), the plan aggregate with deterministic status computation
//! (`node`), scope scheduling (`scope`), Markdown rendering (`render`), and the
//! wire DTOs (`dto`). Every public item is re-exported here through an explicit
//! list, so external paths such as `tauqe_protocol::plan::Plan` remain stable
//! and an accidental name collision between two submodules becomes a compile
//! error instead of a silently ambiguous glob. Keeping the submodules private
//! avoids leaking their names onto the crate root.

mod dto;
mod item;
mod node;
mod render;
mod scope;

#[cfg(test)]
mod tests;

pub use dto::{
    PlanClearHorizonParams, PlanClearHorizonResult, PlanDeleteParams, PlanExecuteParams,
    PlanExecutePipelineParams, PlanGetParams, PlanGetResult, PlanListChangedEvent, PlanListResult,
    PlanRefineParams, PlanRefineResult, PlanReplanParams, PlanSaveParams, PlanSetActiveParams,
    PlanSetActiveResult, PlanUpdateItemParams, PlanUpdateItemResult, PlanUpdatedEvent,
};
pub use item::{ConceptItem, PlanItem, PlanItemStatus};
pub use node::{compute_items_status, Plan, PlanStats, PlanSummary, PlanValidationError};

// The leaf-progress counter is an internal building block shared by the plan
// and review modules only. It stays crate-visible so a subplan and a full plan
// keep one authoritative metric, and it is deliberately absent from the crate
// root, so it never becomes part of the public wire surface.
pub(crate) use node::count_leaf_stats;
pub use scope::PlanScopeEvaluation;
