//! Group-prompt tests for architectural root-cause decomposition.
//!
//! A review group is the operational unit of remediation: one root cause is
//! fixed by one coherent change set. The tests verify that the cause, its
//! manifestations, and the plan scaffolding the translation produces all reach
//! the prompts the Architect and the sub-step turn actually render.

use super::*;
use tauqe_protocol::review::{
    IssueType, ReviewGroup, ReviewItem, ReviewItemStatus, ReviewSession, ReviewSeverity,
};
use tauqe_protocol::{EditProtocolKind, Plan, PlanItem, PlanItemStatus};

/// Builds a group whose shared root cause every prompt must carry.
fn group() -> ReviewGroup {
    ReviewGroup {
        id: "arch-1".to_string(),
        title: "Unified error handling".to_string(),
        description: Some("Every module invents its own error type.".to_string()),
        ..Default::default()
    }
}

/// Builds one manifestation of that root cause.
fn finding() -> ReviewItem {
    ReviewItem {
        id: 7,
        title: "Parser error type".to_string(),
        uuid: None,
        commit: None,
        group: Some("arch-1".to_string()),
        model: None,
        issue_type: IssueType::Bug,
        severity: ReviewSeverity::Critical,
        status: ReviewItemStatus::Todo,
        recommendations: vec!["Return a structured error".to_string()],
        files: vec!["crates/x/src/parser.rs".to_string()],
        readonly_files: vec!["crates/x/src/api.rs".to_string()],
        resolution_notes: None,
        body: "The parser returns bare strings.".to_string(),
    }
}

#[test]
fn group_prompt_carries_the_root_cause_and_every_finding() {
    let session = ReviewSession {
        id: "rev-grp".to_string(),
        title: "Grouped session".to_string(),
        ..Default::default()
    };

    let system = build_review_group_refine_system_prompt();
    assert!(system.contains("Architectural Root-Cause Group Decomposition"));
    assert!(system.contains("Final Content Invariant"));

    let user = build_review_group_refine_user_prompt(
        &session,
        &group(),
        &[finding()],
        None,
        Some("Keep the public API stable"),
    );

    assert!(user.contains("# Review Group Refinement: [rev-grp] Unified error handling"));
    assert!(user.contains("Every module invents its own error type."));
    assert!(user.contains("### Finding `#7` [BUG] Parser error type"));
    assert!(user.contains("The parser returns bare strings."));
    assert!(user.contains("Return a structured error"));
    assert!(user.contains("Keep the public API stable"));
    // The repository material of a synthesis is composed by the turn kernel of
    // the structured contour, so the builder renders none of it: the outline, the
    // loaded files, and the accumulated findings never reach this prompt.
    assert!(!user.contains("## Codebase Symbol Contours"));
    assert!(!user.contains("## Repository Directories"));
    assert!(!user.contains("## Active Context Files"));
}

#[test]
fn group_context_reaches_the_substep_prompt() {
    let mut plan = Plan::from_parts(
        "G-1".to_string(),
        "Group remediation".to_string(),
        None,
        0,
        0,
        vec![],
        vec![PlanItem {
            id: "1".to_string(),
            title: "Unify the error type".to_string(),
            status: PlanItemStatus::Todo,
            ..Default::default()
        }],
    );
    apply_group_architectural_context(&mut plan, &group(), &[finding()]);

    let prompt = crate::plan::prompt::format_plan_step_execution_prompt(
        &plan,
        "1",
        EditProtocolKind::Structured,
        Some("M7"),
    )
    .expect("sub-step prompt");

    assert!(prompt.contains("Every module invents its own error type."));
    assert!(prompt.contains("Architectural decision of review group 'Unified error handling'"));
    assert!(prompt.contains("#7 Parser error type"));
    assert!(prompt.contains("Do not replace the designed data structures"));
}

/// A group dialogue addresses the group as one operational unit: it renders the
/// cause, the blocker recorded on the group, the decision already recorded, and
/// every manifestation, and it instructs the model to record the settled
/// decision through the structured protocol.
#[test]
fn group_discussion_prompt_carries_the_cause_the_blocker_and_the_decision_contract() {
    let session = ReviewSession {
        id: "rev-grp".to_string(),
        title: "Grouped session".to_string(),
        items: vec![finding()],
        ..Default::default()
    };
    let blocked = ReviewGroup {
        blocked_reason: Some("The lock ordering must be decided first".to_string()),
        resolution_notes: Some("The cache owns the lock".to_string()),
        ..group()
    };
    let findings = [finding()];

    let prompt = format_review_group_discussion_prompt(
        &session,
        &blocked,
        &findings,
        "Which lock order do we take?",
    );

    assert!(prompt.contains("# Review Group Discussion: [rev-grp] Unified error handling"));
    assert!(prompt.contains("Every module invents its own error type."));
    assert!(prompt.contains("The lock ordering must be decided first"));
    assert!(prompt.contains("The cache owns the lock"));
    assert!(prompt.contains("Parser error type"));
    assert!(prompt.contains("Return a structured error"));
    assert!(prompt.contains("Which lock order do we take?"));

    // The settled decision travels through the structured protocol: the model
    // names the group, states the decision, and lets the harness apply it.
    assert!(prompt.contains("`review_update`"));
    assert!(prompt.contains("`group_id`"));
    assert!(prompt.contains("clears the group blocker"));
    assert!(!prompt.contains("developer applies status changes through the review interface"));
}

/// A group that carries no blocker states the absence explicitly, so the model
/// never reads a missing section as a blocker it has to guess.
#[test]
fn a_group_without_a_blocker_states_the_absence() {
    let session = ReviewSession {
        id: "rev-grp".to_string(),
        title: "Grouped session".to_string(),
        ..Default::default()
    };
    let findings = [finding()];

    let prompt =
        format_review_group_discussion_prompt(&session, &group(), &findings, "Review the cause");

    assert!(prompt.contains("**Current Blocker**: none recorded"));
    assert!(!prompt.contains("**Recorded Decision**"));
}
