use serde::{Deserialize, Serialize};

use crate::discussion::DiscussionRequest;
use crate::edit::{EditOperation, StructuredChangeProposal};

use super::effort::ReasoningEffort;
use super::reference::ModelRef;
use super::role::ModelRole;
use super::usage::ModelUsageInfo;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelAskParams {
    /// Free-form prompt for a plain interactive turn.
    ///
    /// Empty when the request carries a structured discussion target instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    /// Explicit discussion target.
    ///
    /// When present, the server generates the discussion prompt from these typed
    /// fields instead of relying on an in-band marker inside user-controlled
    /// prompt text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discussion: Option<DiscussionRequest>,
}

/// How an in-flight cancellation should stop a running batch supervisor.
///
/// A soft stop lets the current member finish its bounded turn, verification,
/// and Git commit before the queue halts; a hard stop aborts the current member
/// immediately. Outside a batch the mode is irrelevant and cancellation is
/// always immediate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelMode {
    /// Finish the current member, then stop without starting the next one.
    Soft,
    /// Abort the current member immediately and stop the batch.
    #[default]
    Hard,
}

/// Parameters for `model/cancel`.
///
/// The `mode` field is optional so clients that predate two-mode cancellation
/// keep sending an empty object and receive the default immediate abort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCancelParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CancelMode>,
}

/// Execution context of a model turn driven by a plan step.
///
/// Carries the owning plan identifier, the currently executing step, and the
/// compact ordered list of step identifiers that form the batch scope. This
/// allows clients to render a live plan stepper without switching views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelTurnPlanContext {
    pub plan_id: String,
    pub step_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub batch_step_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStartedEvent {
    pub operation_id: String,
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ModelRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_context: Option<ModelTurnPlanContext>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_batch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDeltaEvent {
    pub operation_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelResultProposal {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub changes: Vec<StructuredChangeProposal>,
    #[serde(default)]
    pub context_requests: Vec<String>,
    #[serde(default)]
    pub context_drops: Vec<String>,
    #[serde(default)]
    pub suggested_actions: Vec<String>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelResult {
    Answer {
        text: String,
    },
    Edit {
        summary: String,
        edits: Vec<EditOperation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal: Option<ModelResultProposal>,
        applied: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default)]
        changed_files: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit_hash: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResultEvent {
    pub operation_id: String,
    pub result: ModelResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ModelUsageInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_total_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFinishedEvent {
    pub operation_id: String,
    pub full_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelErrorEvent {
    pub operation_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Discovery,
    Proposal,
    Staging,
    Verification,
    Healing,
}

impl std::fmt::Display for TurnPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnPhase::Discovery => f.write_str("discovery"),
            TurnPhase::Proposal => f.write_str("proposal"),
            TurnPhase::Staging => f.write_str("staging"),
            TurnPhase::Verification => f.write_str("verification"),
            TurnPhase::Healing => f.write_str("healing"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPhaseEvent {
    pub operation_id: String,
    pub phase: TurnPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMapInfoEvent {
    pub operation_id: String,
    pub tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolchainStartedEvent {
    pub operation_id: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolchainFinishedEvent {
    pub operation_id: String,
    pub command: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_result_answer_serde() {
        let answer = ModelResult::Answer {
            text: "Hello, world!".to_string(),
        };
        let json = serde_json::to_string(&answer).unwrap();
        assert!(json.contains("\"kind\":\"answer\""));

        let de: ModelResult = serde_json::from_str(&json).unwrap();
        assert_eq!(de, answer);
    }

    #[test]
    fn test_cancel_mode_and_model_cancel_params_serde() {
        assert_eq!(CancelMode::default(), CancelMode::Hard);
        assert_eq!(serde_json::to_string(&CancelMode::Soft).unwrap(), "\"soft\"");
        assert_eq!(serde_json::to_string(&CancelMode::Hard).unwrap(), "\"hard\"");
        assert_eq!(
            serde_json::from_str::<CancelMode>("\"soft\"").unwrap(),
            CancelMode::Soft
        );

        // Legacy clients send an empty object; the mode stays absent and the
        // server falls back to an immediate abort.
        let legacy: ModelCancelParams = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.mode, None);
        assert_eq!(serde_json::to_string(&legacy).unwrap(), "{}");

        let soft = ModelCancelParams {
            mode: Some(CancelMode::Soft),
        };
        let json = serde_json::to_string(&soft).unwrap();
        assert_eq!(json, "{\"mode\":\"soft\"}");
        let de: ModelCancelParams = serde_json::from_str(&json).unwrap();
        assert_eq!(de.mode, Some(CancelMode::Soft));
    }
}
