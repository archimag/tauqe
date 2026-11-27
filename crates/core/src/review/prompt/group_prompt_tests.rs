//! Group-prompt tests for architectural root-cause decomposition.
//!
//! A review group is the operational unit of remediation: one root cause is
//! fixed by one coherent change set. The tests verify that the cause, its
//! manifestations, and the plan scaffolding the translation produces all reach
//! the prompts the Architect and the sub-step turn actually render.

use super::*;
use crate::plan::{Plan, PlanItem, PlanItemStatus};
use crate::review::{
    IssueType, ReviewGroup, ReviewGroupKind, ReviewItem, ReviewItemStatus, ReviewSession,
    ReviewSeverity,
};
use crate::workflow::EditProtocolKind;

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
        &[],
        &[],
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

    let prompt = format_review_group_discussion_prompt(
        &session,
        &group(),
        &findings,
        &[],
        &[],
        "Review the cause",
    );

    assert!(prompt.contains("**Group Blocker**: none recorded for the group"));
    assert!(prompt.contains("**Recorded Group Decision**: none recorded"));
}

#[test]
fn saved_horizon_is_shared_by_group_discussion_refinement_and_finding_discussion() {
    let mut parent = group();
    parent.kind = ReviewGroupKind::Architectural;
    parent.resolution_notes = Some("Keep <API> & callers".into());
    parent.implementation.items = vec![
        PlanItem {
            id: "done".into(),
            title: "Preserve <old>".into(),
            status: PlanItemStatus::Done,
            commit: Some("abc&123".into()),
            ..Default::default()
        },
        PlanItem {
            id: "phase".into(),
            title: "Repair phase".into(),
            children: vec![
                PlanItem {
                    id: "blocked".into(),
                    title: "Fix <parser>".into(),
                    status: PlanItemStatus::Blocked,
                    details: Some("Handle <invalid> & retry".into()),
                    blocked_reason: Some("Cannot <compile> & link".into()),
                    files: vec!["src/<parser>.rs".into()],
                    readonly_files: vec!["src/api.rs".into()],
                    outer_docs: vec!["docs/ref".into()],
                    concept_refs: vec!["c-1".into()],
                    depends_on: vec!["done".into()],
                    ..Default::default()
                },
                PlanItem {
                    id: "silent".into(),
                    status: PlanItemStatus::Blocked,
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
    ];
    let other = ReviewGroup {
        id: "other".into(),
        kind: ReviewGroupKind::Architectural,
        blocked_reason: Some("Unrelated blocker".into()),
        ..Default::default()
    };
    let mut second = finding();
    second.id = 8;
    let session = ReviewSession {
        id: "rev-grp".into(),
        items: vec![finding(), second.clone()],
        groups: vec![parent.clone(), other],
        ..Default::default()
    };
    let group_discussion = format_review_group_discussion_prompt(
        &session,
        &parent,
        &session.items,
        &[],
        &[],
        "Question",
    );
    let refine =
        build_review_group_refine_user_prompt(&session, &parent, &session.items, None, None);
    let focused = format_review_discussion_prompt_items(&session, &[7, 8], "Question");
    for prompt in [&group_discussion, &refine, &focused] {
        for fact in [
            "none recorded for the group",
            "#done",
            "abc&amp;123",
            "#phase",
            "#blocked",
            "Fix &lt;parser&gt;",
            "Handle &lt;invalid&gt; &amp; retry",
            "Cannot &lt;compile&gt; &amp; link",
            "src/&lt;parser&gt;.rs",
            "src/api.rs",
            "docs/ref",
            "c-1",
            "Depends on: `done`",
            "no saved reason for this blocked step",
            "Keep &lt;API&gt; &amp; callers",
        ] {
            assert!(prompt.contains(fact), "missing {fact}");
        }
    }
    assert_eq!(focused.matches("## Architectural Parent Group:").count(), 1);
    assert!(!focused.contains("Unrelated blocker"));
    assert!(
        !format_review_discussion_prompt_items(&session, &[], "Question")
            .contains("Saved Group Implementation")
    );
}

#[test]
fn empty_group_horizon_is_explicit_in_both_prompts() {
    let group = group();
    let session = ReviewSession::default();
    assert!(
        format_review_group_discussion_prompt(&session, &group, &[], &[], &[], "Question")
            .contains("No implementation steps saved.")
    );
    assert!(
        build_review_group_refine_user_prompt(&session, &group, &[], None, None)
            .contains("No implementation steps saved.")
    );
}

#[test]
fn refinement_distinguishes_closed_findings_and_escapes_saved_diagnostics() {
    let mut parent = group();
    parent.id = "arch<&>".into();
    parent.kind = ReviewGroupKind::Architectural;
    parent.title = "Root <cause> & fix".into();
    parent.implementation.items = vec![PlanItem {
        id: "phase".into(),
        title: "Phase <one>".into(),
        children: vec![PlanItem {
            id: "blocked".into(),
            title: "Fix <parser>".into(),
            status: PlanItemStatus::Blocked,
            blocked_reason: Some("Cannot <compile> & link".into()),
            ..Default::default()
        }],
        ..Default::default()
    }];
    let mut open = finding();
    open.group = Some(parent.id.clone());
    open.status = ReviewItemStatus::Discussion;
    let mut fixed = finding();
    fixed.id = 8;
    fixed.status = ReviewItemStatus::Fixed;
    let mut skipped = finding();
    skipped.id = 9;
    skipped.status = ReviewItemStatus::Skipped;
    let mut rejected = finding();
    rejected.id = 10;
    rejected.status = ReviewItemStatus::Rejected;
    let findings = vec![open, fixed, skipped, rejected];
    let session = ReviewSession {
        id: "rev<&>".into(),
        title: "Audit <API> & callers".into(),
        description: Some("Scope <xml> & data".into()),
        items: findings.clone(),
        groups: vec![parent.clone()],
        ..Default::default()
    };
    let discussion =
        format_review_group_discussion_prompt(&session, &parent, &findings, &[], &[], "Question");
    let focused = format_review_discussion_prompt_items(&session, &[7], "Question");
    let refine = build_review_group_refine_user_prompt(&session, &parent, &findings, None, None);
    let diagnostics = render_group_implementation(&parent);
    for prompt in [&discussion, &focused, &refine, &diagnostics] {
        assert!(prompt.contains("none recorded for the group"));
        assert!(prompt.contains("#blocked"));
        assert!(prompt.contains("Cannot &lt;compile&gt; &amp; link"));
        assert!(prompt.contains("Phase &lt;one&gt;"));
    }
    for (id, status) in [
        (7, "DISCUSSION"),
        (8, "FIXED"),
        (9, "SKIPPED"),
        (10, "REJECTED"),
    ] {
        assert!(refine.contains(&format!(
            "Finding `#{id}` [BUG] Parser error type (status: {status})"
        )));
    }
    assert!(refine.contains("historical context, not work to repeat"));
    assert!(refine.contains("Audit &lt;API&gt; &amp; callers (rev&lt;&amp;&gt;)"));
    assert!(refine.contains("Scope &lt;xml&gt; &amp; data"));
    assert!(refine.contains("**ID**: `arch&lt;&amp;&gt;`"));
}
