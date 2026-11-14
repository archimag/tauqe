//! Review batch outcomes: the queue stops, the fixes it completed, and the
//! members it had to leave behind.
//!
//! Every member is executed by the engine's unified implementation pipeline, so
//! these cases assert the batch framing only: which scope reached the provider,
//! what the queue did with the verdict, and what the terminal event reports.

use std::sync::Arc;

use super::{
    init_repo_with_file, make_tasks, save_grouped_session, subplan_item, test_config, test_item,
    test_session, test_state, test_state_with_provider, TestDir,
};

use super::super::batch::{
    group_member_id, run_review_batch, run_review_batch_with_options, BatchOutcome, BatchTelemetry,
    FailedMember,
};

use crate::state::AppState;
use crate::transport::OutChannel;

use tauqe_core::config::AppConfig;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::providers::LlmProvider;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::model::TurnWorkContext;
use tauqe_protocol::{
    events, IssueStatus, ModelEntry, ModelRef, ModelStartedEvent, PlanItemStatus,
    ReviewUpdatedEvent,
};

use tokio::sync::mpsc;

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
        .into_iter()
        .map(|item| item.status)
        .collect()
}

fn json_edit_response(content: &str) -> String {
    serde_json::json!({
        "message": "Applied architecture fix",
        "action": {
            "kind": "edit",
            "changes": [
                {
                    "op": "replace",
                    "path": "src/target.rs",
                    "to": "",
                    "start": "",
                    "end": "",
                    "old_text": "pub fn target() {}\n",
                    "new_text": format!("{content}\n"),
                    "content": ""
                }
            ],
            "commit_message": "Fix architecture issue",
            "message": "",
            "reason": ""
        },
        "scratchpad": ""
    })
    .to_string()
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

    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        json_edit_response("pub fn target() { let value = 9; }"),
    )]));
    let state = test_state_with_provider(&dir, test_config(), provider).await;

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

/// One server event captured from the outbound channel.
///
/// The batch is driven to completion before the channel is drained, so the
/// captured order is the order of publication.
struct CapturedEvent {
    method: String,
    params: serde_json::Value,
}

/// Builds a state whose outbound channel is owned by the test.
///
/// A deterministic provider is injected through the state's test seam and the
/// turn marker is pinned, so the scripted protocol payload addresses the exact
/// tags the batch emits. The returned receiver keeps every published event, and
/// the test drains it once the batch has finished.
async fn event_state(
    dir: &TestDir,
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    _marker: &str,
) -> (Arc<AppState>, mpsc::UnboundedReceiver<String>) {
    let (out, rx) = OutChannel::new();
    let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), history_sender, out);
    *state.provider_override.lock().await = Some(provider);
    (state, rx)
}

/// Drains every queued event, preserving the order of publication.
fn drain_events(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<CapturedEvent> {
    let mut captured = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        let value: serde_json::Value =
            serde_json::from_str(raw.trim()).expect("every published line is a JSON event");
        let method = value
            .get("method")
            .and_then(serde_json::Value::as_str)
            .expect("a published event names its method")
            .to_string();
        let params = value.get("params").cloned().unwrap_or_default();
        captured.push(CapturedEvent { method, params });
    }
    captured
}

/// Decodes the payload of a captured `model/started` event.
fn model_started(event: &CapturedEvent) -> ModelStartedEvent {
    serde_json::from_value(event.params.clone())
        .expect("model/started carries a typed ModelStartedEvent")
}

/// Decodes the payload of a captured `review/updated` event.
fn review_updated(event: &CapturedEvent) -> ReviewUpdatedEvent {
    serde_json::from_value(event.params.clone()).expect("review/updated carries a typed session")
}

/// A single finding executes as one bounded turn, so the client observes its
/// whole lifecycle: the session published with `InProgress` before the model
/// starts, the declared work context that names the running finding, and the
/// settled verdict published after the turn.
#[tokio::test]
async fn batch_publishes_the_lifecycle_of_a_single_finding_run() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");
    save_session(&dir, "rev-lifecycle", &[IssueStatus::Todo]);

    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        json_edit_response("pub fn target() { let value = 9; }"),
    )]));
    let (state, mut event_rx) = event_state(&dir, test_config(), provider, "").await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_review_batch_with_options(
        &state,
        "rev-lifecycle",
        vec!["rev-lifecycle:1".to_string()],
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

    let captured = drain_events(&mut event_rx);

    // The executor writes `InProgress` before it asks for the turn, and the
    // host publishes that state before the model starts.
    let started = captured
        .iter()
        .position(|event| event.method == events::MODEL_STARTED)
        .expect("model/started must be published");
    let in_progress = captured[..started]
        .iter()
        .filter(|event| event.method == events::REVIEW_UPDATED)
        .map(review_updated)
        .next_back()
        .expect("review/updated must precede model/started");
    let observed = in_progress
        .session
        .find_item(1)
        .expect("the finding is stored");
    assert_eq!(observed.status, IssueStatus::InProgress);

    // The turn declares the work it executes instead of leaving the client to
    // reconstruct it from the stored session.
    let started_event = model_started(&captured[started]);
    assert!(
        started_event.is_batch,
        "a batch step reports itself as a member of a batch"
    );
    match started_event
        .work_context
        .expect("model/started declares the running work")
    {
        TurnWorkContext::ReviewStep {
            review_id,
            review_title,
            queue,
        } => {
            assert_eq!(review_id, "rev-lifecycle");
            // The frame carries the horizon title, so the review is addressed
            // by the identifier the client can resolve.
            assert_eq!(review_title, "rev-lifecycle");
            assert_eq!(queue.active_index, 0);
            assert_eq!(queue.items.len(), 1);
            assert_eq!(queue.items[0].id, "1");
            assert_eq!(queue.items[0].title, "Finding 1");
        }
        other => panic!("unexpected work context: {other:?}"),
    }

    // The verdict is published once the turn settled, after the model finished.
    let finished = captured
        .iter()
        .position(|event| event.method == events::MODEL_FINISHED)
        .expect("model/finished must be published");
    let settled = captured[finished + 1..]
        .iter()
        .filter(|event| event.method == events::REVIEW_UPDATED)
        .map(review_updated)
        .next()
        .expect("review/updated must follow model/finished");
    let verdict = settled.session.find_item(1).expect("the finding is stored");
    assert_eq!(verdict.status, IssueStatus::Fixed);
    assert!(
        verdict
            .resolution_notes
            .as_deref()
            .is_some_and(|note| note.contains("Addressed by the implementation run")),
        "the settlement must state why the finding became fixed: {:?}",
        verdict.resolution_notes
    );

    // The published verdict is the persisted one.
    let stored = ReviewStorage::load_review(dir.path(), "rev-lifecycle")
        .unwrap()
        .expect("the session is stored");
    let persisted = stored.find_item(1).expect("the finding is stored");
    assert_eq!(persisted.status, IssueStatus::Fixed);
}
/// A group member executes its unified horizon as one bounded turn, so the
/// client observes the same lifecycle the single-finding run publishes: the
/// session that states the taken step while the horizon runs, the declared
/// group work context, and the settled verdict that closes the findings the
/// horizon manifests.
#[tokio::test]
async fn batch_publishes_the_lifecycle_of_a_group_member_run() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");
    save_grouped_session(
        &dir,
        "rev-group-lifecycle",
        "arch-1",
        vec![subplan_item(
            "s-1",
            "Step 1",
            PlanItemStatus::Todo,
            vec!["src/target.rs"],
        )],
    );

    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        json_edit_response("pub fn target() { let value = 9; }"),
    )]));
    let (state, mut event_rx) = event_state(&dir, test_config(), provider, "").await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_review_batch_with_options(
        &state,
        "rev-group-lifecycle",
        vec![group_member_id("rev-group-lifecycle", "arch-1")],
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

    let captured = drain_events(&mut event_rx);

    // The executor takes the group step `InProgress` before it asks for the
    // turn, and the findings stay frozen while the horizon runs.
    let started = captured
        .iter()
        .position(|event| event.method == events::MODEL_STARTED)
        .expect("model/started must be published");
    let in_progress = captured[..started]
        .iter()
        .filter(|event| event.method == events::REVIEW_UPDATED)
        .map(review_updated)
        .next_back()
        .expect("review/updated must precede model/started");
    let running_group = in_progress
        .session
        .groups
        .iter()
        .find(|group| group.id == "arch-1")
        .expect("the group is stored");
    assert_eq!(
        running_group.implementation.items[0].status,
        PlanItemStatus::InProgress
    );
    let pending_finding = in_progress
        .session
        .find_item(1)
        .expect("the finding is stored");
    assert_eq!(pending_finding.status, IssueStatus::Todo);

    // The turn declares the group horizon it executes instead of leaving the
    // client to reconstruct it from the stored session.
    let started_event = model_started(&captured[started]);
    match started_event
        .work_context
        .expect("model/started declares the running work")
    {
        TurnWorkContext::ReviewGroupStep {
            review_id,
            review_title,
            group_id,
            group_title,
            queue,
        } => {
            assert_eq!(review_id, "rev-group-lifecycle");
            assert_eq!(review_title, "rev-group-lifecycle");
            assert_eq!(group_id, "arch-1");
            // The frame the executor resolves carries the group title as the
            // horizon title, so the step names the group it executes.
            assert_eq!(group_title, "Group arch-1");
            assert_eq!(queue.active_index, 0);
            assert_eq!(queue.items.len(), 1);
            assert_eq!(queue.items[0].id, "s-1");
            assert_eq!(queue.items[0].title, "Step 1");
        }
        other => panic!("unexpected work context: {other:?}"),
    }

    // The verdict is published once the horizon settled, after the model
    // finished: the completed horizon closes the findings it manifests.
    let finished = captured
        .iter()
        .position(|event| event.method == events::MODEL_FINISHED)
        .expect("model/finished must be published");
    let settled = captured[finished + 1..]
        .iter()
        .filter(|event| event.method == events::REVIEW_UPDATED)
        .map(review_updated)
        .next()
        .expect("review/updated must follow model/finished");
    let settled_group = settled
        .session
        .groups
        .iter()
        .find(|group| group.id == "arch-1")
        .expect("the group is stored");
    assert_eq!(
        settled_group.implementation.items[0].status,
        PlanItemStatus::Done
    );
    let verdict = settled.session.find_item(1).expect("the finding is stored");
    assert_eq!(verdict.status, IssueStatus::Fixed);
    assert!(
        verdict
            .resolution_notes
            .as_deref()
            .is_some_and(|note| note.contains("Resolved by the completed implementation")),
        "the settlement must state why the finding became fixed: {:?}",
        verdict.resolution_notes
    );

    // The published verdicts are the persisted ones.
    let stored = ReviewStorage::load_review(dir.path(), "rev-group-lifecycle")
        .unwrap()
        .expect("the session is stored");
    let persisted_group = stored
        .groups
        .iter()
        .find(|group| group.id == "arch-1")
        .expect("the group is stored");
    assert_eq!(
        persisted_group.implementation.items[0].status,
        PlanItemStatus::Done
    );
    assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Fixed);
}
/// A coder ladder exhausted by a single plain-text attempt ends the run, and
/// the finding is settled as `Skipped` with the diagnostic of that attempt.
#[tokio::test]
async fn batch_publishes_the_skipped_verdict_of_an_exhausted_run() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");
    save_session(&dir, "rev-exhausted", &[IssueStatus::Todo]);

    let marker = "REVIEWEXHAUSTED1";
    // A plain-text answer carries no edit and no terminal success tag, so the
    // step cannot satisfy its contract.
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        "I could not determine a safe change to apply.",
    )]));
    // One rung with a single attempt: the first content failure exhausts the
    // ladder instead of retrying it on the same model.
    let mut config = test_config();
    let mut coder = ModelEntry::new(ModelRef::openrouter("base/model"), None);
    coder.attempts = Some(1);
    config.models.coder = Some(vec![coder]);
    let (state, mut event_rx) = event_state(&dir, config, provider, marker).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_review_batch_with_options(
        &state,
        "rev-exhausted",
        vec!["rev-exhausted:1".to_string()],
        None,
        0,
        None,
        None,
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
                    session_id: "rev-exhausted".to_string(),
                    item_id: 1,
                }
            );
        }
        other => panic!("Expected Failed, got {:?}", other),
    }

    // The settled verdict is the last state the batch publishes for the
    // session: every lifecycle transition of the member precedes it.
    let captured = drain_events(&mut event_rx);
    let settled = captured
        .iter()
        .filter(|event| event.method == events::REVIEW_UPDATED)
        .map(review_updated)
        .next_back()
        .expect("review/updated must be published");
    let skipped = settled.session.find_item(1).expect("the finding is stored");
    assert_eq!(skipped.status, IssueStatus::Skipped);
    let note = skipped.resolution_notes.as_deref().unwrap_or_default();
    let diagnostic = note
        .strip_prefix("The implementation run could not address this finding: ")
        .expect("the settlement states why the finding was skipped");
    assert!(
        !diagnostic.trim().is_empty(),
        "the settlement must keep the diagnostic of the failed run: {note}"
    );

    // The published verdict is the persisted one.
    let stored = ReviewStorage::load_review(dir.path(), "rev-exhausted")
        .unwrap()
        .expect("the session is stored");
    let persisted = stored.find_item(1).expect("the finding is stored");
    assert_eq!(persisted.status, IssueStatus::Skipped);
}
