//! Unit tests for the model turn subsystem.
//!
//! The tests pin the deterministic decisions taken before any provider request
//! is launched: which turn intent a typed discussion target establishes and
//! whether a turn binds the interactive dialogue session. Every assertion
//! targets a pure function, so the suite needs no `AppState`, no provider, and
//! no repository.

use tauqe_protocol::{DiscussionTarget, TurnIntent};

use super::prompt::intent_for_target;
use super::turn::resolve_session_id;

/// A plan discussion is strictly read-only: the model reasons about the plan
/// and never touches files.
#[test]
fn plan_discussion_is_readonly() {
    let intent = intent_for_target(&DiscussionTarget::Plan {
        id: "plan-1".to_string(),
    });
    assert!(intent.is_readonly_discussion());
    assert!(!intent.is_interactive_repl());
}

/// A review finding dialogue stays interactive, because the developer may ask
/// the model to apply the fix directly.
#[test]
fn review_discussion_stays_interactive() {
    let intent = intent_for_target(&DiscussionTarget::Review {
        id: "review-1".to_string(),
    });
    assert!(intent.is_interactive_repl());
    assert!(!intent.is_readonly_discussion());
}

/// An isolated step never inherits the interactive session, so it cannot reuse
/// a prompt-prefix cache built from the dialogue transcript.
#[test]
fn isolated_step_carries_no_dialogue_session() {
    assert_eq!(
        resolve_session_id(TurnIntent::IsolatedStep, "session-1".to_string()),
        None
    );
}

/// A dialogue turn binds the interactive session, so successive rounds reuse the
/// backend's cached prompt prefix.
#[test]
fn interactive_turn_binds_the_dialogue_session() {
    assert!(resolve_session_id(TurnIntent::InteractiveRepl, "session-1".to_string()).is_some());
    assert!(resolve_session_id(TurnIntent::ReadonlyDiscussion, "session-1".to_string()).is_some());
}
