use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewExecuteTarget {
    /// Every actionable finding of every stored review session.
    AllSessions,
    /// The implicit queue of one whole review session.
    Session(ReviewSessionTarget),
    /// One finding of one review session.
    Finding(ReviewFindingTarget),
    /// A client-ordered selection of findings of one review session.
    Findings(ReviewFindingsTarget),
    /// The unified implementation horizon of one architectural review group.
    Group(ReviewGroupTarget),
}

impl ReviewExecuteTarget {
    /// Session the target addresses, or `None` for the cross-session run.
    ///
    /// Only [`Self::AllSessions`] names no session, so a consumer that needs
    /// the session identifier reads it here instead of matching every variant
    /// on its own.
    pub fn review_id(&self) -> Option<&str> {
        match self {
            Self::AllSessions => None,
            Self::Session(target) => Some(&target.review_id),
            Self::Finding(target) => Some(&target.review_id),
            Self::Findings(target) => Some(&target.review_id),
            Self::Group(target) => Some(&target.review_id),
        }
    }
}

/// Whole-session target: the implicit queue of one stored session.
///
/// The payload refuses a field it does not declare, so a request that still
/// names a finding, a selection, a group, or a retired sub-step fails to
/// decode instead of being silently reduced to the session queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSessionTarget {
    /// Identifier of the stored session whose queue is executed.
    pub review_id: String,
}

/// Single-finding target.
///
/// The payload refuses a field it does not declare, so a finding never travels
/// beside an explicit selection, a group identifier, or a retired sub-step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingTarget {
    /// Identifier of the stored session that declares the finding.
    pub review_id: String,
    /// Audit ordinal of the finding inside that session.
    pub item_id: u32,
}

/// Client-ordered selection of findings of one session.
///
/// The ids resolve verbatim in the given order, and unknown or non-actionable
/// ids are dropped instead of being guessed at. The payload refuses a field it
/// does not declare, so a selection never travels beside a single finding, a
/// group identifier, or a retired sub-step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingsTarget {
    /// Identifier of the stored session whose findings are executed.
    pub review_id: String,
    /// Finding ids in client order.
    pub item_ids: Vec<u32>,
}

/// Unified-horizon target of one architectural review group.
///
/// A group owns a single root-cause plan, so the target names the group itself
/// and never one of the findings it collects. The payload refuses a field it
/// does not declare, so a group never travels beside a finding, a selection, or
/// a retired sub-step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewGroupTarget {
    /// Identifier of the stored session that declares the group.
    pub review_id: String,
    /// Identifier of the group inside that session.
    pub group_id: String,
}
