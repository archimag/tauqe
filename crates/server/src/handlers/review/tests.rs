use std::sync::Arc;

use super::batch::{run_review_batch, BatchOutcome};
use super::common::{clear_auto_context, preload_item_files, turn_applied_edits};
use super::*;

use crate::state::AppState;
use crate::transport::OutChannel;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{
    events, ContextAccess, IssueStatus, ModelSelection, PlanItemStatus, Request, ReviewItem,
    ReviewSession,
};
use tokio::sync::{mpsc, watch};

struct TestDir(std::path::PathBuf);
impl TestDir {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tauqe-test-review-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&path);
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_state(dir: &TestDir) -> Arc<AppState> {
    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    )
}

fn test_item(id: u32, status: IssueStatus, file: &str) -> tauqe_protocol::ReviewIssue {
    tauqe_protocol::ReviewIssue {
        id,
        title: format!("Finding {}", id),
        model: None,
        issue_type: tauqe_protocol::IssueType::Quality,
        status,
        description: "Details".to_string(),
        recommendations: Vec::new(),
        files: vec![file.to_string()],
        readonly_files: Vec::new(),
        plan: Vec::new(),
        note: None,
    }
}

fn make_tasks(review_id: &str, ids: &[u32]) -> Vec<(String, u32)> {
    ids.iter().map(|id| (review_id.to_string(), *id)).collect()
}

fn test_session(id: &str, items: Vec<tauqe_protocol::ReviewIssue>) -> ReviewSession {
    ReviewSession {
        id: id.to_string(),
        title: format!("Session {}", id),
        created_at: 100,
        model: tauqe_protocol::ModelRef::openrouter("test-model"),
        description: None,
        user_prompt: None,
        target_files: vec!["foo.rs".to_string()],
        items: items.into_iter().map(ReviewItem::from).collect(),
        raw_markdown: String::new(),
    }
}

fn test_config() -> tauqe_core::config::AppConfig {
    let mut config = tauqe_core::config::AppConfig::default();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
    config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
    config.models.auto_level_up = false;
    config
}

fn init_repo_with_file(dir: &TestDir, path: &str) {
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    let full = dir.path().join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&full, "pub fn target() {}\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &[path.to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add target", false).unwrap();
}

async fn test_state_with_provider(
    dir: &TestDir,
    config: tauqe_core::config::AppConfig,
    provider: Arc<dyn LlmProvider>,
) -> Arc<AppState> {
    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);
    *state.provider_override.lock().await = Some(provider);
    state
}

#[tokio::test]
async fn test_handle_review_start_defaults_to_reviewer_role() {
    let dir = TestDir::new();
    let target_file = dir.path().join("file.rs");
    std::fs::write(&target_file, "fn foo() {}\n").unwrap();

    let mut config = tauqe_core::config::AppConfig::default();
    config.models.reviewer =
        Some(vec![tauqe_protocol::ModelRef::openrouter("test/reviewer-model").into()]);
    config.models.primary =
        vec![tauqe_protocol::ModelRef::openrouter("test/primary-model").into()];

    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);

    state
        .context
        .write()
        .await
        .add_file("file.rs", ContextAccess::ReadOnly)
        .unwrap();

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_START.to_string(),
        params: None,
    };
    let res = handle_review_start(req, &state).await;
    let err = res.error.expect("should fail without api key");
    assert_eq!(err.code, "NO_API_KEY");
}

#[tokio::test]
async fn test_review_start_streams_clean_summary_without_json_leakage() {
    let dir = TestDir::new();
    let target_file = dir.path().join("file.rs");
    std::fs::write(&target_file, "fn foo() {}\n").unwrap();

    let payload = r#"{"summary":"The architecture is sound, but locking is racy.","findings":[{"title":"Race condition","severity":"critical","file_path":"file.rs","line_start":1,"line_end":2,"body":"Lock ordering is inconsistent."}]}"#;
    let provider: Arc<dyn LlmProvider> =
        Arc::new(MockLlmProvider::scripted([MockResponse::text(payload)]));

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );
    *state.provider_override.lock().await = Some(provider);

    state
        .context
        .write()
        .await
        .add_file("file.rs", ContextAccess::ReadOnly)
        .unwrap();

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_START.to_string(),
        params: None,
    };
    let res = handle_review_start(req, &state).await;
    assert!(res.error.is_none(), "review start must succeed");

    // Drain streamed events until the review terminates.
    let mut lines = Vec::new();
    while let Ok(Some(line)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
    {
        let terminal = line.contains(events::REVIEW_FINISHED)
            || line.contains(events::REVIEW_ERROR);
        lines.push(line);
        if terminal {
            break;
        }
    }

    // Only cleaned summary deltas reach the client: raw JSON syntax never
    // leaks into the content stream.
    let mut streamed = String::new();
    for line in &lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if val.get("method").and_then(|m| m.as_str()) == Some(events::REVIEW_CONTENT_DELTA)
            {
                if let Some(delta) = val
                    .get("params")
                    .and_then(|p| p.get("delta"))
                    .and_then(|d| d.as_str())
                {
                    streamed.push_str(delta);
                }
            }
        }
    }
    assert_eq!(streamed, "The architecture is sound, but locking is racy.");
    assert!(!streamed.contains('{'));
    assert!(!streamed.contains('"'));
    assert!(!streamed.contains("summary"));
    assert!(!streamed.contains("findings"));

    // The summary is persisted as the session description and the findings
    // are stored structurally.
    let session = ReviewStorage::load_latest(dir.path())
        .unwrap()
        .expect("review session must be saved");
    assert_eq!(
        session.description.as_deref(),
        Some("The architecture is sound, but locking is racy.")
    );
    assert_eq!(session.items.len(), 1);
    let issues = session.issues();
    assert_eq!(issues[0].issue_type, tauqe_protocol::IssueType::Bug);
}

#[tokio::test]
async fn test_handle_review_execute_validations() {
    let dir = TestDir::new();
    let state = test_state(&dir);
    let make_req = |id: u64, params: Option<serde_json::Value>| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
        params,
    };

    let res = handle_review_execute(make_req(1, None), &state).await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    let res = handle_review_execute(
        make_req(2, Some(serde_json::json!({ "review_id": "non-existent", "item_id": 1 }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "NOT_FOUND");

    let session = test_session(
        "rev-val-1",
        vec![
            test_item(1, IssueStatus::Discussion, "foo.rs"),
            test_item(2, IssueStatus::Rejected, "foo.rs"),
            test_item(3, IssueStatus::Fixed, "foo.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let cases = [
        (Some(1), "ITEM_IN_DISCUSSION"),
        (Some(2), "ITEM_REJECTED"),
        (Some(3), "NOTHING_TO_EXECUTE"),
        (Some(9), "NOT_FOUND"),
        (None, "NOTHING_TO_EXECUTE"),
    ];
    for (idx, (item_id, code)) in cases.iter().enumerate() {
        let params = serde_json::json!({ "review_id": "rev-val-1", "item_id": item_id });
        let res = handle_review_execute(make_req(10 + idx as u64, Some(params)), &state).await;
        assert_eq!(res.error.unwrap().code, *code);
    }

    // A session with mixed Discussion and Todo items executes only Todo items without blocking.
    let mixed_session = test_session(
        "rev-mixed",
        vec![
            test_item(1, IssueStatus::Discussion, "foo.rs"),
            test_item(2, IssueStatus::Todo, "foo.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &mixed_session).unwrap();
    let res = handle_review_execute(
        make_req(20, Some(serde_json::json!({ "review_id": "rev-mixed" }))),
        &state,
    )
    .await;
    assert!(res.error.is_none());
    let val: serde_json::Value = res.result.unwrap();
    assert_eq!(val["item_ids"], serde_json::json!([2]));
}

#[tokio::test]
async fn test_handle_review_execute_cross_session_validations() {
    let dir = TestDir::new();
    let state = test_state(&dir);
    let make_req = |id: u64, params: Option<serde_json::Value>| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
        params,
    };

    // 1. Specifying item_id without review_id must return INVALID_PARAMS.
    let res = handle_review_execute(
        make_req(1, Some(serde_json::json!({ "item_id": 10 }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // 2. Cross-session execution with no sessions in repo returns NOTHING_TO_EXECUTE.
    let res = handle_review_execute(make_req(2, Some(serde_json::json!({}))), &state).await;
    assert_eq!(res.error.unwrap().code, "NOTHING_TO_EXECUTE");

    // 3. Sessions exist, but none contain TODO items -> NOTHING_TO_EXECUTE.
    let session_disc = test_session(
        "rev-cross-disc",
        vec![
            test_item(1, IssueStatus::Discussion, "foo.rs"),
            test_item(2, IssueStatus::Fixed, "foo.rs"),
            test_item(3, IssueStatus::Rejected, "foo.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session_disc).unwrap();
    let res = handle_review_execute(make_req(3, Some(serde_json::json!({}))), &state).await;
    assert_eq!(res.error.unwrap().code, "NOTHING_TO_EXECUTE");

    // 4. Multiple sessions with TODO items are collected and returned.
    let session_todo = test_session(
        "rev-cross-todo",
        vec![
            test_item(1, IssueStatus::Todo, "bar.rs"),
            test_item(2, IssueStatus::Discussion, "bar.rs"),
            test_item(3, IssueStatus::InProgress, "bar.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session_todo).unwrap();
    let res = handle_review_execute(make_req(4, Some(serde_json::json!({}))), &state).await;
    assert!(res.error.is_none());
    let val: serde_json::Value = res.result.unwrap();
    assert_eq!(val["review_id"], serde_json::Value::Null);
    let item_ids = val["item_ids"].as_array().expect("array of items");
    assert_eq!(item_ids.len(), 2);
    assert_eq!(item_ids, &vec![serde_json::json!(1), serde_json::json!(3)]);
}

#[tokio::test]
async fn test_review_batch_cross_session_skips_and_continues() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session1 = test_session(
        "rev-cross-skip-1",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    let session2 = test_session(
        "rev-cross-skip-2",
        vec![test_item(2, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session1).unwrap();
    ReviewStorage::save_session(dir.path(), &session2).unwrap();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut config = test_config();
    config.develop.max_retries = 0;

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text("No code changes proposed."),
        MockResponse::text("No code changes proposed."),
    ]));

    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);
    *state.provider_override.lock().await = Some(provider);

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let tasks = vec![
        ("rev-cross-skip-1".to_string(), 1),
        ("rev-cross-skip-2".to_string(), 2),
    ];

    let outcome = run_review_batch(&state, "all", tasks, operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 2 }),
        "expected the batch to skip and continue across sessions, got {outcome:?}"
    );

    // A domain failure skips the finding in its own session without aborting
    // the queue or leaking into the other session.
    let reloaded1 = ReviewStorage::load_review(dir.path(), "rev-cross-skip-1").unwrap().unwrap();
    assert_eq!(reloaded1.find_item(1).unwrap().status, IssueStatus::Skipped);

    let reloaded2 = ReviewStorage::load_review(dir.path(), "rev-cross-skip-2").unwrap().unwrap();
    assert_eq!(reloaded2.find_item(2).unwrap().status, IssueStatus::Skipped);

    // Event review/batch_finished emitted with owner_id "all".
    let mut raw_events = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        raw_events.push(raw);
    }
    let finished = raw_events
        .iter()
        .find(|e| e.contains(events::REVIEW_BATCH_FINISHED))
        .expect("review/batch_finished must be emitted");
    assert!(finished.contains("\"all\""));
    assert!(finished.contains("\"completed\""));
}

#[tokio::test]
async fn test_review_batch_cross_session_success_and_active_id_tracking() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session1 = test_session(
        "rev-cross-ok-1",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    let session2 = test_session(
        "rev-cross-ok-2",
        vec![test_item(2, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session1).unwrap();
    ReviewStorage::save_session(dir.path(), &session2).unwrap();

    let marker = "CROSSOK";
    let skip_response1 = format!(
        "<skip_{m} reason=\"Validated\">Handled in session 1</skip_{m}>",
        m = marker
    );
    let skip_response2 = format!(
        "<skip_{m} reason=\"Validated\">Handled in session 2</skip_{m}>",
        m = marker
    );
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(skip_response1),
        MockResponse::text(skip_response2),
    ]));

    let mut config = test_config();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;

    let state = test_state_with_provider(&dir, config, provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let tasks = vec![
        ("rev-cross-ok-1".to_string(), 1),
        ("rev-cross-ok-2".to_string(), 2),
    ];

    let outcome = run_review_batch(&state, "all", tasks, operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 2 }),
        "expected completed cross-session batch, got {:?}",
        outcome
    );

    let reloaded1 = ReviewStorage::load_review(dir.path(), "rev-cross-ok-1").unwrap().unwrap();
    assert_eq!(reloaded1.find_item(1).unwrap().status, IssueStatus::Skipped);

    let reloaded2 = ReviewStorage::load_review(dir.path(), "rev-cross-ok-2").unwrap().unwrap();
    assert_eq!(reloaded2.find_item(2).unwrap().status, IssueStatus::Skipped);

    // Active session ID tracks the latest executed session.
    let active_id = ReviewStorage::load_active_id(dir.path()).unwrap();
    assert_eq!(active_id.as_deref(), Some("rev-cross-ok-2"));
}

#[tokio::test]
async fn test_review_batch_fail_fast_resets_status_and_preloads_file() {
    let dir = TestDir::new();
    let _ = tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();

    let target_path = dir.path().join("src");
    std::fs::create_dir_all(&target_path).unwrap();
    std::fs::write(target_path.join("target.rs"), "fn target() {}\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src/target.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add target", false).unwrap();

    let session = test_session(
        "rev-exec-1",
        vec![
            test_item(1, IssueStatus::Todo, "./src/target.rs"),
            test_item(2, IssueStatus::Todo, "./src/target.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();
    let state = test_state(&dir);

    // Verify preload_item_files directly
    preload_item_files(&state, dir.path(), &["./src/target.rs".to_string()]).await;
    {
        let ctx = state.context.read().await;
        assert!(ctx.is_editable("src/target.rs").unwrap());
    }
    clear_auto_context(&state).await;

    // Without an API key the first turn cannot start: the batch must stop at finding 1.
    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-exec-1", make_tasks("rev-exec-1", &[1, 2]), operation).await;
    match outcome {
        BatchOutcome::Failed {
            completed, item_id, ..
        } => {
            assert_eq!(completed, 0);
            assert_eq!(item_id, 1);
        }
        other => panic!("Expected Failed, got {:?}", other),
    }

    // Auto context is cleared after batch execution finishes so files do not leak.
    {
        let ctx = state.context.read().await;
        assert!(!ctx.contains("src/target.rs").unwrap());
    }

    // Fail-Fast: statuses are back to TODO and nothing is marked FIXED.
    let updated = ReviewStorage::load_review(dir.path(), "rev-exec-1")
        .unwrap()
        .unwrap();
    assert!(updated.items.iter().all(|i| i.status == IssueStatus::Todo));

    let active_id = ReviewStorage::load_active_id(dir.path()).unwrap();
    assert_eq!(active_id.as_deref(), Some("rev-exec-1"));
}

#[tokio::test]
async fn test_review_batch_skips_fixed_and_stops_on_rejected() {
    let dir = TestDir::new();
    let session = test_session(
        "rev-skip",
        vec![
            test_item(1, IssueStatus::Fixed, "foo.rs"),
            test_item(2, IssueStatus::Rejected, "foo.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();
    let state = test_state(&dir);

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-skip", make_tasks("rev-skip", &[1, 2]), operation).await;
    match outcome {
        BatchOutcome::Failed {
            completed, item_id, ..
        } => {
            assert_eq!(completed, 1);
            assert_eq!(item_id, 2);
        }
        other => panic!("Expected Failed, got {:?}", other),
    }
}

#[tokio::test]
async fn test_review_batch_emits_finished_event() {
    let dir = TestDir::new();
    let session = test_session(
        "rev-emit",
        vec![
            test_item(1, IssueStatus::Todo, "foo.rs"),
            test_item(2, IssueStatus::Todo, "foo.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    // Without an API key the first turn cannot start: the batch fails at finding 1.
    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-emit", make_tasks("rev-emit", &[1, 2]), operation).await;
    assert!(matches!(outcome, BatchOutcome::Failed { .. }));

    let mut raw_events = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        raw_events.push(raw);
    }
    let finished = raw_events
        .iter()
        .find(|e| e.contains(events::REVIEW_BATCH_FINISHED))
        .expect("review/batch_finished must be emitted");
    assert!(finished.contains("\"rev-emit\""));
    assert!(finished.contains("\"failed\""));
}

#[tokio::test]
async fn test_review_execute_blocked_by_failed_preflight() {
    let dir = TestDir::new();
    let session = test_session(
        "rev-preflight",
        vec![
            test_item(1, IssueStatus::Todo, "foo.rs"),
            test_item(2, IssueStatus::Todo, "foo.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();
    // A malformed manifest makes the deterministic toolchain pipeline fail.
    std::fs::write(dir.path().join("Cargo.toml"), "[package\n").unwrap();
    let state = test_state(&dir);

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_EXECUTE.to_string(),
        params: Some(serde_json::json!({ "review_id": "rev-preflight" })),
    };
    let res = handle_review_execute(req, &state).await;

    let err = res.error.expect("preflight must block the batch");
    assert_eq!(err.code, "PREFLIGHT_VERIFICATION_FAILED");
    assert!(err.message.contains("Pre-flight verification failed"));

    // The failure is detected before any turn starts: no active operation.
    assert!(!state.is_operation_active());

    // No finding was touched: every status stays TODO.
    let reloaded = ReviewStorage::load_review(dir.path(), "rev-preflight")
        .unwrap()
        .unwrap();
    assert!(reloaded.items.iter().all(|i| i.status == IssueStatus::Todo));
}

#[test]
fn test_turn_applied_edits_requires_applied_change() {
    use crate::handlers::model::ModelTurnResult;

    let applied = ModelTurnResult {
        operation_id: "op-1".to_string(),
        result: Some(tauqe_protocol::ModelResult::Edit {
            summary: "fix".to_string(),
            edits: vec![],
            proposal: None,
            outcome: tauqe_protocol::EditOutcome {
                applied: true,
                error: None,
                changed_files: vec!["src/lib.rs".to_string()],
                commit_hash: Some("abc".to_string()),
            },
        }),
        assistant_text: "done".to_string(),
        is_cancelled: false,
        error: None,
        has_terminal_success: false,
        blocked: None,
    };
    assert!(applied.is_success());
    assert!(turn_applied_edits(&applied));

    // Explanatory text with a terminal success tag satisfies an interactive
    // turn but must not complete a code-modifying review finding.
    let answer = ModelTurnResult {
        operation_id: "op-2".to_string(),
        result: Some(tauqe_protocol::ModelResult::Answer {
            text: "Nothing to fix.".to_string(),
        }),
        assistant_text: "Nothing to fix.".to_string(),
        is_cancelled: false,
        error: None,
        has_terminal_success: true,
        blocked: None,
    };
    assert!(answer.is_success());
    assert!(!turn_applied_edits(&answer));

    let failed = ModelTurnResult {
        operation_id: "op-3".to_string(),
        result: Some(tauqe_protocol::ModelResult::Edit {
            summary: "fix".to_string(),
            edits: vec![],
            proposal: None,
            outcome: tauqe_protocol::EditOutcome {
                applied: false,
                error: Some("compiler error".to_string()),
                changed_files: vec![],
                commit_hash: None,
            },
        }),
        assistant_text: String::new(),
        is_cancelled: false,
        error: Some("compiler error".to_string()),
        has_terminal_success: false,
        blocked: None,
    };
    assert!(!turn_applied_edits(&failed));
}

#[tokio::test]
async fn test_review_retry_exhaustion_skips_item_and_continues() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-retry-exhaust",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    // One initial attempt plus one retry: both answer without any edits, so
    // the finding never satisfies the code-modification contract.
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text("I inspected the file but propose no edits."),
        MockResponse::text("Still no edits to propose."),
    ]));

    let mut config = test_config();
    config.develop.max_retries = 1;
    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-retry-exhaust", make_tasks("rev-retry-exhaust", &[1]), operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 1 }),
        "expected the exhausted finding to be skipped and the batch to complete, got {outcome:?}"
    );

    // Exhausting the attempt budget skips the finding with diagnostics instead
    // of aborting the batch.
    let updated = ReviewStorage::load_review(dir.path(), "rev-retry-exhaust")
        .unwrap()
        .unwrap();
    let item = updated.find_item(1).unwrap();
    assert_eq!(item.status, IssueStatus::Skipped);
    let notes = item.resolution_notes.as_deref().unwrap_or("");
    assert!(notes.contains("Skipped after execution failure"), "notes: {notes}");
    assert!(notes.contains("no code changes"), "notes: {notes}");

    // The retry loop is bounded: one initial attempt plus `max_retries`.
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn test_review_batch_skips_no_change_findings_without_hanging() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-no-change",
        vec![
            test_item(1, IssueStatus::Todo, "src/target.rs"),
            test_item(2, IssueStatus::Todo, "src/target.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text("No edits."),
        MockResponse::text("No edits."),
        MockResponse::text("No edits."),
        MockResponse::text("No edits."),
    ]));

    let mut config = test_config();
    config.develop.max_retries = 1;
    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-no-change", make_tasks("rev-no-change", &[1, 2]), operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 2 }),
        "expected the batch to skip both findings and complete, got {outcome:?}"
    );

    // The batch terminates deterministically instead of hanging: each finding is
    // attempted `max_retries + 1` times before being skipped.
    assert_eq!(provider.requests().len(), 4);

    let updated = ReviewStorage::load_review(dir.path(), "rev-no-change")
        .unwrap()
        .unwrap();
    assert_eq!(updated.find_item(1).unwrap().status, IssueStatus::Skipped);
    assert_eq!(updated.find_item(2).unwrap().status, IssueStatus::Skipped);
}

#[tokio::test]
async fn test_review_semantic_block_skips_finding_without_retry() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-blocked",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        "Cannot fix this finding.\n<blocked reason=\"Target API was removed\" />",
    )]));

    let state = test_state_with_provider(&dir, test_config(), provider.clone()).await;

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-blocked", make_tasks("rev-blocked", &[1]), operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 1 }),
        "expected the blocked finding to be skipped and the batch to complete, got {outcome:?}"
    );

    // A semantic block skips immediately: no patch-retry or healing cycle runs,
    // so the provider is consulted exactly once.
    assert_eq!(provider.requests().len(), 1);

    let updated = ReviewStorage::load_review(dir.path(), "rev-blocked")
        .unwrap()
        .unwrap();
    let item = updated.find_item(1).unwrap();
    assert_eq!(item.status, IssueStatus::Skipped);
    let notes = item.resolution_notes.as_deref().unwrap_or("");
    assert!(notes.contains("Target API was removed"), "notes: {notes}");
}

#[tokio::test]
async fn test_review_explicit_model_disables_ladder_escalation() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let mut item = test_item(1, IssueStatus::Todo, "src/target.rs");
    item.model = Some(ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter(
        "explicit/model",
    )));

    let session = test_session("rev-explicit", vec![item]);
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text("No edits."),
        MockResponse::text("No edits."),
    ]));

    // Escalation is enabled, but an explicitly assigned model is pinned
    // across every retry and must never be replaced by a ladder tier.
    let mut config = test_config();
    config.develop.max_retries = 1;
    config.models.auto_level_up = true;
    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-explicit", make_tasks("rev-explicit", &[1]), operation).await;
    assert!(matches!(outcome, BatchOutcome::Completed { items: 1 }));

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests.iter().all(|r| r.model == "explicit/model"),
        "an explicit model must never be replaced by a ladder tier"
    );
}

#[tokio::test]
async fn test_review_step_execution_uses_turn_marker() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-marker",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let marker = "REVMARKER1";
    let response = format!(
        "<tauqe_edits_{m} summary=\"Fix finding\">\n<edit_{m} path=\"src/target.rs\">\n<search_{m}>\npub fn target() {{}}\n</search_{m}>\n<replace_{m}>\npub fn target() {{ }}\n</replace_{m}>\n</edit_{m}>\n</tauqe_edits_{m}>",
        m = marker
    );
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));

    let mut config = test_config();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;

    let state = test_state_with_provider(&dir, config, provider.clone()).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-marker", make_tasks("rev-marker", &[1]), operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 1 }),
        "expected a completed batch, got {:?}",
        outcome
    );

    // Both the system prompt and the finding prompt must carry the marker suffix.
    let request = &provider.requests()[0];
    assert!(request.messages[0]
        .content
        .contains(&format!("scratchpad_{}", marker)));
    assert!(request.messages[0]
        .content
        .contains(&format!("success_{}", marker)));

    // The marker-bound edit fixed the finding.
    let reloaded = ReviewStorage::load_review(dir.path(), "rev-marker")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.find_item(1).unwrap().status, IssueStatus::Fixed);
}

#[tokio::test]
async fn test_review_step_execution_with_skip_tag_marks_skipped_and_saves_notes() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-skip-single",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let marker = "SKIPTAG1";
    let response = format!(
        "I checked this code and it is already resolved.\n<skip_{m} reason=\"Already fixed in prior commit\">Refactored earlier</skip_{m}>",
        m = marker
    );
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));

    let mut config = test_config();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;

    let state = test_state_with_provider(&dir, config, provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-skip-single", make_tasks("rev-skip-single", &[1]), operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 1 }),
        "expected batch completed, got {:?}",
        outcome
    );

    let reloaded = ReviewStorage::load_review(dir.path(), "rev-skip-single")
        .unwrap()
        .unwrap();
    let item = reloaded.find_item(1).unwrap();
    assert_eq!(item.status, IssueStatus::Skipped);
    assert_eq!(
        item.resolution_notes.as_deref(),
        Some("Already fixed in prior commit: Refactored earlier")
    );
}

#[tokio::test]
async fn test_review_batch_continues_after_skipped_item() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-skip-continue",
        vec![
            test_item(1, IssueStatus::Todo, "src/target.rs"),
            test_item(2, IssueStatus::Todo, "src/target.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let marker = "SKIPCONT";
    let skip_response = format!(
        "<skip_{m} reason=\"False positive\">Function contract already guarantees non-null</skip_{m}>",
        m = marker
    );
    let edit_response = format!(
        "<tauqe_edits_{m} summary=\"Fix second finding\">\n<edit_{m} path=\"src/target.rs\">\n<search_{m}>\npub fn target() {{}}\n</search_{m}>\n<replace_{m}>\npub fn target() {{ /* resolved */ }}\n</replace_{m}>\n</edit_{m}>\n</tauqe_edits_{m}>",
        m = marker
    );
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(skip_response),
        MockResponse::text(edit_response),
    ]));

    let mut config = test_config();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;

    let state = test_state_with_provider(&dir, config, provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-skip-continue", make_tasks("rev-skip-continue", &[1, 2]), operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 2 }),
        "expected batch to execute both items, got {:?}",
        outcome
    );

    let reloaded = ReviewStorage::load_review(dir.path(), "rev-skip-continue")
        .unwrap()
        .unwrap();
    let item1 = reloaded.find_item(1).unwrap();
    assert_eq!(item1.status, IssueStatus::Skipped);
    assert_eq!(
        item1.resolution_notes.as_deref(),
        Some("False positive: Function contract already guarantees non-null")
    );

    let item2 = reloaded.find_item(2).unwrap();
    assert_eq!(item2.status, IssueStatus::Fixed);
}

#[tokio::test]
async fn test_review_batch_preloads_multiple_files() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();

    let file_a = dir.path().join("src/first.rs");
    let file_b = dir.path().join("src/second.rs");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(&file_a, "pub fn first() {}\n").unwrap();
    std::fs::write(&file_b, "pub fn second() {}\n").unwrap();
    tauqe_core::git::cmd::stage_files(
        dir.path(),
        &["src/first.rs".to_string(), "src/second.rs".to_string()],
    )
    .unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add initial files", false)
        .unwrap();

    let session = test_session(
        "rev-preload-multi",
        vec![tauqe_protocol::ReviewIssue {
            id: 1,
            title: "Multi-file finding".to_string(),
            model: None,
            issue_type: tauqe_protocol::IssueType::Architecture,
            status: IssueStatus::Todo,
            description: "First calls second without validation".to_string(),
            recommendations: Vec::new(),
            files: vec!["src/first.rs".to_string(), "src/second.rs".to_string()],
            readonly_files: Vec::new(),
            plan: Vec::new(),
            note: None,
        }],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let marker = "PRELOADM";
    let skip_response = format!(
        "<skip_{m} reason=\"Validated\">Files were verified to be in sync</skip_{m}>",
        m = marker
    );
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(skip_response)]));

    let mut config = test_config();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;

    let state = test_state_with_provider(&dir, config, provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    // Pre-condition: Context does not contain these files yet
    {
        let ctx = state.context.read().await;
        assert!(!ctx.contains("src/first.rs").unwrap());
        assert!(!ctx.contains("src/second.rs").unwrap());
    }

    // Verify preload_item_files directly
    preload_item_files(
        &state,
        dir.path(),
        &["src/first.rs".to_string(), "src/second.rs".to_string()],
    )
    .await;
    {
        let ctx = state.context.read().await;
        assert!(ctx.is_editable("src/first.rs").unwrap());
        assert!(ctx.is_editable("src/second.rs").unwrap());
    }
    clear_auto_context(&state).await;

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-preload-multi", make_tasks("rev-preload-multi", &[1]), operation).await;
    assert!(matches!(outcome, BatchOutcome::Completed { items: 1 }));

    // After batch execution, auto context is cleaned up so preloaded files do not leak
    {
        let ctx = state.context.read().await;
        assert!(!ctx.contains("src/first.rs").unwrap());
        assert!(!ctx.contains("src/second.rs").unwrap());
    }
}

#[tokio::test]
async fn test_handle_review_update_item_clears_and_updates_notes() {
    let dir = TestDir::new();
    let state = test_state(&dir);

    let mut item = test_item(1, IssueStatus::Todo, "foo.rs");
    item.note = Some("Initial note".to_string());
    let session = test_session("rev-update-notes", vec![item]);
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    // 1. Update notes to a new value
    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_UPDATE_ITEM.to_string(),
        params: Some(serde_json::json!({
            "review_id": "rev-update-notes",
            "item_id": 1,
            "resolution_notes": "Updated note"
        })),
    };
    let res = handle_review_update_item(req, &state).await;
    assert!(res.error.is_none());

    let reloaded = ReviewStorage::load_review(dir.path(), "rev-update-notes")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.find_item(1).unwrap().resolution_notes.as_deref(),
        Some("Updated note")
    );

    // 2. Clear notes by sending an empty string
    let req_clear = Request {
        id: tauqe_protocol::RequestId::Number(2),
        method: tauqe_protocol::methods::REVIEW_UPDATE_ITEM.to_string(),
        params: Some(serde_json::json!({
            "review_id": "rev-update-notes",
            "item_id": 1,
            "resolution_notes": "   "
        })),
    };
    let res_clear = handle_review_update_item(req_clear, &state).await;
    assert!(res_clear.error.is_none());

    let reloaded_cleared = ReviewStorage::load_review(dir.path(), "rev-update-notes")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded_cleared.find_item(1).unwrap().resolution_notes,
        None
    );
}

#[tokio::test]
async fn test_handle_review_update_item_requires_explicit_review_id() {
    let dir = TestDir::new();
    let state = test_state(&dir);

    let session = test_session(
        "rev-iso-1",
        vec![test_item(1, IssueStatus::Todo, "foo.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();
    ReviewStorage::save_active_id(dir.path(), Some("rev-iso-1")).unwrap();

    // Omitting review_id must never resolve through the process-global active
    // marker: the update is rejected instead of silently hitting rev-iso-1.
    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_UPDATE_ITEM.to_string(),
        params: Some(serde_json::json!({ "item_id": 1 })),
    };
    let res = handle_review_update_item(req, &state).await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // The active session is left untouched.
    let reloaded = ReviewStorage::load_review(dir.path(), "rev-iso-1")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.find_item(1).unwrap().status, IssueStatus::Todo);
}

#[tokio::test]
async fn test_handle_review_cancel_soft_keeps_operation_active() {
    let dir = TestDir::new();
    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    let operation = state.try_begin_operation().expect("register operation");
    let soft_rx = operation.soft_stop_receiver();
    let cancel_rx = operation.cancel_receiver();
    assert!(!*soft_rx.borrow());
    assert!(!*cancel_rx.borrow());

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_CANCEL.to_string(),
        params: Some(serde_json::json!({ "mode": "soft" })),
    };
    let res = handle_review_cancel(req, &state).await;
    assert!(res.error.is_none());

    assert!(*soft_rx.borrow());
    assert!(!*cancel_rx.borrow());
    assert!(state.is_operation_active());
}

#[tokio::test]
async fn test_review_batch_stops_gracefully_on_soft_stop() {
    let dir = TestDir::new();
    let session = test_session(
        "rev-soft-stop",
        vec![
            test_item(1, IssueStatus::Todo, "foo.rs"),
            test_item(2, IssueStatus::Todo, "foo.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    // A soft stop requested before the batch starts stops the queue before
    // the first finding runs.
    state.soft_stop_active_operation();

    let outcome = run_review_batch(
        &state,
        "rev-soft-stop",
        make_tasks("rev-soft-stop", &[1, 2]),
        operation,
    )
    .await;

    assert_eq!(outcome, BatchOutcome::Stopped { completed: 0 });

    // No finding was touched: every status stays TODO.
    let reloaded = ReviewStorage::load_review(dir.path(), "rev-soft-stop")
        .unwrap()
        .unwrap();
    assert!(reloaded.items.iter().all(|i| i.status == IssueStatus::Todo));

    // The supervisor publishes a single typed completion event carrying the
    // graceful "stopped" status.
    let mut raw_events = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        raw_events.push(raw);
    }
    let finished = raw_events
        .iter()
        .find(|e| e.contains(events::REVIEW_BATCH_FINISHED))
        .expect("review/batch_finished must be emitted");
    assert!(finished.contains("\"rev-soft-stop\""));
    assert!(finished.contains("\"stopped\""));
}

#[tokio::test]
async fn test_review_start_with_path_pattern_preserves_user_context() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/a.rs");
    let extra_file = dir.path().join("src/b.rs");
    std::fs::write(&extra_file, "pub fn b() {}\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src/b.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add b", false).unwrap();

    let doc_file = dir.path().join("docs/readme.md");
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::write(&doc_file, "# Readme\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["docs/readme.md".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add doc", false).unwrap();

    let payload = r#"{"summary":"Reviewed pattern files.","findings":[]}"#;
    let provider: Arc<dyn LlmProvider> =
        Arc::new(MockLlmProvider::scripted([MockResponse::text(payload)]));

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        test_config(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );
    *state.provider_override.lock().await = Some(provider);

    state
        .context
        .write()
        .await
        .add_file("src/a.rs", ContextAccess::Editable)
        .unwrap();

    let rev_before = state.context.read().await.get_state().revision;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_START.to_string(),
        params: Some(serde_json::json!({
            "path_pattern": "src/*.rs",
            "user_prompt": "Check code quality"
        })),
    };
    let res = handle_review_start(req, &state).await;
    assert!(res.error.is_none(), "review start must succeed: {:?}", res.error);

    while let Ok(Some(line)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
    {
        if line.contains(events::REVIEW_FINISHED) || line.contains(events::REVIEW_ERROR) {
            break;
        }
    }

    let session = ReviewStorage::load_latest(dir.path())
        .unwrap()
        .expect("review session must be saved");

    assert!(session.target_files.contains(&"src/a.rs".to_string()));
    assert!(session.target_files.contains(&"src/b.rs".to_string()));
    assert!(!session.target_files.contains(&"docs/readme.md".to_string()));

    let ctx = state.context.read().await;
    assert_eq!(ctx.get_state().revision, rev_before);
    assert!(ctx.contains("src/a.rs").unwrap());
    assert!(!ctx.contains("src/b.rs").unwrap());
}

#[tokio::test]
async fn test_review_step_execution_records_resolution_notes_on_fixed() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-fixed-notes",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let marker = "FIXNOTES1";
    let response = format!(
        "<tauqe_edits_{m} summary=\"Fix finding target\">\n<edit_{m} path=\"src/target.rs\">\n<search_{m}>\npub fn target() {{}}\n</search_{m}>\n<replace_{m}>\npub fn target() {{ }}\n</replace_{m}>\n</edit_{m}>\n</tauqe_edits_{m}>\n<success_{m} summary=\"Resolved target bug\" />",
        m = marker
    );
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));

    let mut config = test_config();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;

    let state = test_state_with_provider(&dir, config, provider.clone()).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(&state, "rev-fixed-notes", make_tasks("rev-fixed-notes", &[1]), operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 1 }),
        "expected a completed batch, got {:?}",
        outcome
    );

    let reloaded = ReviewStorage::load_review(dir.path(), "rev-fixed-notes")
        .unwrap()
        .unwrap();
    let item = reloaded.find_item(1).unwrap();
    assert_eq!(item.status, IssueStatus::Fixed);
    assert!(item.resolution_notes.is_some());
}

fn subplan_item(
    id: &str,
    title: &str,
    status: PlanItemStatus,
    files: Vec<&str>,
) -> tauqe_protocol::PlanItem {
    tauqe_protocol::PlanItem {
        id: id.to_string(),
        title: title.to_string(),
        status,
        files: files.into_iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn test_handle_review_refine_item_persists_subplan() {
    let dir = TestDir::new();
    let session = test_session(
        "rev-refine",
        vec![test_item(1, IssueStatus::Todo, "foo.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Split into two sub-steps",
        "reason": null,
        "context_requests": [],
        "items": [
            {
                "id": "1.1",
                "title": "First",
                "status": "todo",
                "files": ["foo.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            },
            {
                "id": "1.2",
                "title": "Second",
                "status": "todo",
                "files": ["foo.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": ["1.1"],
                "children": []
            }
        ]
    });
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        refine_output.to_string(),
    )]));
    let state = test_state_with_provider(&dir, test_config(), provider).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_REFINE_ITEM.to_string(),
        params: Some(serde_json::json!({ "review_id": "rev-refine", "item_id": 1 })),
    };
    let res = handle_review_refine_item(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    let reloaded = ReviewStorage::load_review(dir.path(), "rev-refine")
        .unwrap()
        .unwrap();
    let item = reloaded.find_item(1).unwrap();
    assert_eq!(item.subplan.len(), 2);
    assert_eq!(item.subplan[0].id, "1.1");
    assert_eq!(item.subplan[1].id, "1.2");
    // Decomposition alone does not change the finding status.
    assert_eq!(item.status, IssueStatus::Todo);
}

#[tokio::test]
async fn test_handle_review_refine_item_preloads_target_files_into_prompt() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-refine-preload",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Decomposed the finding",
        "reason": null,
        "context_requests": [],
        "items": [
            {
                "id": "1.1",
                "title": "First sub-step",
                "status": "todo",
                "files": ["src/target.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        refine_output.to_string(),
    )]));
    let state = test_state_with_provider(&dir, test_config(), provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_REFINE_ITEM.to_string(),
        params: Some(serde_json::json!({ "review_id": "rev-refine-preload", "item_id": 1 })),
    };
    let res = handle_review_refine_item(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    // The finding's target file is preloaded into the architect prompt on the
    // very first generation round, so decomposition sees the real code.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let user_prompt = &requests[0].messages[1].content;
    assert!(user_prompt.contains("## Active Context Files:"));
    assert!(user_prompt.contains("### File: `src/target.rs`"));
    assert!(user_prompt.contains("pub fn target() {}"));
}

#[tokio::test]
async fn test_handle_review_refine_item_retries_when_requested_files_already_loaded() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-refine-satisfied",
        vec![test_item(1, IssueStatus::Todo, "src/target.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    // Round 1: the architect asks for a file that preloading already placed in
    // context. Round 2: it finalizes once it sees the context feedback.
    let needs_context = serde_json::json!({
        "status": "needs_context",
        "explanation": "Verifying the target contract before decomposing",
        "reason": null,
        "context_requests": ["src/target.rs"],
        "items": []
    });
    let success = serde_json::json!({
        "status": "success",
        "explanation": "Decomposed after verifying the contract",
        "reason": null,
        "context_requests": [],
        "items": [
            {
                "id": "1.1",
                "title": "Only sub-step",
                "status": "todo",
                "files": ["src/target.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(needs_context.to_string()),
        MockResponse::text(success.to_string()),
    ]));
    let state = test_state_with_provider(&dir, test_config(), provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_REFINE_ITEM.to_string(),
        params: Some(serde_json::json!({ "review_id": "rev-refine-satisfied", "item_id": 1 })),
    };
    let res = handle_review_refine_item(req, &state).await;

    // Requesting already-loaded files is not a dead end: the round feeds the
    // context back and the loop proceeds instead of failing with NEEDS_CONTEXT.
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let feedback_prompt = &requests[1].messages[1].content;
    assert!(feedback_prompt.contains("<context_feedback>"));
    assert!(feedback_prompt.contains("already loaded in context"));

    let reloaded = ReviewStorage::load_review(dir.path(), "rev-refine-satisfied")
        .unwrap()
        .unwrap();
    let item = reloaded.find_item(1).unwrap();
    assert_eq!(item.subplan.len(), 1);
}

#[tokio::test]
async fn test_review_subplan_execution_marks_fixed_when_all_steps_succeed() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let mut item = test_item(1, IssueStatus::Todo, "src/target.rs");
    item.plan = vec![
        subplan_item("1.1", "First step", PlanItemStatus::Todo, vec!["src/target.rs"]),
        subplan_item("1.2", "Second step", PlanItemStatus::Todo, vec!["src/target.rs"]),
    ];
    let session = test_session("rev-subplan-ok", vec![item]);
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let marker = "SUBPLANOK1";
    let edit1 = format!(
        "<tauqe_edits_{m} summary=\"Step 1\">\n<edit_{m} path=\"src/target.rs\">\n<search_{m}>\npub fn target() {{}}\n</search_{m}>\n<replace_{m}>\npub fn target() {{ /* one */ }}\n</replace_{m}>\n</edit_{m}>\n<success_{m} summary=\"step 1 done\" />\n</tauqe_edits_{m}>",
        m = marker
    );
    let edit2 = format!(
        "<tauqe_edits_{m} summary=\"Step 2\">\n<edit_{m} path=\"src/target.rs\">\n<search_{m}>\npub fn target() {{ /* one */ }}\n</search_{m}>\n<replace_{m}>\npub fn target() {{ /* two */ }}\n</replace_{m}>\n</edit_{m}>\n<success_{m} summary=\"step 2 done\" />\n</tauqe_edits_{m}>",
        m = marker
    );
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(edit1),
        MockResponse::text(edit2),
    ]));

    let mut config = test_config();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
    let state = test_state_with_provider(&dir, config, provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(
        &state,
        "rev-subplan-ok",
        make_tasks("rev-subplan-ok", &[1]),
        operation,
    )
    .await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 1 }),
        "expected a completed batch, got {:?}",
        outcome
    );

    let reloaded = ReviewStorage::load_review(dir.path(), "rev-subplan-ok")
        .unwrap()
        .unwrap();
    let item = reloaded.find_item(1).unwrap();
    assert_eq!(item.status, IssueStatus::Fixed);
    assert_eq!(item.subplan[0].status, PlanItemStatus::Done);
    assert_eq!(item.subplan[1].status, PlanItemStatus::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/target.rs")).unwrap(),
        "pub fn target() { /* two */ }\n"
    );
}

#[tokio::test]
async fn test_review_subplan_interruption_preserves_done_and_marks_skipped() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let mut item = test_item(1, IssueStatus::Todo, "src/target.rs");
    item.plan = vec![
        subplan_item("1.1", "First step", PlanItemStatus::Todo, vec!["src/target.rs"]),
        subplan_item("1.2", "Second step", PlanItemStatus::Todo, vec!["src/target.rs"]),
    ];
    let session = test_session("rev-subplan-stop", vec![item]);
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let marker = "SUBPLANSTOP1";
    let edit1 = format!(
        "<tauqe_edits_{m} summary=\"Step 1\">\n<edit_{m} path=\"src/target.rs\">\n<search_{m}>\npub fn target() {{}}\n</search_{m}>\n<replace_{m}>\npub fn target() {{ /* one */ }}\n</replace_{m}>\n</edit_{m}>\n<success_{m} summary=\"step 1 done\" />\n</tauqe_edits_{m}>",
        m = marker
    );
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(edit1),
        MockResponse::text("No edits proposed for the second step."),
    ]));

    let mut config = test_config();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
    config.develop.max_retries = 0;
    let state = test_state_with_provider(&dir, config, provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(
        &state,
        "rev-subplan-stop",
        make_tasks("rev-subplan-stop", &[1]),
        operation,
    )
    .await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 1 }),
        "expected the interrupted subplan to be skipped and the batch to complete, got {:?}",
        outcome
    );

    let reloaded = ReviewStorage::load_review(dir.path(), "rev-subplan-stop")
        .unwrap()
        .unwrap();
    let item = reloaded.find_item(1).unwrap();
    // The finding is Skipped, its completed sub-step stays Done, the failed
    // one is not, and the stop reason is preserved.
    assert_eq!(item.status, IssueStatus::Skipped);
    assert_eq!(item.subplan[0].status, PlanItemStatus::Done);
    assert_ne!(item.subplan[1].status, PlanItemStatus::Done);
    let notes = item.resolution_notes.as_deref().unwrap_or("");
    assert!(notes.contains("Subplan interrupted"), "notes: {notes}");
}

#[tokio::test]
async fn test_review_batch_skips_finding_on_verification_failure_and_continues() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    // A failing check command makes toolchain verification fail deterministically
    // without compiling a fixture project, so a verification failure is a domain
    // failure that must be confined to the current finding.
    std::fs::write(
        dir.path().join("tauqe.toml"),
        "[toolchain]\ncheck_command = \"false\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/target.rs"), "pub fn target() {}\n").unwrap();
    tauqe_core::git::cmd::stage_files(
        dir.path(),
        &["tauqe.toml".to_string(), "src/target.rs".to_string()],
    )
    .unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add fixture", false).unwrap();

    let session = test_session(
        "rev-verify-skip",
        vec![
            test_item(1, IssueStatus::Todo, "src/target.rs"),
            test_item(2, IssueStatus::Todo, "src/target.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let edit = "<tauqe_edits summary=\"Fix finding\">\n<edit path=\"src/target.rs\">\n<search>\npub fn target() {}\n</search>\n<replace>\npub fn target() { /* fixed */ }\n</replace>\n</edit>\n</tauqe_edits>";
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(edit),
        MockResponse::text(edit),
    ]));

    let mut config = test_config();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Git;
    config.develop.max_retries = 0;
    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(
        &state,
        "rev-verify-skip",
        make_tasks("rev-verify-skip", &[1, 2]),
        operation,
    )
    .await;

    assert!(
        matches!(outcome, BatchOutcome::Completed { items: 2 }),
        "a verification failure is a domain failure: the batch must skip the finding and continue, got {outcome:?}"
    );

    let updated = ReviewStorage::load_review(dir.path(), "rev-verify-skip")
        .unwrap()
        .unwrap();
    for id in [1u32, 2] {
        let item = updated.find_item(id).unwrap();
        assert_eq!(
            item.status,
            IssueStatus::Skipped,
            "finding {id} must be skipped instead of aborting the batch"
        );
        let notes = item.resolution_notes.as_deref().unwrap_or("");
        assert!(
            notes.contains("Skipped after execution failure"),
            "finding {id} notes: {notes}"
        );
        assert!(notes.contains("verification"), "finding {id} notes: {notes}");
    }

    // Each failed turn rolled back cleanly, so the source is left unmodified.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/target.rs")).unwrap(),
        "pub fn target() {}\n"
    );
    // One attempt per finding, with no healing round because `max_retries = 0`.
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn test_review_batch_stops_on_provider_error() {
    let dir = TestDir::new();
    init_repo_with_file(&dir, "src/target.rs");

    let session = test_session(
        "rev-provider-abort",
        vec![
            test_item(1, IssueStatus::Todo, "src/target.rs"),
            test_item(2, IssueStatus::Todo, "src/target.rs"),
        ],
    );
    ReviewStorage::save_session(dir.path(), &session).unwrap();

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::error("Provider connection reset"),
        MockResponse::text("This response must never be consumed."),
    ]));

    let state = test_state_with_provider(&dir, test_config(), provider.clone()).await;

    let operation = Arc::new(state.try_begin_operation().expect("register batch operation"));
    let outcome = run_review_batch(
        &state,
        "rev-provider-abort",
        make_tasks("rev-provider-abort", &[1, 2]),
        operation,
    )
    .await;

    match outcome {
        BatchOutcome::Failed {
            completed, item_id, ..
        } => {
            assert_eq!(
                completed, 0,
                "no finding may be reported completed before the provider aborts the batch"
            );
            assert_eq!(item_id, 1);
        }
        other => panic!("a provider error must abort the batch, got {other:?}"),
    }

    // An infrastructure failure is fatal: the failed finding returns to Todo
    // rather than being skipped, and the next finding is never touched.
    let updated = ReviewStorage::load_review(dir.path(), "rev-provider-abort")
        .unwrap()
        .unwrap();
    assert_eq!(updated.find_item(1).unwrap().status, IssueStatus::Todo);
    assert_eq!(updated.find_item(2).unwrap().status, IssueStatus::Todo);

    // The batch stopped before launching the second finding.
    assert_eq!(provider.remaining(), 1);
}

#[tokio::test]
async fn test_parallel_review_updates_are_isolated_by_review_id() {
    let dir = TestDir::new();
    let state = test_state(&dir);

    let session_a = test_session(
        "rev-par-a",
        vec![test_item(1, IssueStatus::Todo, "foo.rs")],
    );
    let session_b = test_session(
        "rev-par-b",
        vec![test_item(1, IssueStatus::Todo, "foo.rs")],
    );
    ReviewStorage::save_session(dir.path(), &session_a).unwrap();
    ReviewStorage::save_session(dir.path(), &session_b).unwrap();
    // The process-global active marker points at session A only; it must never
    // leak into the update addressed at session B.
    ReviewStorage::save_active_id(dir.path(), Some("rev-par-a")).unwrap();

    let make_req = |id: u64, review_id: &str, status: &str| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::REVIEW_UPDATE_ITEM.to_string(),
        params: Some(serde_json::json!({
            "review_id": review_id,
            "item_id": 1,
            "status": status,
        })),
    };

    let (res_a, res_b) = tokio::join!(
        handle_review_update_item(make_req(1, "rev-par-a", "fixed"), &state),
        handle_review_update_item(make_req(2, "rev-par-b", "rejected"), &state),
    );
    assert!(res_a.error.is_none(), "session A update failed: {:?}", res_a.error);
    assert!(res_b.error.is_none(), "session B update failed: {:?}", res_b.error);

    // Each update is scoped to the session it names: the two findings sharing
    // id #1 keep their own, independent status.
    let a = ReviewStorage::load_review(dir.path(), "rev-par-a")
        .unwrap()
        .unwrap();
    let b = ReviewStorage::load_review(dir.path(), "rev-par-b")
        .unwrap()
        .unwrap();
    assert_eq!(a.find_item(1).unwrap().status, IssueStatus::Fixed);
    assert_eq!(b.find_item(1).unwrap().status, IssueStatus::Rejected);
}

/// A provider that reports cancellation exclusively through an explicit
/// `StreamEvent::Cancelled`, without the turn's cancel signal ever being
/// raised. This reproduces the race the review handler must tolerate:
/// cancellation reported by the stream event alone.
struct StreamCancellingProvider;

#[async_trait::async_trait]
impl LlmProvider for StreamCancellingProvider {
    fn kind(&self) -> tauqe_protocol::ProviderKind {
        tauqe_protocol::ProviderKind::OpenRouter
    }

    async fn stream_chat(
        &self,
        _model: &str,
        _messages: Vec<ChatMessage>,
        _response_format: Option<ResponseFormat>,
        tx: mpsc::Sender<StreamEvent>,
        _cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let _ = tx.send(StreamEvent::Cancelled).await;
        Ok(())
    }
}

#[tokio::test]
async fn test_review_start_surfaces_stream_cancelled_event() {
    let dir = TestDir::new();
    let target_file = dir.path().join("file.rs");
    std::fs::write(&target_file, "fn foo() {}\n").unwrap();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );
    *state.provider_override.lock().await = Some(Arc::new(StreamCancellingProvider));

    state
        .context
        .write()
        .await
        .add_file("file.rs", ContextAccess::ReadOnly)
        .unwrap();

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_START.to_string(),
        params: None,
    };
    let res = handle_review_start(req, &state).await;
    assert!(
        res.error.is_none(),
        "review start must acknowledge the operation"
    );

    // An explicit `Cancelled` stream event must surface as `review/cancelled`,
    // never as a truncated-parse error or an empty-review error.
    let mut saw_cancelled = false;
    let mut saw_error = false;
    while let Ok(Some(line)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
    {
        if line.contains(events::REVIEW_CANCELLED) {
            saw_cancelled = true;
        }
        if line.contains(events::REVIEW_ERROR) {
            saw_error = true;
        }
        if saw_cancelled || saw_error {
            break;
        }
    }

    assert!(
        saw_cancelled,
        "an explicit Cancelled event must surface as review/cancelled"
    );
    assert!(
        !saw_error,
        "cancellation must not be misreported as a review error"
    );
}
