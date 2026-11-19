//! Model-facing resolution of a review finding.

use serde::{Deserialize, Serialize};

/// Status a review discussion may move a finding to once the engineer decided.
///
/// The vocabulary is deliberately narrower than the stored finding lifecycle: a discussion
/// records what the engineer decided, so it may queue a finding for execution
/// (`todo`), dismiss it as not reproducible (`skipped`), or reject it as a false
/// positive (`rejected`). It can never declare a finding `fixed`, because a fix
/// becomes true only after the harness verifies it and commits the change, and it
/// can never start one, because execution belongs to the finding workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecisionStatus {
    /// The finding must be executed.
    Todo,
    /// The finding is not reproducible or was already addressed.
    Skipped,
    /// The finding is a false positive or an accepted trade-off.
    Rejected,
}

impl ReviewDecisionStatus {
    /// Wire spelling of the decision, identical to its serialized form.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Skipped => "skipped",
            Self::Rejected => "rejected",
        }
    }
}

/// Decision recorded from a discussion of one review finding.
///
/// The dialogue is where an engineer actually decides what a finding is worth:
/// whether it is a real defect that must be executed, whether it is not
/// reproducible, or whether it is a false positive. Recording that decision in
/// the dialogue payload keeps the reasoning and the resulting state transition in
/// one turn, instead of leaving the developer to re-enter the outcome by hand in
/// the review interface afterwards.
///
/// A decision is not an edit: it never carries the fix itself. The fix travels
/// through the ordinary finding workflow, which verifies the change and owns the
/// transition to `fixed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewUpdateProposal {
    /// Identifier of the finding inside the discussed review session.
    ///
    /// The strict discussion schema publishes the domain of this field as the
    /// `minimum` and the `maximum` of its `item_id` property, so the contract
    /// the model reads admits exactly the values this decoder accepts. An
    /// unbounded property would let one out-of-range identifier discard the
    /// whole response, the conversational message included.
    pub item_id: u32,
    /// Identifier of the review group the decision is recorded for.
    ///
    /// A decision taken for an architectural group belongs to the group, not to
    /// one of its manifestations: the dialogue names the group here alongside
    /// the finding whose text carried the discussion, so the turn records the
    /// resolution once on the group instead of once per symptom. Null when the
    /// decision addresses the finding alone.
    #[serde(default)]
    pub(crate) group_id: Option<String>,
    /// Status the finding moves to once the decision is applied.
    pub status: ReviewDecisionStatus,
    /// The decision itself, in the engineer's words.
    ///
    /// Serialized as `note`, the single canonical wire name of the datum shared
    /// with the stored finding.
    pub note: String,
    /// Additional remediation recommendations agreed in the discussion.
    #[serde(default)]
    pub(crate) recommendations: Vec<String>,
}

impl ReviewUpdateProposal {
    pub(crate) const ITEM_ID_MINIMUM: u32 = u32::MIN;
    pub(crate) const ITEM_ID_MAXIMUM: u32 = u32::MAX;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_review_update_proposal_serde() {
        let update = ReviewUpdateProposal {
            item_id: 3,
            group_id: Some("arch-1".to_string()),
            status: ReviewDecisionStatus::Todo,
            note: "The lock must be held across the turn".to_string(),
            recommendations: vec!["Extend the guard".to_string()],
        };
        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains("\"status\":\"todo\""), "got: {json}");
        assert!(json.contains("\"group_id\":\"arch-1\""), "got: {json}");
        assert!(
            json.contains("\"note\":\"The lock must be held across the turn\""),
            "got: {json}"
        );
        let de: ReviewUpdateProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(de, update);

        // The decision, the status it moves the finding to, and the group it is
        // recorded for are the payload; only the recommendations may be absent.
        let minimal = r#"{"item_id":3,"status":"skipped","note":"Not reproducible"}"#;
        let de: ReviewUpdateProposal = serde_json::from_str(minimal).unwrap();
        assert_eq!(de.status, ReviewDecisionStatus::Skipped);
        assert!(de.recommendations.is_empty());
        assert!(de.group_id.is_none());
        assert!(
            serde_json::from_str::<ReviewUpdateProposal>(r#"{"item_id":3,"status":"todo"}"#)
                .is_err()
        );
    }
}
