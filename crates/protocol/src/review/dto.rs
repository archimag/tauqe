use serde::{Deserialize, Serialize};

use crate::model::{CancelMode, ModelRef, ModelSelection};

use super::item::{IssueStatus, ReviewIssue};
use super::session::ReviewSession;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewStartParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,
    /// Optional directory or glob pattern selecting the files to review.
    ///
    /// Matched files are merged with the current User Context without mutating
    /// it: the developer keeps exclusive control of the user layer, so the
    /// selected paths are added alongside it rather than replacing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_pattern: Option<String>,
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
    #[serde(default, alias = "resolution_notes", skip_serializing_if = "Option::is_none")]
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
