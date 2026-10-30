//! A single finding executed as an isolated bounded turn.
//!
//! The turn is marker-bound exactly like a plan step, so the same protocol
//! vocabulary drives both workflows and the finding reaches `Fixed` only when the
//! applied change set satisfies the success contract.

use std::sync::Arc;

use super::{
    architecture_edit, init_repo_with_file, make_tasks, test_config, test_item, test_session,
    test_state_with_provider, TestDir,
};

use super::super::batch::{run_review_batch, BatchOutcome};

use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::IssueStatus;

#[tokio::test]
async fn review_step_execution_uses_the_turn_marker() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");
    let session = test_session(
        "rev-marker",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let marker = "REVIEWMARKER1";
    let response = architecture_edit(marker, "pub fn target() { let value = 1; }");
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));
    let state = test_state_with_provider(&dir, test_config(), provider.clone()).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_review_batch(
        &state,
        "rev-marker",
        make_tasks("rev-marker", &[1]),
        operation,
    )
    .await;
    assert_eq!(outcome, BatchOutcome::Completed { items: 1 });

    // The system prompt carries the marker suffix, so the model speaks the same
    // marker-bound protocol the harness parses.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages[0]
        .content
        .contains(&format!("scratchpad_{}", marker)));
    assert!(requests[0].messages[0]
        .content
        .contains(&format!("success_{}", marker)));

    // The marker-bound edits were applied and the finding reached FIXED.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/target.rs")).unwrap(),
        "pub fn target() { let value = 1; }\n"
    );
    let stored = ReviewStorage::load_review(dir.path(), "rev-marker")
        .unwrap()
        .unwrap();
    assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Fixed);
}
