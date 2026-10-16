use serde::{Deserialize, Serialize};

use crate::model::ModelSelection;
use crate::plan::{count_leaf_stats, PlanItem};

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
    /// Legacy `done` alias retained so review sessions written before the
    /// rename stay readable; scheduled for removal in 0.2.0 and tracked by the
    /// wire-contract test in `lib.rs`.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ReviewItemRaw")]
pub struct ReviewItem {
    pub id: u32,
    pub title: String,
    /// Model tier or explicit model assigned to resolve this finding. `None` means the default tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    pub severity: ReviewSeverity,
    pub status: ReviewItemStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Embedded step plan that decomposes this finding into traceable sub-steps.
    ///
    /// The subplan reuses [`PlanItem`] verbatim, so sub-step scheduling,
    /// dependency evaluation, and status computation share the plan engine's
    /// single source of truth. An empty vector means the finding carries no
    /// decomposition.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subplan: Vec<PlanItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_notes: Option<String>,
    pub body: String,
}

impl ReviewItem {
    /// Returns `true` when the finding carries an embedded step plan.
    pub fn has_subplan(&self) -> bool {
        !self.subplan.is_empty()
    }

    /// Counts executable leaves in the embedded subplan as `(total, completed)`.
    ///
    /// Delegates to [`crate::plan::count_leaf_stats`], the plan engine's single
    /// authoritative progress metric, so a finding's subplan progress can never
    /// drift from a full plan's progress.
    pub fn subplan_stats(&self) -> (usize, usize) {
        count_leaf_stats(&self.subplan)
    }
}

/// Legacy on-disk shape of a review finding.
///
/// Loading goes through this raw shape so sessions written by earlier versions
/// stay readable. The obsolete `line_range` location field is no longer parsed:
/// findings now reference whole files through `files`, so the line range is
/// intentionally dropped during migration instead of being read and silently
/// discarded.
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
    /// Legacy single-file location folded into `files` on load.
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub subplan: Vec<PlanItem>,
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
            subplan: raw.subplan,
            resolution_notes: raw.resolution_notes,
            body: raw.body,
        }
    }
}
