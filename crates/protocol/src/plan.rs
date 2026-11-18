//! Plan RPC documents and request/result/event DTOs. Client projections live in TUI.

mod dto;
mod implementation;
mod item;
mod node;
mod rejection;

pub use dto::{
    PlanClearHorizonParams, PlanClearHorizonResult, PlanCreatePipelineParams, PlanDeleteParams,
    PlanExecuteParams, PlanExecutePipelineParams, PlanGetParams, PlanGetResult,
    PlanListChangedEvent, PlanListResult, PlanRefineParams, PlanRefineResult, PlanReplanParams,
    PlanSaveParams, PlanSetActiveParams, PlanUpdateItemParams, PlanUpdatedEvent,
};

pub use implementation::PlanImplementation;
pub use item::{ConceptItem, PlanItem, PlanItemStatus};

pub use node::{Plan, PlanSummary};
pub use rejection::{PlanRejection, PlanRejectionKind};

#[cfg(test)]
mod tests;
