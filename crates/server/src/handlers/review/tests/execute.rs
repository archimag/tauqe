//! `review/execute`: request validation and queue resolution before any turn.
//!
//! Refusals happen before a member starts and leave no operation registered.

use super::{
    architectural_session, horizon_config, horizon_leaf, horizon_response, init_repo_with_file,
    test_item, test_session, test_state, TestDir,
};

use super::super::handle_review_execute;

use std::sync::Arc;
use tauqe_core::plan::PlanItemStatus;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::review::IssueStatus;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{errors, Request};

pub(super) async fn execute_and_collect(
    state: &std::sync::Arc<crate::state::AppState>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    request: Request,
) -> Vec<tauqe_protocol::Event> {
    use tauqe_protocol::{events, Event};

    let response = handle_review_execute(request, state).await;
    assert!(
        response.error.is_none(),
        "review execution refused: {response:?}"
    );
    let result = response.result.expect("review execution is acknowledged");
    let operation_id = result["operation_id"].as_str().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut published = Vec::new();
        loop {
            let raw = rx
                .recv()
                .await
                .expect("the review event channel remains open");
            let event: Event = serde_json::from_str(&raw).unwrap();
            let finished = event.method == events::REVIEW_BATCH_FINISHED;
            if finished {
                assert_eq!(event.params.as_ref().unwrap()["operation_id"], operation_id);
            }
            published.push(event);
            if finished {
                break;
            }
        }
        while state.is_operation_active() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        while let Ok(raw) = rx.try_recv() {
            published.push(serde_json::from_str(&raw).unwrap());
        }
        assert_eq!(
            published
                .iter()
                .filter(|event| event.method == events::REVIEW_BATCH_FINISHED)
                .count(),
            1,
            "the batch publishes exactly one terminal event"
        );
        published
    })
    .await
    .expect("the review batch must finish and release its operation")
}

/// Builds a `review/execute` request carrying the given payload.
pub(super) fn execute_request(params: serde_json::Value) -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
        params: Some(params),
    }
}

/// Saves a session whose findings carry the given statuses in declaration order.
fn save_session(dir: &TestDir, review_id: &str, statuses: &[IssueStatus]) {
    let items = statuses
        .iter()
        .enumerate()
        .map(|(index, status)| test_item(index as u32 + 1, *status, "src/target.rs"))
        .collect::<Vec<_>>();
    ReviewStorage::save_session(dir.path(), &test_session(review_id, items)).unwrap();
}

#[tokio::test]
async fn execute_rejects_a_missing_payload() {
    let dir = TestDir::new();
    let state = test_state(&dir);

    let res = handle_review_execute(
        Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
            params: None,
        },
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("the request is refused").code,
        errors::INVALID_PARAMS
    );
}

#[tokio::test]
async fn execute_reports_an_unknown_session() {
    let dir = TestDir::new();
    let state = test_state(&dir);

    let res = handle_review_execute(
        execute_request(serde_json::json!({ "kind": "session", "review_id": "ghost" })),
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("the session is unknown").code,
        errors::NOT_FOUND
    );
}

#[tokio::test]
async fn execute_rejects_item_id_combined_with_item_ids() {
    let dir = TestDir::new();
    save_session(&dir, "rev-selection", &[IssueStatus::Todo]);
    let state = test_state(&dir);

    let res = handle_review_execute(
        execute_request(serde_json::json!({
            "kind": "finding",
            "review_id": "rev-selection",
            "item_id": 1,
            "item_ids": [1]
        })),
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("the selection is ambiguous").code,
        errors::INVALID_PARAMS
    );
}
#[tokio::test]
async fn execute_rejects_a_group_combined_with_findings() {
    let dir = TestDir::new();
    let state = test_state(&dir);

    // The addressing dichotomy: a request names either a group or findings.
    for params in [
        serde_json::json!({
            "kind": "group",
            "review_id": "rev-mix",
            "group_id": "arch-1",
            "item_id": 1
        }),
        serde_json::json!({
            "kind": "group",
            "review_id": "rev-mix",
            "group_id": "arch-1",
            "item_ids": [1]
        }),
    ] {
        let res = handle_review_execute(execute_request(params), &state).await;
        assert_eq!(
            res.error.expect("the mixture is refused").code,
            errors::INVALID_PARAMS
        );
    }
}

#[tokio::test]
async fn execute_requires_a_session_for_a_group() {
    let dir = TestDir::new();
    let state = test_state(&dir);

    let res = handle_review_execute(
        execute_request(serde_json::json!({"kind": "group", "group_id": "arch-1"})),
        &state,
    )
    .await;

    assert_eq!(
        res.error
            .expect("a group is addressed inside a session")
            .code,
        errors::INVALID_PARAMS
    );
}

#[tokio::test]
async fn execute_reports_an_unknown_group() {
    let dir = TestDir::new();
    let session = architectural_session("rev-ghost-group", "arch-1");
    ReviewStorage::save_session(dir.path(), &session).unwrap();
    let state = test_state(&dir);

    let res = handle_review_execute(
        execute_request(serde_json::json!({
            "kind": "group",
            "review_id": "rev-ghost-group",
            "group_id": "ghost"
        })),
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("the group is unknown").code,
        errors::NOT_FOUND
    );
}

#[tokio::test]
async fn execute_refuses_a_finding_of_an_architectural_group() {
    let dir = TestDir::new();
    let session = architectural_session("rev-arch", "arch-1");
    ReviewStorage::save_session(dir.path(), &session).unwrap();
    let state = test_state(&dir);

    // A symptom of one shared root cause is never executed on its own: both
    // a single targeted finding and an explicit selection are refused and
    // directed to the group run.
    for params in [
        serde_json::json!({"kind": "finding", "review_id": "rev-arch", "item_id": 1}),
        serde_json::json!({"kind": "findings", "review_id": "rev-arch", "item_ids": [1]}),
    ] {
        let res = handle_review_execute(execute_request(params), &state).await;
        assert_eq!(
            res.error.expect("the symptom is refused").code,
            errors::ARCHITECTURAL_GROUP_EXECUTION_REQUIRED
        );
    }
}

#[tokio::test]
async fn execute_rejects_a_stray_sub_step_target() {
    let dir = TestDir::new();
    save_session(&dir, "rev-targeted", &[IssueStatus::Todo]);
    let state = test_state(&dir);

    let res = handle_review_execute(
        execute_request(serde_json::json!({
            "kind": "session",
            "review_id": "rev-targeted",
            "step_id": "s-1"
        })),
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("a stray sub-step target is refused").code,
        errors::INVALID_PARAMS
    );
}

#[tokio::test]
async fn execute_requires_a_session_for_an_item_selection() {
    let dir = TestDir::new();
    let state = test_state(&dir);

    let res = handle_review_execute(
        execute_request(serde_json::json!({ "kind": "finding", "item_id": 1 })),
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("a finding needs its session").code,
        errors::INVALID_PARAMS
    );
}

#[tokio::test]
async fn execute_refuses_an_untriaged_finding() {
    let dir = TestDir::new();
    save_session(&dir, "rev-discussion", &[IssueStatus::Discussion]);
    let state = test_state(&dir);

    let res = handle_review_execute(
        execute_request(serde_json::json!({
            "kind": "finding",
            "review_id": "rev-discussion",
            "item_id": 1
        })),
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("triage gates execution").code,
        errors::ITEM_IN_DISCUSSION
    );
}

#[tokio::test]
async fn execute_refuses_a_rejected_finding() {
    let dir = TestDir::new();
    save_session(&dir, "rev-rejected", &[IssueStatus::Rejected]);
    let state = test_state(&dir);

    let res = handle_review_execute(
        execute_request(serde_json::json!({
            "kind": "finding",
            "review_id": "rev-rejected",
            "item_id": 1
        })),
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("a rejected finding is terminal").code,
        errors::ITEM_REJECTED
    );
}

#[tokio::test]
async fn execute_refuses_a_finished_finding() {
    let dir = TestDir::new();
    save_session(
        &dir,
        "rev-finished",
        &[IssueStatus::Fixed, IssueStatus::Skipped],
    );
    let state = test_state(&dir);

    for item_id in [1_u32, 2] {
        let res = handle_review_execute(
            execute_request(serde_json::json!({
                "kind": "finding",
                "review_id": "rev-finished",
                "item_id": item_id
            })),
            &state,
        )
        .await;
        assert_eq!(
            res.error
                .expect("a finished finding is not re-executed")
                .code,
            errors::NOTHING_TO_EXECUTE
        );
    }
}

#[tokio::test]
async fn execute_refuses_a_group_that_still_poses_an_open_question() {
    let dir = TestDir::new();
    // A finding that still waits for an engineering decision keeps its group
    // out of the queue, so an explicit group launch is refused before any
    // member is scheduled.
    let mut item = test_item(1, IssueStatus::Discussion, "src/target.rs");
    item.group = Some("arch-1".to_string());
    let mut session = test_session("rev-open", vec![item]);
    session.groups = vec![tauqe_core::review::ReviewGroup {
        id: "arch-1".to_string(),
        title: "Group arch-1".to_string(),
        kind: tauqe_core::review::ReviewGroupKind::Architectural,
        ..Default::default()
    }];
    ReviewStorage::save_session(dir.path(), &session).unwrap();
    let state = test_state(&dir);

    for params in
        [serde_json::json!({"kind": "group", "review_id": "rev-open", "group_id": "arch-1"})]
    {
        let res = handle_review_execute(execute_request(params), &state).await;
        let err = res.error.expect("an unanswered question is refused");
        assert_eq!(err.code, errors::GROUP_UNRESOLVED_QUESTIONS);
        assert!(
            err.message.contains("arch-1"),
            "the diagnostic must name the group: {}",
            err.message
        );
    }
    assert!(
        !state.is_operation_active(),
        "a refused request releases the operation slot"
    );
}

/// The public RPC prepares the missing horizon before starting its coder turn.
#[tokio::test]
async fn execute_synthesizes_an_empty_group_before_its_coder_turn() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");
    let mut session = architectural_session("rev-auto", "arch-1");
    session.groups[0].blocked_reason = Some("owns no implementation horizon yet".into());
    session.groups[0].description = Some("One shared root cause".into());
    ReviewStorage::save_session(dir.path(), &session).unwrap();
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(horizon_response(serde_json::json!([horizon_leaf("s-1")]))),
        MockResponse::text(super::batch::json_edit_response(
            "pub fn target() { let value = 9; }",
        )),
    ]));
    let (out, mut rx) = crate::transport::OutChannel::new();
    let (history, _) = tokio::sync::mpsc::unbounded_channel();
    let state = crate::state::AppState::new(horizon_config(), dir.path().into(), history, out);
    state.application.test_set_provider(provider.clone()).await;
    let events = execute_and_collect(
        &state,
        &mut rx,
        execute_request(serde_json::json!({
            "kind": "group", "review_id": "rev-auto", "group_id": "arch-1"
        })),
    )
    .await;
    let requests = provider.requests();
    assert_eq!(
        requests
            .iter()
            .map(|r| r.model.as_str())
            .collect::<Vec<_>>(),
        ["review/architect", "review/coder"]
    );
    let architect_prompt = requests[0]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(architect_prompt.contains("One shared root cause"));
    assert!(architect_prompt.contains("Finding 1"));
    let started: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.method == tauqe_protocol::events::MODEL_STARTED)
        .collect();
    assert_eq!(started.len(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.method == tauqe_protocol::events::MODEL_FINISHED)
            .count(),
        2
    );
    let prepared = events[started[0].0..started[1].0]
        .iter()
        .filter(|e| e.method == tauqe_protocol::events::REVIEW_UPDATED)
        .map(|e| {
            serde_json::from_value::<tauqe_protocol::ReviewUpdatedEvent>(e.params.clone().unwrap())
                .unwrap()
        })
        .find(|e| !e.session.groups[0].implementation.items.is_empty())
        .expect("the horizon is published before coder starts");
    assert_eq!(
        prepared.session.groups[0].implementation.items[0].status,
        tauqe_protocol::PlanItemStatus::Todo
    );
    assert!(prepared.session.groups[0].blocked_reason.is_none());
    let stored = ReviewStorage::load_review(dir.path(), "rev-auto")
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.groups[0].implementation.items[0].status,
        PlanItemStatus::Done
    );
    assert_eq!(
        stored.groups[0]
            .implementation
            .architect
            .as_ref()
            .unwrap()
            .model,
        tauqe_core::models::ModelRef::openrouter("review/architect")
    );
    assert!(stored.groups[0]
        .implementation
        .last_replanned_step
        .is_none());
    assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Fixed);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.method == tauqe_protocol::events::REVIEW_BATCH_FINISHED)
            .count(),
        1
    );
    assert!(!dir.path().join(".tauqe/plans/rev-auto.json").exists());
}

#[tokio::test]
async fn execute_implicit_queues_synthesize_a_group_once_for_multiple_findings() {
    for kind in ["session", "all_sessions"] {
        let dir = TestDir::new();
        init_repo_with_file(&dir, "src/target.rs");
        let mut session = architectural_session("rev-implicit", "arch-1");
        let mut second = test_item(2, IssueStatus::Todo, "src/target.rs");
        second.group = Some("arch-1".into());
        session.items.push(second.into());
        ReviewStorage::save_session(dir.path(), &session).unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(horizon_response(serde_json::json!([horizon_leaf("s-1")]))),
            MockResponse::text(super::batch::json_edit_response(
                "pub fn target() { let value = 9; }",
            )),
        ]));
        let (out, mut rx) = crate::transport::OutChannel::new();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let state = crate::state::AppState::new(horizon_config(), dir.path().into(), history, out);
        state.application.test_set_provider(provider.clone()).await;
        let events = execute_and_collect(
            &state,
            &mut rx,
            execute_request(serde_json::json!({
                "kind": kind, "review_id": "rev-implicit"
            })),
        )
        .await;
        assert_eq!(
            provider
                .requests()
                .iter()
                .map(|r| r.model.as_str())
                .collect::<Vec<_>>(),
            ["review/architect", "review/coder"]
        );
        let stored = ReviewStorage::load_review(dir.path(), "rev-implicit")
            .unwrap()
            .unwrap();
        assert_eq!(stored.groups[0].implementation.items.len(), 1);
        assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Fixed);
        assert_eq!(stored.find_item(2).unwrap().status, IssueStatus::Fixed);
        assert_eq!(
            events.last().unwrap().params.as_ref().unwrap()["summary"]["fixed_count"],
            2
        );
    }
}
