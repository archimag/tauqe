use serde::{Deserialize, Serialize};

use crate::escape::{escape_xml_attr, escape_xml_text, single_line};
use crate::model::{ModelRef, ModelSelection, ModelUsageInfo};

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
    Rejected,
}

impl ReviewItemStatus {
    pub fn next(&self) -> Self {
        match self {
            Self::Discussion => Self::Todo,
            Self::Todo => Self::InProgress,
            Self::InProgress => Self::Fixed,
            Self::Fixed => Self::Rejected,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_range: Option<(usize, usize)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_notes: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewSession {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub created_at: u64,
    pub model: String,
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
    pub model: String,
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
    pub review_id: String,
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

pub const REVIEW_DISCUSSION_PREFIX: &str = "# Review Discussion:";

/// Formats a targeted prompt for discussing and analyzing review findings in Structured Output mode.
pub fn format_review_discussion_prompt(
    session: &ReviewSession,
    focused_item_id: Option<u32>,
    user_comment: &str,
) -> String {
    let ids: Vec<u32> = focused_item_id.into_iter().collect();
    format_review_discussion_prompt_items(session, &ids, user_comment)
}

/// Formats a targeted prompt for discussing multiple focused review findings in Structured Output mode.
pub fn format_review_discussion_prompt_items(
    session: &ReviewSession,
    focused_item_ids: &[u32],
    user_comment: &str,
) -> String {
    let mut out = String::new();
    let clean_title = single_line(&session.title);
    let title = if clean_title.is_empty() {
        session.id.as_str()
    } else {
        clean_title.as_str()
    };
    out.push_str(&format!(
        "{} [{}] {}\n\n",
        REVIEW_DISCUSSION_PREFIX,
        single_line(&session.id),
        title
    ));

    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!(
                "## Review Scope & Description:\n{}\n\n",
                escape_xml_text(trimmed)
            ));
        }
    }

    out.push_str("## Target Review Session Context:\n");
    let (total, fixed) = session.stats();
    out.push_str(&format!(
        "<active_review id=\"{}\" title=\"{}\" progress=\"{}/{} fixed\">\n",
        escape_xml_attr(&session.id),
        escape_xml_attr(title),
        fixed,
        total
    ));

    for item in &session.items {
        let is_focused = focused_item_ids.contains(&item.id);
        let focus_mark = if is_focused { " [FOCUSED]" } else { "" };
        let loc = match (&item.file_path, item.line_range) {
            (Some(f), Some((s, e))) if s == e => format!(" ({}:{})", escape_xml_text(f), s),
            (Some(f), Some((s, e))) => format!(" ({}:{}-{})", escape_xml_text(f), s, e),
            (Some(f), None) => format!(" ({})", escape_xml_text(f)),
            (None, _) => String::new(),
        };
        out.push_str(&format!(
            "  - #{}: [{}] {}{}{}\n",
            item.id,
            item.severity,
            escape_xml_text(&item.title),
            loc,
            focus_mark
        ));
    }
    out.push_str("</active_review>\n\n");

    if !focused_item_ids.is_empty() {
        out.push_str("### Items Focused by User for Discussion:\n");
        for id in focused_item_ids {
            if let Some(item) = session.find_item(*id) {
                out.push_str(&format!(
                    "- **#{} [{}] {}** (status: {})\n",
                    item.id,
                    item.severity,
                    escape_xml_text(&item.title),
                    item.status
                ));
                if let Some(path) = &item.file_path {
                    if let Some((start, end)) = item.line_range {
                        out.push_str(&format!(
                            "  Location: `{}:{}-{}`\n",
                            escape_xml_text(path),
                            start,
                            end
                        ));
                    } else {
                        out.push_str(&format!("  Location: `{}`\n", escape_xml_text(path)));
                    }
                }
                if let Some(notes) = &item.resolution_notes {
                    if !notes.trim().is_empty() {
                        out.push_str(&format!(
                            "  Resolution Strategy: {}\n",
                            escape_xml_text(notes.trim())
                        ));
                    }
                }
                if !item.body.trim().is_empty() {
                    out.push_str(&format!("  Details: {}\n", escape_xml_text(item.body.trim())));
                }
            }
        }
        out.push('\n');
    }

    out.push_str("## Developer Comment / Instructions:\n");
    out.push_str(user_comment.trim());
    out.push_str("\n\n");

    out.push_str("## Instructions for Model:\n");
    out.push_str("1. Address the developer's question or feedback regarding the code review findings.\n");
    out.push_str("2. Discuss the problem, edge cases, and potential solutions conceptually. Do NOT emit file edits or code patches in this discussion turn.\n");
    out.push_str("3. If the review findings or their statuses should be updated based on this discussion, explain the proposed modifications clearly in conversational text in `message`; the developer applies status changes through the review interface.\n");
    out.push_str("4. If you need to inspect related files to verify your hypothesis, specify them in the `context_requests` array with `access: \"read_only\"`.\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_status_lifecycle() {
        let status = ReviewItemStatus::Discussion;
        assert_eq!(status.next(), ReviewItemStatus::Todo);
        assert_eq!(status.next().next(), ReviewItemStatus::InProgress);
        assert_eq!(status.next().next().next(), ReviewItemStatus::Fixed);
        assert_eq!(status.next().next().next().next(), ReviewItemStatus::Rejected);
        assert_eq!(status.next().next().next().next().next(), ReviewItemStatus::Discussion);
        assert_eq!(status.to_string(), "DISCUSSION");
    }

    #[test]
    fn test_review_item_serde() {
        let item = ReviewItem {
            id: 1,
            title: "Potential race condition".to_string(),
            model: Some(ModelSelection::Role(crate::model::ModelRole::Architect)),
            severity: ReviewSeverity::Critical,
            status: ReviewItemStatus::Discussion,
            file_path: Some("crates/server/src/state.rs".to_string()),
            line_range: Some((42, 50)),
            resolution_notes: Some("Acquire lock across turn lifetime".to_string()),
            body: "Lock is released before turn concludes".to_string(),
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"critical\""));
        assert!(json.contains("\"discussion\""));
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
    }

    #[test]
    fn test_review_discussion_prompt_formatting() {
        let session = ReviewSession {
            id: "rev-42".to_string(),
            title: "Security Audit".to_string(),
            created_at: 100,
            model: "test-model".to_string(),
            description: Some("Security review for core crate".to_string()),
            user_prompt: None,
            target_files: vec!["src/lib.rs".to_string()],
            items: vec![ReviewItem {
                id: 1,
                title: "Race condition in cache".to_string(),
                model: None,
                severity: ReviewSeverity::Critical,
                status: ReviewItemStatus::Discussion,
                file_path: Some("src/lib.rs".to_string()),
                line_range: Some((10, 20)),
                resolution_notes: Some("Extend scope of mutex guard".to_string()),
                body: "Lock is dropped prematurely.".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let prompt = format_review_discussion_prompt(&session, Some(1), "Is this really possible?");
        assert!(prompt.contains("# Review Discussion: [rev-42] Security Audit"));
        assert!(prompt.contains("Race condition in cache"));
        assert!(prompt.contains("Resolution Strategy: Extend scope of mutex guard"));
        assert!(prompt.contains("Is this really possible?"));
        assert!(!prompt.contains("review_update"));
        assert!(prompt.contains("explain the proposed modifications clearly in conversational text"));
    }
}
