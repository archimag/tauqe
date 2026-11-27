//! Batch refusals decided before the first finding runs.
//!
//! A batch either claims the operation slot and passes the deterministic
//! pre-flight gate before any turn starts, or it is refused synchronously and
//! leaves every persisted finding untouched.

use super::{
    horizon_config, horizon_leaf, horizon_response, init_repo_with_file, save_grouped_session,
    subplan_item, test_config, test_item, test_session, test_state, TestDir,
};

use super::super::handle_review_execute;

use crate::state::AppState;
use crate::transport::OutChannel;
use std::sync::Arc;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::providers::ModelUsageInfo;
use tauqe_core::review::IssueStatus;
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{errors, events, Request};

/// Builds a `review/execute` request for one stored session.
fn execute_request(review_id: &str) -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
        params: Some(serde_json::json!({ "kind": "session", "review_id": review_id })),
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
async fn invalid_initial_horizons_block_only_the_group_and_leave_findings_open() {
    for (name, response, diagnosis) in [
        ("refused", serde_json::json!({"status":"blocked","explanation":"Cannot design","reason":"Contradictory contract","items":[]}).to_string(), "Contradictory contract"),
        ("empty", horizon_response(serde_json::json!([])), "empty"),
        ("duplicate", horizon_response(serde_json::json!([horizon_leaf("s-1"), horizon_leaf("s-1")])), "s-1"),
        ("cycle", horizon_response(serde_json::json!([{
            "id":"s-1", "title":"First", "status":"todo", "files":["src/target.rs"], "depends_on":["s-2"]
        }, {
            "id":"s-2", "title":"Second", "status":"todo", "files":["src/target.rs"], "depends_on":["s-1"]}])), "cycle"),
    ] {
        let dir = TestDir::new();
        init_repo_with_file(&dir, "src/target.rs");
        save_grouped_session(&dir, name, "arch-1", vec![]);
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response).with_usage(ModelUsageInfo {total_tokens: 7, ..Default::default()})]));
        let (out, mut rx) = OutChannel::new();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(horizon_config(), dir.path().into(), history, out);
        state.application.test_set_provider(provider.clone()).await;
        let published = super::execute::execute_and_collect(&state, &mut rx, super::execute::execute_request(serde_json::json!({
            "kind":"group", "review_id":name, "group_id":"arch-1"
        }))).await;
        assert_eq!(provider.requests().len(), 1, "{name}");
        assert_eq!(provider.requests()[0].model, "review/architect");
        let stored = ReviewStorage::load_review(dir.path(), name).unwrap().unwrap();
        assert!(stored.groups[0].implementation.items.is_empty(), "{name}");
        assert!(stored.groups[0].blocked_reason.as_deref().unwrap().to_lowercase().contains(&diagnosis.to_lowercase()), "{name}");
        assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Todo);
        assert_eq!(published.last().unwrap().params.as_ref().unwrap()["summary"]["failed_count"], 1);
        let usage = state.application.usage_snapshot().await;
        assert_eq!(usage.session.total_tokens, 7, "{name}");
        assert_eq!(usage.lifetime.total_tokens, 7, "{name}");
    }
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
async fn review_queue_continues_after_a_block_but_stops_after_abort_or_provider_failure() {
    const REVIEW: &str = "rev-queue-failure";
    const PATH: &str = "src/target.rs";
    for failure_diagnostic in [None, Some("503"), Some("Start the database service")] {
        let dir = TestDir::new();
        init_repo_with_file(&dir, PATH);
        save_session(&dir, REVIEW, &[IssueStatus::Todo, IssueStatus::Todo]);
        let head =
            || tauqe_core::git::cmd::run_git(Some(dir.path()), &["rev-parse", "HEAD"]).unwrap();
        let initial_head = head();
        let first = match failure_diagnostic {
            Some("503") => MockResponse::error("Provider service unavailable (503)"),
            Some(recommendation) => MockResponse::text(serde_json::json!({
                "message": "Cannot address the first finding",
                "action": {"kind": "abort", "abort": {
                    "is_aborted": true,
                    "reason": "The database is unavailable",
                    "recommendation": recommendation
                }}
            }).to_string()),
            None => MockResponse::text(r#"{"message":"Cannot address the first finding","action":{"kind":"blocked","blocked":{"is_blocked":true,"reason":"A precondition is not met","details":"A required invariant is missing"}}}"#)
                .with_usage(ModelUsageInfo { total_tokens: 7, ..Default::default() }),
        };
        let provider = Arc::new(MockLlmProvider::scripted([
            first,
            MockResponse::text(
                r#"{"message":"Already satisfied","action":{"kind":"edit","changes":[],"commit_message":"","error_message":"","reason":"The second finding is already addressed"}}"#,
            ),
        ]));
        let mut config = test_config();
        config.repl = serde_json::from_value(serde_json::json!({
            "workflow": "git", "patch_strategy": "search_replace"
        }))
        .unwrap();
        let (out, mut rx) = OutChannel::new();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(config, dir.path().to_path_buf(), history, out);
        state.application.test_set_provider(provider.clone()).await;
        let published =
            super::execute::execute_and_collect(&state, &mut rx, execute_request(REVIEW)).await;
        let summary = &published.last().unwrap().params.as_ref().unwrap()["summary"];
        assert_eq!(summary["deferred_count"], 0);
        let last_snapshot = published
            .iter()
            .rposition(|event| event.method == events::REVIEW_UPDATED)
            .unwrap();
        assert!(last_snapshot < published.len() - 1);
        let stored = ReviewStorage::load_review(dir.path(), REVIEW)
            .unwrap()
            .unwrap();
        assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Skipped);
        let second = stored.find_item(2).unwrap();
        assert!(second.commit.is_none());
        let requests = provider.requests();
        if let Some(diagnostic) = failure_diagnostic {
            assert_eq!(summary["status"], "failed");
            assert_eq!(summary["fixed_count"], 0);
            assert_eq!(summary["failed_count"], 0);
            assert!(summary["error"].as_str().unwrap().contains(diagnostic));
            assert_eq!(second.status, IssueStatus::Todo);
            assert_eq!(requests.len(), 1);
        } else {
            assert!(stored
                .find_item(1)
                .unwrap()
                .resolution_notes
                .as_deref()
                .unwrap()
                .contains("A required invariant is missing"));
            assert_eq!(summary["status"], "completed");
            assert_eq!(summary["fixed_count"], 1);
            assert_eq!(summary["failed_count"], 1);
            assert_eq!(second.status, IssueStatus::Fixed);
            assert_eq!(requests.len(), 2);
        }
        let lifecycle: Vec<_> = published
            .iter()
            .filter_map(|event| match event.method.as_str() {
                method @ (events::MODEL_STARTED
                | events::MODEL_FINISHED
                | events::REVIEW_BATCH_FINISHED) => Some(method),
                _ => None,
            })
            .collect();
        let mut expected = [events::MODEL_STARTED, events::MODEL_FINISHED].repeat(requests.len());
        expected.push(events::REVIEW_BATCH_FINISHED);
        assert_eq!(lifecycle, expected);
        assert_eq!(head(), initial_head);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(PATH)).unwrap(),
            "pub fn target() {}\n"
        );
    }
}

#[tokio::test]
async fn review_group_preserves_its_first_commit_when_the_second_step_blocks() {
    use tauqe_core::git::cmd::run_git;
    use tauqe_core::models::{ModelEntry, ModelRef};
    use tauqe_core::plan::PlanItemStatus;
    use tauqe_protocol::ReviewUpdatedEvent;

    const REVIEW: &str = "rev-partial";
    const GROUP: &str = "arch-1";
    const PATH: &str = "src/target.rs";
    const BLOCKER: &str = "The second step requires an unavailable contract";
    let dir = TestDir::new();
    init_repo_with_file(&dir, PATH);
    save_grouped_session(
        &dir,
        REVIEW,
        GROUP,
        vec![
            subplan_item("s-1", "First step", PlanItemStatus::Todo, vec![PATH]),
            subplan_item("s-2", "Second step", PlanItemStatus::Todo, vec![PATH]),
        ],
    );
    let git = |args: &[&str]| run_git(Some(dir.path()), args).unwrap();
    let initial_head = git(&["rev-parse", "HEAD"]);
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(serde_json::json!({
            "message": "Implement the first step",
            "action": {
                "kind": "edit",
                "patch": format!("*** Begin Patch\n*** Update File: {PATH}\n@@\n-pub fn target() {{}}\n+pub fn target() -> u32 {{ 1 }}\n*** End Patch"),
                "commit_message": "fix(review): implement the first step",
                "error_message": "",
                "reason": ""
            }
        }).to_string()),
        MockResponse::text(r#"{"message":"Retry needed","action":{"kind":"edit","patch":"","commit_message":"","error_message":"Could not finish this attempt","reason":""}}"#),
        MockResponse::text(serde_json::json!({
            "message": "Cannot implement the second step",
            "action": {"kind": "blocked", "blocked": {
                "is_blocked": true, "reason": BLOCKER, "details": ""
            }}
        }).to_string()),
    ]));
    let mut config = test_config();
    config.repl = serde_json::from_value(serde_json::json!({
        "workflow": "git", "patch_strategy": "apply_patch", "max_turn_rounds": 1
    }))
    .unwrap();
    let mut coder = ModelEntry::new(ModelRef::openrouter("coder/one"), None);
    coder.attempts = Some(2);
    config.models.repl = vec![coder.clone()];
    config.models.coder = Some(vec![coder]);
    let (out, mut rx) = OutChannel::new();
    let (history, _) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), history, out);
    state.application.test_set_provider(provider.clone()).await;

    let published = super::execute::execute_and_collect(
        &state,
        &mut rx,
        super::execute::execute_request(serde_json::json!({
            "kind": "group", "review_id": REVIEW, "group_id": GROUP
        })),
    )
    .await;
    assert_eq!(provider.requests().len(), 3);
    let lifecycle: Vec<_> = published
        .iter()
        .filter_map(|event| match event.method.as_str() {
            method @ (events::REVIEW_UPDATED
            | events::MODEL_STARTED
            | events::MODEL_FINISHED
            | events::REVIEW_BATCH_FINISHED) => Some(method),
            _ => None,
        })
        .collect();
    assert_eq!(
        lifecycle,
        [
            events::REVIEW_UPDATED,
            events::MODEL_STARTED,
            events::MODEL_FINISHED,
            events::REVIEW_UPDATED,
            events::REVIEW_BATCH_FINISHED,
        ]
    );
    let snapshots: Vec<ReviewUpdatedEvent> = published
        .iter()
        .filter(|event| event.method == events::REVIEW_UPDATED)
        .map(|event| serde_json::from_value(event.params.clone().unwrap()).unwrap())
        .collect();
    let running = &snapshots[0].session.groups[0].implementation.items;
    assert_eq!(
        running[0].status,
        tauqe_protocol::PlanItemStatus::InProgress
    );
    assert_eq!(running[1].status, tauqe_protocol::PlanItemStatus::Todo);

    let stored = ReviewStorage::load_review(dir.path(), REVIEW)
        .unwrap()
        .unwrap();
    let group = &stored.groups[0];
    let first = &group.implementation.items[0];
    let second = &group.implementation.items[1];
    assert_eq!(first.status, PlanItemStatus::Done);
    assert_eq!(second.status, PlanItemStatus::Blocked);
    assert!(second.commit.is_none());
    let commit = first
        .commit
        .as_deref()
        .expect("the first step kept its commit");
    let head = git(&["rev-parse", "HEAD"]);
    assert_eq!(git(&["rev-parse", commit]), head);
    assert_eq!(git(&["rev-parse", "HEAD^"]), initial_head);
    assert_eq!(
        git(&["show", "HEAD:src/target.rs"]).trim(),
        "pub fn target() -> u32 { 1 }"
    );
    assert_eq!(stored.find_item(1).unwrap().status, IssueStatus::Todo);

    let settled = &snapshots[1].session;
    assert_eq!(settled.items[0].status, tauqe_protocol::IssueStatus::Todo);
    let settled_group = &settled.groups[0];
    assert_eq!(
        settled_group.implementation.items[0].commit.as_deref(),
        Some(commit)
    );
    let reason = settled_group.blocked_reason.as_deref().unwrap();
    assert!(reason.contains(BLOCKER), "{reason}");
    assert_eq!(group.blocked_reason.as_deref(), Some(reason));

    // The interrupted sequence retains the earlier commit receipt and blocker
    // without announcing another commit or a successful aggregate edit.
    let commits: Vec<_> = published
        .iter()
        .filter(|event| event.method == events::GIT_COMMIT_CREATED)
        .collect();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].params.as_ref().unwrap()["commit_hash"], commit);
    let edit_finishes: Vec<_> = published
        .iter()
        .filter(|event| event.method == events::EDIT_FINISHED)
        .collect();
    let (terminal, withdrawn) = edit_finishes
        .split_last()
        .expect("the interrupted sequence must close its edit cycle");
    for event in withdrawn {
        let params = event.params.as_ref().unwrap();
        assert_eq!(params["applied"], false);
        assert!(params["commit_hash"].is_null());
    }
    let params = terminal.params.as_ref().unwrap();
    assert_eq!(params["applied"], false);
    assert_eq!(params["commit_hash"], commit);
    assert_eq!(params["changed_files"], serde_json::json!([PATH]));
    let diagnostic = params["error"]
        .as_str()
        .expect("the interrupted sequence must publish its blocker");
    assert!(diagnostic.contains(BLOCKER), "{diagnostic}");
    assert!(!diagnostic.contains("without proposing any file change"));
    let summary = &published.last().unwrap().params.as_ref().unwrap()["summary"];
    assert_eq!(summary["status"], "completed");
    assert_eq!(summary["fixed_count"], 0);
    assert_eq!(summary["failed_count"], 1);
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
