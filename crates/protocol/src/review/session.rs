use serde::{Deserialize, Serialize};

use crate::model::{deserialize_model_ref_lenient, ModelRef};

use super::group::{ReviewGroup, ReviewLaunchConfig, ReviewPostAnalysis};
use super::item::ReviewItem;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewSession {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub created_at: u64,
    #[serde(default, deserialize_with = "deserialize_model_ref_lenient")]
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ReviewItem>,
    #[serde(default)]
    pub raw_markdown: String,
    /// Groups this session audits, in launch order.
    ///
    /// Empty for a session written before grouping existed and for a
    /// single-audit run; the client/core resolve the implicit group independently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ReviewGroup>,
    /// Classification produced by the Architect role over the whole session.
    ///
    /// Replacing it discards only the previous classification: the findings it
    /// references are immutable and are never rescanned to rebuild this layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_analysis: Option<ReviewPostAnalysis>,
    /// Snapshot of the configuration that launched this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_config: Option<ReviewLaunchConfig>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewSummary {
    id: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    created_at: u64,
    #[serde(default, deserialize_with = "deserialize_model_ref_lenient")]
    pub(crate) model: ModelRef,
    /// Actionable findings, excluding the terminal `Skipped` and `Rejected`
    /// outcomes (see [`ReviewSession::stats`]).
    pub(crate) total_items: usize,
    /// Findings resolved as `Fixed`.
    pub(crate) fixed_items: usize,
}
