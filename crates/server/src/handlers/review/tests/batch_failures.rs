//! Batch refusals decided before the first finding runs.
//!
//! A batch either claims the operation slot and passes the deterministic
//! pre-flight gate before any turn starts, or it is refused synchronously and
//! leaves every persisted finding untouched.

use super::{test_item, test_session, test_state, TestDir};

use super::super::handle_review_execute;

use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{errors, IssueStatus, Request};

/// Builds a `review/execute` request for one stored session.
fn execute_request(review_id: &str) -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
        params: Some(serde_json::json!({ "review_id": review_id })),
    }
}

/// Saves a session whose findings carry the given statuses.
fn save_session(dir: &TestDir, review_id: &str, statuses: &[IssueStatus]) {
    let items = statuses
        .iter()
        .enumerate()
        .map(|(index, status)| test_item(index as u32 + 1, *status, "src/target.rs"))
        .collect::<Vec<_>>();
    ReviewStorage::save_session(dir.path(), &test_session(review_id, items)).unwrap();
}

#[tokio::test]
async fn execute_is_blocked_by_a_failed_preflight_verification() {
    let dir = TestDir::new();
    save_session(
        &dir,
        "rev-preflight",
        &[IssueStatus::Todo, IssueStatus::Todo],
    );
    // A failing verification command makes the deterministic pipeline fail.
    std::fs::write(
        dir.path().join("tauqe.toml"),
        "[project.verify]\ncommand = \"false\"\n",
    )
    .unwrap();
    let state = test_state(&dir);

    let res = handle_review_execute(execute_request("rev-preflight"), &state).await;

    let err = res.error.expect("preflight must block the batch");
    assert_eq!(err.code, errors::PREFLIGHT_VERIFICATION_FAILED);
    assert!(err.message.contains("Pre-flight verification failed"));

    // The failure is detected before any turn starts: no active operation.
    assert!(!state.is_operation_active());

    // No finding was started: every status stays TODO.
    let reloaded = ReviewStorage::load_review(dir.path(), "rev-preflight")
        .unwrap()
        .unwrap();
    assert!(reloaded
        .items
        .iter()
        .all(|item| item.status == IssueStatus::Todo));
}

#[tokio::test]
async fn execute_is_refused_while_another_operation_runs() {
    let dir = TestDir::new();
    save_session(&dir, "rev-busy", &[IssueStatus::Todo]);
    let state = test_state(&dir);
    let _operation = state
        .try_begin_operation()
        .expect("register the competing operation");

    let res = handle_review_execute(execute_request("rev-busy"), &state).await;

    assert_eq!(
        res.error.expect("the batch is refused").code,
        errors::OPERATION_IN_PROGRESS
    );
}
