use serde::{Deserialize, Deserializer, Serialize};

use crate::model::ModelSelection;

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
    #[serde(alias = "style", alias = "suggestion", alias = "info")]
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

/// Lifecycle status of a review issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IssueStatus {
    #[default]
    Discussion,
    #[serde(alias = "refine")]
    Todo,
    InProgress,
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
            Self::InProgress => Self::Fixed,
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

/// A structured review issue with a classified type and a remediation strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewIssue {
    pub id: u32,
    pub title: String,
    /// Stable identifier of the finding, an RFC 9562 UUIDv7 minted once.
    ///
    /// The post-analysis layer references findings by this identifier, so a
    /// finding keeps its identity across regroupings, refinements, and re-runs
    /// of the analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Identifier of the review group that produced the finding.
    ///
    /// `None` means the implicit default group of a single-audit session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    #[serde(
        default,
        alias = "resolution_notes",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<String>,
}

/// A review finding stored as an individual item of a review session.
///
/// The semantic category (`issue_type`) is the single source of truth on the
/// wire: it preserves every category losslessly, including the pairs the
/// coarser severity classification merges (`Bug`/`Security` and
/// `Architecture`/`Performance`). `severity` is a presentation-only
/// classification recomputed from the category whenever an item is read from
/// storage, so the two can never drift apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewItem {
    pub id: u32,
    pub title: String,
    /// Stable identifier of the finding, an RFC 9562 UUIDv7 minted once.
    ///
    /// The post-analysis layer references findings by this identifier, so a
    /// finding keeps its identity across regroupings, refinements, and re-runs
    /// of the analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Identifier of the review group that produced the finding.
    ///
    /// `None` means the implicit default group of a single-audit session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    /// Authoritative semantic category of the finding.
    #[serde(rename = "type")]
    pub issue_type: IssueType,
    /// Coarse severity derived from `issue_type`; never read from or written
    /// to the wire.
    #[serde(skip)]
    pub severity: ReviewSeverity,
    #[serde(default)]
    pub status: ReviewItemStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommendations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly_files: Vec<String>,
    #[serde(default, alias = "note", skip_serializing_if = "Option::is_none")]
    pub resolution_notes: Option<String>,
    #[serde(default, rename = "body", alias = "description")]
    pub body: String,
}

/// Deserialization shadow of [`ReviewItem`].
///
/// Findings written before the semantic category was stored verbatim carry only
/// a coarse `severity` spelling. The shadow accepts every historical key for the
/// category (`type`, `severity`, `issue_type`) and lets the item recompute its
/// presentation severity from it, so exactly one classification is read and a
/// session file can never desynchronize the two.
#[derive(Deserialize)]
struct ReviewItemWire {
    id: u32,
    title: String,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    model: Option<ModelSelection>,
    #[serde(rename = "type", alias = "severity", alias = "issue_type", default)]
    issue_type: IssueType,
    #[serde(default)]
    status: ReviewItemStatus,
    #[serde(default)]
    recommendations: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    readonly_files: Vec<String>,
    #[serde(default, alias = "note")]
    resolution_notes: Option<String>,
    #[serde(default, alias = "description")]
    body: String,
}

impl From<ReviewItemWire> for ReviewItem {
    fn from(wire: ReviewItemWire) -> Self {
        Self {
            id: wire.id,
            title: wire.title,
            uuid: wire.uuid,
            group: wire.group,
            model: wire.model,
            issue_type: wire.issue_type,
            severity: ReviewSeverity::from(wire.issue_type),
            status: wire.status,
            recommendations: wire.recommendations,
            files: wire.files,
            readonly_files: wire.readonly_files,
            resolution_notes: wire.resolution_notes,
            body: wire.body,
        }
    }
}

impl<'de> Deserialize<'de> for ReviewItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ReviewItemWire::deserialize(deserializer).map(ReviewItem::from)
    }
}

impl From<ReviewIssue> for ReviewItem {
    fn from(issue: ReviewIssue) -> Self {
        Self {
            id: issue.id,
            title: issue.title,
            uuid: issue.uuid,
            group: issue.group,
            model: issue.model,
            issue_type: issue.issue_type,
            severity: ReviewSeverity::from(issue.issue_type),
            status: issue.status,
            recommendations: issue.recommendations,
            files: issue.files,
            readonly_files: issue.readonly_files,
            resolution_notes: issue.note,
            body: issue.description,
        }
    }
}

/// Projects a stored finding into its analytical view.
///
/// Every field the view models round-trips unchanged. The finding identifier
/// and the group that produced it are the identity the post-analysis layer
/// references, so they survive the projection exactly as stored.
impl From<ReviewItem> for ReviewIssue {
    fn from(item: ReviewItem) -> Self {
        Self {
            id: item.id,
            title: item.title,
            uuid: item.uuid,
            group: item.group,
            model: item.model,
            issue_type: item.issue_type,
            status: item.status,
            description: item.body,
            recommendations: item.recommendations,
            files: item.files,
            readonly_files: item.readonly_files,
            note: item.resolution_notes,
        }
    }
}
