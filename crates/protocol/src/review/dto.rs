use serde::{Deserialize, Serialize};

use crate::model::{CancelMode, ModelSelection};

use super::group::ArchitecturalIssueGroup;
use super::item::{IssueStatus, ReviewIssue};
use super::session::ReviewSession;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewStartParams {
    /// Reviewer selection: a semantic role, an explicit model reference, or an
    /// explicit role-list rung carrying the reasoning depth of the configured
    /// entry the developer picked.
    ///
    /// The whole selection travels on the wire, so the server never re-derives
    /// the reasoning depth by matching the model against the configured reviewer
    /// entries. A legacy client that sends only a model reference keeps
    /// decoding: the untagged `ModelSelection` form maps it to
    /// `ModelSelection::Specific`, and an absent selection runs the base rung of
    /// the reviewer role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
    /// Optional list of directory or glob patterns selecting the files to
    /// review.
    ///
    /// Coverage is the union of every entry: a repository file is audited when
    /// at least one pattern selects it, so several lines widen the audit
    /// instead of competing for it. Blank entries narrow nothing and are
    /// dropped before matching. Matched files are merged with the current User
    /// Context without mutating it: the developer keeps exclusive control of
    /// the user layer, so the selected paths are added alongside it rather
    /// than replacing it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_patterns: Vec<String>,
    /// Name of the configured review preset to launch.
    ///
    /// The server resolves the preset into the explicit groups the run audits
    /// and records the result as the session launch snapshot, so a later edit
    /// of the project configuration never rewrites a stored session. Absent for
    /// a legacy single-audit run and for a one-off custom launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
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

/// Parameters for `review/cancel`.
///
/// The `mode` field is optional so clients that predate two-mode cancellation
/// keep sending an empty object and receive the default immediate abort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCancelParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CancelMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewExecuteParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// Client-ordered finding ids limiting a session queue to the findings
    /// one rendered section shows.
    ///
    /// `None` runs every actionable finding of the session; the ids are
    /// resolved verbatim in the given order, and unknown or non-actionable
    /// ids are dropped instead of guessed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_ids: Option<Vec<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewUpdateItemParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    pub item_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<IssueStatus>,
    /// Resolution note recorded on the finding.
    ///
    /// Serialized as `note`; the legacy `resolution_notes` key is still
    /// accepted on read so older clients keep updating findings unchanged.
    #[serde(
        default,
        alias = "resolution_notes",
        skip_serializing_if = "Option::is_none"
    )]
    pub note: Option<String>,
}

/// Parameters for `review/refineItem`.
///
/// Decomposes a single finding into an embedded plan carried by
/// `ReviewItem::subplan`. Refinement is conceptually identical to plan
/// refinement: both delegate to the Architect role, so the parameter shape
/// mirrors `PlanRefineParams`. `review_id` is optional so the finding is
/// addressed in the active session when omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewRefineItemParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    pub item_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Outcome of refining a single finding into an embedded plan.
///
/// The Architect role either produces a decomposed subplan carried by the
/// updated `item`, or reports a semantic block. Tagged by `status`, mirroring
/// `PlanRefineResult`, so clients branch deterministically without inspecting
/// free-form text.
///
/// The size difference between the variants is inherent to the wire contract:
/// `Success` carries the complete decomposed finding while `Blocked` carries
/// only a reason string. Boxing the payload would add indirection for every
/// consumer without shrinking the serialized form, so the size lint is
/// allowed here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReviewRefineItemResult {
    Success {
        item: ReviewIssue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
    },
    Blocked {
        reason: String,
    },
}

impl ReviewRefineItemResult {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub fn item(&self) -> Option<&ReviewIssue> {
        match self {
            Self::Success { item, .. } => Some(item),
            Self::Blocked { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Success { .. } => None,
            Self::Blocked { reason } => Some(reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewUpdateItemResult {
    pub item: ReviewIssue,
}

/// Parameters for `review/analyze`.
///
/// Post-analysis classifies the findings of one stored session into independent
/// issues and architectural clusters. The session identifier is mandatory and
/// never defaulted: a run that addressed the active session implicitly would
/// rewrite the classification of whichever session happened to be focused, so
/// the caller always names its target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAnalyzeParams {
    /// Identifier of the stored session whose findings are classified.
    pub review_id: String,
}

/// Outcome of a completed post-analysis run.
///
/// Carries the whole session so the client renders the new layer without a
/// follow-up fetch. Only the classification layer is replaced; the findings it
/// references by identifier are immutable and are never rescanned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAnalyzeResult {
    /// The session carrying the freshly replaced post-analysis layer.
    pub session: ReviewSession,
}

/// Parameters for `review/refineGroup`.
///
/// Decomposes an architectural issue group — a cluster of findings that share
/// one root cause — into the unified subplan carried by
/// `ArchitecturalIssueGroup::subplan`. Group decomposition mirrors finding
/// refinement: both dispatch to the Architect role, so the parameter shape
/// mirrors `ReviewRefineItemParams`. `review_id` is required so the group is
/// addressed in an explicit session, never the process-global active marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewRefineGroupParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    /// Identifier of the architectural group in the session's post-analysis
    /// layer.
    pub group_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Outcome of refining an architectural issue group into a unified subplan.
///
/// The Architect role either produces the decomposed group carrying its
/// unified subplan, or reports a semantic block. Tagged by `status`, mirroring
/// `ReviewRefineItemResult`, so clients branch deterministically without
/// inspecting free-form text.
///
/// The size difference between the variants is inherent to the wire contract:
/// `Success` carries the complete decomposed group while `Blocked` carries
/// only a reason string. Boxing the payload would add indirection for every
/// consumer without shrinking the serialized form, so the size lint is
/// allowed here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReviewRefineGroupResult {
    Success {
        group: ArchitecturalIssueGroup,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
    },
    Blocked {
        reason: String,
    },
}

impl ReviewRefineGroupResult {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub fn group(&self) -> Option<&ArchitecturalIssueGroup> {
        match self {
            Self::Success { group, .. } => Some(group),
            Self::Blocked { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Success { .. } => None,
            Self::Blocked { reason } => Some(reason),
        }
    }
}
