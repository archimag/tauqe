use serde::{Deserialize, Serialize};

use crate::model::ModelSelection;
use crate::plan::{count_leaf_stats, PlanItem};

/// Semantic category of a code review issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IssueType {
    #[default]
    #[serde(alias = "critical")]
    Bug,
    Security,
    #[serde(alias = "warning", alias = "warn")]
    Architecture,
    Performance,
    #[serde(alias = "style", alias = "suggestion")]
    Quality,
}

impl IssueType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Security => "security",
            Self::Architecture => "architecture",
            Self::Performance => "performance",
            Self::Quality => "quality",
        }
    }

    pub fn display_badge(&self) -> &'static str {
        match self {
            Self::Bug => "BUG",
            Self::Security => "SEC",
            Self::Architecture => "ARCH",
            Self::Performance => "PERF",
            Self::Quality => "QUAL",
        }
    }
}

impl std::fmt::Display for IssueType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_badge())
    }
}

/// Lifecycle status of a review issue, supporting refinement before resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IssueStatus {
    #[default]
    Discussion,
    Todo,
    InProgress,
    Refine,
    #[serde(alias = "done")]
    Fixed,
    Skipped,
    Rejected,
}

impl IssueStatus {
    pub fn next(&self) -> Self {
        match self {
            Self::Discussion => Self::Todo,
            Self::Todo => Self::InProgress,
            Self::InProgress => Self::Refine,
            Self::Refine => Self::Fixed,
            Self::Fixed => Self::Skipped,
            Self::Skipped => Self::Rejected,
            Self::Rejected => Self::Discussion,
        }
    }
}

impl std::fmt::Display for IssueStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discussion => write!(f, "DISCUSSION"),
            Self::Todo => write!(f, "TODO"),
            Self::InProgress => write!(f, "IN_PROGRESS"),
            Self::Refine => write!(f, "REFINE"),
            Self::Fixed => write!(f, "FIXED"),
            Self::Skipped => write!(f, "SKIPPED"),
            Self::Rejected => write!(f, "REJECTED"),
        }
    }
}

/// Unified status alias for review items and findings.
pub type ReviewItemStatus = IssueStatus;

/// Severity classification for review findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    #[serde(alias = "bug", alias = "security")]
    Critical,
    #[serde(alias = "architecture", alias = "performance")]
    Warning,
    #[serde(alias = "quality", alias = "style")]
    Suggestion,
    #[default]
    Info,
}

impl ReviewSeverity {
    pub fn display_badge(&self) -> &'static str {
        match self {
            Self::Critical => "CRIT",
            Self::Warning => "WARN",
            Self::Suggestion => "SUGG",
            Self::Info => "INFO",
        }
    }
}

impl std::fmt::Display for ReviewSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_badge())
    }
}

impl From<ReviewSeverity> for IssueType {
    fn from(s: ReviewSeverity) -> Self {
        match s {
            ReviewSeverity::Critical => IssueType::Bug,
            ReviewSeverity::Warning => IssueType::Architecture,
            ReviewSeverity::Suggestion => IssueType::Quality,
            ReviewSeverity::Info => IssueType::Quality,
        }
    }
}

impl From<IssueType> for ReviewSeverity {
    fn from(t: IssueType) -> Self {
        match t {
            IssueType::Bug | IssueType::Security => ReviewSeverity::Critical,
            IssueType::Architecture | IssueType::Performance => ReviewSeverity::Warning,
            IssueType::Quality => ReviewSeverity::Suggestion,
        }
    }
}

/// A structured review issue with classified type, recommendations, and resolution plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewIssue {
    pub id: u32,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(rename = "type", alias = "severity", alias = "issue_type")]
    pub issue_type: IssueType,
    #[serde(default)]
    pub status: IssueStatus,
    #[serde(default, alias = "body")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommendations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_files: Vec<String>,
    #[serde(default, alias = "subplan", skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<PlanItem>,
    #[serde(default, alias = "resolution_notes", skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ReviewIssue {
    pub fn has_plan(&self) -> bool {
        !self.plan.is_empty()
    }

    pub fn plan_stats(&self) -> (usize, usize) {
        count_leaf_stats(&self.plan)
    }

    pub fn has_subplan(&self) -> bool {
        self.has_plan()
    }

    pub fn subplan_stats(&self) -> (usize, usize) {
        self.plan_stats()
    }
}

/// A review finding stored as an individual item of a review session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: u32,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(rename = "severity", alias = "type", alias = "issue_type", default)]
    pub severity: ReviewSeverity,
    #[serde(default)]
    pub status: ReviewItemStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommendations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_files: Vec<String>,
    #[serde(default, alias = "plan", skip_serializing_if = "Vec::is_empty")]
    pub subplan: Vec<PlanItem>,
    #[serde(default, alias = "note", skip_serializing_if = "Option::is_none")]
    pub resolution_notes: Option<String>,
    #[serde(default, rename = "body", alias = "description")]
    pub body: String,
}

impl ReviewItem {
    pub fn has_subplan(&self) -> bool {
        !self.subplan.is_empty()
    }

    pub fn subplan_stats(&self) -> (usize, usize) {
        count_leaf_stats(&self.subplan)
    }
}

impl From<ReviewIssue> for ReviewItem {
    fn from(issue: ReviewIssue) -> Self {
        Self {
            id: issue.id,
            title: issue.title,
            model: issue.model,
            severity: ReviewSeverity::from(issue.issue_type),
            status: issue.status,
            recommendations: issue.recommendations,
            files: issue.files,
            readonly_files: issue.readonly_files,
            subplan: issue.plan,
            resolution_notes: issue.note,
            body: issue.description,
        }
    }
}

impl From<ReviewItem> for ReviewIssue {
    fn from(item: ReviewItem) -> Self {
        Self {
            id: item.id,
            title: item.title,
            model: item.model,
            issue_type: IssueType::from(item.severity),
            status: item.status,
            description: item.body,
            recommendations: item.recommendations,
            files: item.files,
            readonly_files: item.readonly_files,
            plan: item.subplan,
            note: item.resolution_notes,
        }
    }
}
