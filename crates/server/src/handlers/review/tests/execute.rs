//! `review/execute`: request validation and queue resolution before any turn.
//!
//! Every refusal asserted here happens before the batch claims the operation
//! slot, so a rejected request can never leave a finding half executed.

use super::{architectural_session, test_item, test_session, test_state, TestDir};

use super::super::handle_review_execute;

use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{errors, IssueStatus, Request};

/// Builds a `review/execute` request carrying the given payload.
fn execute_request(params: serde_json::Value) -> Request {
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
        execute_request(serde_json::json!({ "review_id": "ghost" })),
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
        serde_json::json!({"review_id": "rev-mix", "group_id": "arch-1", "item_id": 1}),
        serde_json::json!({"review_id": "rev-mix", "group_id": "arch-1", "item_ids": [1]}),
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
        execute_request(serde_json::json!({"group_id": "arch-1"})),
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
        serde_json::json!({"review_id": "rev-arch", "item_id": 1}),
        serde_json::json!({"review_id": "rev-arch", "item_ids": [1]}),
    ] {
        let res = handle_review_execute(execute_request(params), &state).await;
        assert_eq!(
            res.error.expect("the symptom is refused").code,
            errors::ARCHITECTURAL_GROUP_EXECUTION_REQUIRED
        );
    }
}

#[tokio::test]
async fn execute_requires_an_item_for_a_targeted_sub_step() {
    let dir = TestDir::new();
    save_session(&dir, "rev-targeted", &[IssueStatus::Todo]);
    let state = test_state(&dir);

    let res = handle_review_execute(
        execute_request(serde_json::json!({
            "review_id": "rev-targeted",
            "step_id": "s-1"
        })),
        &state,
    )
    .await;

    assert_eq!(
        res.error.expect("a sub-step needs its finding").code,
        errors::INVALID_PARAMS
    );
}

#[tokio::test]
async fn execute_requires_a_session_for_an_item_selection() {
    let dir = TestDir::new();
    let state = test_state(&dir);

    let res =
        handle_review_execute(execute_request(serde_json::json!({ "item_id": 1 })), &state).await;

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
