use serde::{Deserialize, Serialize};

use super::item::PlanItemStatus;
use super::node::Plan;
use crate::model::ModelSelection;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanGetParams {
    /// Absent id means "the active plan": the client requests the current
    /// plan with an empty object, so the DTO — not a server-side fallback —
    /// defines the empty payload.
    #[serde(default)]
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanGetResult {
    pub plan: Option<Plan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanListResult {
    pub plans: Vec<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSaveParams {
    pub plan: Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanUpdateItemParams {
    pub plan_id: String,
    pub item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PlanItemStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanUpdateItemResult {
    pub plan: Plan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDeleteParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanSetActiveParams {
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanSetActiveResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanClearHorizonParams {
    pub plan_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanClearHorizonResult {
    pub plan: Plan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanExecutePipelineParams {
    #[serde(default)]
    pub plan_ids: Vec<String>,
    /// Coder selection the pipeline starts from.
    ///
    /// It carries the same untagged semantics as `ReviewExecuteParams::model`:
    /// a semantic role, an explicit model reference, or an explicit role-list
    /// rung with the reasoning depth of the configured entry the developer
    /// picked. An absent selection starts the coder ladder at its base rung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanExecuteParams {
    pub plan_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// Coder selection the execution starts from.
    ///
    /// It carries the same untagged semantics as `ReviewExecuteParams::model`,
    /// so the model together with its reasoning depth travels on the wire and
    /// the server never re-derives the depth by matching the model against the
    /// configured coder entries. An absent selection starts the coder ladder
    /// at its base rung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
}

/// Parameters of the two-phase plan creation pipeline (`plan/createPipeline`).
///
/// The pipeline drafts a plan from the directives, has the draft audited by an
/// independent Architect entry, and reconciles the critique into the plan it
/// issues. Both Architect selections travel on the request, so one typed dialog
/// configures the two phases instead of delegating them to an unstructured
/// dialogue, and the critique can be opted out of explicitly. The request
/// answers with [`PlanRefineResult`], exactly as the directed modification
/// does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanCreatePipelineParams {
    /// Engineering directives the synthesized plan must satisfy.
    ///
    /// This is the mandatory coordinate of the pipeline: a request without
    /// directives has nothing to plan, so the key is required and the handler
    /// rejects a blank value instead of drafting an empty plan.
    pub directives: String,
    /// Explicit Architect selection of the drafting phase.
    ///
    /// An absent selection leaves the phase to the runtime default instead of
    /// pinning a model on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner_model: Option<ModelSelection>,
    /// Explicit Architect selection of the critique phase.
    ///
    /// The critic is selected independently of the planner, so the draft can be
    /// audited by a different Architect entry than the one that wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critic_model: Option<ModelSelection>,
    /// Whether the independent critique phase is skipped.
    ///
    /// The flag is independent of `critic_model`: an opt-out expresses the
    /// single-phase intent and never depends on which critic was selected.
    #[serde(default)]
    pub skip_critique: bool,
}

/// Parameters of a directed plan modification (`plan/refine`).
///
/// The request carries the same two-phase selection the creation pipeline
/// declares: the instructions state how the concept and the steps must change,
/// the planner drafts and reconciles, and the critic audits the draft
/// independently. The synthesis answers with the mutated plan or a blocker
/// ([`PlanRefineResult`]) rather than with prose, so a modification always
/// lands as a structure change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanRefineParams {
    pub plan_id: String,
    /// Explicit Architect selection of the drafting phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    /// Explicit Architect selection of the critique phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critic_model: Option<ModelSelection>,
    /// Whether the independent critique phase is skipped.
    #[serde(default)]
    pub skip_critique: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanReplanParams {
    pub plan_id: String,
    pub step_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Outcome of a structured plan synthesis issued by the Architect contour.
///
/// The directed modification (`plan/refine`) and the two-phase creation
/// pipeline (`plan/createPipeline`) report through this one shape: the
/// synthesis either issues the plan together with its reconciliation note, or
/// states why it cannot be issued instead of answering with prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PlanRefineResult {
    Success {
        plan: Plan,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
    },
    Blocked {
        reason: String,
    },
}

impl PlanRefineResult {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub fn plan(&self) -> Option<&Plan> {
        match self {
            Self::Success { plan, .. } => Some(plan),
            Self::Blocked { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Success { .. } => None,
            Self::Blocked { reason } => Some(reason),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanUpdatedEvent {
    pub plan: Plan,
    /// Whether this update created or fully replaced the plan (a `save` action
    /// of the Architect) rather than modifying it incrementally. Clients use
    /// the flag to focus a freshly created plan; an incremental update reports
    /// `false`.
    #[serde(default)]
    pub is_save: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanListChangedEvent {
    pub plans: Vec<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}
