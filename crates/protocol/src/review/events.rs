use serde::{Deserialize, Serialize};

use crate::model::{deserialize_model_ref_lenient, ModelRef, ModelUsageInfo};

use super::session::ReviewSession;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewStartedEvent {
    pub operation_id: String,
    #[serde(default, deserialize_with = "deserialize_model_ref_lenient")]
    pub model: ModelRef,
    pub files_count: usize,
    pub estimated_tokens: usize,
    /// Pattern-matched candidate files that were not loaded because the
    /// configured context batch or token budget stopped the expansion.
    /// Zero unless a `path_pattern` widened the audit beyond its limits.
    #[serde(default)]
    pub omitted_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewReasoningDeltaEvent {
    pub operation_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewContentDeltaEvent {
    pub operation_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinishedEvent {
    pub operation_id: String,
    pub session: ReviewSession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ModelUsageInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_total_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewErrorEvent {
    pub operation_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewUpdatedEvent {
    pub session: ReviewSession,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewListChangedEvent {
    pub reviews: Vec<ReviewSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}
