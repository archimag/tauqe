//! Model, role, effort, and turn-lifecycle wire contracts.
//!
//! The model subsystem is decomposed into focused submodules: provider and
//! model references (`reference`), semantic roles (`role`), reasoning effort
//! (`effort`), configured entries and coder tiers (`entry`), selection DTOs
//! (`selection`), turn parameters and lifecycle events (`turn`), usage
//! accounting (`usage`), and batch completion summaries (`batch`). Every public
//! item is re-exported here through an explicit list, so external paths such as
//! `tauqe_protocol::model::ModelRef` remain stable and an accidental name
//! collision between two submodules becomes a compile error instead of a
//! silently ambiguous glob. Keeping the submodules private avoids leaking their
//! names onto the crate root.

mod batch;
mod effort;
mod entry;
mod reference;
mod role;
mod selection;
mod turn;
mod usage;

pub use batch::{
    BatchFinishStatus, BatchFinishSummary, PlanBatchFinishedEvent, ReviewBatchFinishedEvent,
};
pub use effort::{ModelRolesEffort, ReasoningEffort};
pub use entry::{
    ArchitectTier, CoderTier, ModelEntry, ModelRolesEntries, ModelRolesSummary,
};
pub use reference::{deserialize_model_ref_lenient, ModelRef, ProviderKind};
pub use role::ModelRole;
pub use selection::{
    ModelChangedEvent, ModelCurrentResult, ModelSelectParams, ModelSelectResult, ModelSelection,
};
pub use turn::{
    CancelMode, ModelAskParams, ModelCancelParams, ModelDeltaEvent, ModelErrorEvent,
    ModelFinishedEvent, ModelResult, ModelResultEvent, ModelResultProposal, ModelStartedEvent,
    ModelTurnPlanContext, RepoMapInfoEvent, ToolchainFinishedEvent, ToolchainStartedEvent,
    TurnPhase, TurnPhaseEvent,
};
pub use usage::{
    ModelUsageEvent, ModelUsageInfo, ModelUsageTotalsEvent, UsageTotals, UsageTotalsSnapshot,
};
