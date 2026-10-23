use super::batch::{
    handle_plan_execute, handle_plan_execute_pipeline, handle_plan_refine, handle_plan_replan,
    run_plan_batch, BatchOutcome,
};
use super::common::{turn_applied_edits, unmet_dependencies};
use crate::state::AppState;
use crate::transport::OutChannel;
use std::sync::Arc;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_protocol::{errors, events, ContextAccess, Plan, PlanItemStatus, Request};

struct TestDir(std::path::PathBuf);
impl TestDir {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tauqe-test-plan-{}-{}",
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

fn test_item(id: &str, status: PlanItemStatus) -> tauqe_protocol::PlanItem {
    tauqe_protocol::PlanItem {
        id: id.to_string(),
        title: format!("Step {}", id),
        status,
        ..Default::default()
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

async fn test_state_with_provider(
    dir: &TestDir,
    config: tauqe_core::config::AppConfig,
    provider: Arc<dyn tauqe_core::providers::LlmProvider>,
) -> Arc<AppState> {
    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);
    *state.provider_override.lock().await = Some(provider);
    state
}

#[tokio::test]
async fn test_plan_batch_fail_fast_resets_status() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "batch".to_string(),
        title: "Batch".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![
            test_item("1", PlanItemStatus::Todo),
            test_item("2", PlanItemStatus::Todo),
        ],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let state = test_state(&dir);

    // Without an API key the first turn cannot start: the batch must stop at step 1.
    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "batch",
        vec!["1".to_string(), "2".to_string()],
        operation,
    )
    .await;
    match outcome {
        BatchOutcome::Failed {
            completed, step_id, ..
        } => {
            assert_eq!(completed, 0);
            assert_eq!(step_id, "1");
        }
        other => panic!("Expected Failed, got {:?}", other),
    }

    let reloaded = PlanStorage::load_plan(dir.path(), "batch")
        .unwrap()
        .unwrap();
    assert!(reloaded
        .items
        .iter()
        .all(|i| i.status == PlanItemStatus::Todo));
    assert_eq!(
        PlanStorage::load_active_id(dir.path()).unwrap().as_deref(),
        Some("batch")
    );
}

#[tokio::test]
async fn test_plan_batch_emits_finished_event() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "emit".to_string(),
        title: "Emit".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![
            test_item("1", PlanItemStatus::Todo),
            test_item("2", PlanItemStatus::Todo),
        ],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    // Without an API key the first turn cannot start: the batch fails at step 1.
    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "emit",
        vec!["1".to_string(), "2".to_string()],
        operation,
    )
    .await;
    assert!(matches!(outcome, BatchOutcome::Failed { .. }));

    // The supervisor publishes a single typed completion event for the whole
    // batch, carrying the aggregate status and step counters.
    let mut raw_events = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        raw_events.push(raw);
    }
    let finished = raw_events
        .iter()
        .find(|e| e.contains(events::PLAN_BATCH_FINISHED))
        .expect("plan/batch_finished must be emitted");
    assert!(finished.contains("\"emit\""));
    assert!(finished.contains("\"failed\""));
}

#[tokio::test]
async fn test_plan_batch_skips_done_and_stops_on_discussion() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "skip".to_string(),
        title: "Skip".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![
            test_item("1", PlanItemStatus::Done),
            test_item("2", PlanItemStatus::Discussion),
        ],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let state = test_state(&dir);

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "skip",
        vec!["1".to_string(), "2".to_string()],
        operation,
    )
    .await;
    match outcome {
        BatchOutcome::Failed {
            completed, step_id, ..
        } => {
            assert_eq!(completed, 1);
            assert_eq!(step_id, "2");
        }
        other => panic!("Expected Failed, got {:?}", other),
    }
    let reloaded = PlanStorage::load_plan(dir.path(), "skip").unwrap().unwrap();
    assert_eq!(
        reloaded.find_item("2").unwrap().status,
        PlanItemStatus::Discussion
    );
}

#[tokio::test]
async fn test_plan_batch_skips_done_and_stops_on_blocked() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "blocked-batch".to_string(),
        title: "Blocked Batch".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![
            test_item("1", PlanItemStatus::Done),
            test_item("2", PlanItemStatus::Blocked),
            test_item("3", PlanItemStatus::Todo),
        ],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let state = test_state(&dir);

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "blocked-batch",
        vec!["1".to_string(), "2".to_string(), "3".to_string()],
        operation,
    )
    .await;
    match outcome {
        BatchOutcome::Failed {
            completed,
            step_id,
            reason,
        } => {
            assert_eq!(completed, 1);
            assert_eq!(step_id, "2");
            assert!(reason.contains("BLOCKED"));
        }
        other => panic!("Expected Failed on blocked step, got {:?}", other),
    }
    let reloaded = PlanStorage::load_plan(dir.path(), "blocked-batch")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.find_item("2").unwrap().status,
        PlanItemStatus::Blocked
    );
    assert_eq!(
        reloaded.find_item("3").unwrap().status,
        PlanItemStatus::Todo
    );
}

#[tokio::test]
async fn test_handle_plan_execute_validation() {
    let dir = TestDir::new();
    let state = test_state(&dir);
    let make_req = |id: u64, params: Option<serde_json::Value>| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::PLAN_EXECUTE.to_string(),
        params,
    };

    let res = handle_plan_execute(make_req(1, None), &state).await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    let res = handle_plan_execute(
        make_req(2, Some(serde_json::json!({ "plan_id": "missing" }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "NOT_FOUND");

    let mut dep_item = test_item("5", PlanItemStatus::Todo);
    dep_item.depends_on = vec!["1".to_string()];

    let plan = Plan {
        id: "val".to_string(),
        title: "Validation".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![
            test_item("1", PlanItemStatus::Discussion),
            test_item("2", PlanItemStatus::Cancelled),
            test_item("3", PlanItemStatus::Done),
            test_item("4", PlanItemStatus::Blocked),
            dep_item,
        ],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let cases = [
        (None, "STEP_IN_DISCUSSION"),
        (Some("1"), "STEP_IN_DISCUSSION"),
        (Some("2"), "STEP_CANCELLED"),
        (Some("3"), "NOTHING_TO_EXECUTE"),
        (Some("4"), "STEP_BLOCKED"),
        (Some("5"), "STEP_BLOCKED_BY_DEPENDENCIES"),
        (Some("9"), "NOT_FOUND"),
    ];
    for (idx, (step, code)) in cases.iter().enumerate() {
        let params = serde_json::json!({ "plan_id": "val", "step_id": step });
        let res = handle_plan_execute(make_req(10 + idx as u64, Some(params)), &state).await;
        assert_eq!(res.error.unwrap().code, *code);
    }
}

#[tokio::test]
async fn test_handle_plan_refine_defaults_to_architect_role() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "refine-role".to_string(),
        title: "Refine Role".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let mut config = tauqe_core::config::AppConfig::default();
    config.models.architect = Some(vec![tauqe_protocol::ModelRef::openrouter(
        "test/architect-model",
    )
    .into()]);
    config.models.primary = vec![tauqe_protocol::ModelRef::openrouter("test/primary-model").into()];

    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "refine-role" })),
    };
    let res = handle_plan_refine(req, &state).await;
    let err = res.error.expect("should fail without api key");
    assert_eq!(err.code, "NO_API_KEY");
}

#[tokio::test]
async fn test_handle_plan_refine_validation() {
    let dir = TestDir::new();
    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    // Missing params
    let req_missing = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: None,
    };
    let res = handle_plan_refine(req_missing, &state).await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // Non-existent plan
    let req_not_found = Request {
        id: tauqe_protocol::RequestId::Number(2),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({
            "plan_id": "non-existent"
        })),
    };
    let res_nf = handle_plan_refine(req_not_found, &state).await;
    assert_eq!(res_nf.error.unwrap().code, "NOT_FOUND");
}

#[tokio::test]
async fn test_handle_plan_refine_clears_auto_context() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "refine-auto".to_string(),
        title: "Refine Auto".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    std::fs::write(dir.path().join("file.rs"), "pub fn foo() {}\n").unwrap();

    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Refined successfully",
        "items": [
            {
                "id": "1",
                "title": "Refined step 1",
                "status": "todo",
                "files": ["file.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        refine_output.to_string(),
    )]));

    let config = tauqe_core::config::AppConfig::default();
    let state = test_state_with_provider(&dir, config, mock_provider).await;

    state
        .context
        .write()
        .await
        .add_auto_file("file.rs", ContextAccess::ReadOnly)
        .unwrap();
    assert_eq!(state.context.read().await.auto_files().len(), 1);

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "refine-auto" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "plan_refine failed: {:?}", res.error);

    assert!(
        state.context.read().await.auto_files().is_empty(),
        "auto context should be cleared after plan refinement"
    );
}

#[tokio::test]
async fn test_plan_execute_blocked_by_failed_preflight() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "preflight".to_string(),
        title: "Preflight".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![
            test_item("1", PlanItemStatus::Todo),
            test_item("2", PlanItemStatus::Todo),
        ],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    // A malformed manifest makes the deterministic toolchain pipeline fail.
    std::fs::write(dir.path().join("Cargo.toml"), "[package\n").unwrap();
    let state = test_state(&dir);

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_EXECUTE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "preflight" })),
    };
    let res = handle_plan_execute(req, &state).await;

    let err = res.error.expect("preflight must block the batch");
    assert_eq!(err.code, "PREFLIGHT_VERIFICATION_FAILED");
    assert!(err.message.contains("Pre-flight verification failed"));

    // The failure is detected before any turn starts: no active operation.
    assert!(!state.is_operation_active());

    // No step was started: every status stays TODO.
    let reloaded = PlanStorage::load_plan(dir.path(), "preflight")
        .unwrap()
        .unwrap();
    assert!(reloaded
        .items
        .iter()
        .all(|i| i.status == PlanItemStatus::Todo));
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
    // turn but must not complete a code-modifying plan step.
    let answer = ModelTurnResult {
        operation_id: "op-2".to_string(),
        result: Some(tauqe_protocol::ModelResult::Answer {
            text: "Nothing to change.".to_string(),
        }),
        assistant_text: "Nothing to change.".to_string(),
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

#[test]
fn test_unmet_dependencies_reports_prerequisites_not_done() {
    let done = test_item("1", PlanItemStatus::Done);
    let in_progress = test_item("2", PlanItemStatus::InProgress);
    let mut dependent = test_item("3", PlanItemStatus::Todo);
    dependent.depends_on = vec!["1".to_string(), "2".to_string(), "missing".to_string()];

    let plan = Plan {
        id: "deps".to_string(),
        title: "Deps".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![done, in_progress],
        ..Default::default()
    };

    assert_eq!(
        unmet_dependencies(&plan, &dependent),
        vec!["#2".to_string(), "#missing".to_string()]
    );

    let mut satisfied = test_item("4", PlanItemStatus::Todo);
    satisfied.depends_on = vec!["1".to_string()];
    assert!(unmet_dependencies(&plan, &satisfied).is_empty());
}

#[tokio::test]
async fn test_handle_plan_replan_validation() {
    let dir = TestDir::new();
    let state = test_state(&dir);
    let make_req = |id: u64, params: Option<serde_json::Value>| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::PLAN_REPLAN.to_string(),
        params,
    };

    // Missing params entirely.
    let res = handle_plan_replan(make_req(1, None), &state).await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // Unknown plan.
    let res = handle_plan_replan(
        make_req(
            2,
            Some(serde_json::json!({ "plan_id": "missing", "step_id": "1" })),
        ),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "NOT_FOUND");

    let parent = tauqe_protocol::PlanItem {
        id: "3".to_string(),
        title: "Parent".to_string(),
        status: PlanItemStatus::Todo,
        children: vec![test_item("3.1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    let plan = Plan {
        id: "replan".to_string(),
        title: "Replan".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![
            test_item("1", PlanItemStatus::Done),
            test_item("2", PlanItemStatus::Blocked),
            parent,
        ],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // Missing step id.
    let res = handle_plan_replan(
        make_req(3, Some(serde_json::json!({ "plan_id": "replan" }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // Unknown step.
    let res = handle_plan_replan(
        make_req(
            4,
            Some(serde_json::json!({ "plan_id": "replan", "step_id": "9" })),
        ),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "NOT_FOUND");

    // DONE steps are immutable.
    let res = handle_plan_replan(
        make_req(
            5,
            Some(serde_json::json!({ "plan_id": "replan", "step_id": "1" })),
        ),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "STEP_IMMUTABLE");

    // Non-leaf steps cannot be replanned.
    let res = handle_plan_replan(
        make_req(
            6,
            Some(serde_json::json!({ "plan_id": "replan", "step_id": "3" })),
        ),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "NOT_A_LEAF");
}

#[tokio::test]
async fn test_handle_plan_replan_with_mock_provider() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "replan-mock".to_string(),
        title: "Replan Mock".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![
            test_item("1", PlanItemStatus::Done),
            test_item("2", PlanItemStatus::Blocked),
        ],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let replan_output = serde_json::json!({
        "status": "success",
        "explanation": "Replanned step 2",
        "items": [
            {
                "id": "2.1",
                "title": "Replacement step 2.1",
                "status": "todo",
                "files": [],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        replan_output.to_string(),
    )]));

    let config = tauqe_core::config::AppConfig::default();
    let state = test_state_with_provider(&dir, config, mock_provider).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REPLAN.to_string(),
        params: Some(serde_json::json!({
            "plan_id": "replan-mock",
            "step_id": "2",
            "reason": "Missing dependency"
        })),
    };
    let res = handle_plan_replan(req, &state).await;
    assert!(res.error.is_none(), "replan failed: {:?}", res.error);

    let reloaded = PlanStorage::load_plan(dir.path(), "replan-mock")
        .unwrap()
        .unwrap();
    // The Done foundation is preserved, the uncompleted horizon is replaced.
    assert_eq!(
        reloaded.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
    assert!(reloaded.find_item("2.1").is_some());
    assert!(reloaded.find_item("2").is_none());
}

#[tokio::test]
async fn test_handle_plan_replan_preloads_blocked_step_files() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/target.rs"), "pub fn target() {}\n").unwrap();
    std::fs::write(
        dir.path().join("src/reference.rs"),
        "pub fn reference_helper() -> u64 { 7 }\n",
    )
    .unwrap();
    tauqe_core::git::cmd::stage_files(
        dir.path(),
        &["src/target.rs".to_string(), "src/reference.rs".to_string()],
    )
    .unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add files", false).unwrap();

    let mut blocked = test_item("2", PlanItemStatus::Blocked);
    blocked.title = "Blocked step".to_string();
    blocked.files = vec!["src/target.rs".to_string()];
    blocked.readonly_files = vec!["src/reference.rs".to_string()];

    let plan = Plan {
        id: "replan-preload".to_string(),
        title: "Replan Preload".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Done), blocked],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let replan_output = serde_json::json!({
        "status": "success",
        "explanation": "Replanned the blocked step",
        "reason": null,
        "context_requests": [],
        "items": [
            {
                "id": "2.1",
                "title": "Replacement step",
                "status": "todo",
                "files": ["src/target.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        replan_output.to_string(),
    )]));
    let config = tauqe_core::config::AppConfig::default();
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REPLAN.to_string(),
        params: Some(serde_json::json!({
            "plan_id": "replan-preload",
            "step_id": "2",
            "reason": "Precondition missing"
        })),
    };
    let res = handle_plan_replan(req, &state).await;
    assert!(res.error.is_none(), "replan failed: {:?}", res.error);

    // The blocked step's declared target and reference files are preloaded into
    // the architect prompt before the very first generation round.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 1);
    let user_prompt = &requests[0].messages[1].content;
    assert!(user_prompt.contains("## Active Context Files:"));
    assert!(user_prompt.contains("### File: `src/target.rs`"));
    assert!(user_prompt.contains("### File: `src/reference.rs`"));
    assert!(user_prompt.contains("pub fn target() {}"));
    assert!(user_prompt.contains("pub fn reference_helper() -> u64 { 7 }"));
}

#[tokio::test]
async fn test_handle_plan_replan_retries_when_requested_files_already_loaded() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/target.rs"), "pub fn target() {}\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src/target.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add file", false).unwrap();

    let mut blocked = test_item("2", PlanItemStatus::Blocked);
    blocked.title = "Blocked step".to_string();
    blocked.files = vec!["src/target.rs".to_string()];

    let plan = Plan {
        id: "replan-satisfied".to_string(),
        title: "Replan Satisfied".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Done), blocked],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // Round 1: the architect asks for a file that preloading already placed in
    // context. Round 2: it finalizes once it sees the context feedback.
    let needs_context = serde_json::json!({
        "status": "needs_context",
        "explanation": "Verifying the target contract before replanning",
        "reason": null,
        "context_requests": ["src/target.rs"],
        "items": []
    });
    let success = serde_json::json!({
        "status": "success",
        "explanation": "Replanned after verifying the contract",
        "reason": null,
        "context_requests": [],
        "items": [
            {
                "id": "2.1",
                "title": "Replacement step",
                "status": "todo",
                "files": ["src/target.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(needs_context.to_string()),
        MockResponse::text(success.to_string()),
    ]));
    let config = tauqe_core::config::AppConfig::default();
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REPLAN.to_string(),
        params: Some(serde_json::json!({
            "plan_id": "replan-satisfied",
            "step_id": "2",
            "reason": "Precondition missing"
        })),
    };
    let res = handle_plan_replan(req, &state).await;

    // Requesting already-loaded files is not a dead end: the round feeds the
    // context back and the loop proceeds instead of failing with NEEDS_CONTEXT.
    assert!(res.error.is_none(), "replan failed: {:?}", res.error);

    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 2);
    let feedback_prompt = &requests[1].messages[1].content;
    assert!(feedback_prompt.contains("<context_feedback>"));
    assert!(feedback_prompt.contains("already loaded in context"));

    let reloaded = PlanStorage::load_plan(dir.path(), "replan-satisfied")
        .unwrap()
        .unwrap();
    assert!(reloaded.find_item("2.1").is_some());
}

#[tokio::test]
async fn test_plan_refine_binds_the_architect_session() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "arch-session".to_string(),
        title: "Architect Session".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Synthesized the horizon",
        "items": [
            {
                "id": "1",
                "title": "Refined step 1",
                "status": "todo",
                "files": [],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        refine_output.to_string(),
    )]));
    let state = test_state_with_provider(
        &dir,
        tauqe_core::config::AppConfig::default(),
        mock_provider.clone(),
    )
    .await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "arch-session" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    // Architect synthesis is an isolated design dialogue: its cached prefix is
    // keyed by the plan, so the very first round already binds the session.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].session_id.as_deref(),
        Some("tauqe-architect-arch-session")
    );
}

#[tokio::test]
async fn test_plan_step_binds_the_plan_session() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan {
        id: "plan-session".to_string(),
        title: "Plan Session".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![item],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let response = "<tauqe_edits summary=\"Set value\">\n<edit path=\"src.rs\">\n<search>\npub fn value() -> i32 { 0 }\n</search>\n<replace>\npub fn value() -> i32 { 1 }\n</replace>\n</edit>\n<success summary=\"done\" />\n</tauqe_edits>";
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
    config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
    config.models.auto_level_up = false;
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(&state, "plan-session", vec!["1".to_string()], operation).await;
    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });

    // Every step and retry of one plan extends the same warm provider prefix.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].session_id.as_deref(),
        Some("tauqe-plan-plan-session")
    );
}

#[test]
fn test_collect_plan_context_files_dedups_and_recurses() {
    let mut nested = test_item("2.1", PlanItemStatus::Todo);
    nested.files = vec!["src/shared.rs".to_string(), "./src/nested.rs".to_string()];
    nested.readonly_files = vec!["src/shared.rs".to_string()];

    let parent = tauqe_protocol::PlanItem {
        id: "2".to_string(),
        title: "Parent".to_string(),
        status: PlanItemStatus::Todo,
        children: vec![nested],
        ..Default::default()
    };

    let mut top = test_item("1", PlanItemStatus::Todo);
    top.files = vec!["src/shared.rs".to_string()];
    top.readonly_files = vec!["src/ref.rs".to_string()];

    let plan = Plan {
        id: "collect".to_string(),
        title: "Collect".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![top, parent],
        ..Default::default()
    };

    // Recurses into children, collapses normalized duplicates across `files`
    // and `readonly_files`, and preserves first-seen order.
    assert_eq!(
        super::engine::collect_plan_context_files(&plan),
        vec![
            "src/shared.rs".to_string(),
            "src/ref.rs".to_string(),
            "src/nested.rs".to_string(),
        ]
    );
}

#[tokio::test]
async fn test_handle_plan_refine_preloads_all_existing_step_files() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(dir.path().join("src/b.rs"), "pub fn b() {}\n").unwrap();
    std::fs::write(dir.path().join("src/ref.rs"), "pub fn r() -> u64 { 1 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(
        dir.path(),
        &[
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/ref.rs".to_string(),
        ],
    )
    .unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add files", false).unwrap();

    let mut step1 = test_item("1", PlanItemStatus::Todo);
    step1.files = vec!["src/a.rs".to_string()];
    step1.readonly_files = vec!["src/ref.rs".to_string()];
    let mut step2 = test_item("2", PlanItemStatus::Todo);
    step2.files = vec!["src/b.rs".to_string()];

    let plan = Plan {
        id: "refine-preload".to_string(),
        title: "Refine Preload".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![step1, step2],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Refined the horizon",
        "items": [
            {
                "id": "1",
                "title": "Refined step 1",
                "status": "todo",
                "files": ["src/a.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        refine_output.to_string(),
    )]));
    let config = tauqe_core::config::AppConfig::default();
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "refine-preload" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    // Deep planning preloads every existing step's declared target and
    // reference files before the architect's first generation round, not just a
    // single blocked step's.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 1);
    let user_prompt = &requests[0].messages[1].content;
    assert!(user_prompt.contains("## Active Context Files:"));
    assert!(user_prompt.contains("### File: `src/a.rs`"));
    assert!(user_prompt.contains("### File: `src/b.rs`"));
    assert!(user_prompt.contains("### File: `src/ref.rs`"));
    assert!(user_prompt.contains("pub fn a() {}"));
    assert!(user_prompt.contains("pub fn b() {}"));
    assert!(user_prompt.contains("pub fn r() -> u64 { 1 }"));
}

#[tokio::test]
async fn test_plan_horizon_synthesis_history_transactionality() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "tx-history".to_string(),
        title: "Tx History".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // Synthesize an invalid horizon that introduces a circular dependency
    // to force `replace_uncompleted_horizon` to fail during persistence.
    let cyclic_output = serde_json::json!({
        "status": "success",
        "explanation": "Cyclic horizon",
        "items": [
            {
                "id": "a",
                "title": "Step A",
                "status": "todo",
                "depends_on": ["b"],
                "children": []
            },
            {
                "id": "b",
                "title": "Step B",
                "status": "todo",
                "depends_on": ["a"],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        cyclic_output.to_string(),
    )]));

    let config = tauqe_core::config::AppConfig::default();
    let state = test_state_with_provider(&dir, config, mock_provider).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "tx-history" })),
    };
    let res = handle_plan_refine(req, &state).await;

    let err = res.error.expect("should fail due to dependency cycle");
    assert_eq!(err.code, errors::PLAN_SAVE_FAILED);
    assert!(err.message.contains("Dependency cycle detected"));

    // Strict history transactionality invariant: no assistant response was recorded
    // into session history because persistence failed.
    let entries = state.history.lock().await.get_entries().unwrap();
    let responses = entries
        .iter()
        .filter(|e| matches!(e, tauqe_core::history::HistoryEntry::Response { .. }))
        .count();
    assert_eq!(
        responses, 0,
        "no response entry should be committed on persistence failure"
    );
}

#[tokio::test]
async fn test_plan_step_execution_uses_turn_marker() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn compute() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add src", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Implement compute".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan {
        id: "marker-plan".to_string(),
        title: "Marker Plan".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![item],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let marker = "TESTMARKER1";
    let response = format!(
        "<tauqe_edits_{m} summary=\"Implement compute\">\n<edit_{m} path=\"src.rs\">\n<search_{m}>\npub fn compute() -> i32 {{ 0 }}\n</search_{m}>\n<replace_{m}>\npub fn compute() -> i32 {{ 42 }}\n</replace_{m}>\n</edit_{m}>\n<success_{m} summary=\"done\" />\n</tauqe_edits_{m}>",
        m = marker
    );
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
    config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
    config.models.auto_level_up = false;

    let state = test_state_with_provider(&dir, config, provider.clone()).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(&state, "marker-plan", vec!["1".to_string()], operation).await;
    assert!(
        matches!(outcome, BatchOutcome::Completed { steps: 1 }),
        "expected a completed batch, got {:?}",
        outcome
    );

    // Both the system prompt and the step prompt must carry the marker suffix.
    let request = &provider.requests()[0];
    assert!(request.messages[0]
        .content
        .contains(&format!("scratchpad_{}", marker)));
    assert!(request.messages[0]
        .content
        .contains(&format!("success_{}", marker)));

    // The marker-bound edits were applied and the step reached DONE.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn compute() -> i32 { 42 }\n"
    );
    let reloaded = PlanStorage::load_plan(dir.path(), "marker-plan")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
}

#[tokio::test]
async fn test_plan_batch_automated_replanning_on_blocked_step() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan {
        id: "auto-replan".to_string(),
        title: "Auto Replan".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![item],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let marker = "AUTOREPLANM1";
    // 1. Coder turn returns a semantic blocked outcome:
    let blocked_response = format!(
        "<blocked_{m} reason=\"Precondition missing: value signature invalid\" />",
        m = marker
    );
    // 2. Architect replanning turn returns a new horizon with replacement step "1.1":
    let replan_output = serde_json::json!({
        "status": "success",
        "explanation": "Replanned step 1 into replacement 1.1",
        "items": [
            {
                "id": "1.1",
                "title": "Step 1.1 replacement",
                "status": "todo",
                "files": ["src.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    // 3. Coder turn for step "1.1" succeeds:
    let success_response = format!(
        "<tauqe_edits_{m} summary=\"Fix value\">\n<edit_{m} path=\"src.rs\">\n<search_{m}>\npub fn value() -> i32 {{ 0 }}\n</search_{m}>\n<replace_{m}>\npub fn value() -> i32 {{ 100 }}\n</replace_{m}>\n</edit_{m}>\n<success_{m} summary=\"done\" />\n</tauqe_edits_{m}>",
        m = marker
    );

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(blocked_response),
        MockResponse::text(replan_output.to_string()),
        MockResponse::text(success_response),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
    config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
    config.develop.max_replans = 2;
    config.models.auto_level_up = false;

    let state = test_state_with_provider(&dir, config, provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(&state, "auto-replan", vec!["1".to_string()], operation).await;

    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });

    let reloaded = PlanStorage::load_plan(dir.path(), "auto-replan")
        .unwrap()
        .unwrap();
    assert!(reloaded.find_item("1.1").is_some());
    assert_eq!(
        reloaded.find_item("1.1").unwrap().status,
        PlanItemStatus::Done
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 100 }\n"
    );
}

#[tokio::test]
async fn test_plan_batch_automated_replanning_limit_exhausted() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan {
        id: "limit-replan".to_string(),
        title: "Limit Replan".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![item],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let marker = "LIMITREPLANM1";
    // 1. First coder turn blocks:
    let blocked_1 = format!("<blocked_{m} reason=\"First blocker\" />", m = marker);
    // 2. Architect replanning replaces step 1 with 1.1:
    let replan_output = serde_json::json!({
        "status": "success",
        "explanation": "Replanned into 1.1",
        "items": [
            {
                "id": "1.1",
                "title": "Step 1.1",
                "status": "todo",
                "files": ["src.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    // 3. Second coder turn on step 1.1 also blocks:
    let blocked_2 = format!("<blocked_{m} reason=\"Second blocker\" />", m = marker);

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(blocked_1),
        MockResponse::text(replan_output.to_string()),
        MockResponse::text(blocked_2),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
    config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
    // Limit max_replans to 1:
    config.develop.max_replans = 1;
    config.models.auto_level_up = false;

    let state = test_state_with_provider(&dir, config, provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(&state, "limit-replan", vec!["1".to_string()], operation).await;

    match outcome {
        BatchOutcome::Blocked {
            completed,
            step_id,
            reason,
        } => {
            assert_eq!(completed, 0);
            assert_eq!(step_id, "1.1");
            assert!(
                reason.contains("Automated replanning limit reached (max_replans = 1)"),
                "unexpected block reason: {}",
                reason
            );
            assert!(reason.contains("Manual developer intervention required"));
        }
        other => panic!(
            "Expected Blocked outcome with limit reached, got {:?}",
            other
        ),
    }

    let reloaded = PlanStorage::load_plan(dir.path(), "limit-replan")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.find_item("1.1").unwrap().status,
        PlanItemStatus::Blocked
    );
}

#[tokio::test]
async fn test_plan_batch_stops_gracefully_on_soft_stop() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "soft-stop".to_string(),
        title: "Soft Stop".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![
            test_item("1", PlanItemStatus::Todo),
            test_item("2", PlanItemStatus::Todo),
        ],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    // A soft stop requested before the batch starts stops the queue before
    // the first step runs.
    state.soft_stop_active_operation();

    let outcome = run_plan_batch(
        &state,
        "soft-stop",
        vec!["1".to_string(), "2".to_string()],
        operation,
    )
    .await;

    assert_eq!(outcome, BatchOutcome::Stopped { completed: 0 });

    // No step was started: every status stays TODO.
    let reloaded = PlanStorage::load_plan(dir.path(), "soft-stop")
        .unwrap()
        .unwrap();
    assert!(reloaded
        .items
        .iter()
        .all(|i| i.status == PlanItemStatus::Todo));

    // The supervisor publishes a single typed completion event carrying the
    // graceful "stopped" status.
    let mut raw_events = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        raw_events.push(raw);
    }
    let finished = raw_events
        .iter()
        .find(|e| e.contains(events::PLAN_BATCH_FINISHED))
        .expect("plan/batch_finished must be emitted");
    assert!(finished.contains("\"soft-stop\""));
    assert!(finished.contains("\"stopped\""));
}

/// A provider that reports cancellation exclusively through an explicit
/// `StreamEvent::Cancelled`, without the turn's cancel signal ever being
/// raised. This reproduces the race the architect stream must tolerate:
/// cancellation reported by the stream event alone.
struct StreamCancellingProvider;

#[async_trait::async_trait]
impl tauqe_core::providers::LlmProvider for StreamCancellingProvider {
    fn kind(&self) -> tauqe_protocol::ProviderKind {
        tauqe_protocol::ProviderKind::OpenRouter
    }

    async fn stream_chat(
        &self,
        _model: &str,
        _messages: Vec<tauqe_core::providers::ChatMessage>,
        _response_format: Option<tauqe_core::providers::ResponseFormat>,
        tx: tokio::sync::mpsc::Sender<tauqe_core::providers::StreamEvent>,
        _cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let _ = tx.send(tauqe_core::providers::StreamEvent::Cancelled).await;
        Ok(())
    }
}

#[tokio::test]
async fn test_plan_refine_reports_cancellation_from_stream_event() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "cancel-refine".to_string(),
        title: "Cancel Refine".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let provider: Arc<dyn tauqe_core::providers::LlmProvider> = Arc::new(StreamCancellingProvider);
    let state =
        test_state_with_provider(&dir, tauqe_core::config::AppConfig::default(), provider).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "cancel-refine" })),
    };
    let res = handle_plan_refine(req, &state).await;

    // An explicit `Cancelled` stream event must collapse into `CANCELLED`,
    // never into a `PARSE_ERROR` on the truncated JSON buffer.
    let err = res.error.expect("a cancelled refine must return an error");
    assert_eq!(err.code, errors::CANCELLED);
}

#[tokio::test]
async fn test_plan_batch_projects_conceptual_horizon_just_in_time() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    // A plan carrying only its architectural concept has no leaf to schedule.
    let plan = Plan {
        id: "jit-horizon".to_string(),
        title: "JIT Horizon".to_string(),
        created_at: 1,
        updated_at: 1,
        concept: vec![tauqe_protocol::ConceptItem {
            id: "c-1".to_string(),
            title: "Invariant".to_string(),
            details: None,
            children: Vec::new(),
        }],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let marker = "JITHORIZONM1";
    // 1. The Architect projects the first horizon just-in-time:
    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Projected the first horizon",
        "items": [
            {
                "id": "1",
                "title": "Step 1",
                "status": "todo",
                "files": ["src.rs"],
                "readonly_files": [],
                "concept_refs": ["c-1"],
                "depends_on": [],
                "children": []
            }
        ]
    });
    // 2. The projected step is then executed by the coder:
    let success_response = format!(
        "<tauqe_edits_{m} summary=\"Set value\">\n<edit_{m} path=\"src.rs\">\n<search_{m}>\npub fn value() -> i32 {{ 0 }}\n</search_{m}>\n<replace_{m}>\npub fn value() -> i32 {{ 5 }}\n</replace_{m}>\n</edit_{m}>\n<success_{m} summary=\"done\" />\n</tauqe_edits_{m}>",
        m = marker
    );

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(refine_output.to_string()),
        MockResponse::text(success_response),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
    config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
    config.models.auto_level_up = false;

    let state = test_state_with_provider(&dir, config, provider).await;
    *state.marker_override.lock().await = Some(marker.to_string());

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(&state, "jit-horizon", Vec::new(), operation).await;

    // An empty scheduled scope is not a finished batch: the synthesized step is
    // executed within the same supervisor run.
    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 5 }\n"
    );
    let reloaded = PlanStorage::load_plan(dir.path(), "jit-horizon")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
}

#[tokio::test]
async fn test_handle_plan_execute_pipeline_validation() {
    let dir = TestDir::new();
    let state = test_state(&dir);
    let make_req = |id: u64, params: Option<serde_json::Value>| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::PLAN_EXECUTE_PIPELINE.to_string(),
        params,
    };

    // Missing params entirely.
    let res = handle_plan_execute_pipeline(make_req(1, None), &state).await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // An empty queue is not a pipeline.
    let res = handle_plan_execute_pipeline(
        make_req(2, Some(serde_json::json!({ "plan_ids": [] }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // Unknown plan rejects the whole queue before any turn starts.
    let res = handle_plan_execute_pipeline(
        make_req(3, Some(serde_json::json!({ "plan_ids": ["missing"] }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "NOT_FOUND");

    // A queue whose plans are all finished has no leaf to schedule.
    let plan = Plan {
        id: "done-pipeline".to_string(),
        title: "Done Pipeline".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Done)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let res = handle_plan_execute_pipeline(
        make_req(
            4,
            Some(serde_json::json!({ "plan_ids": ["done-pipeline"] })),
        ),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "NOTHING_TO_EXECUTE");

    // No rejected pipeline leaves the operation slot registered.
    assert!(!state.is_operation_active());
}

#[tokio::test]
async fn test_plan_pipeline_blocked_by_failed_preflight() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "pipeline-preflight".to_string(),
        title: "Pipeline Preflight".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    // A malformed manifest makes the deterministic toolchain pipeline fail.
    std::fs::write(dir.path().join("Cargo.toml"), "[package\n").unwrap();
    let state = test_state(&dir);

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_EXECUTE_PIPELINE.to_string(),
        params: Some(serde_json::json!({ "plan_ids": ["pipeline-preflight"] })),
    };
    let res = handle_plan_execute_pipeline(req, &state).await;

    let err = res.error.expect("preflight must block the pipeline");
    assert_eq!(err.code, "PREFLIGHT_VERIFICATION_FAILED");
    assert!(err.message.contains("Pre-flight verification failed"));

    // The failure is detected before any plan starts: no active operation.
    assert!(!state.is_operation_active());
    let reloaded = PlanStorage::load_plan(dir.path(), "pipeline-preflight")
        .unwrap()
        .unwrap();
    assert!(reloaded
        .items
        .iter()
        .all(|i| i.status == PlanItemStatus::Todo));
}

#[tokio::test]
async fn test_handle_plan_refine_renders_targeted_repo_map_feedback() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/target.rs"), "pub fn target() {}\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src/target.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add src", false).unwrap();

    let plan = Plan {
        id: "refine-repomap".to_string(),
        title: "Refine RepoMap".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // Round 1: the architect requests the symbol contour of a subsystem it has
    // not inspected yet. Round 2: it finalizes once it receives that contour.
    let needs_context = serde_json::json!({
        "status": "needs_context",
        "explanation": "Outlining the src subsystem",
        "reason": null,
        "context_requests": [],
        "repo_searches": [],
        "repo_maps": ["src"],
        "items": []
    });
    let success = serde_json::json!({
        "status": "success",
        "explanation": "Decomposed the subsystem",
        "reason": null,
        "context_requests": [],
        "repo_searches": [],
        "repo_maps": [],
        "items": [
            {
                "id": "1.1",
                "title": "Implement target",
                "status": "todo",
                "files": ["src/target.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(needs_context.to_string()),
        MockResponse::text(success.to_string()),
    ]));
    let state = test_state_with_provider(
        &dir,
        tauqe_core::config::AppConfig::default(),
        mock_provider.clone(),
    )
    .await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "refine-repomap" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    // The rendered contour of the requested subsystem is fed back to the next
    // architect round instead of being treated as a dead end.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 2);
    let feedback_prompt = &requests[1].messages[1].content;
    assert!(feedback_prompt.contains("<repomap_feedback path=\"src\">"));
    assert!(feedback_prompt.contains("pub fn target"));

    let reloaded = PlanStorage::load_plan(dir.path(), "refine-repomap")
        .unwrap()
        .unwrap();
    assert!(reloaded.find_item("1.1").is_some());
}

#[tokio::test]
async fn test_handle_plan_refine_renders_directory_outline_when_repo_map_disabled() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    std::fs::write(
        dir.path().join("src/nested/helper.rs"),
        "pub fn helper() {}\n",
    )
    .unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src/nested/helper.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add src", false).unwrap();

    let plan = Plan {
        id: "refine-dirs".to_string(),
        title: "Refine Dirs".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let success = serde_json::json!({
        "status": "success",
        "explanation": "Decomposed the subsystem",
        "reason": null,
        "context_requests": [],
        "repo_searches": [],
        "repo_maps": [],
        "items": [
            {
                "id": "2",
                "title": "Implement helper",
                "status": "todo",
                "files": ["src/nested/helper.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        success.to_string(),
    )]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.context.include_repo_map = false;
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "refine-dirs" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    // With eager symbol maps disabled the architect still learns where the
    // subsystems live from the compact directory tree, never from an absent
    // contour section.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 1);
    let user_prompt = &requests[0].messages[1].content;
    assert!(user_prompt.contains("## Repository Directories (<repo_dirs>):"));
    assert!(user_prompt.contains("src\n  nested"));
    assert!(!user_prompt.contains("## Codebase Symbol Contours"));
}

#[tokio::test]
async fn test_plan_refine_regenerates_round_after_unparsable_response() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "parse-retry".to_string(),
        title: "Parse Retry".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // The first round is cut off mid-JSON, the second one is complete.
    let truncated = "{\"status\": \"success\", \"explanation\": \"Decomposed";
    let complete = serde_json::json!({
        "status": "success",
        "explanation": "Decomposed after the regenerated round",
        "reason": null,
        "context_requests": [],
        "repo_searches": [],
        "repo_maps": [],
        "items": [
            {
                "id": "1",
                "title": "Refined step 1",
                "status": "todo",
                "files": [],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(truncated),
        MockResponse::text(complete.to_string()),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.develop.max_retries = 2;
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "parse-retry" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    // An interrupted structured payload regenerates the round instead of
    // aborting the whole synthesis on its first occurrence.
    assert_eq!(mock_provider.requests().len(), 2);

    let reloaded = PlanStorage::load_plan(dir.path(), "parse-retry")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.find_item("1").unwrap().title, "Refined step 1");
}

#[tokio::test]
async fn test_plan_refine_recovers_from_a_transport_break() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "stream-break".to_string(),
        title: "Stream Break".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Decomposed after the interrupted round",
        "reason": null,
        "context_requests": [],
        "repo_searches": [],
        "repo_maps": [],
        "items": [
            {
                "id": "1.1",
                "title": "Survived the interruption",
                "status": "todo",
                "files": [],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });

    // The first attempt is cut off at the transport layer; the retry
    // decorator re-issues the request and the second attempt answers with a
    // complete structured horizon.
    let mock = MockLlmProvider::scripted([
        MockResponse::error("OpenRouter stream ended prematurely without [DONE] marker"),
        MockResponse::text(refine_output.to_string()),
    ]);
    let provider: Arc<dyn tauqe_core::providers::LlmProvider> =
        Arc::new(tauqe_core::providers::RetryingProvider::new(
            Box::new(mock),
            tauqe_core::providers::RetryConfig {
                max_attempts: 3,
                base_delay_ms: 1,
                max_delay_ms: 1,
            },
        ));
    let state =
        test_state_with_provider(&dir, tauqe_core::config::AppConfig::default(), provider).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "stream-break" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    // The interrupted round never poisons the synthesis: the retried round
    // replaces the uncompleted horizon.
    let reloaded = PlanStorage::load_plan(dir.path(), "stream-break")
        .unwrap()
        .unwrap();
    assert!(reloaded.find_item("1.1").is_some());
    assert!(reloaded.find_item("1").is_none());
}

#[tokio::test]
async fn test_plan_refine_fails_after_parse_retries_are_exhausted() {
    let dir = TestDir::new();
    let plan = Plan {
        id: "parse-exhausted".to_string(),
        title: "Parse Exhausted".to_string(),
        created_at: 1,
        updated_at: 1,
        items: vec![test_item("1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // Both the round and its single retry answer with an empty payload.
    let mock_provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(""),
        MockResponse::text("   "),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.develop.max_retries = 1;
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "parse-exhausted" })),
    };
    let res = handle_plan_refine(req, &state).await;

    // The retry budget is bounded: an exhausted budget terminates the synthesis
    // instead of looping forever.
    let err = res.error.expect("an exhausted retry budget must fail");
    assert_eq!(err.code, errors::PARSE_ERROR);
    assert_eq!(mock_provider.requests().len(), 2);

    // A failed synthesis leaves the stored plan untouched.
    let reloaded = PlanStorage::load_plan(dir.path(), "parse-exhausted")
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.find_item("1").unwrap().title, "Step 1");
    assert_eq!(
        reloaded.find_item("1").unwrap().status,
        PlanItemStatus::Todo
    );
}
