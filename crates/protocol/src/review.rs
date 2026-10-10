use serde::{Deserialize, Serialize};

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

pub type ReviewStatus = ReviewItemStatus;

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

pub type Review = ReviewSession;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewExecuteItemParams {
    pub review_id: String,
    pub item_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewUpdateItemParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    pub item_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ReviewItemStatus>,
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

/// Returns true if the prompt represents an explicit review discussion turn.
pub fn is_review_discussion_prompt(prompt: &str) -> bool {
    prompt.starts_with(REVIEW_DISCUSSION_PREFIX)
}

/// Returns true if the prompt represents an explicit discussion turn (Plan or Review).
pub fn is_discussion_prompt(prompt: &str) -> bool {
    crate::plan::is_plan_discussion_prompt(prompt) || is_review_discussion_prompt(prompt)
}

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
    let title = if session.title.trim().is_empty() {
        &session.id
    } else {
        session.title.trim()
    };
    out.push_str(&format!(
        "{} [{}] {}\n\n",
        REVIEW_DISCUSSION_PREFIX, session.id, title
    ));

    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("## Review Scope & Description:\n{}\n\n", trimmed));
        }
    }

    out.push_str("## Target Review Session Context:\n");
    let (total, fixed) = session.stats();
    out.push_str(&format!(
        "<active_review id=\"{}\" title=\"{}\" progress=\"{}/{} fixed\">\n",
        session.id, title, fixed, total
    ));

    for item in &session.items {
        let is_focused = focused_item_ids.contains(&item.id);
        let focus_mark = if is_focused { " [FOCUSED]" } else { "" };
        let loc = match (&item.file_path, item.line_range) {
            (Some(f), Some((s, e))) if s == e => format!(" ({}:{})", f, s),
            (Some(f), Some((s, e))) => format!(" ({}:{}-{})", f, s, e),
            (Some(f), None) => format!(" ({})", f),
            (None, _) => String::new(),
        };
        out.push_str(&format!(
            "  - #{}: [{}] {}{}{}\n",
            item.id, item.severity, item.title, loc, focus_mark
        ));
    }
    out.push_str("</active_review>\n\n");

    if !focused_item_ids.is_empty() {
        out.push_str("### Items Focused by User for Discussion:\n");
        for id in focused_item_ids {
            if let Some(item) = session.find_item(*id) {
                out.push_str(&format!(
                    "- **#{} [{}] {}** (status: {})\n",
                    item.id, item.severity, item.title, item.status
                ));
                if let Some(path) = &item.file_path {
                    if let Some((start, end)) = item.line_range {
                        out.push_str(&format!("  Location: `{}:{}-{}`\n", path, start, end));
                    } else {
                        out.push_str(&format!("  Location: `{}`\n", path));
                    }
                }
                if !item.body.trim().is_empty() {
                    out.push_str(&format!("  Details: {}\n", item.body.trim()));
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
    out.push_str("3. If the review findings or their statuses should be updated based on this discussion, explain the proposed modifications in conversational text in `message` AND populate the structured `review_update` field in your response object. Never describe review changes solely in conversational text without populating `review_update`.\n");
    out.push_str("4. If you need to inspect related files to verify your hypothesis, specify them in the `context_requests` array with `access: \"read_only\"`.\n");

    out
}

pub const REVIEW_STEP_EXECUTION_PREFIX: &str = "# Target Review Step Execution:";

/// Target review and item identifiers parsed from an isolated review step execution prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewStepTarget {
    pub review_id: Option<String>,
    pub item_id: u32,
}

/// Returns true if the prompt represents an isolated review step execution turn.
pub fn is_review_step_execution_prompt(prompt: &str) -> bool {
    prompt.starts_with(REVIEW_STEP_EXECUTION_PREFIX)
}

/// Extracts the target review ID and item ID from an isolated review step execution prompt.
pub fn extract_review_step_execution_target(prompt: &str) -> Option<ReviewStepTarget> {
    if !prompt.starts_with(REVIEW_STEP_EXECUTION_PREFIX) {
        return None;
    }
    let rest = prompt.strip_prefix(REVIEW_STEP_EXECUTION_PREFIX)?.trim_start();
    if let Some(rest) = rest.strip_prefix('[') {
        let (spec, _) = rest.split_once(']')?;
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        if let Some((rev_id, item_id_str)) = spec.split_once(':') {
            let rev_id = rev_id.trim();
            let item_id: u32 = item_id_str.trim().parse().ok()?;
            return Some(ReviewStepTarget {
                review_id: if rev_id.is_empty() { None } else { Some(rev_id.to_string()) },
                item_id,
            });
        } else if let Ok(item_id) = spec.parse::<u32>() {
            return Some(ReviewStepTarget {
                review_id: None,
                item_id,
            });
        }
    }
    None
}

/// Extracts the target item ID from an isolated review step execution prompt.
pub fn extract_review_step_execution_id(prompt: &str) -> Option<u32> {
    extract_review_step_execution_target(prompt).map(|t| t.item_id)
}

/// Formats a targeted prompt for resolving an isolated review finding.
pub fn format_review_step_execution_prompt(
    session: &ReviewSession,
    item_id: u32,
    marker_suffix: &str,
) -> Result<String, String> {
    let item = session
        .items
        .iter()
        .find(|i| i.id == item_id)
        .ok_or_else(|| format!("Review item #{} not found in review session '{}'", item_id, session.id))?;

    let mut out = String::new();
    out.push_str(&format!(
        "{} [{}:{}] {}\n\n",
        REVIEW_STEP_EXECUTION_PREFIX, session.id, item.id, item.title
    ));

    let title = if session.title.trim().is_empty() {
        &session.id
    } else {
        session.title.trim()
    };
    out.push_str(&format!("## Review Session: {}\n", title));
    if let Some(desc) = &session.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("{}\n\n", trimmed));
        }
    }

    out.push_str("### Target Finding to Resolve:\n");
    out.push_str(&format!("- **ID**: `#{}`\n", item.id));
    out.push_str(&format!("- **Severity**: {}\n", item.severity));
    out.push_str(&format!("- **Title**: {}\n", item.title));
    if let Some(path) = &item.file_path {
        if let Some((start, end)) = item.line_range {
            out.push_str(&format!("- **Location**: `{}:{}-{}`\n", path, start, end));
        } else {
            out.push_str(&format!("- **Location**: `{}`\n", path));
        }
    }
    if !item.body.trim().is_empty() {
        out.push_str(&format!("- **Details / Recommendation**:\n{}\n", item.body.trim()));
    }
    out.push('\n');

    out.push_str("## Execution Directives:\n");
    out.push_str(&format!(
        "1. Focus STRICTLY and EXCLUSIVELY on resolving review finding `#{}`.\n",
        item.id
    ));
    out.push_str("2. Do NOT touch unrelated code or address other findings outside of this scope.\n");
    out.push_str("3. Keep changes minimal, coherent, and verified with toolchain checks.\n");
    out.push_str(&format!(
        "4. When implementation and verification are complete, emit the completion tag: <review_step_done{0} id=\"{1}\" />.\n",
        marker_suffix, item.id
    ));
    out.push_str(&format!(
        "5. Discovery Phase: If you need to inspect or edit files listed in <repo_map> that are not yet loaded in <context>, request them immediately using <context_request{0} path=\"...\" access=\"read_only|editable\" /> before writing code or explanations. Do NOT halt with conversational text if the necessary files can simply be requested.\n",
        marker_suffix
    ));
    out.push_str("6. If you encounter an unsolvable blocking issue or need developer architectural decisions, explain it in text and DO NOT emit the completion tag.\n");

    Ok(out)
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
            model: Some(ModelSelection::Tier(crate::model::ModelTier::Senior)),
            severity: ReviewSeverity::Critical,
            status: ReviewItemStatus::Discussion,
            file_path: Some("crates/server/src/state.rs".to_string()),
            line_range: Some((42, 50)),
            body: "Lock is released before turn concludes".to_string(),
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"critical\""));
        assert!(json.contains("\"discussion\""));

        let de: ReviewItem = serde_json::from_str(&json).unwrap();
        assert_eq!(de, item);

        // Test serde alias "done" -> Fixed
        let legacy_json = r#"{"id":2,"title":"Legacy","severity":"info","status":"done","body":""}"#;
        let de_legacy: ReviewItem = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(de_legacy.status, ReviewItemStatus::Fixed);
    }

    #[test]
    fn test_review_discussion_prompt() {
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
                body: "Lock is dropped prematurely.".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let prompt = format_review_discussion_prompt(&session, Some(1), "Is this really possible?");
        assert!(is_review_discussion_prompt(&prompt));
        assert!(is_discussion_prompt(&prompt));
        assert!(prompt.contains("# Review Discussion: [rev-42] Security Audit"));
        assert!(prompt.contains("Race condition in cache"));
        assert!(prompt.contains("Is this really possible?"));
        assert!(prompt.contains("populate the structured `review_update` field"));
    }

    #[test]
    fn test_review_step_execution_prompt() {
        let session = ReviewSession {
            id: "rev-7".to_string(),
            title: "Performance Review".to_string(),
            created_at: 100,
            model: "model-x".to_string(),
            description: Some("Optimize string copies".to_string()),
            user_prompt: None,
            target_files: vec!["crates/tui/src/view.rs".to_string()],
            items: vec![ReviewItem {
                id: 3,
                title: "Excessive clone in hot path".to_string(),
                model: None,
                severity: ReviewSeverity::Warning,
                status: ReviewItemStatus::Todo,
                file_path: Some("crates/tui/src/view.rs".to_string()),
                line_range: Some((100, 110)),
                body: "Replace clone with reference borrowing".to_string(),
            }],
            raw_markdown: String::new(),
        };

        let prompt = format_review_step_execution_prompt(&session, 3, "_TAG").unwrap();
        assert!(is_review_step_execution_prompt(&prompt));
        assert!(prompt.contains("# Target Review Step Execution: [rev-7:3] Excessive clone in hot path"));
        assert!(prompt.contains("<review_step_done_TAG id=\"3\" />"));

        let target = extract_review_step_execution_target(&prompt).unwrap();
        assert_eq!(target.review_id.as_deref(), Some("rev-7"));
        assert_eq!(target.item_id, 3);
        assert_eq!(extract_review_step_execution_id(&prompt), Some(3));
    }

    #[test]
    fn test_review_session_summary_and_stats() {
        let session = ReviewSession {
            id: "rev-123".to_string(),
            title: "Session 123".to_string(),
            created_at: 1700000000,
            model: "openrouter:anthropic/claude-3.7-sonnet".to_string(),
            description: None,
            user_prompt: Some("Focus on security".to_string()),
            target_files: vec!["crates/server/src/state.rs".to_string()],
            items: vec![
                ReviewItem {
                    id: 1,
                    title: "Issue 1".to_string(),
                    model: None,
                    severity: ReviewSeverity::Warning,
                    status: ReviewItemStatus::Fixed,
                    file_path: None,
                    line_range: None,
                    body: "".to_string(),
                },
                ReviewItem {
                    id: 2,
                    title: "Issue 2".to_string(),
                    model: None,
                    severity: ReviewSeverity::Critical,
                    status: ReviewItemStatus::Todo,
                    file_path: None,
                    line_range: None,
                    body: "".to_string(),
                },
            ],
            raw_markdown: "## [CRITICAL] Issue".to_string(),
        };

        let stats = session.stats();
        assert_eq!(stats, (2, 1));
        let summary = session.summary();
        assert_eq!(summary.total_items, 2);
        assert_eq!(summary.fixed_items, 1);
        assert_eq!(summary.title, "Session 123");
    }
}
