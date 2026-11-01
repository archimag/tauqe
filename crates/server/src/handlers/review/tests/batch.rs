//! Review batch outcomes: the queue stops, the fixes it completed, and the
//! members it had to leave behind.
//!
//! Every member is executed by the engine's unified implementation pipeline, so
//! these cases assert the batch framing only: which scope reached the provider,
//! what the queue did with the verdict, and what the terminal event reports.

use std::sync::Arc;

use super::{
    architecture_edit, init_repo_with_file, make_tasks, save_grouped_session, subplan_item,
    test_config, test_item, test_session, test_state, test_state_with_provider, TestDir,
};

use super::super::batch::{
    group_member_id, run_review_batch, run_review_batch_with_options, BatchOutcome, BatchTelemetry,
    FailedMember,
};

use crate::state::AppState;
use crate::transport::OutChannel;

use tauqe_core::config::AppConfig;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{events, IssueStatus, PlanItemStatus};

/// Saves a session whose findings carry the given statuses in declaration order.
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

/// A member the queue cannot run stops it: without credentials the first turn
/// never starts, so the second member is never scheduled.
#[tokio::test]
async fn batch_fails_fast_when_a_member_cannot_run() {
    let dir = TestDir::new();
    save_session(
        &dir,
        "rev-fail-fast",
        &[IssueStatus::Todo, IssueStatus::Todo],
    );
    let state = test_state(&dir);

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

    // The engine settles the member it could not implement and never touches
    // the member the queue did not reach.
    assert_eq!(
        stored_statuses(&dir, "rev-fail-fast"),
        vec![IssueStatus::Skipped, IssueStatus::Todo]
    );
}

/// A soft stop requested before the queue starts ends it before the first member.
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
    state.soft_stop_active_operation();

    let outcome = run_review_batch(
        &state,
        "rev-soft-stop",
        make_tasks("rev-soft-stop", &[1, 2]),
        operation,
    )
    .await;

    assert_eq!(
        outcome,
        BatchOutcome::Stopped {
            completed: 0,
            telemetry: BatchTelemetry::default(),
        }
    );
    assert_eq!(
        stored_statuses(&dir, "rev-soft-stop"),
        vec![IssueStatus::Todo, IssueStatus::Todo]
    );
}

/// A group member is decoded as a group and executed through the unified
/// implementation pipeline, which closes every finding the group collects.
#[tokio::test]
async fn batch_dispatches_a_group_member_through_the_implementation_pipeline() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");
    save_grouped_session(
        &dir,
        "rev-group-run",
        "arch-1",
        vec![subplan_item(
            "s-1",
            "Step 1",
            PlanItemStatus::Todo,
            vec!["src/target.rs"],
        )],
    );

    let marker = "REVIEWGROUPRUN1";
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        architecture_edit(marker, "pub fn target() { let value = 9; }"),
    )]));
    let state = test_state_with_provider(&dir, test_config(), provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_review_batch_with_options(
        &state,
        "rev-group-run",
        vec![group_member_id("rev-group-run", "arch-1")],
        None,
        0,
        None,
        None,
        operation,
    )
    .await;

    assert_eq!(
        outcome,
        BatchOutcome::Completed {
            items: 1,
            telemetry: BatchTelemetry {
                fixed: 1,
                deferred: 0,
                failed: 0,
            },
        }
    );

    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/target.rs")).unwrap(),
        "pub fn target() { let value = 9; }\n"
    );
    let stored = ReviewStorage::load_review(dir.path(), "rev-group-run")
        .unwrap()
        .expect("the session is stored");
    assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Fixed);
    let group = stored
        .groups
        .iter()
        .find(|group| group.id == "arch-1")
        .expect("the group is stored");
    assert_eq!(group.implementation.items[0].status, PlanItemStatus::Done);
}

/// The supervisor publishes a single typed completion event for the whole batch.
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
    match &outcome {
        BatchOutcome::Failed { .. } => {}
        other => panic!("Expected Failed, got {:?}", other),
    }

    let mut raw_events = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        raw_events.push(raw);
    }
    let finished = raw_events
        .iter()
        .find(|event| event.contains(events::REVIEW_BATCH_FINISHED))
        .expect("review/batch_finished must be emitted");
    assert!(finished.contains("rev-emit"));
    // Every counter is published explicitly, so a client never has to guess
    // which figure is absent.
    assert!(finished.contains("\"fixed_count\":0"), "{finished}");
    assert!(finished.contains("\"deferred_count\":0"), "{finished}");
    assert!(finished.contains("\"failed_count\":0"), "{finished}");
}
