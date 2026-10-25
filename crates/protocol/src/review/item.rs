use serde::{Deserialize, Deserializer, Serialize};

use crate::model::{ArchitectTier, ModelSelection};
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
    #[serde(
        default,
        alias = "resolution_notes",
        skip_serializing_if = "Option::is_none"
    )]
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
    #[serde(default, alias = "plan", skip_serializing_if = "Vec::is_empty")]
    pub subplan: Vec<PlanItem>,
    /// The Architect configuration that produced `subplan`.
    ///
    /// Planning is the *What* role's output, so the finding records which
    /// Architect model, at which reasoning depth, decomposed it. A later
    /// replanning wave reads this back instead of re-deriving a configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subplan_architect: Option<ArchitectTier>,
    /// Identifier of the sub-step whose horizon was most recently reprojected.
    ///
    /// Mirrors `Plan::last_replanned_step` for the finding's embedded subplan,
    /// so a repeated failure of the same sub-step is recognized from persisted
    /// state and escalates the Architect exactly once per failed step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subplan_last_replanned_step: Option<String>,
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
    #[serde(default, alias = "plan")]
    subplan: Vec<PlanItem>,
    #[serde(default)]
    subplan_architect: Option<ArchitectTier>,
    #[serde(default)]
    subplan_last_replanned_step: Option<String>,
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
            model: wire.model,
            issue_type: wire.issue_type,
            severity: ReviewSeverity::from(wire.issue_type),
            status: wire.status,
            recommendations: wire.recommendations,
            files: wire.files,
            readonly_files: wire.readonly_files,
            subplan: wire.subplan,
            subplan_architect: wire.subplan_architect,
            subplan_last_replanned_step: wire.subplan_last_replanned_step,
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
            issue_type: issue.issue_type,
            severity: ReviewSeverity::from(issue.issue_type),
            status: issue.status,
            recommendations: issue.recommendations,
            files: issue.files,
            readonly_files: issue.readonly_files,
            subplan: issue.plan,
            // The issue view carries no Architect provenance, so a finding
            // rebuilt from it starts without one and the caller records the
            // configuration that actually produced the plan. The replan
            // provenance is carried by the caller for the same reason.
            subplan_architect: None,
            subplan_last_replanned_step: None,
            resolution_notes: issue.note,
            body: issue.description,
        }
    }
}

/// Projects a stored finding into its analytical view.
///
/// The view carries the plan the Architect produced, but not the provenance of
/// how that plan was produced: `subplan_architect` and
/// `subplan_last_replanned_step` are deliberately dropped here. Escalation
/// state belongs to the persisted finding, never to a projection of it, so a
/// caller that persists a finding rebuilt from this view must decide what
/// provenance it keeps instead of inheriting a stale escalation marker. The
/// storage primitives carry both markers over explicitly for exactly that
/// reason.
impl From<ReviewItem> for ReviewIssue {
    fn from(item: ReviewItem) -> Self {
        Self {
            id: item.id,
            title: item.title,
            model: item.model,
            issue_type: item.issue_type,
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
