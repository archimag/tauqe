use serde::{Deserialize, Deserializer, Serialize};

use crate::model::{ModelRef, ModelSelection, ModelUsageInfo};

/// Leniently deserializes a [`ModelRef`] from either a structured JSON object
/// (`{"provider":"openrouter","name":"..."}`) or a legacy string representation
/// (`"openrouter:..."` or `"vendor/model"`).
pub fn deserialize_model_ref_lenient<'de, D>(deserializer: D) -> Result<ModelRef, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ModelRefOrString {
        Structured(ModelRef),
        Raw(String),
    }

    match ModelRefOrString::deserialize(deserializer)? {
        ModelRefOrString::Structured(m) => Ok(m),
        ModelRefOrString::Raw(s) => {
            if s.trim().is_empty() {
                Ok(ModelRef::default())
            } else {
                s.parse::<ModelRef>().map_err(serde::de::Error::custom)
            }
        }
    }
}

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
pub enum ReviewItemStatus {
    #[default]
    Discussion,
    Todo,
    InProgress,
    #[serde(alias = "done")]
    Fixed,
    Skipped,
    Rejected,
}

impl ReviewItemStatus {
    pub fn next(&self) -> Self {
        match self {
            Self::Discussion => Self::Todo,
            Self::Todo => Self::InProgress,
            Self::InProgress => Self::Fixed,
            Self::Fixed => Self::Skipped,
            Self::Skipped => Self::Rejected,
            Self::Rejected => Self::Discussion,
        }
    }
}

impl std::fmt::Display for ReviewItemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discussion => write!(f, "DISCUSSION"),
            Self::Todo => write!(f, "TODO"),
            Self::InProgress => write!(f, "IN_PROGRESS"),
            Self::Fixed => write!(f, "FIXED"),
            Self::Skipped => write!(f, "SKIPPED"),
            Self::Rejected => write!(f, "REJECTED"),
        }
    }
}

/// Convenience alias for [`ReviewItemStatus`].
///
/// Client surfaces and wire consumers refer to a review finding's status as
/// `ReviewStatus`; the alias keeps that name resolvable at the crate root
/// without duplicating the enum definition.
pub type ReviewStatus = ReviewItemStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ReviewItemRaw")]
pub struct ReviewItem {
    pub id: u32,
    pub title: String,
    /// Model tier or explicit model assigned to resolve this finding. `None` means the default tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default)]
    pub severity: ReviewSeverity,
    #[serde(default)]
    pub status: ReviewItemStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_notes: Option<String>,
    pub body: String,
}

#[derive(Deserialize)]
struct ReviewItemRaw {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub model: Option<ModelSelection>,
    #[serde(default)]
    pub severity: ReviewSeverity,
    #[serde(default)]
    pub status: ReviewItemStatus,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub line_range: Option<(usize, usize)>,
    #[serde(default)]
    pub resolution_notes: Option<String>,
    #[serde(default)]
    pub body: String,
}

impl From<ReviewItemRaw> for ReviewItem {
    fn from(raw: ReviewItemRaw) -> Self {
        let mut files = raw.files;
        if files.is_empty() {
            if let Some(path) = raw.file_path {
                let trimmed = path.trim();
                if !trimmed.is_empty() {
                    files.push(trimmed.to_string());
                }
            }
        }
        Self {
            id: raw.id,
            title: raw.title,
            model: raw.model,
            severity: raw.severity,
            status: raw.status,
            files,
            resolution_notes: raw.resolution_notes,
            body: raw.body,
        }
    }
}

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: u64,
    #[serde(default, deserialize_with = "deserialize_model_ref_lenient")]
    pub model: ModelRef,
    pub total_items: usize,
    pub fixed_items: usize,
}

impl ReviewSession {
    pub fn summary(&self) -> ReviewSummary {
        let (total, fixed) = self.stats();
        ReviewSummary {
            id: self.id.clone(),
            title: if self.title.trim().is_empty() {
                self.id.clone()
            } else {
                self.title.clone()
            },
            description: self.description.clone(),
            created_at: self.created_at,
            model: self.model.clone(),
            total_items: total,
            fixed_items: fixed,
        }
    }

    pub fn stats(&self) -> (usize, usize) {
        let total = self.items.len();
        let fixed = self
            .items
            .iter()
            .filter(|i| i.status == ReviewItemStatus::Fixed)
            .count();
        (total, fixed)
    }

    pub fn find_item(&self, item_id: u32) -> Option<&ReviewItem> {
        self.items.iter().find(|i| i.id == item_id)
    }

    pub fn find_item_mut(&mut self, item_id: u32) -> Option<&mut ReviewItem> {
        self.items.iter_mut().find(|i| i.id == item_id)
    }

    pub fn update_item_status(&mut self, item_id: u32, status: ReviewItemStatus) -> bool {
        if let Some(item) = self.find_item_mut(item_id) {
            item.status = status;
            true
        } else {
            false
        }
    }

    pub fn update_item_resolution_notes(&mut self, item_id: u32, notes: Option<String>) -> bool {
        if let Some(item) = self.find_item_mut(item_id) {
            item.resolution_notes = notes;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewStartParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewListParams {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewListResult {
    pub reviews: Vec<ReviewSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewGetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewGetResult {
    pub session: Option<ReviewSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDeleteParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDeleteResult {
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewExecuteParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewUpdateItemParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    pub item_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ReviewItemStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewStartedEvent {
    pub operation_id: String,
    #[serde(default, deserialize_with = "deserialize_model_ref_lenient")]
    pub model: ModelRef,
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
pub struct ReviewUpdateItemResult {
    pub item: ReviewItem,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_execute_params_serde() {
        let single: ReviewExecuteParams =
            serde_json::from_str(r#"{"review_id":"rev-1","item_id":2}"#).unwrap();
        assert_eq!(single.review_id.as_deref(), Some("rev-1"));
        assert_eq!(single.item_id, Some(2));

        let cross_session: ReviewExecuteParams = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(cross_session.review_id, None);
        assert_eq!(cross_session.item_id, None);

        let json = serde_json::to_string(&cross_session).unwrap();
        assert_eq!(json, "{}");

        let single_session_only: ReviewExecuteParams =
            serde_json::from_str(r#"{"review_id":"rev-2"}"#).unwrap();
        assert_eq!(single_session_only.review_id.as_deref(), Some("rev-2"));
        assert_eq!(single_session_only.item_id, None);
    }

    #[test]
    fn test_review_status_lifecycle() {
        let status = ReviewItemStatus::Discussion;
        assert_eq!(status.next(), ReviewItemStatus::Todo);
        assert_eq!(status.next().next(), ReviewItemStatus::InProgress);
        assert_eq!(status.next().next().next(), ReviewItemStatus::Fixed);
        assert_eq!(status.next().next().next().next(), ReviewItemStatus::Skipped);
        assert_eq!(status.next().next().next().next().next(), ReviewItemStatus::Rejected);
        assert_eq!(status.next().next().next().next().next().next(), ReviewItemStatus::Discussion);
        assert_eq!(status.to_string(), "DISCUSSION");
        assert_eq!(ReviewItemStatus::Skipped.to_string(), "SKIPPED");
        assert_eq!(
            serde_json::to_string(&ReviewItemStatus::Skipped).unwrap(),
            "\"skipped\""
        );
        assert_eq!(
            serde_json::from_str::<ReviewItemStatus>("\"skipped\"").unwrap(),
            ReviewItemStatus::Skipped
        );
    }

    #[test]
    fn test_review_item_serde() {
        let item = ReviewItem {
            id: 1,
            title: "Potential race condition".to_string(),
            model: Some(ModelSelection::Role(crate::model::ModelRole::Architect)),
            severity: ReviewSeverity::Critical,
            status: ReviewItemStatus::Discussion,
            files: vec!["crates/server/src/state.rs".to_string()],
            resolution_notes: Some("Acquire lock across turn lifetime".to_string()),
            body: "Lock is released before turn concludes".to_string(),
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"critical\""));
        assert!(json.contains("\"discussion\""));
        assert!(json.contains("\"files\":[\"crates/server/src/state.rs\"]"));
        assert!(json.contains("\"resolution_notes\":\"Acquire lock across turn lifetime\""));

        let de: ReviewItem = serde_json::from_str(&json).unwrap();
        assert_eq!(de, item);

        let update_params = ReviewUpdateItemParams {
            review_id: Some("rev-1".to_string()),
            item_id: 1,
            status: Some(ReviewItemStatus::InProgress),
            resolution_notes: Some("Plan defined".to_string()),
        };
        let update_json = serde_json::to_string(&update_params).unwrap();
        assert!(update_json.contains("\"resolution_notes\":\"Plan defined\""));

        // The legacy "done" status alias still maps to Fixed.
        let legacy_json = r#"{"id":2,"title":"Legacy","severity":"info","status":"done","body":"","model":"architect"}"#;
        let de_legacy: ReviewItem = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(de_legacy.status, ReviewItemStatus::Fixed);
        assert_eq!(de_legacy.model, Some(ModelSelection::Role(crate::model::ModelRole::Architect)));
        assert!(de_legacy.files.is_empty());
    }

    #[test]
    fn test_review_item_legacy_file_path_backward_compatibility() {
        let legacy_json = r#"{
            "id": 1,
            "title": "Legacy Finding",
            "severity": "warning",
            "status": "todo",
            "file_path": "crates/core/src/review.rs",
            "line_range": [10, 25],
            "body": "Legacy issue description"
        }"#;

        let item: ReviewItem = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(item.id, 1);
        assert_eq!(item.title, "Legacy Finding");
        assert_eq!(item.severity, ReviewSeverity::Warning);
        assert_eq!(item.status, ReviewItemStatus::Todo);
        assert_eq!(item.files, vec!["crates/core/src/review.rs"]);
        assert_eq!(item.body, "Legacy issue description");

        // Explicit empty files array falls back to file_path
        let empty_files_json = r#"{
            "id": 2,
            "title": "Empty files fallback",
            "severity": "info",
            "status": "discussion",
            "files": [],
            "file_path": "crates/tui/src/views.rs",
            "body": "Empty files details"
        }"#;
        let item_empty: ReviewItem = serde_json::from_str(empty_files_json).unwrap();
        assert_eq!(item_empty.files, vec!["crates/tui/src/views.rs"]);

        // Multi-file finding preserves all paths and Skipped status
        let multi_json = r#"{
            "id": 3,
            "title": "Multi-file Finding",
            "severity": "critical",
            "status": "skipped",
            "files": ["crates/protocol/src/review.rs", "crates/core/src/review.rs"],
            "resolution_notes": "Skipped because already fixed",
            "body": "Details"
        }"#;

        let multi_item: ReviewItem = serde_json::from_str(multi_json).unwrap();
        assert_eq!(multi_item.id, 3);
        assert_eq!(multi_item.status, ReviewItemStatus::Skipped);
        assert_eq!(
            multi_item.files,
            vec![
                "crates/protocol/src/review.rs",
                "crates/core/src/review.rs"
            ]
        );
        assert_eq!(
            multi_item.resolution_notes.as_deref(),
            Some("Skipped because already fixed")
        );
    }

    #[test]
    fn test_review_session_and_summary_model_ref_lenient_serde() {
        // 1. Structured ModelRef JSON
        let structured_session_json = r#"{
            "id": "rev-1",
            "title": "Structured",
            "created_at": 100,
            "model": {"provider": "openrouter", "name": "anthropic/claude-3.7-sonnet"}
        }"#;
        let session: ReviewSession = serde_json::from_str(structured_session_json).unwrap();
        assert_eq!(
            session.model,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );

        let structured_summary_json = r#"{
            "id": "rev-1",
            "title": "Structured",
            "created_at": 100,
            "model": {"provider": "openrouter", "name": "anthropic/claude-3.7-sonnet"},
            "total_items": 5,
            "fixed_items": 2
        }"#;
        let summary: ReviewSummary = serde_json::from_str(structured_summary_json).unwrap();
        assert_eq!(
            summary.model,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );

        // 2. Legacy string JSON (bare model name)
        let legacy_bare_session = r#"{
            "id": "rev-2",
            "title": "Legacy Bare",
            "created_at": 200,
            "model": "anthropic/claude-3.7-sonnet"
        }"#;
        let session: ReviewSession = serde_json::from_str(legacy_bare_session).unwrap();
        assert_eq!(
            session.model,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );

        let legacy_bare_summary = r#"{
            "id": "rev-2",
            "title": "Legacy Bare",
            "created_at": 200,
            "model": "anthropic/claude-3.7-sonnet",
            "total_items": 3,
            "fixed_items": 1
        }"#;
        let summary: ReviewSummary = serde_json::from_str(legacy_bare_summary).unwrap();
        assert_eq!(
            summary.model,
            ModelRef::openrouter("anthropic/claude-3.7-sonnet")
        );

        // 3. Legacy string JSON with provider prefix
        let legacy_prefixed_session = r#"{
            "id": "rev-3",
            "title": "Legacy Prefixed",
            "created_at": 300,
            "model": "openrouter:openai/gpt-4o"
        }"#;
        let session: ReviewSession = serde_json::from_str(legacy_prefixed_session).unwrap();
        assert_eq!(session.model, ModelRef::openrouter("openai/gpt-4o"));

        // 4. Missing model falls back to default
        let missing_model_session = r#"{
            "id": "rev-4",
            "title": "Missing Model",
            "created_at": 400
        }"#;
        let session: ReviewSession = serde_json::from_str(missing_model_session).unwrap();
        assert_eq!(session.model, ModelRef::default());

        // 5. Summary method derives ModelRef
        let derived_summary = session.summary();
        assert_eq!(derived_summary.model, ModelRef::default());

        // 6. Serialized session roundtrips cleanly
        let serialized = serde_json::to_string(&session).unwrap();
        let de: ReviewSession = serde_json::from_str(&serialized).unwrap();
        assert_eq!(de.model, session.model);
    }
}
