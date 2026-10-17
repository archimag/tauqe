use serde::{Deserialize, Serialize};

use super::item::PlanItemStatus;
use super::node::Plan;
use crate::model::ModelSelection;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanGetParams {
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
pub struct PlanExecuteParams {
    pub plan_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanRefineParams {
    pub plan_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

// The `id` and `item_id` aliases on this RPC parameter type are legacy wire
// names retained so older clients keep working; scheduled for removal in 0.2.0
// and tracked by the wire-contract test in `lib.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanReplanParams {
    #[serde(alias = "id")]
    pub plan_id: String,
    #[serde(alias = "item_id")]
    pub step_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

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
