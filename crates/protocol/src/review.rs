use serde::{Deserialize, Serialize};

use crate::model::{ModelRef, ModelUsageInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    Critical,
    Warning,
    Suggestion,
    #[default]
    Info,
}

impl std::fmt::Display for ReviewSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Critical => write!(f, "CRITICAL"),
            Self::Warning => write!(f, "WARN"),
            Self::Suggestion => write!(f, "SUGGESTION"),
            Self::Info => write!(f, "INFO"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    #[default]
    Todo,
    Done,
    Rejected,
}

impl ReviewStatus {
    pub fn next(&self) -> Self {
        match self {
            Self::Todo => Self::Done,
            Self::Done => Self::Rejected,
            Self::Rejected => Self::Todo,
        }
    }
}

impl std::fmt::Display for ReviewStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Todo => write!(f, "TODO"),
            Self::Done => write!(f, "DONE"),
            Self::Rejected => write!(f, "REJECTED"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub severity: ReviewSeverity,
    #[serde(default)]
    pub status: ReviewStatus,
    #[serde(default)]
    pub is_checked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_range: Option<(usize, usize)>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewSession {
    pub id: String,
    pub created_at: u64,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ReviewItem>,
    #[serde(default)]
    pub raw_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewStartParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewUpdateItemParams {
    pub item_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ReviewStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_checked: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewStartedEvent {
    pub operation_id: String,
    pub model: String,
    pub files_count: usize,
    pub estimated_tokens: usize,
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
pub struct ReviewGetResult {
    pub session: Option<ReviewSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewUpdateItemResult {
    pub item: ReviewItem,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_status_lifecycle() {
        let status = ReviewStatus::Todo;
        assert_eq!(status.next(), ReviewStatus::Done);
        assert_eq!(status.next().next(), ReviewStatus::Rejected);
        assert_eq!(status.next().next().next(), ReviewStatus::Todo);
        assert_eq!(status.to_string(), "TODO");
    }

    #[test]
    fn test_review_item_serde() {
        let item = ReviewItem {
            id: 1,
            title: "Potential race condition".to_string(),
            severity: ReviewSeverity::Critical,
            status: ReviewStatus::Todo,
            is_checked: true,
            file_path: Some("crates/server/src/state.rs".to_string()),
            line_range: Some((42, 50)),
            body: "Lock is released before turn concludes".to_string(),
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"critical\""));
        assert!(json.contains("\"todo\""));
        assert!(json.contains("\"is_checked\":true"));

        let de: ReviewItem = serde_json::from_str(&json).unwrap();
        assert_eq!(de, item);
    }

    #[test]
    fn test_review_session_serde() {
        let session = ReviewSession {
            id: "rev-123".to_string(),
            created_at: 1700000000,
            model: "openrouter:anthropic/claude-3.7-sonnet".to_string(),
            user_prompt: Some("Focus on security".to_string()),
            target_files: vec!["crates/server/src/state.rs".to_string()],
            items: vec![],
            raw_markdown: "## [CRITICAL] Issue".to_string(),
        };

        let json = serde_json::to_string(&session).unwrap();
        let de: ReviewSession = serde_json::from_str(&json).unwrap();
        assert_eq!(de, session);
    }
}
