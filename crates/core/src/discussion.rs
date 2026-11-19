//! Shared domain identity and session focus of a discussion.
use serde::{Deserialize, Serialize};
/// Explicit target of a discussion turn.
///
/// The discussion mode and its target are carried as typed request fields rather
/// than inferred from a magic prefix inside user-controlled prompt text, so a
/// message whose body merely resembles a discussion heading can never enter
/// discussion mode, and identifiers are transported verbatim.
///
/// The adapter also uses this identity when resolving an active dialogue focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiscussionTarget {
    /// Discussion focused on a plan, addressed by plan identifier.
    Plan { id: String },
    /// Discussion focused on a review session, addressed by session identifier.
    Review { id: String },
    /// Discussion focused on one group of a review session.
    ///
    /// An architectural group owns the root cause that blocks it, so the
    /// dialogue addresses the group by the pair of its session and its own
    /// identifier: a group identifier is unique only inside one session.
    ReviewGroup {
        session_id: String,
        group_id: String,
    },
}

impl DiscussionTarget {
    /// Session that owns the finding discussed by a review or group focus.
    pub fn review_session_id(&self) -> Option<&str> {
        match self {
            Self::Review { id } => Some(id),
            Self::ReviewGroup { session_id, .. } => Some(session_id),
            Self::Plan { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DiscussionFocus {
    #[default]
    None,
    Focused {
        target: DiscussionTarget,
        focused_items: Vec<String>,
    },
}
