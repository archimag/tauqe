//! Ordered, fail-fast traversal of a review batch.
//!
//! The supervisor runs the findings of one session in the order the client
//! listed them and stops at the first infrastructure failure, so a finding
//! whose turn could not even start must never keep the transient
//! `InProgress` status after the batch has terminated.
//!
//! A member is dispatched by its identifier: a group member reaches the
//! group walk through `group_exec`, and a finding member the isolated
//! finding turn, so a group is never executed as a finding and vice versa.

use std::sync::Arc;

use super::{make_tasks, test_item, test_session, test_state, TestDir};

use super::super::batch::{
    run_review_batch, run_review_batch_with_options, BatchOutcome, FailedMember,
};
use super::super::group_exec::group_step_id;

use crate::state::AppState;
use crate::transport::OutChannel;

use tauqe_core::config::AppConfig;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{events, IssueStatus};

/// Saves a session whose findings carry the given statuses in declaration
/// order.
fn save_session(dir: &TestDir, review_id: &str, statuses: &[IssueStatus]) {
    let items = statuses
        .iter()
        .enumerate()
        .map(|(index, status)| test_item(index as u32 + 1, *status, "src/target.rs"))
        .collect::<Vec<_>>();
    ReviewStorage::save_session(dir.path(), &test_session(review_id, items)).unwrap();
}

/// Reads the persisted statuses of a stored session.
fn stored_statuses(dir: &TestDir, review_id: &str) -> Vec<IssueStatus> {
    ReviewStorage::load_review(dir.path(), review_id)
        .unwrap()
        .expect("the session is stored")
        .items
        .iter()
        .map(|item| item.status)
        .collect()
}

#[tokio::test]
async fn batch_fails_fast_and_returns_the_started_finding_to_todo() {
    let dir = TestDir::new();
    save_session(
        &dir,
        "rev-fail-fast",
        &[IssueStatus::Todo, IssueStatus::Todo],
    );
    let state = test_state(&dir);

    // Without credentials the first finding's turn cannot start, so the batch
    // must stop there instead of scheduling the second one.
    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_review_batch(
        &state,
        "rev-fail-fast",
        make_tasks("rev-fail-fast", &[1, 2]),
        operation,
    )
    .await;

    match outcome {
        BatchOutcome::Failed {
            completed, member, ..
        } => {
            assert_eq!(completed, 0);
            assert_eq!(
                member,
                FailedMember::Finding {
                    session_id: "rev-fail-fast".to_string(),
                    item_id: 1,
                }
            );
        }
        other => panic!("Expected Failed, got {:?}", other),
    }
    assert_eq!(
        stored_statuses(&dir, "rev-fail-fast"),
        vec![IssueStatus::Todo, IssueStatus::Todo]
    );
}

#[tokio::test]
async fn batch_skips_a_finished_finding_and_stops_on_a_rejected_one() {
    let dir = TestDir::new();
    save_session(
        &dir,
        "rev-skip",
        &[IssueStatus::Fixed, IssueStatus::Rejected],
    );
    let state = test_state(&dir);

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_review_batch(
        &state,
        "rev-skip",
        make_tasks("rev-skip", &[1, 2]),
        operation,
    )
    .await;

    match outcome {
        BatchOutcome::Failed {
            completed,
            member,
            reason,
        } => {
            assert_eq!(completed, 1);
            assert_eq!(
                member,
                FailedMember::Finding {
                    session_id: "rev-skip".to_string(),
                    item_id: 2,
                }
            );
            assert!(reason.contains("REJECTED"), "unexpected reason: {reason}");
        }
        other => panic!("Expected Failed, got {:?}", other),
    }
}

#[tokio::test]
async fn batch_emits_a_single_finished_event() {
    let dir = TestDir::new();
    save_session(&dir, "rev-emit", &[IssueStatus::Todo, IssueStatus::Todo]);
    let (out, mut rx) = OutChannel::new();
    let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        AppConfig::default(),
        dir.path().to_path_buf(),
        history_sender,
        out,
    );

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_review_batch(
        &state,
        "rev-emit",
        make_tasks("rev-emit", &[1, 2]),
        operation,
    )
    .await;
    assert!(matches!(outcome, BatchOutcome::Failed { .. }));

    // The supervisor publishes a single typed completion event for the whole
    // batch, carrying the aggregate status and the owning session.
    let mut raw_events = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        raw_events.push(raw);
    }
    let finished = raw_events
        .iter()
        .find(|event| event.contains(events::REVIEW_BATCH_FINISHED))
        .expect("review/batch_finished must be emitted");
    assert!(finished.contains("rev-emit"));
    assert!(finished.contains("\"failed\""));
}

#[tokio::test]
async fn batch_stops_gracefully_on_a_soft_stop() {
    let dir = TestDir::new();
    save_session(
        &dir,
        "rev-soft-stop",
        &[IssueStatus::Todo, IssueStatus::Todo],
    );
    let state = test_state(&dir);

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    // A soft stop requested before the queue starts ends it before the first
    // finding runs.
    state.soft_stop_active_operation();

    let outcome = run_review_batch(
        &state,
        "rev-soft-stop",
        make_tasks("rev-soft-stop", &[1, 2]),
        operation,
    )
    .await;

    assert_eq!(outcome, BatchOutcome::Stopped { completed: 0 });
    assert_eq!(
        stored_statuses(&dir, "rev-soft-stop"),
        vec![IssueStatus::Todo, IssueStatus::Todo]
    );
}

#[tokio::test]
async fn batch_dispatches_a_group_member_through_the_group_walk() {
    let dir = TestDir::new();
    save_session(&dir, "rev-groups", &[IssueStatus::Todo]);
    let state = test_state(&dir);

    // The session exists but the group does not: the executor must decode the
    // member as a group, fail to reload it from the durable record, and
    // report the failed step instead of executing a finding as if the member
    // were one.
    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_review_batch_with_options(
        &state,
        "rev-groups",
        vec![group_step_id("rev-groups", "ghost-group")],
        None,
        None,
        None,
        operation,
    )
    .await;

    match outcome {
        BatchOutcome::Failed {
            completed,
            member,
            reason,
        } => {
            assert_eq!(completed, 0);
            // A group member carries no finding ordinal, so the failure must
            // name the group instead of reporting a finding number.
            assert_eq!(
                member,
                FailedMember::Group {
                    session_id: "rev-groups".to_string(),
                    group_id: "ghost-group".to_string(),
                }
            );
            assert!(
                reason.contains("Review group 'ghost-group' not found"),
                "unexpected reason: {reason}"
            );
        }
        other => panic!("Expected Failed, got {:?}", other),
    }
}
