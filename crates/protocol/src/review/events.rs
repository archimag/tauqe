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
    /// Zero unless a `path_patterns` entry widened the audit beyond its
    /// limits.
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

/// Emitted when an isolated review group begins auditing its paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewGroupStartedEvent {
    pub operation_id: String,
    pub group_id: String,
    pub group_title: String,
    /// Files the group loaded into its own audit context.
    pub files_count: usize,
    /// Estimated tokens those files occupy in the group prompt.
    pub estimated_tokens: usize,
}

/// Emitted when one isolated review group finishes.
///
/// A group is an isolated unit of work: its failure or cancellation never
/// invalidates the findings the other groups already produced, so the outcome
/// travels per group and the supervisor alone decides whether the run goes on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewGroupFinishedEvent {
    pub operation_id: String,
    pub group_id: String,
    pub findings_count: usize,
    pub success: bool,
    /// Failure reason of an unsuccessful group; absent when it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Emitted when the post-analysis layer starts classifying the findings.
///
/// Classification is delegated to the Architect role, so the event reports the
/// model that actually ran it alongside the number of findings it received.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewAnalyzeStartedEvent {
    pub operation_id: String,
    #[serde(default, deserialize_with = "deserialize_model_ref_lenient")]
    pub model: ModelRef,
    pub findings_count: usize,
}

/// Emitted when the post-analysis layer finished and replaced the session layer.
///
/// The event carries the whole session so a client swaps in the new
/// classification without a follow-up fetch. The findings it references are
/// untouched: only the layer above them changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewAnalyzeFinishedEvent {
    pub operation_id: String,
    pub session: ReviewSession,
}
