use super::batch::{
    handle_plan_execute, handle_plan_execute_pipeline, handle_plan_refine, handle_plan_replan,
    run_plan_batch, BatchOutcome, PlanBatchOptions,
};
use crate::handlers::common::test_support::scratch_root;
use crate::state::AppState;
use crate::transport::OutChannel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauqe_core::context::ContextAccess;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::plan::{Plan, PlanItemStatus};
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_protocol::{errors, events, PlanRefineResult, Request};

struct TestDir(std::path::PathBuf);
impl TestDir {
    fn new() -> Self {
        Self(scratch_root("test-plan"))
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

fn test_item(id: &str, status: PlanItemStatus) -> tauqe_core::plan::PlanItem {
    tauqe_core::plan::PlanItem {
        id: id.to_string(),
        title: format!("Step {}", id),
        status,
        files: vec![format!("src/step-{id}.rs")],
        ..Default::default()
    }
}

/// Declares one rung of a model role ladder that receives `attempts` content
/// attempts before the ladder escalates to the next rung.
fn tier_with_attempts(model: &str, attempts: u32) -> tauqe_core::models::ModelEntry {
    let mut entry =
        tauqe_core::models::ModelEntry::new(tauqe_core::models::ModelRef::openrouter(model), None);
    entry.attempts = Some(attempts);
    entry
}

/// Returns the whole prompt surface of one round.
///
/// The context files of a round travel as their own transcript messages, so a
/// test that asks what the model was told reads every message of the request
/// instead of the system head alone.
fn round_text(messages: &[tauqe_core::providers::ChatMessage]) -> String {
    messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<&str>>()
        .join("\n")
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
    state.application.test_set_provider(provider).await;
    state
}

#[cfg(unix)]
struct PlanExecutionFixture {
    dir: TestDir,
    state: Arc<AppState>,
    events: tokio::sync::mpsc::UnboundedReceiver<String>,
    initial_head: String,
}

#[cfg(unix)]
impl PlanExecutionFixture {
    async fn new(verify: &str, leaves: usize, provider: Arc<MockLlmProvider>) -> Self {
        let items = (1..=leaves)
            .map(|id| public_plan_leaf(&id.to_string()))
            .collect();
        Self::with_items(verify, items, provider).await
    }

    async fn with_items(
        verify: &str,
        items: Vec<tauqe_core::plan::PlanItem>,
        provider: Arc<MockLlmProvider>,
    ) -> Self {
        use tauqe_core::git::{cmd, init_repository};
        let dir = TestDir::new();
        let root = dir.path();
        init_repository(Some(root), true).unwrap();
        std::fs::write(root.join("source.txt"), "original\n").unwrap();
        std::fs::write(root.join("helper.txt"), "Reference for the coder\n").unwrap();
        std::fs::write(
            root.join("tauqe.toml"),
            format!("[models]\nrepl = ['openrouter:coder/one']\n[project.verify]\ncommand = 'sh'\nargs = ['-c', '''{verify}''']\n"),
        )
        .unwrap();
        cmd::stage_files(
            root,
            &[
                "source.txt".into(),
                "helper.txt".into(),
                "tauqe.toml".into(),
            ],
        )
        .unwrap();
        cmd::commit_with_developer_author(root, "Add plan fixtures", false).unwrap();
        let initial_head = cmd::run_git(Some(root), &["rev-parse", "HEAD"]).unwrap();
        let plan = Plan::from_parts(
            "public-run".into(),
            "Public run".into(),
            None,
            1,
            1,
            vec![],
            items,
        );
        PlanStorage::save_plan(root, &plan).unwrap();

        let mut config = tauqe_core::config::AppConfig {
            repl: serde_json::from_value(serde_json::json!({
                "workflow": "git", "patch_strategy": "apply_patch", "max_turn_rounds": 4
            }))
            .unwrap(),
            ..Default::default()
        };
        config.models.auto_level_up = true;
        config.models.coder = Some(vec![
            tier_with_attempts("coder/one", 1),
            tier_with_attempts("coder/two", 1),
        ]);
        config.context.include_repo_map = false;
        let (out, events) = OutChannel::new();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(config, root.to_path_buf(), history, out);
        state.application.test_set_provider(provider).await;
        Self {
            dir,
            state,
            events,
            initial_head,
        }
    }

    async fn start(&self) -> String {
        self.start_scope(None).await["operation_id"]
            .as_str()
            .unwrap()
            .into()
    }

    async fn start_scope(&self, step_id: Option<&str>) -> serde_json::Value {
        let response = handle_plan_execute(
            Request {
                id: tauqe_protocol::RequestId::Number(1),
                method: tauqe_protocol::methods::PLAN_EXECUTE.into(),
                params: Some(serde_json::json!({"plan_id": "public-run", "step_id": step_id})),
            },
            &self.state,
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        response.result.unwrap()
    }

    async fn finish(&mut self, operation_id: &str, status: &str) -> Vec<serde_json::Value> {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let mut observed = Vec::new();
            loop {
                let raw = self.events.recv().await.expect("plan event channel closed");
                let event: serde_json::Value = serde_json::from_str(&raw).unwrap();
                let terminal = event["method"] == events::PLAN_BATCH_FINISHED;
                if terminal {
                    assert_eq!(event["params"]["operation_id"], operation_id);
                    assert_eq!(event["params"]["summary"]["status"], status, "{event}");
                }
                observed.push(event);
                if terminal {
                    return observed;
                }
            }
        })
        .await
        .expect("the plan batch must settle")
    }

    fn git(&self, args: &[&str]) -> String {
        tauqe_core::git::cmd::run_git(Some(self.dir.path()), args).unwrap()
    }

    fn leaf(&self, id: &str) -> tauqe_core::plan::PlanItem {
        PlanStorage::load_plan(self.dir.path(), "public-run")
            .unwrap()
            .unwrap()
            .implementation
            .find_item(id)
            .unwrap()
            .clone()
    }

    fn assert_committed(&self) -> String {
        let leaf = self.leaf("1");
        assert_eq!(leaf.status, PlanItemStatus::Done);
        let hash = leaf.commit.unwrap();
        assert_eq!(
            self.git(&["rev-parse", &hash]),
            self.git(&["rev-parse", "HEAD"])
        );
        assert_eq!(self.git(&["rev-parse", "HEAD^"]), self.initial_head);
        assert_eq!(self.git(&["show", "HEAD:source.txt"]).trim(), "verified");
        hash
    }
}

#[cfg(unix)]
fn public_plan_leaf(id: &str) -> tauqe_core::plan::PlanItem {
    let mut item = test_item(id, PlanItemStatus::Todo);
    item.files = vec!["source.txt".into()];
    item
}

#[cfg(unix)]
fn plan_patch(old: &str, new: &str) -> MockResponse {
    MockResponse::text(serde_json::json!({
        "message": "Update source",
        "action": {
            "kind": "edit",
            "patch": format!("*** Begin Patch\n*** Update File: source.txt\n@@\n-{old}\n+{new}\n*** End Patch"),
            "commit_message": "fix(source): update source",
            "error_message": "",
            "reason": ""
        }
    }).to_string())
}

#[cfg(unix)]
#[tokio::test]
async fn public_plan_executes_only_the_addressed_subtree() {
    let provider = Arc::new(MockLlmProvider::scripted([
        plan_patch("original", "first"),
        plan_patch("first", "second"),
    ]));
    let branch = tauqe_core::plan::PlanItem {
        children: vec![public_plan_leaf("2.1"), public_plan_leaf("2.2")],
        ..test_item("2", PlanItemStatus::Todo)
    };
    let mut run = PlanExecutionFixture::with_items(
        "true",
        vec![public_plan_leaf("1"), branch],
        provider.clone(),
    )
    .await;

    let started = run.start_scope(Some("2")).await;
    assert_eq!(started["step_ids"], serde_json::json!(["2.1", "2.2"]));
    let id = started["operation_id"].as_str().unwrap();
    let observed = run.finish(id, "completed").await;
    assert_eq!(
        observed
            .iter()
            .filter(|event| event["method"] == events::MODEL_STARTED)
            .count(),
        2
    );
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(run.leaf("2.1").status, PlanItemStatus::Done);
    assert_eq!(run.leaf("2.2").status, PlanItemStatus::Done);
    let sibling = run.leaf("1");
    assert_eq!(sibling.status, PlanItemStatus::Todo);
    assert!(sibling.commit.is_none());
    assert_eq!(run.git(&["show", "HEAD:source.txt"]).trim(), "second");
}

#[cfg(unix)]
#[tokio::test]
async fn public_plan_repairs_and_verifies_in_one_turn_before_committing() {
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(r#"{"message":"Read helper","action":{"kind":"research","context_requests":[{"path":"helper.txt","access":"read_only"}]}}"#),
        plan_patch("no matching line", "temporary"),
        plan_patch("original", "temporary"),
        plan_patch("temporary", "verified"),
    ].map(|response| response.with_usage(tauqe_core::providers::ModelUsageInfo {
        prompt_tokens: 3,
        completion_tokens: 4,
        total_tokens: 7,
        ..Default::default()
    }))));
    let mut run = PlanExecutionFixture::new(
        "grep -qx original source.txt || grep -qx verified source.txt",
        1,
        provider.clone(),
    )
    .await;
    let id = run.start().await;
    let observed = run.finish(&id, "completed").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests.iter().all(|request| request.model == "coder/one"
        && request.session_id.as_deref() == Some("tauqe-plan-public-run")));
    assert!(
        round_text(&requests[0].messages).contains("<file path=\"source.txt\" access=\"editable\"")
    );
    assert!(!round_text(&requests[0].messages).contains("Reference for the coder"));
    assert!(round_text(&requests[1].messages).contains("Reference for the coder"));
    assert!(round_text(&requests[2].messages).contains("no matching line"));
    assert!(round_text(&requests[3].messages).contains("Code verification failed"));
    assert!(requests
        .windows(2)
        .all(|pair| pair[1].messages.starts_with(&pair[0].messages)));

    let rounds: Vec<_> = observed
        .iter()
        .filter(|event| event["method"] == events::TURN_ROUND_STARTED)
        .map(|event| &event["params"])
        .collect();
    assert_eq!(rounds.len(), 4);
    for (index, round) in rounds.iter().enumerate() {
        assert_eq!(round["round"], index + 1);
        assert_eq!(round["max_rounds"], 4);
        assert_eq!(round["operation_id"], rounds[0]["operation_id"]);
    }
    let verification: Vec<_> = observed
        .iter()
        .enumerate()
        .filter(|(_, event)| event["method"] == events::TOOLCHAIN_FINISHED)
        .map(|(index, event)| {
            (
                index,
                event["params"]["command"].as_str().unwrap(),
                event["params"]["success"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        verification
            .iter()
            .map(|(_, command, passed)| (*command, *passed))
            .collect::<Vec<_>>(),
        [("pre-flight", true), ("verify", false), ("verify", true)]
    );
    let model_started = observed
        .iter()
        .position(|event| event["method"] == events::MODEL_STARTED)
        .expect("the admitted step starts a model turn");
    assert!(verification[0].0 < model_started);
    let preflight_started: Vec<_> = observed
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["method"] == events::TOOLCHAIN_STARTED
                && event["params"]["command"] == "pre-flight"
        })
        .collect();
    assert_eq!(preflight_started.len(), 1);
    assert!(preflight_started[0].0 < verification[0].0);
    assert_eq!(preflight_started[0].1["params"]["operation_id"], id);
    assert_eq!(observed[verification[0].0]["params"]["operation_id"], id);
    let edits: Vec<_> = observed
        .iter()
        .enumerate()
        .filter(|(_, event)| event["method"] == events::EDIT_FINISHED)
        .collect();
    assert_eq!(edits.len(), 1);
    assert!(verification[2].0 < edits[0].0);
    let hash = run.assert_committed();
    assert_eq!(edits[0].1["params"]["commit_hash"], hash);
    assert_eq!(
        std::fs::read(run.dir.path().join("source.txt")).unwrap(),
        b"verified\n"
    );
    let usage = run.state.application.usage_snapshot().await;
    assert!(usage.cur.is_none());
    assert_eq!(usage.prev.total_tokens, 28);
}

#[cfg(unix)]
#[tokio::test]
async fn public_plan_finalization_refusal_is_terminal_without_claiming_rollback() {
    let provider = Arc::new(MockLlmProvider::scripted([plan_patch(
        "original", "verified",
    )]));
    let mut run = PlanExecutionFixture::new(
        "grep -qx original source.txt || (grep -qx verified source.txt && touch .git/index.lock)",
        1,
        provider.clone(),
    )
    .await;
    let id = run.start().await;
    let observed = run.finish(&id, "failed").await;
    let terminal = observed.last().unwrap();
    let reason = terminal["params"]["summary"]["error"].as_str().unwrap();
    assert!(reason.contains("index.lock"), "{reason}");
    assert!(!reason.to_lowercase().contains("rolled back"), "{reason}");
    assert_eq!(provider.requests().len(), 1);
    assert!(observed
        .iter()
        .all(|event| event["method"] != events::MODEL_ESCALATED));
    let edit = observed
        .iter()
        .find(|event| event["method"] == events::EDIT_FINISHED)
        .unwrap();
    assert_eq!(edit["params"]["applied"], false);
    assert!(edit["params"]["commit_hash"].is_null());
    assert!(edit["params"]["error"]
        .as_str()
        .unwrap()
        .contains("index.lock"));
    assert_eq!(run.leaf("1").status, PlanItemStatus::Todo);
    assert!(run.leaf("1").commit.is_none());
    assert_eq!(
        std::fs::read(run.dir.path().join("source.txt")).unwrap(),
        b"verified\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn public_plan_stops_the_queue_only_after_the_applied_step_settles() {
    use std::time::Duration;

    for hard in [true, false] {
        let provider = Arc::new(MockLlmProvider::scripted([plan_patch(
            "developer work",
            "verified",
        )]));
        let mut run = PlanExecutionFixture::new(
            "if grep -qx verified source.txt; then touch .git/verify-entered; \
             for attempt in $(seq 1 200); do test ! -f .git/verify-release || exit 0; sleep 0.1; done; exit 1; fi",
            2,
            provider.clone(),
        )
        .await;
        let file = run.dir.path().join("source.txt");
        std::fs::write(&file, "developer work\n").unwrap();
        let id = run.start().await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !run.dir.path().join(".git/verify-entered").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the first step must reach verification after temporary application");
        assert_eq!(std::fs::read(&file).unwrap(), b"verified\n");
        assert!(run.state.is_operation_active());
        assert_eq!(run.leaf("1").status, PlanItemStatus::InProgress);
        assert_eq!(run.leaf("2").status, PlanItemStatus::Todo);

        if hard {
            run.state.hard_cancel_active_operation();
        } else {
            run.state.soft_stop_active_operation();
            assert!(run.state.is_operation_active());
            assert_eq!(provider.requests().len(), 1);
            std::fs::write(run.dir.path().join(".git/verify-release"), "").unwrap();
        }
        let observed = run
            .finish(&id, if hard { "cancelled" } else { "stopped" })
            .await;
        assert_eq!(
            observed
                .iter()
                .filter(|event| event["method"] == events::MODEL_STARTED)
                .count(),
            1
        );
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(run.leaf("2").status, PlanItemStatus::Todo);
        assert!(!run
            .dir
            .path()
            .join(".tauqe/active-transaction.json")
            .exists());
        if hard {
            assert_eq!(std::fs::read(&file).unwrap(), b"developer work\n");
            assert_eq!(run.git(&["rev-parse", "HEAD"]), run.initial_head);
            let leaf = run.leaf("1");
            assert_eq!(leaf.status, PlanItemStatus::Todo);
            assert!(leaf.commit.is_none());
        } else {
            assert_eq!(std::fs::read(&file).unwrap(), b"verified\n");
            run.assert_committed();
        }
    }
}

#[tokio::test]
async fn test_plan_batch_fail_fast_resets_status() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "batch".to_string(),
        "Batch".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![
            test_item("1", PlanItemStatus::Todo),
            test_item("2", PlanItemStatus::Todo),
        ],
    );
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
        PlanBatchOptions::default(),
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
        .implementation
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
    let plan = Plan::from_parts(
        "emit".to_string(),
        "Emit".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![
            test_item("1", PlanItemStatus::Todo),
            test_item("2", PlanItemStatus::Todo),
        ],
    );
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
        PlanBatchOptions::default(),
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
    let plan = Plan::from_parts(
        "skip".to_string(),
        "Skip".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![
            test_item("1", PlanItemStatus::Done),
            test_item("2", PlanItemStatus::Discussion),
        ],
    );
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
        PlanBatchOptions::default(),
    )
    .await;
    match outcome {
        BatchOutcome::Failed {
            completed, reason, ..
        } => {
            assert_eq!(completed, 0, "a stalled scope executes no step");
            assert!(
                reason.contains("DISCUSSION"),
                "the diagnostic must name the lifecycle stall: {reason}"
            );
            assert!(
                reason.contains('2'),
                "the diagnostic must name the stalled step: {reason}"
            );
        }
        other => panic!("Expected Failed, got {:?}", other),
    }
    let reloaded = PlanStorage::load_plan(dir.path(), "skip").unwrap().unwrap();
    assert_eq!(
        reloaded.implementation.find_item("2").unwrap().status,
        PlanItemStatus::Discussion
    );
}

#[tokio::test]
async fn test_plan_batch_skips_done_and_stops_on_blocked() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "blocked-batch".to_string(),
        "Blocked Batch".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![
            test_item("1", PlanItemStatus::Done),
            test_item("2", PlanItemStatus::Blocked),
            test_item("3", PlanItemStatus::Todo),
        ],
    );
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
        PlanBatchOptions::default(),
    )
    .await;
    match outcome {
        BatchOutcome::Failed {
            completed, reason, ..
        } => {
            assert_eq!(completed, 0, "a stalled scope executes no step");
            assert!(reason.contains("BLOCKED"), "got: {reason}");
            assert!(
                reason.contains('2'),
                "the diagnostic must name the blocked step: {reason}"
            );
        }
        other => panic!("Expected Failed on blocked step, got {:?}", other),
    }
    let reloaded = PlanStorage::load_plan(dir.path(), "blocked-batch")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("2").unwrap().status,
        PlanItemStatus::Blocked
    );
    assert_eq!(
        reloaded.implementation.find_item("3").unwrap().status,
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

    let plan = Plan::from_parts(
        "val".to_string(),
        "Validation".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![
            test_item("1", PlanItemStatus::Discussion),
            test_item("2", PlanItemStatus::Cancelled),
            test_item("3", PlanItemStatus::Done),
            test_item("4", PlanItemStatus::Blocked),
            dep_item,
        ],
    );
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

/// `plan/refine` parses strictly through the typed DTO: a malformed field
/// answers `INVALID_PARAMS` instead of degrading to the active-plan fallback
/// or dropping the model selection.
#[tokio::test]
async fn test_handle_plan_refine_rejects_malformed_payloads() {
    let dir = TestDir::new();
    let state = test_state(&dir);
    let make_req = |id: u64, params: Option<serde_json::Value>| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params,
    };

    // A non-string plan_id never silently refines the active plan.
    let res = handle_plan_refine(
        make_req(1, Some(serde_json::json!({ "plan_id": 123 }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // A malformed model selection is rejected instead of being dropped, so
    // the synthesis can never run on the default architect by accident.
    let res = handle_plan_refine(
        make_req(
            2,
            Some(serde_json::json!({ "plan_id": "p", "model": { "garbage": true } })),
        ),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // A malformed instructions field is rejected instead of being dropped.
    let res = handle_plan_refine(
        make_req(
            3,
            Some(serde_json::json!({ "plan_id": "p", "instructions": 42 })),
        ),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // The retired `id` spelling no longer parses: a payload that names only
    // the legacy key is rejected with INVALID_PARAMS instead of silently
    // addressing the active plan, so the canonical field cannot drift.
    let res = handle_plan_refine(
        make_req(4, Some(serde_json::json!({ "id": "ghost" }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // A rejected request never claims the operation slot.
    assert!(!state.is_operation_active());
}

/// `plan/get` parses strictly through the typed DTO while keeping the empty
/// object as the canonical "active plan" request.
#[tokio::test]
async fn test_handle_plan_get_parses_payload_strictly() {
    let dir = TestDir::new();
    let state = test_state(&dir);
    let make_req = |id: u64, params: Option<serde_json::Value>| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::PLAN_GET.to_string(),
        params,
    };

    // The empty object still resolves through the active-plan fallback.
    let res =
        super::storage_ops::handle_plan_get(make_req(1, Some(serde_json::json!({}))), &state).await;
    assert!(res.error.is_none());

    // A non-string id answers INVALID_PARAMS instead of silently resolving
    // through the active plan.
    let res = super::storage_ops::handle_plan_get(
        make_req(2, Some(serde_json::json!({ "id": 123 }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");
}

#[tokio::test]
async fn test_handle_plan_refine_defaults_to_architect_role() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "refine-role".to_string(),
        "Refine Role".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let mut config = tauqe_core::config::AppConfig::default();
    config.models.architect = Some(vec![tauqe_core::models::ModelRef::openrouter(
        "test/architect-model",
    )
    .into()]);
    config.models.repl = vec![tauqe_core::models::ModelRef::openrouter("test/repl-model").into()];

    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Synthesized the horizon",
        "items": [
            {
                "id": "1",
                "title": "Refined step 1",
                "status": "todo",
                "files": ["src.rs"],
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
    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "refine-role" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    // A request that names no model runs the Architect role's configured base
    // rung, never the primary model of the interactive dialogue.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "test/architect-model");
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
    let plan = Plan::from_parts(
        "refine-auto".to_string(),
        "Refine Auto".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
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
        .mutate_context(|context| context.add_auto_file("file.rs", ContextAccess::ReadOnly))
        .await
        .expect("fixture context mutation runs")
        .unwrap();
    assert_eq!(state.context_snapshot().await.auto_files().len(), 1);

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "refine-auto" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "plan_refine failed: {:?}", res.error);

    assert!(
        state.context_snapshot().await.auto_files().is_empty(),
        "auto context should be cleared after plan refinement"
    );
}

#[tokio::test]
async fn test_plan_execute_blocked_by_failed_preflight() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "preflight".to_string(),
        "Preflight".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![
            test_item("1", PlanItemStatus::Todo),
            test_item("2", PlanItemStatus::Todo),
        ],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    // A failing verification command makes the deterministic pipeline fail.
    std::fs::write(
        dir.path().join("tauqe.toml"),
        "[project.verify]\ncommand = \"false\"\n",
    )
    .unwrap();
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
        .implementation
        .items
        .iter()
        .all(|i| i.status == PlanItemStatus::Todo));
}

/// A hard cancel tears the pre-flight verification down while the operation
/// slot is registered.
///
/// The gate inherits the operation's cancellation channel, so the substrate
/// kills the whole process group of the project command at the cancel
/// boundary: the request answers its cancelled verdict promptly instead of
/// waiting a minutes-long build out, the slot is released at once, and no
/// step was started.
#[cfg(unix)]
#[tokio::test]
async fn test_plan_execute_preflight_is_torn_down_by_a_hard_cancel() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "preflight-cancel".to_string(),
        "Preflight Cancel".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    // A verification command that blocks for a minute: a gate that ignored
    // the operation's cancellation channel would keep the slot registered
    // for the whole duration after a hard cancel.
    std::fs::write(
        dir.path().join("tauqe.toml"),
        "[models]\nrepl = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"sh\"\nargs = ['-c', 'printf started > started.marker; sleep 60']\n",
    )
    .unwrap();
    let state = test_state(&dir);

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_EXECUTE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "preflight-cancel" })),
    };
    let state_for_req = Arc::clone(&state);
    let handler = tokio::spawn(async move { handle_plan_execute(req, &state_for_req).await });

    // Cancel only once the verification command is demonstrably running, so
    // the prompt return proves the teardown instead of a slow start.
    let marker = dir.path().join("started.marker");
    let mut running = false;
    for _ in 0..200 {
        if marker.exists() {
            running = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(running, "the verification command never started");

    state.hard_cancel_active_operation();

    let res = tokio::time::timeout(std::time::Duration::from_secs(10), handler)
        .await
        .expect("the cancelled pre-flight must return instead of waiting the suite out")
        .expect("the handler task joins");
    assert_eq!(res.error.unwrap().code, errors::CANCELLED);

    // The cancelled gate released the slot at once, so the next mutation is
    // not refused behind a verification that never completes.
    let mut released = false;
    for _ in 0..1000 {
        if !state.is_operation_active() {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        released,
        "the cancelled pre-flight must release the operation slot"
    );

    // No step was started: every status stays TODO.
    let reloaded = PlanStorage::load_plan(dir.path(), "preflight-cancel")
        .unwrap()
        .unwrap();
    assert!(reloaded
        .implementation
        .items
        .iter()
        .all(|i| i.status == PlanItemStatus::Todo));
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

    let parent = tauqe_core::plan::PlanItem {
        id: "3".to_string(),
        title: "Parent".to_string(),
        status: PlanItemStatus::Todo,
        children: vec![test_item("3.1", PlanItemStatus::Todo)],
        ..Default::default()
    };
    let plan = Plan::from_parts(
        "replan".to_string(),
        "Replan".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![
            test_item("1", PlanItemStatus::Done),
            test_item("2", PlanItemStatus::Blocked),
            parent,
        ],
    );
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

/// `plan/replan` parses strictly through the typed DTO: a malformed field
/// answers `INVALID_PARAMS` while the legacy `id`/`item_id` spellings keep
/// working through the declared aliases.
#[tokio::test]
async fn test_handle_plan_replan_rejects_malformed_payloads() {
    let dir = TestDir::new();
    let state = test_state(&dir);
    let make_req = |id: u64, params: Option<serde_json::Value>| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::PLAN_REPLAN.to_string(),
        params,
    };

    // A non-string step_id answers INVALID_PARAMS instead of degrading to an
    // empty identifier.
    let res = handle_plan_replan(
        make_req(1, Some(serde_json::json!({ "plan_id": "p", "step_id": 7 }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // A malformed model selection is rejected instead of being dropped.
    let res = handle_plan_replan(
        make_req(
            2,
            Some(serde_json::json!({ "plan_id": "p", "step_id": "1", "model": 3 })),
        ),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");

    // The retired `id`/`item_id` spellings no longer parse: a payload that
    // names only the legacy keys is rejected with INVALID_PARAMS instead of
    // silently degrading to an unnamed plan or step.
    let res = handle_plan_replan(
        make_req(3, Some(serde_json::json!({ "id": "p", "item_id": "1" }))),
        &state,
    )
    .await;
    assert_eq!(res.error.unwrap().code, "INVALID_PARAMS");
}

#[tokio::test]
async fn test_handle_plan_replan_with_mock_provider() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "replan-mock".to_string(),
        "Replan Mock".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![
            test_item("1", PlanItemStatus::Done),
            test_item("2", PlanItemStatus::Blocked),
        ],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let replan_output = serde_json::json!({
        "status": "success",
        "explanation": "Replanned step 2",
        "items": [
            {
                "id": "2.1",
                "title": "Replacement step 2.1",
                "status": "todo",
                "files": ["src.rs"],
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
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
    assert!(reloaded.implementation.find_item("2.1").is_some());
    assert!(reloaded.implementation.find_item("2").is_none());
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

    let plan = Plan::from_parts(
        "replan-preload".to_string(),
        "Replan Preload".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Done), blocked],
    );
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
    // the context of the architect prompt before the very first generation
    // round, and the transcript carries each of them as a `<file>` element.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 1);
    let prompt = round_text(&requests[0].messages);
    assert!(prompt.contains("<file path=\"src/target.rs\""));
    assert!(prompt.contains("<file path=\"src/reference.rs\""));
    assert!(prompt.contains("pub fn target() {}"));
    assert!(prompt.contains("pub fn reference_helper() -> u64 { 7 }"));
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

    let plan = Plan::from_parts(
        "replan-satisfied".to_string(),
        "Replan Satisfied".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Done), blocked],
    );
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
    // The context feedback extends the task message, which closes the round.
    let feedback_prompt = round_text(&requests[1].messages);
    assert!(feedback_prompt.contains("<context_feedback>"));
    assert!(feedback_prompt.contains("already loaded in context"));

    let reloaded = PlanStorage::load_plan(dir.path(), "replan-satisfied")
        .unwrap()
        .unwrap();
    assert!(reloaded.implementation.find_item("2.1").is_some());
}

#[tokio::test]
async fn test_plan_refine_binds_the_architect_session() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "arch-session".to_string(),
        "Architect Session".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Synthesized the horizon",
        "items": [
            {
                "id": "1",
                "title": "Refined step 1",
                "status": "todo",
                "files": ["src.rs"],
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
async fn test_plan_batch_retries_model_error_then_accepts_explicit_skip_without_commit() {
    let dir = TestDir::new();
    let root = dir.path();
    tauqe_core::git::init_repository(Some(root), true).unwrap();
    let source = "pub fn value() -> i32 { 1 }\n";
    std::fs::write(root.join("src.rs"), source).unwrap();
    tauqe_core::git::cmd::stage_files(root, &["src.rs".into()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root, "Initial", false).unwrap();
    let head = || tauqe_core::git::cmd::run_git(Some(root), &["rev-parse", "HEAD"]).unwrap();
    let initial_head = head();
    let mut item = test_item("1", PlanItemStatus::Todo);
    item.files = vec!["src.rs".into()];
    let plan = Plan::from_parts(
        "skip-leaf".into(),
        "Skip leaf".into(),
        None,
        1,
        1,
        vec![],
        vec![item],
    );
    PlanStorage::save_plan(root, &plan).unwrap();

    let mut config = tauqe_core::config::AppConfig {
        repl: serde_json::from_value(serde_json::json!({
            "workflow": "git", "patch_strategy": "search_replace", "max_turn_rounds": 1
        }))
        .unwrap(),
        ..Default::default()
    };
    config.models.auto_level_up = false;
    config.models.coder = Some(vec![tier_with_attempts("coder/one", 2)]);
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(
            r#"{"message":"Could not finish","action":{"kind":"edit","changes":[],"commit_message":"","error_message":"Could not finish","reason":""}}"#,
        ),
        MockResponse::text(
            r#"{"message":"Already satisfied","action":{"kind":"edit","changes":[],"commit_message":"","error_message":"","reason":"The requested value is already present"}}"#,
        ),
    ]));
    let state = test_state_with_provider(&dir, config, provider.clone()).await;
    let operation = Arc::new(state.try_begin_operation().unwrap());
    let outcome = run_plan_batch(
        &state,
        "skip-leaf",
        vec!["1".into()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;
    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "the second attempt has a fresh one-round budget"
    );
    assert!(requests.iter().all(|request| request.model == "coder/one"));
    assert!(requests[1]
        .messages
        .iter()
        .all(|message| message.role != "assistant"));
    let stored = PlanStorage::load_plan(root, "skip-leaf").unwrap().unwrap();
    let leaf = stored.implementation.find_item("1").unwrap();
    assert_eq!(leaf.status, PlanItemStatus::Done);
    assert!(leaf.commit.is_none());
    assert_eq!(head(), initial_head);
    assert_eq!(
        std::fs::read_to_string(root.join("src.rs")).unwrap(),
        source
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
    let plan = Plan::from_parts(
        "plan-session".to_string(),
        "Plan Session".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let response = serde_json::json!({
        "message": "Set value",
        "action": {
            "kind": "edit",
            "patch": "*** Begin Patch\n*** Update File: src.rs\n@@\n-pub fn value() -> i32 { 0 }\n+pub fn value() -> i32 { 1 }\n*** End Patch",
            "commit_message": "done",
            "message": "",
            "reason": ""
        }
    })
    .to_string();
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    config.models.auto_level_up = false;
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "plan-session",
        vec!["1".to_string()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;
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

    let parent = tauqe_core::plan::PlanItem {
        id: "2".to_string(),
        title: "Parent".to_string(),
        status: PlanItemStatus::Todo,
        children: vec![nested],
        ..Default::default()
    };

    let mut top = test_item("1", PlanItemStatus::Todo);
    top.files = vec!["src/shared.rs".to_string()];
    top.readonly_files = vec!["src/ref.rs".to_string()];

    let plan = Plan::from_parts(
        "collect".to_string(),
        "Collect".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![top, parent],
    );

    // Recurses into children, collapses normalized duplicates across `files`
    // and `readonly_files`, and preserves first-seen order.
    assert_eq!(
        tauqe_flow::api::collect_plan_context_files(&plan),
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

    let plan = Plan::from_parts(
        "refine-preload".to_string(),
        "Refine Preload".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![step1, step2],
    );
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
    // single blocked step's. The transcript renders them as `<file>` elements of
    // the context, so the task fragment never restates a file it does not own.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 1);
    let prompt = round_text(&requests[0].messages);
    assert!(prompt.contains("<file path=\"src/a.rs\""));
    assert!(prompt.contains("<file path=\"src/b.rs\""));
    assert!(prompt.contains("<file path=\"src/ref.rs\""));
    assert!(prompt.contains("pub fn a() {}"));
    assert!(prompt.contains("pub fn b() {}"));
    assert!(prompt.contains("pub fn r() -> u64 { 1 }"));
}

#[tokio::test]
async fn plan_creation_runs_only_the_requested_phases_and_publishes_once() {
    for skip_critique in [false, true] {
        let dir = TestDir::new();
        let (out, mut events_rx) = OutChannel::new();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(Default::default(), dir.path().to_path_buf(), history, out);
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(
                r#"{"status":"success","explanation":"Drafted","items":[{"id":"1","title":"Draft","status":"todo","files":["src.rs"],"readonly_files":[],"concept_refs":[],"depends_on":[],"children":[]}]}"#,
            ),
            MockResponse::text(
                r#"{"verdict":"approved","summary":"Ready to issue","findings":[]}"#,
            ),
        ]));
        state.application.test_set_provider(provider.clone()).await;
        let response = super::two_phase::handle_plan_create_pipeline(
            Request {
                id: tauqe_protocol::RequestId::Number(1),
                method: tauqe_protocol::methods::PLAN_CREATE_PIPELINE.into(),
                params: Some(serde_json::json!({
                    "directives": "Create a sample plan",
                    "planner_model": tauqe_protocol::ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter("planner/model")),
                    "critic_model": tauqe_protocol::ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter("critic/model")),
                    "skip_critique": skip_critique
                })),
            },
            &state,
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let PlanRefineResult::Success { plan, .. } =
            serde_json::from_value(response.result.unwrap()).unwrap()
        else {
            panic!("creation must return the stored plan");
        };
        let requests = provider.requests();
        let models: Vec<_> = requests
            .iter()
            .map(|request| request.model.as_str())
            .collect();
        if skip_critique {
            assert_eq!(models, ["planner/model"]);
        } else {
            assert_eq!(models, ["planner/model", "critic/model"]);
        }
        let stored = PlanStorage::load_plan(dir.path(), &plan.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.implementation.items[0].title, "Draft");
        let updates: Vec<_> = std::iter::from_fn(|| events_rx.try_recv().ok())
            .map(|raw| serde_json::from_str::<serde_json::Value>(&raw).unwrap())
            .filter(|event| event["method"] == events::PLAN_UPDATED)
            .collect();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["params"]["is_save"], true);
        assert!(!state.is_operation_active());
    }
}

/// Mutate the source at the critic boundary, after the Planner's final draft.
/// The critic and reconciliation must consume the frozen evidence instead.
struct MutateAtCritique {
    provider: MockLlmProvider,
    source: std::path::PathBuf,
    original: String,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl tauqe_core::providers::LlmProvider for MutateAtCritique {
    fn kind(&self) -> tauqe_core::models::ProviderKind {
        tauqe_core::models::ProviderKind::OpenRouter
    }

    async fn stream(
        &self,
        request: tauqe_core::providers::LlmRequest<'_>,
        tx: tokio::sync::mpsc::Sender<tauqe_core::providers::StreamEvent>,
        cancel: tauqe_core::stop::HardCancellation,
    ) -> tauqe_core::providers::ProviderResult {
        let critique = self.calls.fetch_add(1, Ordering::SeqCst) == 2;
        if critique {
            std::fs::write(&self.source, "changed after draft\n").unwrap();
        }
        let result = self.provider.stream(request, tx, cancel).await;
        if critique {
            std::fs::write(&self.source, &self.original).unwrap();
        }
        result
    }
}

fn horizon_reply(title: &str) -> MockResponse {
    MockResponse::text(
        serde_json::json!({
            "status": "success", "explanation": "Complete horizon", "items": [{
                "id": "2", "title": title, "status": "todo", "files": ["target.rs"],
                "readonly_files": [], "concept_refs": [], "depends_on": [], "children": []
            }]
        })
        .to_string(),
    )
}

fn critique_reply(verdict: &str) -> MockResponse {
    MockResponse::text(
        serde_json::json!({
            "verdict": verdict, "summary": "Review of the complete draft",
            "findings": if verdict == "approved" { vec![] } else {
                vec![serde_json::json!({"category": "invariant_gap", "target": "2",
                    "detail": "Include the additional contract in step 2"})]
            }
        })
        .to_string(),
    )
}

fn synthesis_request(method: &str, params: serde_json::Value) -> Request {
    Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: method.into(),
        params: Some(params),
    }
}

fn synthesis_events(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> Vec<serde_json::Value> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|raw| serde_json::from_str(&raw).unwrap())
        .collect()
}

#[tokio::test]
async fn plan_creation_keeps_research_through_critique_and_reconciliation() {
    const ORIGINAL: &str = "pub fn unique_contract_marker() -> u32 { 17 }\n";
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    for (file, body) in [
        ("contract.rs", ORIGINAL),
        ("target.rs", "pub fn target() {}\n"),
        ("additional.rs", "pub fn additional_evidence() {}\n"),
    ] {
        std::fs::write(dir.path().join(file), body).unwrap();
    }
    tauqe_core::git::cmd::stage_files(
        dir.path(),
        &[
            "contract.rs".into(),
            "target.rs".into(),
            "additional.rs".into(),
        ],
    )
    .unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let research = |file: &str, search: bool| {
        MockResponse::text(
            serde_json::json!({
                "status": "needs_context", "explanation": "Inspect contract",
                "context_requests": [file],
                "repo_searches": if search {
                    vec![serde_json::json!({"path": "", "regex": "unique_contract_marker"})]
                } else { vec![] },
                "items": []
            })
            .to_string(),
        )
    };
    let scripts = [
        research("contract.rs", true),
        horizon_reply("Draft title"),
        critique_reply("changes_required"),
        research("additional.rs", false),
        horizon_reply("Reconciled title"),
    ]
    .map(|response| {
        response.with_usage(tauqe_core::providers::ModelUsageInfo {
            prompt_tokens: 3,
            completion_tokens: 4,
            total_tokens: 7,
            ..Default::default()
        })
    });
    let provider = Arc::new(MutateAtCritique {
        provider: MockLlmProvider::scripted(scripts),
        source: dir.path().join("contract.rs"),
        original: ORIGINAL.into(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let (out, mut rx) = OutChannel::new();
    let (history, _) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(Default::default(), dir.path().to_path_buf(), history, out);
    state.application.test_set_provider(provider.clone()).await;
    let res = super::two_phase::handle_plan_create_pipeline(
        synthesis_request(
            tauqe_protocol::methods::PLAN_CREATE_PIPELINE,
            serde_json::json!({
                "directives": "Build a contract-aware plan",
                "planner_model": tauqe_protocol::ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter("planner/model")),
                "critic_model": tauqe_protocol::ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter("critic/model"))
            }),
        ),
        &state,
    )
    .await;
    assert!(res.error.is_none(), "{:?}", res.error);
    let requests = provider.provider.requests();
    assert_eq!(
        requests
            .iter()
            .map(|r| r.model.as_str())
            .collect::<Vec<_>>(),
        [
            "planner/model",
            "planner/model",
            "critic/model",
            "planner/model",
            "planner/model"
        ]
    );
    let critique = round_text(&requests[2].messages);
    for expected in [
        ORIGINAL,
        "<search_feedback",
        "Draft title",
        "Build a contract-aware plan",
    ] {
        assert!(critique.contains(expected), "critic lacks {expected}");
    }
    assert!(!critique.contains("changed after draft"));
    assert!(!critique.contains("<scratchpad>"));
    let reconciliation = round_text(&requests[3].messages);
    for expected in [
        ORIGINAL,
        "Draft title",
        "Include the additional contract in step 2",
    ] {
        assert!(
            reconciliation.contains(expected),
            "reconciliation lacks {expected}"
        );
    }
    assert!(!reconciliation.contains("changed after draft"));
    assert!(!reconciliation.contains("<file path=\"additional.rs\""));
    assert!(round_text(&requests[4].messages).contains("<file path=\"additional.rs\""));
    assert!(round_text(&requests[4].messages).contains("pub fn additional_evidence"));
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.model == "critic/model")
            .count(),
        1
    );
    let events = synthesis_events(&mut rx);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["method"] == events::PLAN_UPDATED)
            .count(),
        1
    );
    let usage = state.application.usage_snapshot().await;
    assert_eq!(usage.session.total_tokens, 35);
    assert!(usage.cur.is_none());
    let entries = state
        .application
        .test_mutate_history(|h| h.get_entries())
        .await
        .unwrap()
        .unwrap();
    let recorded = format!("{entries:?}");
    assert!(!recorded.contains(ORIGINAL));
    assert!(!recorded.contains("additional_evidence"));
}

#[tokio::test]
async fn plan_refine_preserves_done_foundation_and_does_not_reuse_prior_research() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(
        dir.path().join("evidence.rs"),
        "pub fn previous_evidence() {}\n",
    )
    .unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["evidence.rs".into()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();
    let mut done = test_item("1", PlanItemStatus::Done);
    done.title = "Completed foundation".into();
    done.commit = Some("existing-commit".into());
    let plan = Plan::from_parts(
        "continuity".into(),
        "Continuity".into(),
        None,
        1,
        1,
        vec![],
        vec![done],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(
            r#"{"status":"needs_context","explanation":"Read evidence","context_requests":["evidence.rs"],"items":[]}"#,
        ),
        horizon_reply("First horizon"),
        critique_reply("approved"),
        horizon_reply("Second horizon"),
    ]));
    let (out, mut rx) = OutChannel::new();
    let (history, _) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(Default::default(), dir.path().to_path_buf(), history, out);
    state.application.test_set_provider(provider.clone()).await;
    for (index, critic) in [true, false].into_iter().enumerate() {
        let critic_model = critic.then(|| {
            tauqe_protocol::ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter(
                "critic/model",
            ))
        });
        let res = handle_plan_refine(
            synthesis_request(
                tauqe_protocol::methods::PLAN_REFINE,
                serde_json::json!({"plan_id": "continuity", "critic_model": critic_model}),
            ),
            &state,
        )
        .await;
        assert!(res.error.is_none(), "{:?}", res.error);
        let requests = provider.requests();
        if index == 0 {
            assert_eq!(requests.len(), 3, "approval skips reconciliation");
            assert!(round_text(&requests[2].messages).contains("previous_evidence"));
            assert!(round_text(&requests[2].messages).contains("Completed foundation"));
        } else {
            assert_eq!(requests.len(), 4, "disabled critique makes no request");
            assert!(!round_text(&requests[3].messages).contains("<file path=\"evidence.rs\""));
        }
        let stored = PlanStorage::load_plan(dir.path(), "continuity")
            .unwrap()
            .unwrap();
        let foundation = stored.implementation.find_item("1").unwrap();
        assert_eq!(foundation.status, PlanItemStatus::Done);
        assert_eq!(foundation.commit.as_deref(), Some("existing-commit"));
        assert_eq!(
            synthesis_events(&mut rx)
                .iter()
                .filter(|e| e["method"] == events::PLAN_UPDATED)
                .count(),
            1
        );
    }
    assert!(state.context_snapshot().await.auto_files().is_empty());
}

#[tokio::test]
async fn invalid_critique_ends_after_one_request_without_writing_the_draft() {
    for reply in ["", "not a critique"] {
        let dir = TestDir::new();
        let plan = Plan::from_parts(
            "invalid-critique".into(),
            "Invalid Critique".into(),
            None,
            1,
            1,
            vec![],
            vec![test_item("1", PlanItemStatus::Todo)],
        );
        PlanStorage::save_plan(dir.path(), &plan).unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([
            horizon_reply("Unaudited draft"),
            MockResponse::text(reply),
        ]));
        let (out, mut rx) = OutChannel::new();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(Default::default(), dir.path().to_path_buf(), history, out);
        state.application.test_set_provider(provider.clone()).await;
        let res = handle_plan_refine(
            synthesis_request(
                tauqe_protocol::methods::PLAN_REFINE,
                serde_json::json!({"plan_id": "invalid-critique",
                "critic_model": tauqe_protocol::ModelSelection::Specific(tauqe_protocol::ModelRef::openrouter("critic/model"))}),
            ),
            &state,
        )
        .await;
        assert!(
            res.error.is_some()
                || matches!(
                    serde_json::from_value::<PlanRefineResult>(res.result.unwrap()).unwrap(),
                    PlanRefineResult::Blocked { .. }
                )
        );
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].model, "critic/model");
        let stored = PlanStorage::load_plan(dir.path(), "invalid-critique")
            .unwrap()
            .unwrap();
        assert!(stored.implementation.find_item("2").is_none());
        assert_eq!(
            stored.implementation.find_item("1").unwrap().status,
            PlanItemStatus::Todo
        );
        assert!(synthesis_events(&mut rx)
            .iter()
            .all(|e| e["method"] != events::PLAN_UPDATED));
        assert!(state.context_snapshot().await.auto_files().is_empty());
    }
}

#[tokio::test]
async fn test_plan_horizon_synthesis_history_transactionality() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "tx-history".to_string(),
        "Tx History".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
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

    let (out, mut events_rx) = OutChannel::new();
    let (history, _) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(Default::default(), dir.path().to_path_buf(), history, out);
    state.application.test_set_provider(mock_provider).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "tx-history" })),
    };
    let res = handle_plan_refine(req, &state).await;

    let err = res.error.expect("should fail due to dependency cycle");
    assert_eq!(err.code, errors::PLAN_SAVE_FAILED);
    assert!(err.message.contains("Dependency cycle detected"));
    assert!(std::iter::from_fn(|| events_rx.try_recv().ok()).all(|raw| {
        let event: serde_json::Value = serde_json::from_str(&raw).unwrap();
        event["method"] != events::PLAN_UPDATED && event["method"] != events::PLAN_LIST_CHANGED
    }));

    // Strict history transactionality invariant: no assistant response was recorded
    // into session history because persistence failed.
    let entries = state
        .application
        .test_mutate_history(|history| history.get_entries())
        .await
        .unwrap()
        .unwrap();
    let responses = entries
        .iter()
        .filter(|e| matches!(e, tauqe_core::history::HistoryEntry::Response { .. }))
        .count();
    assert_eq!(
        responses, 0,
        "no response entry should be committed on persistence failure"
    );
}

// Retired XML marker test.

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
    let plan = Plan::from_parts(
        "auto-replan".to_string(),
        "Auto Replan".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let blocked_response = serde_json::json!({
        "message": "Blocked",
        "action": {
            "kind": "blocked",
            "blocked": {
                "is_blocked": true,
                "reason": "Precondition missing: value signature invalid",
                "details": ""
            }
        }
    })
    .to_string();
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
    let success_response = serde_json::json!({
        "message": "Fix value",
        "action": {
            "kind": "edit",
            "patch": "*** Begin Patch\n*** Update File: src.rs\n@@\n-pub fn value() -> i32 { 0 }\n+pub fn value() -> i32 { 100 }\n*** End Patch",
            "commit_message": "done",
            "message": "",
            "reason": ""
        }
    })
    .to_string();

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(blocked_response),
        MockResponse::text(replan_output.to_string()),
        MockResponse::text(success_response),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    config.repl.max_replans = 2;
    config.models.auto_level_up = false;

    let state = test_state_with_provider(&dir, config, provider).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "auto-replan",
        vec!["1".to_string()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;

    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });

    let reloaded = PlanStorage::load_plan(dir.path(), "auto-replan")
        .unwrap()
        .unwrap();
    assert!(reloaded.implementation.find_item("1.1").is_some());
    assert_eq!(
        reloaded.implementation.find_item("1.1").unwrap().status,
        PlanItemStatus::Done
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 100 }\n"
    );
}

#[tokio::test]
async fn test_plan_batch_defers_architect_escalation_until_repeated_failure() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "lazy-level-up".to_string(),
        "Lazy Level Up".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let blocked = serde_json::json!({
        "message": "Blocked",
        "action": {
            "kind": "blocked",
            "blocked": {
                "is_blocked": true,
                "reason": "Precondition missing",
                "details": ""
            }
        }
    })
    .to_string();
    let replan_output = serde_json::json!({
        "status": "success",
        "explanation": "Restated the same step",
        "items": [{
            "id": "1",
            "title": "Step 1 restated",
            "status": "todo",
            "files": ["src.rs"],
            "readonly_files": [],
            "concept_refs": [],
            "depends_on": [],
            "children": []
        }]
    });
    let success = serde_json::json!({
        "message": "Fix value",
        "action": {
            "kind": "edit",
            "patch": "*** Begin Patch\n*** Update File: src.rs\n@@\n-pub fn value() -> i32 { 0 }\n+pub fn value() -> i32 { 7 }\n*** End Patch",
            "commit_message": "done",
            "message": "",
            "reason": ""
        }
    })
    .to_string();

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(blocked.clone()),
        MockResponse::text(replan_output.to_string()),
        MockResponse::text(blocked.clone()),
        MockResponse::text(replan_output.to_string()),
        MockResponse::text(blocked),
        MockResponse::text(replan_output.to_string()),
        MockResponse::text(success),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    config.repl.max_replans = 2;
    config.models.auto_level_up = true;
    config.models.repl = vec![tauqe_core::models::ModelRef::openrouter("coder/model").into()];
    config.models.architect = Some(vec![
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("planner/one"),
            None,
        ),
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("planner/two"),
            None,
        ),
    ]);

    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "lazy-level-up",
        vec!["1".to_string()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;
    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });

    let requests = provider.requests();
    assert_eq!(requests.len(), 7);
    // The first replanning wave keeps the Architect that planned the horizon.
    assert_eq!(requests[1].model, "planner/one");
    // Both repair waves are tried by the coder before the next blocker promotes.
    assert_eq!(requests[3].model, "planner/one");
    assert_eq!(requests[5].model, "planner/two");

    let reloaded = PlanStorage::load_plan(dir.path(), "lazy-level-up")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 7 }\n"
    );
}

/// Renaming a stalled leaf cannot replenish its budget, while a newly completed
/// leaf does replenish it even when the next blocker occurs in the same queue.
#[tokio::test]
async fn test_plan_replanning_tracks_progress_instead_of_step_names_or_total_calls() {
    for makes_progress in [false, true] {
        let dir = TestDir::new();
        tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
        std::fs::write(dir.path().join("src.rs"), "pub fn value() {}\n").unwrap();
        tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".into()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();
        let leaf = |id: &str| tauqe_core::plan::PlanItem {
            files: vec!["src.rs".into()],
            ..test_item(id, PlanItemStatus::Todo)
        };
        let plan = Plan::from_parts(
            "progress-budget".into(),
            "Progress budget".into(),
            None,
            1,
            1,
            vec![],
            vec![leaf("initial")],
        );
        PlanStorage::save_plan(dir.path(), &plan).unwrap();
        let blocked = || {
            MockResponse::text(
                serde_json::json!({
                    "message": "Blocked", "action": {"kind": "blocked", "blocked": {
                        "is_blocked": true, "reason": "Design still needs repair", "details": ""
                    }}
                })
                .to_string(),
            )
        };
        let complete =
            || {
                MockResponse::text(serde_json::json!({
            "message": "Already satisfied", "action": {"kind": "edit", "patch": "",
                "commit_message": "", "error_message": "", "reason": "Already implemented"}
        }).to_string())
            };
        let mut responses = vec![blocked()];
        for wave in 1..=3 {
            let mut items = Vec::new();
            if makes_progress {
                items.push(leaf(&format!("done-{wave}")));
            }
            items.push(leaf(&format!("renamed-{wave}")));
            responses.push(MockResponse::text(
                serde_json::json!({
                    "status": "success", "explanation": "Replaced remaining horizon", "items": items
                })
                .to_string(),
            ));
            if makes_progress {
                responses.push(complete());
            }
            responses.push(blocked());
        }
        // After three productive waves, explicitly stop the Architect. With no
        // progress the run must stop earlier, before consuming the third wave.
        if makes_progress {
            responses.push(MockResponse::text(
                serde_json::json!({
                    "status": "blocked", "explanation": "Stop fixture", "reason": "Fixture complete"
                })
                .to_string(),
            ));
        }
        let provider = Arc::new(MockLlmProvider::scripted(responses));
        let mut config = tauqe_core::config::AppConfig::default();
        config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
        config.repl.max_replans = 2;
        config.models.auto_level_up = true;
        config.models.repl = vec![tauqe_core::models::ModelRef::openrouter("coder/model").into()];
        config.models.architect = Some(vec![tier_with_attempts("planner/one", 2)]);
        let state = test_state_with_provider(&dir, config, provider.clone()).await;
        let operation = Arc::new(state.try_begin_operation().unwrap());
        let outcome = run_plan_batch(
            &state,
            "progress-budget",
            vec!["initial".into()],
            operation,
            PlanBatchOptions::default(),
        )
        .await;
        let BatchOutcome::Blocked {
            completed, reason, ..
        } = outcome
        else {
            panic!("expected a blocked run, got {outcome:?}");
        };
        let requests = provider.requests();
        let architect_calls = requests
            .iter()
            .filter(|request| request.model == "planner/one")
            .count();
        if makes_progress {
            assert_eq!(completed, 3);
            assert_eq!(architect_calls, 4);
            assert!(reason.contains("Fixture complete"), "{reason}");
        } else {
            assert_eq!(completed, 0);
            assert_eq!(architect_calls, 2);
            assert!(
                reason.contains("without implementation progress"),
                "{reason}"
            );
        }
    }
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
    let plan = Plan::from_parts(
        "limit-replan".to_string(),
        "Limit Replan".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let blocked_1 = serde_json::json!({
        "message": "Blocked",
        "action": {
            "kind": "blocked",
            "blocked": {
                "is_blocked": true,
                "reason": "First blocker",
                "details": ""
            }
        }
    })
    .to_string();
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
    let blocked_2 = serde_json::json!({
        "message": "Blocked",
        "action": {
            "kind": "blocked",
            "blocked": {
                "is_blocked": true,
                "reason": "Second blocker",
                "details": ""
            }
        }
    })
    .to_string();

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(blocked_1),
        MockResponse::text(replan_output.to_string()),
        MockResponse::text(blocked_2),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    config.repl.max_replans = 1;
    config.models.auto_level_up = false;

    let state = test_state_with_provider(&dir, config, provider).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "limit-replan",
        vec!["1".to_string()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;

    match outcome {
        BatchOutcome::Blocked {
            completed,
            step_id,
            reason,
        } => {
            assert_eq!(completed, 0);
            assert_eq!(step_id, "1.1");
            assert!(
                reason.contains("Replanning budget exhausted (max_replans = 1)"),
                "unexpected block reason: {}",
                reason
            );
            assert!(
                reason.contains("Second blocker"),
                "the diagnostic must keep the cause: {}",
                reason
            );
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
        reloaded.implementation.find_item("1.1").unwrap().status,
        PlanItemStatus::Blocked
    );
}

#[tokio::test]
async fn test_plan_batch_stops_on_abort_without_replanning() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut first = test_item("1", PlanItemStatus::Todo);
    first.title = "Step 1".to_string();
    first.files = vec!["src.rs".to_string()];
    let mut second = test_item("2", PlanItemStatus::Todo);
    second.title = "Step 2".to_string();
    second.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "abort-batch".to_string(),
        "Abort Batch".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![first, second],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let abort_response = serde_json::json!({
        "message": "Aborted",
        "action": {
            "kind": "abort",
            "abort": {
                "is_aborted": true,
                "reason": "Native build approval required",
                "recommendation": "pnpm approve-builds"
            }
        }
    })
    .to_string();
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        abort_response,
    )]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    // protocol is always structured
    config.repl.max_replans = 2;
    config.models.auto_level_up = false;
    config.models.repl = vec![tauqe_core::models::ModelRef::openrouter("coder/model").into()];
    config.models.architect = Some(vec![tauqe_core::models::ModelEntry::new(
        tauqe_core::models::ModelRef::openrouter("planner/one"),
        None,
    )]);

    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "abort-batch",
        vec!["1".to_string(), "2".to_string()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;

    match outcome {
        BatchOutcome::Failed {
            completed,
            step_id,
            reason,
        } => {
            assert_eq!(completed, 0);
            assert_eq!(step_id, "1");
            assert!(
                reason.contains("Execution aborted"),
                "unexpected abort reason: {}",
                reason
            );
            assert!(
                reason.contains("pnpm approve-builds"),
                "the recommendation must reach the developer: {}",
                reason
            );
        }
        other => panic!("Expected Failed outcome on abort, got {:?}", other),
    }

    // Exactly one generation ran: the aborting coder turn. No architect round
    // was issued and the second step was never started.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "coder/model");

    // The aborted step returns to TODO and nothing runs behind it; the working
    // tree stays exactly as it was.
    let reloaded = PlanStorage::load_plan(dir.path(), "abort-batch")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Todo
    );
    assert_eq!(
        reloaded.implementation.find_item("2").unwrap().status,
        PlanItemStatus::Todo
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 0 }\n"
    );
}

#[tokio::test]
async fn test_plan_batch_stops_gracefully_on_soft_stop() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "soft-stop".to_string(),
        "Soft Stop".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![
            test_item("1", PlanItemStatus::Todo),
            test_item("2", PlanItemStatus::Todo),
        ],
    );
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
        PlanBatchOptions::default(),
    )
    .await;

    assert_eq!(outcome, BatchOutcome::Stopped { completed: 0 });

    // No step was started: every status stays TODO.
    let reloaded = PlanStorage::load_plan(dir.path(), "soft-stop")
        .unwrap()
        .unwrap();
    assert!(reloaded
        .implementation
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
    fn kind(&self) -> tauqe_core::models::ProviderKind {
        tauqe_core::models::ProviderKind::OpenRouter
    }

    async fn stream(
        &self,
        _request: tauqe_core::providers::LlmRequest<'_>,
        tx: tokio::sync::mpsc::Sender<tauqe_core::providers::StreamEvent>,
        _cancel: tauqe_core::stop::HardCancellation,
    ) -> tauqe_core::providers::ProviderResult {
        let _ = tx.send(tauqe_core::providers::StreamEvent::Cancelled).await;
        Ok(())
    }
}

#[tokio::test]
async fn test_plan_refine_reports_cancellation_from_stream_event() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "cancel-refine".to_string(),
        "Cancel Refine".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
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
    let plan = Plan::from_parts(
        "jit-horizon".to_string(),
        "JIT Horizon".to_string(),
        None,
        1,
        1,
        vec![tauqe_core::plan::ConceptItem {
            id: "c-1".to_string(),
            title: "Invariant".to_string(),
            details: None,
            children: Vec::new(),
        }],
        Vec::new(),
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

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
    let success_response = serde_json::json!({
        "message": "Set value",
        "action": {
            "kind": "edit",
            "patch": "*** Begin Patch\n*** Update File: src.rs\n@@\n-pub fn value() -> i32 { 0 }\n+pub fn value() -> i32 { 5 }\n*** End Patch",
            "commit_message": "done",
            "message": "",
            "reason": ""
        }
    })
    .to_string();

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(refine_output.to_string()),
        MockResponse::text(success_response),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    // protocol is always structured
    config.models.auto_level_up = false;

    let state = test_state_with_provider(&dir, config, provider).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "jit-horizon",
        Vec::new(),
        operation,
        PlanBatchOptions::default(),
    )
    .await;

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
        reloaded.implementation.find_item("1").unwrap().status,
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
    let plan = Plan::from_parts(
        "done-pipeline".to_string(),
        "Done Pipeline".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Done)],
    );
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
    let plan = Plan::from_parts(
        "pipeline-preflight".to_string(),
        "Pipeline Preflight".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    // A failing verification command makes the deterministic pipeline fail.
    std::fs::write(
        dir.path().join("tauqe.toml"),
        "[project.verify]\ncommand = \"false\"\n",
    )
    .unwrap();
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
        .implementation
        .items
        .iter()
        .all(|i| i.status == PlanItemStatus::Todo));
}

#[cfg(unix)]
#[tokio::test]
async fn public_pipeline_soft_stop_preserves_the_commit_and_does_not_start_the_next_plan() {
    use std::time::Duration;

    let provider = Arc::new(MockLlmProvider::scripted([plan_patch(
        "original", "verified",
    )]));
    let mut run = PlanExecutionFixture::new(
        "if grep -qx verified source.txt; then touch .git/verify-entered; \
         for attempt in $(seq 1 200); do test ! -f .git/verify-release || exit 0; sleep 0.1; done; exit 1; fi",
        2,
        provider.clone(),
    ).await;
    let root = run.dir.path().to_path_buf();
    let next = Plan::from_parts(
        "next-plan".into(),
        "Next plan".into(),
        None,
        1,
        1,
        vec![],
        vec![public_plan_leaf("1")],
    );
    PlanStorage::save_plan(&root, &next).unwrap();
    let response = handle_plan_execute_pipeline(
        Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::PLAN_EXECUTE_PIPELINE.into(),
            params: Some(serde_json::json!({"plan_ids": ["public-run", "next-plan"]})),
        },
        &run.state,
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let result = response.result.unwrap();
    let operation_id = result["operation_id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.join(".git/verify-entered").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the first plan must reach verification");
    run.state.soft_stop_active_operation();
    std::fs::write(root.join(".git/verify-release"), "").unwrap();
    let observed = run.finish(operation_id, "stopped").await;
    let hash = run.assert_committed();
    let snapshot = observed
        .iter()
        .rev()
        .skip(1)
        .find(|event| event["method"] == events::PLAN_UPDATED)
        .unwrap();
    assert!(snapshot.to_string().contains(&hash));
    tokio::time::timeout(Duration::from_secs(10), async {
        while run.state.is_operation_active() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stopped pipeline must release admission");
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        PlanStorage::load_active_id(&root).unwrap().as_deref(),
        Some("public-run")
    );
    assert_eq!(
        PlanStorage::load_plan(&root, "next-plan")
            .unwrap()
            .unwrap()
            .implementation
            .find_item("1")
            .unwrap()
            .status,
        PlanItemStatus::Todo
    );
}

#[tokio::test]
async fn test_handle_plan_refine_renders_targeted_repo_map_feedback() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/target.rs"), "pub fn target() {}\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src/target.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Add src", false).unwrap();

    let plan = Plan::from_parts(
        "refine-repomap".to_string(),
        "Refine RepoMap".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
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
    let feedback_prompt = round_text(&requests[1].messages);
    assert!(feedback_prompt.contains("<repomap_feedback path=\"src\">"));
    assert!(feedback_prompt.contains("pub fn target"));

    let reloaded = PlanStorage::load_plan(dir.path(), "refine-repomap")
        .unwrap()
        .unwrap();
    assert!(reloaded.implementation.find_item("1.1").is_some());
}

#[tokio::test]
async fn test_plan_refine_loads_requested_file_and_saves_horizon_without_edits() {
    const REFERENCE: &str = "Unique horizon reference text";
    let dir = TestDir::new();
    let root = dir.path();
    tauqe_core::git::init_repository(Some(root), true).unwrap();
    std::fs::write(root.join(".gitignore"), "/.tauqe/\n").unwrap();
    std::fs::write(root.join("reference.txt"), REFERENCE).unwrap();
    tauqe_core::git::cmd::stage_files(root, &[".gitignore".into(), "reference.txt".into()])
        .unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root, "Add reference", false).unwrap();
    let head = || tauqe_core::git::cmd::run_git(Some(root), &["rev-parse", "HEAD"]).unwrap();
    let initial_head = head();
    let plan = Plan::from_parts(
        "refine-file".into(),
        "Refine File".into(),
        None,
        1,
        1,
        vec![],
        vec![],
    );
    PlanStorage::save_plan(root, &plan).unwrap();
    let initial_worktree = tauqe_core::git::state::snapshot_worktree(root);

    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(
            r#"{"status":"needs_context","explanation":"Read the reference","reason":null,"context_requests":["reference.txt"],"repo_searches":[],"repo_maps":[],"items":[]}"#,
        ),
        MockResponse::text(
            r#"{"status":"success","explanation":"Reference reviewed","reason":null,"context_requests":[],"repo_searches":[],"repo_maps":[],"items":[{"id":"1","title":"Use the reference","status":"todo","files":["source.rs"],"readonly_files":["reference.txt"],"concept_refs":[],"depends_on":[],"children":[]}]}"#,
        ),
    ]));
    let state = test_state_with_provider(&dir, Default::default(), provider.clone()).await;
    let response = handle_plan_refine(
        Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::PLAN_REFINE.into(),
            params: Some(serde_json::json!({"plan_id": "refine-file"})),
        },
        &state,
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    assert!(matches!(
        serde_json::from_value::<PlanRefineResult>(response.result.unwrap()).unwrap(),
        PlanRefineResult::Success { .. }
    ));

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(!round_text(&requests[0].messages).contains(REFERENCE));
    assert!(requests[1].messages.iter().any(|message| {
        message
            .content
            .contains("<file path=\"reference.txt\" access=\"read_only\"")
            && message.content.contains(REFERENCE)
    }));
    let stored = PlanStorage::load_plan(root, "refine-file")
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.implementation.find_item("1").unwrap().title,
        "Use the reference"
    );
    assert_eq!(head(), initial_head);
    assert_eq!(
        tauqe_core::git::state::snapshot_worktree(root),
        initial_worktree
    );
    assert_eq!(
        std::fs::read_to_string(root.join("reference.txt")).unwrap(),
        REFERENCE
    );
}

#[tokio::test]
async fn test_handle_plan_refine_renders_complete_map_despite_legacy_toggle() {
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

    let plan = Plan::from_parts(
        "refine-dirs".to_string(),
        "Refine Dirs".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
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

    // The retired toggle cannot replace the automatic map with a directory
    // tree: the full tracked-file contour remains in the stable prompt head.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 1);
    let system_prompt = &requests[0].messages[0].content;
    assert!(system_prompt.contains("src/nested/helper.rs"));
    assert!(system_prompt.contains("pub fn helper"));
}

#[tokio::test]
async fn test_plan_refine_heals_the_schema_then_loads_evidence() {
    const EVIDENCE: &str = "Unique evidence requested after schema healing";
    let dir = TestDir::new();
    let root = dir.path();
    tauqe_core::git::init_repository(Some(root), true).unwrap();
    std::fs::write(root.join("evidence.txt"), EVIDENCE).unwrap();
    tauqe_core::git::cmd::stage_files(root, &["evidence.txt".into()]).unwrap();
    let plan = Plan::from_parts(
        "parse-retry".to_string(),
        "Parse Retry".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // The first response is cut off mid-JSON. The correction requests evidence.
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
                "files": ["src.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(truncated),
        MockResponse::text(
            r#"{"status":"needs_context","explanation":"Read the evidence","context_requests":["evidence.txt","missing-evidence.txt"]}"#,
        ),
        MockResponse::text(complete.to_string()),
    ]));

    let config = tauqe_core::config::AppConfig::default();
    // The architect tier budget counts every generation of a round, the first
    // one included: the default budget of the rung is two attempts, so the
    // truncated first round is regenerated on the same rung.
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "parse-retry" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 3);
    let correction = round_text(&requests[1].messages);
    assert!(correction.contains("did not satisfy the active response schema"));
    assert!(!correction.contains("The research round limit is exhausted"));
    assert!(!correction.contains(EVIDENCE));
    assert!(requests[1]
        .messages
        .iter()
        .all(|message| message.role != "assistant"));
    let continuation = round_text(&requests[2].messages);
    // Loaded files arrive as file messages; the missing path produces feedback.
    assert!(continuation.contains("<context_feedback>"));
    assert!(continuation.contains("missing-evidence.txt"));
    assert!(!continuation.contains("did not satisfy the active response schema"));
    assert!(requests[2].messages.iter().any(|message| {
        message
            .content
            .contains("<file path=\"evidence.txt\" access=\"read_only\"")
            && message.content.contains(EVIDENCE)
    }));

    let reloaded = PlanStorage::load_plan(dir.path(), "parse-retry")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().title,
        "Refined step 1"
    );
}

#[tokio::test]
async fn test_plan_refine_recovers_from_a_transport_break() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "stream-break".to_string(),
        "Stream Break".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
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
                "files": ["src.rs"],
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
    assert!(reloaded.implementation.find_item("1.1").is_some());
    assert!(reloaded.implementation.find_item("1").is_none());
}

#[tokio::test]
async fn test_plan_refine_blocks_after_the_tier_budget_is_spent() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "parse-exhausted".to_string(),
        "Parse Exhausted".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // The empty generation is a transport-level non-answer, so the kernel re-asks
    // on its own corrective budget without spending the rung's single attempt. The
    // invalid payload that follows spends it, and the saturated ladder turns the
    // failure into a typed block instead of an RPC error.
    let mock_provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(""),
        MockResponse::text("not a refinement payload"),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.models.architect = Some(vec![tier_with_attempts("planner/one", 1)]);
    let state = test_state_with_provider(&dir, config, mock_provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "parse-exhausted" })),
    };
    let res = handle_plan_refine(req, &state).await;
    let value = res
        .result
        .expect("an exhausted tier budget is a typed outcome");
    let result: PlanRefineResult =
        serde_json::from_value(value).expect("the refinement result decodes");
    match result {
        PlanRefineResult::Blocked { reason } => {
            assert!(
                reason.contains("Failed to parse"),
                "the reason must name the schema failure: {reason}"
            );
            assert!(
                reason.contains("planner/one"),
                "the reason must name the rung that spent its budget: {reason}"
            );
        }
        other => panic!("expected a typed block, got {other:?}"),
    }
    assert_eq!(
        mock_provider.requests().len(),
        2,
        "silence is re-asked by the kernel, the invalid payload spends the rung"
    );

    // A blocked synthesis leaves the stored plan untouched.
    let reloaded = PlanStorage::load_plan(dir.path(), "parse-exhausted")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().title,
        "Step 1"
    );
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Todo
    );
}

/// An architect tier that spends its whole generation budget climbs the
/// architect ladder instead of failing the synthesis: the round is regenerated
/// on the next rung, the escalation is published, and the applied horizon
/// records the rung that actually produced it as its provenance.
#[tokio::test]
async fn test_plan_refine_escalates_the_architect_ladder_after_a_spent_tier_budget() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(
        dir.path().join("reference.txt"),
        "Evidence retained across tiers",
    )
    .unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["reference.txt".into()]).unwrap();
    let plan = Plan::from_parts(
        "architect-ladder".to_string(),
        "Architect Ladder".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // After discovery, two defective answers exhaust their respective tiers.
    let truncated = "{\"status\": \"success\", \"explanation\": \"Decomposed";
    let complete = serde_json::json!({
        "status": "success",
        "explanation": "Decomposed on the escalated rung",
        "reason": null,
        "context_requests": [],
        "repo_searches": [],
        "repo_maps": [],
        "items": [
            {
                "id": "1.1",
                "title": "Refined step 1.1",
                "status": "todo",
                "files": ["src.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    });
    let mock_provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(
            r#"{"status":"needs_context","explanation":"Check the evidence","context_requests":["reference.txt","not-present.txt"]}"#,
        ),
        MockResponse::text(truncated),
        MockResponse::text(truncated),
        MockResponse::text(complete.to_string()),
    ]));

    let mut config = tauqe_core::config::AppConfig {
        repl: serde_json::from_value(serde_json::json!({"max_turn_rounds": 1})).unwrap(),
        ..Default::default()
    };
    let mut deeper = tier_with_attempts("planner/one", 1);
    deeper.effort = Some(tauqe_core::models::ReasoningEffort::High);
    config.models.architect = Some(vec![
        tier_with_attempts("planner/one", 1),
        deeper,
        tier_with_attempts("planner/two", 1),
    ]);

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);
    let provider: Arc<dyn tauqe_core::providers::LlmProvider> = mock_provider.clone();
    state.application.test_set_provider(provider).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "architect-ladder" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    // The spent rung hands the round to the next rung of the ladder, so the
    // regeneration runs on a stronger configuration instead of the failed one.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].model, "planner/one");
    assert_eq!(requests[1].model, "planner/one");
    assert_eq!(requests[2].model, "planner/one");
    assert_eq!(
        requests[2].reasoning_effort,
        Some(tauqe_core::models::ReasoningEffort::High)
    );
    assert_eq!(requests[1].session_id, requests[2].session_id);
    assert_eq!(requests[3].model, "planner/two");
    assert_ne!(requests[2].session_id, requests[3].session_id);
    for request in &requests[1..] {
        let prompt = round_text(&request.messages);
        assert!(prompt.contains("Evidence retained across tiers"));
        assert!(prompt.contains("<context_feedback>"));
        assert!(prompt.contains("not-present.txt"));
        assert!(prompt.contains("The research round limit is exhausted"));
    }
    assert!(
        round_text(&requests[3].messages).contains("did not satisfy the active response schema")
    );

    // The transition is published, so the developer sees which rung answered.
    let mut emitted = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        emitted.push(raw);
    }
    assert!(
        emitted
            .iter()
            .any(|event| event.contains(events::MODEL_ESCALATED)),
        "the escalation must be published to the client: {emitted:?}"
    );

    // The synthesized horizon is applied and records the rung that produced it,
    // so a later replanning wave keeps climbing from that configuration.
    let reloaded = PlanStorage::load_plan(dir.path(), "architect-ladder")
        .unwrap()
        .unwrap();
    assert!(reloaded.implementation.find_item("1.1").is_some());
    assert_eq!(
        reloaded
            .implementation
            .architect
            .as_ref()
            .map(|tier| tier.model.clone()),
        Some(tauqe_core::models::ModelRef::openrouter("planner/two"))
    );
}

#[tokio::test]
async fn test_handle_plan_delete_clears_focus_anchored_on_the_deleted_plan() {
    let dir = TestDir::new();
    for id in ["focus-del", "focus-keep", "focus-other"] {
        let plan = Plan::from_parts(
            id.to_string(),
            id.to_string(),
            None,
            1,
            1,
            Vec::new(),
            vec![test_item("1", PlanItemStatus::Done)],
        );
        PlanStorage::save_plan(dir.path(), &plan).unwrap();
    }

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    let delete_request = |id: u64, plan_id: &str| Request {
        id: tauqe_protocol::RequestId::Number(id),
        method: tauqe_protocol::methods::PLAN_DELETE.to_string(),
        params: Some(serde_json::json!({ "id": plan_id })),
    };

    // A focus anchored on the deleted plan is released with it.
    state
        .set_discussion_focus(tauqe_protocol::DiscussionFocus::plan("focus-del", vec![]))
        .await;
    let res = super::storage_ops::handle_plan_delete(delete_request(1, "focus-del"), &state).await;
    assert!(res.error.is_none(), "delete failed: {:?}", res.error);
    assert!(
        state.discussion_focus().await.is_none(),
        "a focus anchored on a deleted plan must be dropped"
    );

    // The release is broadcast so every client drops the dead anchor.
    let mut emitted = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        emitted.push(raw);
    }
    assert!(
        emitted
            .iter()
            .any(|e| e.contains(events::DISCUSSION_FOCUS_CHANGED)),
        "the focus release must be broadcast to clients"
    );

    // A focus anchored on a surviving plan is untouched by another deletion.
    state
        .set_discussion_focus(tauqe_protocol::DiscussionFocus::plan("focus-keep", vec![]))
        .await;
    let res =
        super::storage_ops::handle_plan_delete(delete_request(2, "focus-other"), &state).await;
    assert!(res.error.is_none(), "delete failed: {:?}", res.error);
    assert_eq!(
        state.discussion_focus().await,
        tauqe_protocol::DiscussionFocus::plan("focus-keep", vec![])
    );
}

/// Deleting the anchored plan releases the anchor as a typed payload.
///
/// The invariant is that a focus can never address a non-existent entity, so the
/// release must travel as `DiscussionFocus::None` inside the broadcast event, not
/// merely as an absent focus: every client reconstructs its anchor from that
/// payload and would otherwise keep rendering a dead one (c-2).
#[tokio::test]
async fn test_handle_plan_delete_broadcasts_typed_focus_release() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "typed-focus".to_string(),
        "Typed Focus".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Done)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    state
        .set_discussion_focus(tauqe_protocol::DiscussionFocus::plan(
            "typed-focus",
            vec!["1".to_string()],
        ))
        .await;
    assert!(state.discussion_focus().await.is_active());
    // The focus set itself broadcasts a change event; only the release that
    // accompanies the deletion is under test here.
    while rx.try_recv().is_ok() {}

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_DELETE.to_string(),
        params: Some(serde_json::json!({ "id": "typed-focus" })),
    };
    let res = super::storage_ops::handle_plan_delete(req, &state).await;
    assert!(res.error.is_none(), "delete failed: {:?}", res.error);

    assert_eq!(res.result, Some(serde_json::json!({"deleted": true})));
    assert!(PlanStorage::load_plan(dir.path(), "typed-focus")
        .unwrap()
        .is_none());
    let published: Vec<tauqe_protocol::Event> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|raw| serde_json::from_str(&raw).unwrap())
        .collect();
    assert_eq!(
        published
            .iter()
            .map(|event| event.method.as_str())
            .collect::<Vec<_>>(),
        [events::DISCUSSION_FOCUS_CHANGED, events::PLAN_LIST_CHANGED]
    );
    assert_eq!(
        published[0].params.as_ref().unwrap()["focus"]["kind"],
        "none"
    );

    assert!(state.discussion_focus().await.is_none());
}

/// The terminal batch event must name the operation the execute acknowledgement
/// returned, so a client can attribute `plan/batch_finished` to the request it
/// sent instead of correlating an unregistered identifier.
#[tokio::test]
async fn test_plan_batch_finished_carries_the_registered_operation_id() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "op-id".to_string(),
        "Operation Id".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_EXECUTE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "op-id" })),
    };
    let res = handle_plan_execute(req, &state).await;
    let val: serde_json::Value = res.result.expect("the batch must be acknowledged");
    let acknowledged_id = val["operation_id"]
        .as_str()
        .expect("the acknowledgement carries the batch operation id")
        .to_string();

    // Without an API key the first turn cannot start, so the batch terminates
    // immediately and publishes its single completion event.
    let mut finished = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while finished.is_none() && std::time::Instant::now() < deadline {
        while let Ok(raw) = rx.try_recv() {
            if raw.contains(events::PLAN_BATCH_FINISHED) {
                finished = Some(raw);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let finished = finished.expect("plan/batch_finished must be emitted");
    let event: serde_json::Value = serde_json::from_str(&finished).expect("event is valid JSON");
    assert_eq!(
        event["params"]["operation_id"], acknowledged_id,
        "the terminal event must name the operation the acknowledgement returned"
    );
}

/// A semantic blocker reported by the coder is durable: the reason lands in the
/// plan file beside the `BLOCKED` status, so the diagnostic survives the
/// interrupted turn and can be read from disk afterwards.
#[tokio::test]
async fn test_plan_batch_persists_the_blocked_reason_on_disk() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "blocked-reason".to_string(),
        "Blocked Reason".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let reason = "Missing capability: native compiler";
    let blocked_response = serde_json::json!({
        "message": "Blocked",
        "action": {
            "kind": "blocked",
            "blocked": {
                "is_blocked": true,
                "reason": reason,
                "details": ""
            }
        }
    })
    .to_string();
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        blocked_response,
    )]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    config.repl.max_replans = 0;
    config.models.auto_level_up = false;

    let state = test_state_with_provider(&dir, config, provider).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "blocked-reason",
        vec!["1".to_string()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;

    match outcome {
        BatchOutcome::Blocked {
            completed, step_id, ..
        } => {
            assert_eq!(completed, 0);
            assert_eq!(step_id, "1");
        }
        other => panic!("Expected a blocked outcome, got {:?}", other),
    }

    // The report is read back through the storage engine, not from the
    // transient turn result.
    let reloaded = PlanStorage::load_plan(dir.path(), "blocked-reason")
        .unwrap()
        .unwrap();
    let stored = reloaded.implementation.find_item("1").unwrap();
    assert_eq!(stored.status, PlanItemStatus::Blocked);
    assert_eq!(stored.blocked_reason.as_deref(), Some(reason));

    // The diagnostic is durable on disk, so it outlives the daemon session.
    let raw =
        std::fs::read_to_string(PlanStorage::plans_dir(dir.path()).join("blocked-reason.json"))
            .unwrap();
    assert!(
        raw.contains(reason),
        "the diagnostic must be durable on disk: {raw}"
    );
}

/// A Structured-protocol plan step turn speaks the field vocabulary of the JSON
/// schema: its user prompt names no marker-bound control tag, the scripted
/// response carries the change set together with a successful `outcome`, and the
/// step reaches DONE through the very same execution pipeline the tagged contour
/// uses.
#[tokio::test]
async fn test_plan_step_structured_protocol_execution() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Set the value".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "structured-step".to_string(),
        "Structured Step".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let response = serde_json::json!({
        "message": "Set value to 42",
        "action": {
            "kind": "edit",
            "patch": "*** Begin Patch\n*** Update File: src.rs\n@@\n-pub fn value() -> i32 { 0 }\n+pub fn value() -> i32 { 42 }\n*** End Patch",
            "commit_message": "done",
            "message": "",
            "reason": ""
        }
    })
    .to_string();
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    config.models.auto_level_up = false;
    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "structured-step",
        vec!["1".to_string()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;
    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });

    // The step prompt describes the structured response fields instead of
    // marker-bound control tags.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let user_prompt = &requests[0]
        .messages
        .last()
        .expect("the round carries a task message")
        .content;
    for tag in ["<success", "<context_request", "<blocked", "<verify"] {
        assert!(
            !user_prompt.contains(tag),
            "a Structured step prompt must not mention {tag}: {user_prompt}"
        );
    }
    assert!(user_prompt.contains("commit_message"));
    assert!(user_prompt.contains("context_requests"));

    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 42 }\n"
    );
    let reloaded = PlanStorage::load_plan(dir.path(), "structured-step")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
}

/// A batch started from an explicitly selected coder rung runs that rung and
/// escalates above it, instead of restarting the ladder at its base.
#[tokio::test]
async fn test_plan_batch_starts_ladder_from_the_selected_coder_tier() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "start-tier".to_string(),
        "Start Tier".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let success = serde_json::json!({
        "message": "Fix value",
        "action": {
            "kind": "edit",
            "patch": "*** Begin Patch\n*** Update File: src.rs\n@@\n-pub fn value() -> i32 { 0 }\n+pub fn value() -> i32 { 9 }\n*** End Patch",
            "commit_message": "done",
            "message": "",
            "reason": ""
        }
    })
    .to_string();
    let provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text(
            r#"{"scratchpad":"Private first attempt marker","message":"Inspect the source","action":{"kind":"research","context_requests":[{"path":"src.rs","access":"editable"}]}}"#,
        ),
        MockResponse::text(success),
    ]));

    let mut config = tauqe_core::config::AppConfig {
        repl: serde_json::from_value(serde_json::json!({
            "workflow": "naive", "max_turn_rounds": 1
        }))
        .unwrap(),
        ..Default::default()
    };
    config.models.auto_level_up = true;
    config.models.repl = vec![tauqe_core::models::ModelRef::openrouter("coder/one").into()];
    config.models.coder = Some(vec![
        tier_with_attempts("coder/one", 1),
        tier_with_attempts("coder/mid", 1),
        tier_with_attempts("coder/strong", 1),
    ]);

    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let options = PlanBatchOptions {
        model: Some(tauqe_core::models::ModelSelection::Specific(
            tauqe_core::models::ModelRef::openrouter("coder/mid"),
        )),
        step_id: None,
    };
    let outcome = run_plan_batch(
        &state,
        "start-tier",
        vec!["1".to_string()],
        operation,
        options,
    )
    .await;
    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    // The first round runs the selected rung of the ladder...
    assert_eq!(requests[0].model, "coder/mid");
    // ...and the retry escalates above it instead of restarting at the base.
    assert_eq!(requests[1].model, "coder/strong");
    assert_eq!(requests[0].session_id, requests[1].session_id);
    assert!(requests[1]
        .messages
        .iter()
        .all(|message| message.role != "assistant"));
    let retry = round_text(&requests[1].messages);
    assert!(retry.contains("Turn round budget exhausted"));
    assert!(!retry.contains("Private first attempt marker"));
    assert!(!retry.contains("<context_feedback>"));

    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 9 }\n"
    );
    let reloaded = PlanStorage::load_plan(dir.path(), "start-tier")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
}

/// A step started from an explicitly selected intermediate coder rung completes
/// on that very rung: the selection is the starting tier, and a verified first
/// attempt neither escalates upward nor falls back to the base of the ladder.
#[tokio::test]
async fn test_plan_batch_selected_coder_rung_completes_on_the_first_attempt() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "mid-tier-plan".to_string(),
        "Mid Tier Plan".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let response = serde_json::json!({
        "message": "Set value",
        "action": {
            "kind": "edit",
            "patch": "*** Begin Patch\n*** Update File: src.rs\n@@\n-pub fn value() -> i32 { 0 }\n+pub fn value() -> i32 { 3 }\n*** End Patch",
            "commit_message": "done",
            "message": "",
            "reason": ""
        }
    })
    .to_string();
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(response)]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    config.models.auto_level_up = true;
    config.models.repl = vec![tauqe_core::models::ModelRef::openrouter("coder/one").into()];
    config.models.coder = Some(vec![
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("coder/one"),
            None,
        ),
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("coder/mid"),
            None,
        ),
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("coder/strong"),
            None,
        ),
    ]);

    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let options = PlanBatchOptions {
        model: Some(tauqe_core::models::ModelSelection::Specific(
            tauqe_core::models::ModelRef::openrouter("coder/mid"),
        )),
        step_id: None,
    };
    let outcome = run_plan_batch(
        &state,
        "mid-tier-plan",
        vec!["1".to_string()],
        operation,
        options,
    )
    .await;
    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });

    // The selected rung runs the step, and no rung above it is consumed.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "coder/mid");

    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 3 }\n"
    );
    let reloaded = PlanStorage::load_plan(dir.path(), "mid-tier-plan")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
}

/// A step the developer starts manually on a coder model excluded from the
/// automatic ladder (`level_up = false`) runs on exactly that model and fails
/// terminally: the ladder is already saturated, so the single adaptive attempt
/// of that rung is spent and no stronger tier is ever entered.
#[tokio::test]
async fn test_plan_batch_manual_excluded_coder_ends_without_escalation() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "manual-saturated".to_string(),
        "Manual Saturated".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // The single adaptive attempt answers without a change set, so the step can
    // never complete; a second scripted round would serve an erroneous retry.
    let mock_provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text("The step looks complete."),
        MockResponse::text("The step looks complete."),
    ]));
    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    config.models.auto_level_up = true;
    config.models.repl = vec![tauqe_core::models::ModelRef::openrouter("coder/one").into()];
    let manual_model = tauqe_core::models::ModelRef::openrouter("coder/manual");
    let mut manual_entry = tauqe_core::models::ModelEntry::new(manual_model.clone(), None);
    manual_entry.level_up = Some(false);
    config.models.coder = Some(vec![
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("coder/one"),
            None,
        ),
        manual_entry,
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("coder/strong"),
            None,
        ),
    ]);

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);
    let provider: Arc<dyn tauqe_core::providers::LlmProvider> = mock_provider.clone();
    state.application.test_set_provider(provider).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let options = PlanBatchOptions {
        model: Some(tauqe_core::models::ModelSelection::Specific(manual_model)),
        step_id: None,
    };
    let outcome = run_plan_batch(
        &state,
        "manual-saturated",
        vec!["1".to_string()],
        operation,
        options,
    )
    .await;

    match outcome {
        BatchOutcome::Failed {
            completed,
            step_id,
            reason,
        } => {
            assert_eq!(completed, 0);
            assert_eq!(step_id, "1");
            assert!(
                reason.contains("escalation is exhausted"),
                "the terminal diagnostic must name the saturated ladder: {reason}"
            );
            assert!(
                reason.contains("coder/manual"),
                "the terminal diagnostic must name the rung the step ran on: {reason}"
            );
        }
        other => panic!("Expected a failed outcome without escalation, got {other:?}"),
    }
    // The manual rung spends its single adaptive attempt, and the ladder never
    // enters another rung.
    let requests = mock_provider.requests();
    assert_eq!(
        requests.len(),
        1,
        "a saturated ladder spends its single adaptive attempt and never retries on another model"
    );
    assert!(requests
        .iter()
        .all(|request| request.model == "coder/manual"));

    let mut emitted = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        emitted.push(raw);
    }
    assert!(
        !emitted
            .iter()
            .any(|event| event.contains(events::MODEL_ESCALATED)),
        "a saturated ladder must never publish an escalation event: {emitted:?}"
    );

    // The failed step returns to TODO so the developer can retry it deliberately.
    let reloaded = PlanStorage::load_plan(dir.path(), "manual-saturated")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Todo
    );
}

/// A content failure that stays inside the tier budget retries the very model
/// the step already runs on: the ladder is consulted only once the whole budget
/// of the current tier is spent, so one unverified answer never promotes it.
#[tokio::test]
async fn test_plan_batch_retries_the_same_tier_within_the_attempt_budget() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "same-tier".to_string(),
        "Same Tier".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();
    let success = serde_json::json!({
        "message": "Fix value",
        "action": {
            "kind": "edit",
            "patch": "*** Begin Patch\n*** Update File: src.rs\n@@\n-pub fn value() -> i32 { 0 }\n+pub fn value() -> i32 { 11 }\n*** End Patch",
            "commit_message": "done",
            "message": "",
            "reason": ""
        }
    })
    .to_string();
    let mock_provider = Arc::new(MockLlmProvider::scripted([
        MockResponse::text("The step looks complete."),
        MockResponse::text(success),
    ]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    // protocol is always structured
    config.models.auto_level_up = true;
    config.models.repl = vec![tauqe_core::models::ModelRef::openrouter("coder/one").into()];
    config.models.coder = Some(vec![
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("coder/one"),
            None,
        ),
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("coder/strong"),
            None,
        ),
    ]);

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);
    let provider: Arc<dyn tauqe_core::providers::LlmProvider> = mock_provider.clone();
    state.application.test_set_provider(provider).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "same-tier",
        vec!["1".to_string()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;
    assert_eq!(outcome, BatchOutcome::Completed { steps: 1 });

    // Both attempts run on the tier the step started on: the budget is not yet
    // spent, so the ladder is never consulted.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.model == "coder/one"));

    let mut emitted = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        emitted.push(raw);
    }
    assert!(
        !emitted
            .iter()
            .any(|event| event.contains(events::MODEL_ESCALATED)),
        "the same tier must be retried without an escalation event: {emitted:?}"
    );

    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 11 }\n"
    );
    let reloaded = PlanStorage::load_plan(dir.path(), "same-tier")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Done
    );
}

/// A transport failure produces no model result, so it is neutral to the tier
/// budget: the step ends on the original cause without spending an attempt,
/// consulting the ladder, or retrying another model.
#[tokio::test]
async fn test_plan_batch_transport_failure_ends_without_spending_an_attempt() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "transport-failure".to_string(),
        "Transport Failure".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    // The upstream breaks before any model content arrives; a single round is
    // scripted, so a retry would exhaust the script instead of passing.
    let mock_provider = Arc::new(MockLlmProvider::scripted([MockResponse::error(
        "OpenRouter HTTP error 503: upstream unavailable",
    )]));

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    config.models.auto_level_up = true;
    config.models.repl = vec![tauqe_core::models::ModelRef::openrouter("coder/one").into()];
    config.models.coder = Some(vec![
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("coder/one"),
            None,
        ),
        tauqe_core::models::ModelEntry::new(
            tauqe_core::models::ModelRef::openrouter("coder/strong"),
            None,
        ),
    ]);

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);
    let provider: Arc<dyn tauqe_core::providers::LlmProvider> = mock_provider.clone();
    state.application.test_set_provider(provider).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let outcome = run_plan_batch(
        &state,
        "transport-failure",
        vec!["1".to_string()],
        operation,
        PlanBatchOptions::default(),
    )
    .await;

    match outcome {
        BatchOutcome::Failed {
            completed,
            step_id,
            reason,
        } => {
            assert_eq!(completed, 0);
            assert_eq!(step_id, "1");
            assert!(
                reason.contains("503"),
                "the original transport cause must survive: {reason}"
            );
        }
        other => panic!("Expected a failed outcome on a transport failure, got {other:?}"),
    }

    // A transport failure is never retried and never promotes the model.
    let requests = mock_provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "coder/one");

    let mut emitted = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        emitted.push(raw);
    }
    assert!(
        !emitted
            .iter()
            .any(|event| event.contains(events::MODEL_ESCALATED)),
        "a transport failure must not escalate the model: {emitted:?}"
    );

    // The step returns to TODO and the working tree stays untouched.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src.rs")).unwrap(),
        "pub fn value() -> i32 { 0 }\n"
    );
    let reloaded = PlanStorage::load_plan(dir.path(), "transport-failure")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Todo
    );
}

/// Provider double whose streams park forever, ignoring the cancellation
/// watch entirely.
///
/// It reproduces a provider wedged in a non-cooperative stretch — exactly the
/// situation the force-termination machinery exists for: the entry signal
/// proves the stream started, and `pending` keeps the workflow task running
/// until the sanctioned abort path tears it down.
struct ParkingProvider {
    /// Signals that a stream call was entered.
    entered: tokio::sync::mpsc::UnboundedSender<()>,
}

#[async_trait::async_trait]
impl tauqe_core::providers::LlmProvider for ParkingProvider {
    fn kind(&self) -> tauqe_core::models::ProviderKind {
        tauqe_core::models::ProviderKind::OpenRouter
    }

    async fn stream(
        &self,
        _request: tauqe_core::providers::LlmRequest<'_>,
        _tx: tokio::sync::mpsc::Sender<tauqe_core::providers::StreamEvent>,
        _cancel: tauqe_core::stop::HardCancellation,
    ) -> tauqe_core::providers::ProviderResult {
        let _ = self.entered.send(());
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// A hard cancel never abandons the step task a batch has already spawned.
///
/// The supervisor awaits its queue cooperatively instead of dropping it, so a
/// step parked in a provider that ignores the cancel watch keeps the
/// operation slot registered — together with the abort handle its turn
/// installed — until the sanctioned force-termination path aborts it. Only
/// then does the batch report its cancelled outcome, release the slot, and
/// leave the session lock free for the next turn.
#[tokio::test]
async fn test_plan_batch_hard_cancel_never_abandons_the_step_task() {
    let dir = TestDir::new();
    tauqe_core::git::init_repository(Some(dir.path()), true).unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn value() -> i32 { 0 }\n").unwrap();
    tauqe_core::git::cmd::stage_files(dir.path(), &["src.rs".to_string()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(dir.path(), "Initial", false).unwrap();

    let mut item = test_item("1", PlanItemStatus::Todo);
    item.title = "Step 1".to_string();
    item.files = vec!["src.rs".to_string()];
    let plan = Plan::from_parts(
        "hard-cancel".to_string(),
        "Hard Cancel".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![item],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let provider: Arc<dyn tauqe_core::providers::LlmProvider> = Arc::new(ParkingProvider {
        entered: entered_tx,
    });

    let mut config = tauqe_core::config::AppConfig::default();
    config.repl.workflow = tauqe_core::workflow::WorkflowKind::Naive;
    // A hard cancel is classified in the turn-classification branch, before the
    // tier budget is consulted: the raised cancel signal ends the step as
    // cancelled instead of spending an attempt on the aborted turn.
    config.models.auto_level_up = false;
    let state = test_state_with_provider(&dir, config, provider).await;

    let operation = Arc::new(
        state
            .try_begin_operation()
            .expect("register batch operation"),
    );
    let op_id = operation.id();

    let state_for_batch = Arc::clone(&state);
    let batch = tokio::spawn(async move {
        run_plan_batch(
            &state_for_batch,
            "hard-cancel",
            vec!["1".to_string()],
            operation,
            PlanBatchOptions::default(),
        )
        .await
    });

    // The step is demonstrably in flight: its turn reached the parked
    // provider and ignores the cancellation watch.
    tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx.recv())
        .await
        .expect("the step turn must reach the provider")
        .expect("the provider signals that its stream was entered");

    // A hard cancel is delivered while the step task is parked. The
    // supervisor must not resolve the queue by dropping it: the slot stays
    // registered together with the abort handle, so the force-termination
    // machinery can still reach the task.
    state.hard_cancel_active_operation();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        state.is_operation_active(),
        "the slot must stay registered while the parked step task still runs"
    );

    // The sanctioned force-termination path makes the parked task exit; the
    // batch then reports its cancelled outcome. The confirmation deadline is
    // compressed instead of the production value: the parked task exits at its
    // first suspension point, so the abort lands immediately.
    state
        .abort_stalled_operation_with_deadline(op_id, std::time::Duration::from_secs(1))
        .await;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), batch)
        .await
        .expect("the cancelled batch must resolve once its step task exited")
        .expect("the batch task joins");
    assert_eq!(outcome, BatchOutcome::Cancelled { completed: 0 });

    let mut released = false;
    for _ in 0..1000 {
        if !state.is_operation_active() {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        released,
        "the cancelled batch must release the operation slot"
    );

    // The interrupted step returned to TODO so the developer can retry it.
    let reloaded = PlanStorage::load_plan(dir.path(), "hard-cancel")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Todo
    );

    // The exited step task no longer holds the session lock: a subsequent
    // interactive turn reaches the provider within a bounded timeout instead
    // of blocking indefinitely behind the abandoned step.
    state
        .application
        .ask(
            tauqe_flow::api::ModelAskInput::Prompt {
                text: "Probe the session lock".into(),
            },
            Arc::new(crate::publication::Publisher {
                out: state.out.clone(),
            }),
        )
        .await
        .expect("the probe turn starts once the operation slot is free");
    tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx.recv())
        .await
        .expect("the probe turn must acquire the session lock and reach the provider")
        .expect("the provider signals that its probe stream was entered");
}

/// An abort declared by the Architect is a typed diagnosis rather than a lost
/// turn: the refine request answers the blocked channel with the exact host
/// action, the recommendation survives verbatim, and the session history records
/// the verdict under its own label.
#[tokio::test]
async fn test_handle_plan_refine_surfaces_the_architect_abort() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "abort-refine".to_string(),
        "Abort Refine".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let abort_output = serde_json::json!({
        "status": "blocked",
        "explanation": "The toolchain cannot run in this environment",
        "reason": null,
        "abort": {
            "is_aborted": true,
            "reason": "clippy is not installed",
            "recommendation": "rustup component add clippy"
        },
        "context_requests": [],
        "repo_searches": [],
        "repo_maps": [],
        "items": []
    });
    let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
        abort_output.to_string(),
    )]));
    let config = tauqe_core::config::AppConfig::default();
    let state = test_state_with_provider(&dir, config, provider.clone()).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "abort-refine" })),
    };
    let res = handle_plan_refine(req, &state).await;
    assert!(res.error.is_none(), "refine failed: {:?}", res.error);

    let value = res.result.expect("an abort is a typed outcome");
    let result: PlanRefineResult = serde_json::from_value(value).expect("the result decodes");
    match result {
        PlanRefineResult::Blocked { reason } => {
            assert!(reason.contains("Execution aborted"), "{reason}");
            assert!(reason.contains("clippy is not installed"), "{reason}");
            assert!(
                reason.contains("rustup component add clippy"),
                "the recommendation must survive verbatim: {reason}"
            );
        }
        other => panic!("expected a typed block carrying the abort, got {other:?}"),
    }

    // The verdict is durable in the session history under its own label.
    let entries = state
        .application
        .test_mutate_history(|history| history.get_entries())
        .await
        .unwrap()
        .unwrap();
    let recorded = entries
        .iter()
        .find_map(|entry| match entry {
            tauqe_core::history::HistoryEntry::Response {
                message, summary, ..
            } => Some((message.clone(), summary.clone())),
            _ => None,
        })
        .expect("the aborted synthesis records its verdict");
    assert!(
        recorded.0.contains("rustup component add clippy"),
        "{:?}",
        recorded.0
    );
    let summary = recorded.1.expect("the verdict carries an abort label");
    assert!(summary.contains("Aborted"), "{summary}");
    assert!(summary.contains("Abort Refine"), "{summary}");

    // An abort reprojects nothing: the stored plan keeps the horizon it had.
    let reloaded = PlanStorage::load_plan(dir.path(), "abort-refine")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.implementation.find_item("1").unwrap().status,
        PlanItemStatus::Todo
    );
}

/// A contour parked mid-round and then force-aborted through the operation slot
/// must answer `CANCELLED` instead of a provider failure.
///
/// The aborted contour never relays `StreamEvent::Cancelled`, so the bridge
/// classifies the dead task through the authoritative cancellation signal.
/// Without that seam the request surfaced as `model/error` with a message that
/// itself read "was cancelled", and a replanning wave persisted the aborted run
/// as a semantic blocker.
#[tokio::test]
async fn test_plan_refine_force_aborted_contour_answers_cancelled() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "force-abort-refine".to_string(),
        "Force Abort Refine".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let provider: Arc<dyn tauqe_core::providers::LlmProvider> = Arc::new(ParkingProvider {
        entered: entered_tx,
    });

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );
    state.application.test_set_provider(provider).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "force-abort-refine" })),
    };
    let state_for_req = Arc::clone(&state);
    let handler = tokio::spawn(async move { handle_plan_refine(req, &state_for_req).await });

    // The synthesis is demonstrably in flight: its provider round is parked and
    // ignores the cancellation watch.
    tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx.recv())
        .await
        .expect("the synthesis must reach the provider")
        .expect("the provider signals that its stream was entered");

    // The hard cancel is raised while the contour is parked, and the registered
    // contour task is then force-aborted: no `StreamEvent::Cancelled` is
    // relayed, so only the authoritative signal describes the abort.
    state.cancel_active_operation();
    state
        .force_abort_active_operation(std::time::Duration::from_secs(10))
        .await;

    let res = tokio::time::timeout(std::time::Duration::from_secs(10), handler)
        .await
        .expect("the aborted synthesis must return instead of parking the request")
        .expect("the handler task joins");
    let err = res
        .error
        .expect("an aborted synthesis must answer an error");
    assert_eq!(
        err.code,
        errors::CANCELLED,
        "a force-aborted contour must never surface as a provider failure: {err:?}"
    );
    assert!(
        err.message.contains("was cancelled"),
        "the verdict must name the cancelled synthesis: {}",
        err.message
    );

    // No terminal answer is published for the aborted synthesis, and the
    // operation never reported a provider failure either.
    let mut emitted = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        emitted.push(raw);
    }
    assert!(
        !emitted
            .iter()
            .any(|event| event.contains(events::MODEL_FINISHED)),
        "an aborted synthesis must never publish model/finished: {emitted:?}"
    );
    assert!(
        !emitted
            .iter()
            .any(|event| event.contains(events::MODEL_ERROR)),
        "a force-aborted contour must never publish model/error: {emitted:?}"
    );
}

/// Provider double whose generation round parks until the test raises the
/// operation's hard-cancel signal, and then answers with a complete synthesized
/// payload.
///
/// The contour consults the cancel signal at the top of every round, so a raise
/// that lands inside the round is invisible to it: the round still proposes a
/// payload the schema accepts. That is exactly the window the synthesis guards
/// immediately before it rewrites the stored horizon.
struct CancellingFinalRoundProvider {
    /// Signals that a generation round was entered.
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    /// Released once the test raised the hard-cancel signal.
    release: Arc<tokio::sync::Notify>,
    /// Complete synthesized payload the parked round answers with.
    payload: String,
}

#[async_trait::async_trait]
impl tauqe_core::providers::LlmProvider for CancellingFinalRoundProvider {
    fn kind(&self) -> tauqe_core::models::ProviderKind {
        tauqe_core::models::ProviderKind::OpenRouter
    }

    async fn stream(
        &self,
        _request: tauqe_core::providers::LlmRequest<'_>,
        tx: tokio::sync::mpsc::Sender<tauqe_core::providers::StreamEvent>,
        _cancel: tauqe_core::stop::HardCancellation,
    ) -> tauqe_core::providers::ProviderResult {
        let _ = self.entered.send(());
        self.release.notified().await;
        let _ = tx
            .send(tauqe_core::providers::StreamEvent::TextDelta(
                self.payload.clone(),
            ))
            .await;
        let _ = tx.send(tauqe_core::providers::StreamEvent::Done).await;
        Ok(())
    }
}

/// A hard cancel delivered after the contour's final provider round resolved but
/// before the destructive horizon write must never be answered with a persisted
/// success.
///
/// The contour consults the cancel signal only at the top of each round, so a
/// raise that lands inside the final round is invisible to it and the round
/// still proposes a payload the schema accepts. The synthesis therefore
/// re-checks the authoritative signal immediately before it replaces the
/// uncompleted horizon, at the same seam the review subsystem guards before it
/// touches its session on disk, and a raised signal releases the transient
/// context, answers `CANCELLED`, records no session history, and leaves the
/// stored plan exactly as it was.
#[tokio::test]
async fn test_plan_refine_answers_cancelled_before_the_destructive_horizon_write() {
    let dir = TestDir::new();
    let plan = Plan::from_parts(
        "cancel-horizon".to_string(),
        "Cancel Horizon".to_string(),
        None,
        1,
        1,
        Vec::new(),
        vec![test_item("1", PlanItemStatus::Todo)],
    );
    PlanStorage::save_plan(dir.path(), &plan).unwrap();

    let refine_output = serde_json::json!({
        "status": "success",
        "explanation": "Synthesized while the operation was being cancelled",
        "items": [
            {
                "id": "1.1",
                "title": "Replacement step",
                "status": "todo",
                "files": ["src.rs"],
                "readonly_files": [],
                "concept_refs": [],
                "depends_on": [],
                "children": []
            }
        ]
    })
    .to_string();

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let release = Arc::new(tokio::sync::Notify::new());
    let provider: Arc<dyn tauqe_core::providers::LlmProvider> =
        Arc::new(CancellingFinalRoundProvider {
            entered: entered_tx,
            release: Arc::clone(&release),
            payload: refine_output,
        });

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );
    state.application.test_set_provider(provider).await;

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::PLAN_REFINE.to_string(),
        params: Some(serde_json::json!({ "plan_id": "cancel-horizon" })),
    };
    let state_for_req = Arc::clone(&state);
    let handler = tokio::spawn(async move { handle_plan_refine(req, &state_for_req).await });

    // The synthesis is demonstrably inside its final generation round, which has
    // already passed the contour's loop-top cancel check.
    tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx.recv())
        .await
        .expect("the synthesis must reach the provider")
        .expect("the provider signals that its round was entered");

    // The developer aborts the operation while that round is in flight, and the
    // round still answers a complete payload the schema accepts.
    state.cancel_active_operation();
    release.notify_one();

    let res = tokio::time::timeout(std::time::Duration::from_secs(10), handler)
        .await
        .expect("the cancelled synthesis must answer instead of parking the request")
        .expect("the handler task joins");
    let err = res
        .error
        .expect("a cancelled horizon write must answer an error");
    assert_eq!(
        err.code,
        errors::CANCELLED,
        "the write must never be reported as a success or a failure: {err:?}"
    );
    assert!(
        err.message.contains("was cancelled"),
        "the verdict must name the cancelled synthesis: {}",
        err.message
    );

    // The stored plan keeps the horizon it had: no replacement was written.
    let reloaded = PlanStorage::load_plan(dir.path(), "cancel-horizon")
        .unwrap()
        .unwrap();
    let previous = reloaded
        .implementation
        .find_item("1")
        .expect("the previous horizon must survive the aborted write");
    assert_eq!(previous.title, "Step 1");
    assert_eq!(previous.status, PlanItemStatus::Todo);
    assert!(
        reloaded.implementation.find_item("1.1").is_none(),
        "the aborted write must not project the synthesized horizon"
    );

    // No event announced a horizon that was never persisted.
    let mut emitted = Vec::new();
    while let Ok(raw) = rx.try_recv() {
        emitted.push(raw);
    }
    assert!(
        !emitted
            .iter()
            .any(|event| event.contains(events::PLAN_UPDATED)),
        "an aborted write must never announce a plan update: {emitted:?}"
    );

    // Strict history transactionality: the aborted write records neither the
    // turn nor its response.
    let entries = state
        .application
        .test_mutate_history(|history| history.get_entries())
        .await
        .unwrap()
        .unwrap();
    assert!(
        entries.is_empty(),
        "an aborted horizon write must record no session history"
    );
}

/// Flips a flag when the owning task future is dropped, so the test can prove
/// that the wedged task really stopped running instead of lingering detached.
struct ExitMarker(Arc<AtomicBool>);

impl Drop for ExitMarker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Budget after which a deliberate non-yielding stretch ends on its own.
///
/// The stretch exists to prove what an abort cannot reach, so it spins
/// synchronously instead of parking on an abort landing point. The budget is the
/// second line of defence behind the test's release guard: the wedged runtime
/// waits for its pinned worker when it shuts down, so an unbounded spin would
/// turn one failed assertion into a hung suite.
const PINNED_STRETCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Releases a pinned stretch on drop, so a failing assertion never leaves a
/// wedged task spinning after the test has returned.
struct StretchRelease(Arc<AtomicBool>);

impl Drop for StretchRelease {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A hard cancel must survive a task that never confirms its exit: the stale
/// registration is released within the shared task-exit deadline, the slot admits
/// the next operation, and the superseded guard a wedged task still owns wipes
/// nothing when it finally drops.
#[test]
fn test_abort_stalled_operation_releases_a_registration_that_never_confirms() {
    let dir = TestDir::new();

    // The wedged step owns a runtime of its own: its whole point is a stretch
    // that never yields back to its scheduler, so the abort delivered to it
    // can land only once that stretch ends.
    let wedged_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("the wedged runtime starts");
    let confirmation_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the confirmation runtime starts");

    let stretch_ends = Arc::new(AtomicBool::new(false));
    // The pinned stretch spins on this flag, and its runtime waits for the
    // pinned worker when it shuts down, so the flag is released on drop.
    let _stretch_guard = StretchRelease(Arc::clone(&stretch_ends));
    let exited = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();

    confirmation_runtime.block_on(async {
        let state = test_state(&dir);
        let guard = Arc::new(state.try_begin_operation().expect("the slot is free"));
        let operation_id = guard.id();
        let task = {
            let guard = Arc::clone(&guard);
            let stretch_ends = Arc::clone(&stretch_ends);
            let exited = Arc::clone(&exited);
            wedged_runtime.spawn(async move {
                let _guard = guard;
                let _marker = ExitMarker(Arc::clone(&exited));
                let _ = entered_tx.send(());
                let deadline = std::time::Instant::now() + PINNED_STRETCH_BUDGET;
                while !stretch_ends.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    std::thread::yield_now();
                }
                std::future::pending::<()>().await
            })
        };
        // The wedged step installs the handle a hard cancel targets, exactly as a
        // running turn does when it spawns its workflow.
        guard.set_abort_handle(task.abort_handle());
        drop(guard);

        // The abort is delivered only after the task provably reached its
        // non-yielding stretch. Aborting a task before its first poll drops the
        // future without ever running it, so the exit marker would never be
        // constructed and the regression below would wait for an exit that was
        // never pending.
        tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx)
            .await
            .expect("the wedged task must reach its non-yielding stretch")
            .expect("the wedged task signals that its stretch began");

        // The confirmation deadline is passed explicitly, so a compressed
        // value keeps the expiry path deterministic without relying on a
        // task-local the spawner would have to propagate.
        let started = std::time::Instant::now();
        state
            .abort_stalled_operation_with_deadline(
                operation_id,
                std::time::Duration::from_millis(50),
            )
            .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the exit confirmation must be bounded"
        );

        assert!(
            !state.is_operation_active(),
            "the expired confirmation must release the stale registration"
        );
        assert!(
            !exited.load(Ordering::SeqCst),
            "the wedged task provably never exited before the slot was released"
        );

        let fresh = state
            .try_begin_operation()
            .expect("the released slot must admit a new operation");
        assert_ne!(fresh.id(), operation_id);

        // The orphan dies at the first suspension point its stretch reaches and
        // drops the superseded guard after the new operation registered.
        stretch_ends.store(true, Ordering::SeqCst);
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !exited.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            stopped.is_ok(),
            "the orphaned task must die once its non-yielding stretch ends"
        );

        // The id-checked release of the superseded guard leaves the newcomer's
        // registration untouched.
        assert!(
            state.is_operation_active(),
            "the superseded guard must never wipe a newer registration"
        );
        assert!(
            state.try_begin_operation().is_err(),
            "the newer registration must stay in force"
        );
        drop(fresh);
    });
}
