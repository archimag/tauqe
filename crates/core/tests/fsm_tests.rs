//! Deterministic end-to-end integration tests for the core FSM task cycles.
//!
//! Every test drives the real turn pipeline against a scripted
//! [`MockLlmProvider`], exercising discovery, edit staging, patch-retry
//! healing, and toolchain verification healing without any network access or
//! manual context assembly.

use std::path::Path;

use tauqe_core::context::ContextManager;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::history::HistoryManager;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::providers::{LlmProvider, StreamEvent};
use tauqe_core::workflow::policy::ApplyPolicy;
use tauqe_core::workflow::{
    AnswerEditLoop, EditLoop, ExecutionMode, GitTransactionPolicy, SingleExecution, Task, TaskEnv,
    WorkflowOptions,
};
use tauqe_protocol::{
    ContextAccess, ModelRef, ModelResult, Plan, PlanItem, PlanItemStatus, PlanScopeEvaluation,
};
use tokio::sync::{mpsc, watch};

/// Turn marker attached to every scripted protocol exchange.
const MARKER: &str = "FSM";

/// Manifest for a self-contained Cargo fixture whose functions start with
/// incompatible return types, so `cargo check` fails until both are repaired.
const HEAL_MANIFEST: &str = "\
[package]
name = \"tauqe-fsm-heal-fixture\"
version = \"0.0.0\"
edition = \"2021\"

[workspace]
";

/// Broken fixture source, repaired across the initial attempt and one healing
/// step.
const HEAL_LIB: &str = "pub fn alpha() -> i32 { \"alpha\" }\npub fn beta() -> i32 { \"beta\" }\n";

/// Builds a single-edit XML response carrying one search/replace hunk.
fn edit_block(summary: &str, path: &str, search: &str, replace: &str) -> String {
    format!(
        "<tauqe_edits_{MARKER} summary=\"{summary}\">\n<edit_{MARKER} path=\"{path}\">\n<search_{MARKER}>\n{search}\n</search_{MARKER}>\n<replace_{MARKER}>\n{replace}\n</replace_{MARKER}>\n</edit_{MARKER}>\n</tauqe_edits_{MARKER}>"
    )
}

/// Creates a temporary Git repository with an initial commit.
fn init_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let repo_root = dir.path().to_path_buf();
    tauqe_core::git::init_repository(Some(&repo_root), true).expect("initialize repository");
    (dir, repo_root)
}

/// Stages and commits the given repository-relative files.
fn commit_fixture(repo_root: &Path, files: &[&str]) {
    let owned: Vec<String> = files.iter().map(|path| (*path).to_string()).collect();
    tauqe_core::git::cmd::stage_files(repo_root, &owned).expect("stage fixture files");
    tauqe_core::git::cmd::commit_with_developer_author(repo_root, "Add fixture", false)
        .expect("commit fixture files");
}

/// Creates a stream channel whose receiver is already dropped, so producers
/// observe a closed channel and can never block the turn.
fn event_channel() -> (mpsc::Sender<StreamEvent>, watch::Receiver<bool>) {
    let (stream_tx, rx) = mpsc::channel(512);
    drop(rx);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    (stream_tx, cancel_rx)
}

/// Borrows the shared turn fixtures into a [`TaskEnv`] ready for execution.
#[allow(clippy::too_many_arguments)]
fn build_env<'a>(
    provider: &'a dyn LlmProvider,
    model: &'a ModelRef,
    context: &'a mut ContextManager,
    history: &'a mut HistoryManager,
    protocol: &'a dyn PromptProtocol,
    policy: &'a mut dyn ApplyPolicy,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    options: &'a WorkflowOptions,
) -> TaskEnv<'a> {
    TaskEnv {
        provider,
        model,
        context,
        history,
        protocol,
        policy,
        workflow_name: "git",
        stream_tx,
        cancel_rx,
        options,
    }
}

#[tokio::test]
async fn answer_edit_loop_stages_matching_edits() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let provider = MockLlmProvider::scripted([MockResponse::text(edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    ))]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let mut env = TaskEnv {
        provider: &provider,
        model: &model,
        context: &mut context,
        history: &mut history,
        protocol: protocol.as_ref(),
        policy: &mut policy,
        workflow_name: "git",
        stream_tx,
        cancel_rx,
        options: &options,
    };

    let output = AnswerEditLoop::new("Change compute to return 42")
        .execute(&mut env)
        .await
        .expect("answer/edit loop executes");

    match &output.result {
        ModelResult::Edit { edits, error, .. } => {
            assert!(error.is_none(), "unexpected error: {error:?}");
            assert_eq!(edits.len(), 1);
        }
        other => panic!("expected staged edits, got {other:?}"),
    }
    assert!(!output.assistant_text.contains("Patch Retry"));
}

#[tokio::test]
async fn answer_edit_loop_accepts_answer_without_edits() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let provider = MockLlmProvider::scripted([MockResponse::text(
        "The compute function already returns the expected value; no edits are required.",
    )]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let mut env = TaskEnv {
        provider: &provider,
        model: &model,
        context: &mut context,
        history: &mut history,
        protocol: protocol.as_ref(),
        policy: &mut policy,
        workflow_name: "git",
        stream_tx,
        cancel_rx,
        options: &options,
    };

    let output = AnswerEditLoop::new("Explain target.rs")
        .execute(&mut env)
        .await
        .expect("answer/edit loop executes");

    assert!(matches!(output.result, ModelResult::Answer { .. }));
}

#[tokio::test]
async fn answer_edit_loop_discovers_requested_context() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n")
        .expect("write helper fixture");
    commit_fixture(&repo_root, &["target.rs", "helper.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!(
        "I need the constant definitions.\n<context_request_{MARKER} path=\"helper.rs\" access=\"read_only\" />"
    );
    let edit = edit_block(
        "Use the helper constant",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = TaskEnv {
            provider: &provider,
            model: &model,
            context: &mut context,
            history: &mut history,
            protocol: protocol.as_ref(),
            policy: &mut policy,
            workflow_name: "git",
            stream_tx,
            cancel_rx,
            options: &options,
        };
        AnswerEditLoop::new("Use the helper constant")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(&output.result, ModelResult::Edit { edits, .. } if !edits.is_empty()));
    assert_eq!(provider.requests().len(), 2);
    let second_request = &provider.requests()[1];
    assert!(second_request.messages[0]
        .content
        .contains("pub const ANSWER: i32 = 42;"));
}

#[tokio::test]
async fn answer_edit_loop_heals_failed_patch_application() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let broken = edit_block(
        "Broken attempt",
        "target.rs",
        "this text does not exist in the file",
        "irrelevant replacement",
    );
    let corrected = edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(broken), MockResponse::text(corrected)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let mut env = TaskEnv {
        provider: &provider,
        model: &model,
        context: &mut context,
        history: &mut history,
        protocol: protocol.as_ref(),
        policy: &mut policy,
        workflow_name: "git",
        stream_tx,
        cancel_rx,
        options: &options,
    };

    let output = AnswerEditLoop::new("Change compute to return 42")
        .execute(&mut env)
        .await
        .expect("answer/edit loop executes");

    assert!(matches!(&output.result, ModelResult::Edit { edits, .. } if !edits.is_empty()));
    assert!(output.assistant_text.contains("Patch Retry"));
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn edit_loop_retries_until_edits_are_produced() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let provider = MockLlmProvider::scripted([
        MockResponse::text("I will describe the change but propose no edits."),
        MockResponse::text(edit_block(
            "Change compute to 42",
            "target.rs",
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        )),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let mut env = TaskEnv {
        provider: &provider,
        model: &model,
        context: &mut context,
        history: &mut history,
        protocol: protocol.as_ref(),
        policy: &mut policy,
        workflow_name: "git",
        stream_tx,
        cancel_rx,
        options: &options,
    };

    let output = EditLoop::new("Change compute to return 42")
        .execute(&mut env)
        .await
        .expect("edit loop executes");

    assert!(matches!(&output.result, ModelResult::Edit { edits, .. } if !edits.is_empty()));
    assert!(output.assistant_text.contains("Edit Retry"));
}

#[tokio::test]
async fn git_workflow_heals_compiler_failure() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("Cargo.toml"), HEAL_MANIFEST).expect("write manifest");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/lib.rs"), HEAL_LIB).expect("write broken library");
    commit_fixture(&repo_root, &["Cargo.toml", "src/lib.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let fix_alpha = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Repair alpha return type",
            "src/lib.rs",
            "pub fn alpha() -> i32 { \"alpha\" }",
            "pub fn alpha() -> i32 { 1 }",
        ),
    );
    let fix_beta = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Repair beta return type",
            "src/lib.rs",
            "pub fn beta() -> i32 { \"beta\" }",
            "pub fn beta() -> i32 { 2 }",
        ),
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(fix_alpha), MockResponse::text(fix_beta)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let (stream_tx, cancel_rx) = event_channel();

    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Fix the mismatched return types in src/lib.rs",
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("workflow execution succeeds");

    match execution.result {
        ModelResult::Edit { applied, error, .. } => {
            assert!(error.is_none(), "unexpected error: {error:?}");
            assert!(applied, "healing should apply and commit the repaired edits");
        }
        other => panic!("expected applied edits, got {other:?}"),
    }

    let healed = std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read healed library");
    assert!(healed.contains("pub fn alpha() -> i32 { 1 }"));
    assert!(healed.contains("pub fn beta() -> i32 { 2 }"));
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn scratchpad_from_discovery_reaches_verification_healing() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("Cargo.toml"), HEAL_MANIFEST).expect("write manifest");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/lib.rs"), HEAL_LIB).expect("write broken library");
    std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n")
        .expect("write helper fixture");
    commit_fixture(&repo_root, &["Cargo.toml", "src/lib.rs", "helper.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!(
        "Recording hypothesis.\n<scratchpad_{MARKER}>\n- Hypothesis: repair both return types\n</scratchpad_{MARKER}>\n<context_request_{MARKER} path=\"helper.rs\" access=\"read_only\" />"
    );
    let fix_alpha = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Repair alpha return type",
            "src/lib.rs",
            "pub fn alpha() -> i32 { \"alpha\" }",
            "pub fn alpha() -> i32 { 1 }",
        ),
    );
    let fix_beta = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Repair beta return type",
            "src/lib.rs",
            "pub fn beta() -> i32 { \"beta\" }",
            "pub fn beta() -> i32 { 2 }",
        ),
    );
    let provider = MockLlmProvider::scripted([
        MockResponse::text(discovery),
        MockResponse::text(fix_alpha),
        MockResponse::text(fix_beta),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let (stream_tx, cancel_rx) = event_channel();

    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Fix the mismatched return types in src/lib.rs",
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("workflow execution succeeds");

    assert!(matches!(
        execution.result,
        ModelResult::Edit { applied: true, .. }
    ));
    assert_eq!(provider.requests().len(), 3);
    let healing_request = &provider.requests()[2];
    assert!(healing_request.messages[0]
        .content
        .contains("<scratchpad>"));
    assert!(healing_request.messages[0]
        .content
        .contains("Hypothesis: repair both return types"));
}

#[tokio::test]
async fn fsm_turn_respects_cancellation() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    // A scripted edit that the cancelled turn must never consume or persist.
    let provider = MockLlmProvider::scripted([MockResponse::text(edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    ))]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = GitTransactionPolicy::new();
    let options = WorkflowOptions::default();

    let (stream_tx, rx) = mpsc::channel(512);
    drop(rx);
    // Raise the cancellation flag before the turn starts; the mock provider
    // observes it and reports a cancellation on the very first call.
    let (cancel_tx, cancel_rx) = watch::channel(false);
    cancel_tx.send(true).expect("signal cancellation");

    let outcome = tauqe_core::workflow::fsm::run_fsm_turn(
        "Change compute to return 42",
        &provider,
        &model,
        &mut context,
        &mut history,
        protocol.as_ref(),
        &mut policy,
        "git",
        stream_tx,
        cancel_rx,
        &options,
    )
    .await
    .expect("fsm turn executes");

    assert!(outcome.is_cancelled, "turn must report cancellation");
    assert!(matches!(outcome.result, ModelResult::Answer { .. }));
    assert_eq!(
        provider.remaining(),
        1,
        "the cancelled turn must not consume the scripted edit"
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "pub fn compute() -> i32 { 0 }\n",
        "a cancelled turn must not write to disk"
    );
}

#[tokio::test]
async fn fsm_turn_saves_and_activates_plan() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let plan_output = format!(
        "Recording the plan.\n\
        <plan_{MARKER} action=\"save\" id=\"integration-plan\" title=\"Integration Plan\">\n\
          <concept_{MARKER}>\n\
            <concept_item_{MARKER} id=\"c-1\" title=\"Plan Invariant\" />\n\
          </concept_{MARKER}>\n\
          <item_{MARKER} id=\"1\" title=\"First step\" status=\"todo\" />\n\
        </plan_{MARKER}>"
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(plan_output)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Create the integration plan")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(output.result, ModelResult::Answer { .. }));

    let loaded = tauqe_core::plan::storage::PlanStorage::load_plan(&repo_root, "integration-plan")
        .expect("load plan")
        .expect("plan must be persisted");
    assert_eq!(loaded.title, "Integration Plan");
    assert_eq!(loaded.items.len(), 1);
    assert_eq!(loaded.items[0].id, "1");

    let active = tauqe_core::plan::storage::PlanStorage::load_active_id(&repo_root)
        .expect("load active plan id");
    assert_eq!(active.as_deref(), Some("integration-plan"));
}

#[tokio::test]
async fn fsm_turn_evicts_context_via_drop() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n")
        .expect("write helper fixture");
    commit_fixture(&repo_root, &["target.rs", "helper.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!(
        "Loading helper context.\n<context_request_{MARKER} path=\"helper.rs\" access=\"read_only\" />"
    );
    let eviction =
        format!("Releasing helper context.\n<context_drop_{MARKER} path=\"helper.rs\" />");
    let provider = MockLlmProvider::scripted([
        MockResponse::text(discovery),
        MockResponse::text(eviction),
        MockResponse::text("The helper file is no longer needed."),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions {
        discovery_mode: tauqe_core::config::DiscoveryMode::Incremental,
        ..WorkflowOptions::default()
    };
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Check whether helper.rs is still needed")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(output.result, ModelResult::Answer { .. }));
    assert_eq!(provider.requests().len(), 3);

    let paths: Vec<String> = context
        .get_state()
        .items
        .iter()
        .map(|item| item.path.clone())
        .collect();
    assert!(paths.contains(&"target.rs".to_string()));
    assert!(
        !paths.contains(&"helper.rs".to_string()),
        "the dropped file must be evicted from the context"
    );
    assert!(
        !context.auto_files().contains(&"helper.rs".to_string()),
        "the dropped file must leave the auto layer"
    );
}

#[tokio::test]
async fn scratchpad_persists_between_proposal_rounds() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n")
        .expect("write helper fixture");
    commit_fixture(&repo_root, &["target.rs", "helper.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!(
        "Recording working notes.\n\
        <scratchpad_{MARKER}>\n- Finding: helper.rs holds ANSWER\n</scratchpad_{MARKER}>\n\
        <context_request_{MARKER} path=\"helper.rs\" access=\"read_only\" />"
    );
    let edit = edit_block(
        "Use the helper constant",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Use the helper constant")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(&output.result, ModelResult::Edit { edits, .. } if !edits.is_empty()));
    assert_eq!(provider.requests().len(), 2);
    let follow_up = &provider.requests()[1];
    assert!(follow_up.messages[0].content.contains("<scratchpad>"));
    assert!(follow_up.messages[0]
        .content
        .contains("Finding: helper.rs holds ANSWER"));
}

fn event_channel_with_rx() -> (mpsc::Sender<StreamEvent>, mpsc::Receiver<StreamEvent>, watch::Receiver<bool>) {
    let (stream_tx, rx) = mpsc::channel(512);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    (stream_tx, rx, cancel_rx)
}

#[tokio::test]
async fn answer_discussing_protocol_tags_never_starts_edits() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n").unwrap();
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    let mut history = HistoryManager::new(repo_root.clone());

    let provider = MockLlmProvider::scripted([MockResponse::text(
        "To save a plan, use <plan action=\"save\" id=\"1\">...</plan>. Do not use <edit path=\"foo\"> directly.",
    )]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    
    let (stream_tx, mut stream_rx, cancel_rx) = event_channel_with_rx();

    let mut env = build_env(
        &provider,
        &model,
        &mut context,
        &mut history,
        protocol.as_ref(),
        &mut policy,
        stream_tx,
        cancel_rx,
        &options,
    );
    
    let output = AnswerEditLoop::new("How to save a plan?")
        .execute(&mut env)
        .await
        .expect("answer/edit loop executes");

    assert!(matches!(output.result, ModelResult::Answer { .. }));
    
    let mut saw_edit_started = false;
    while let Ok(ev) = stream_rx.try_recv() {
        if matches!(ev, StreamEvent::EditStarted | StreamEvent::EditFileStarted { .. }) {
            saw_edit_started = true;
        }
    }
    assert!(!saw_edit_started, "Conversational text discussing tags should not emit edit events");
}

#[tokio::test]
async fn terminal_error_outcome_blocks_without_applying_edits() {
    let (_dir, repo_root) = init_repo();
    let mut context = ContextManager::new(repo_root.clone());
    let mut history = HistoryManager::new(repo_root.clone());

    let provider = MockLlmProvider::scripted([MockResponse::text(
        format!("I cannot proceed.\n<error_{MARKER} message=\"Missing dependencies\" />")
    )]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let mut env = build_env(
        &provider,
        &model,
        &mut context,
        &mut history,
        protocol.as_ref(),
        &mut policy,
        stream_tx,
        cancel_rx,
        &options,
    );
    
    let output = AnswerEditLoop::new("Do something impossible")
        .execute(&mut env)
        .await
        .expect("answer/edit loop executes");

    match output.result {
        ModelResult::Edit { applied, error, .. } => {
            assert!(!applied);
            assert_eq!(error, Some("Missing dependencies".to_string()));
        }
        _ => panic!("Expected unapplied Edit result with error"),
    }
}

#[tokio::test]
async fn terminal_success_outcome_labels_applied_edits() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n").unwrap();
    commit_fixture(&repo_root, &["target.rs"]);
    
    let mut context = ContextManager::new(repo_root.clone());
    context.add_file("target.rs", ContextAccess::Editable).unwrap();
    let mut history = HistoryManager::new(repo_root.clone());

    let edits = edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    
    let provider = MockLlmProvider::scripted([MockResponse::text(
        format!("{edits}\n<success_{MARKER} summary=\"Operation completed successfully\" />")
    )]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let mut env = build_env(
        &provider,
        &model,
        &mut context,
        &mut history,
        protocol.as_ref(),
        &mut policy,
        stream_tx,
        cancel_rx,
        &options,
    );
    
    let output = AnswerEditLoop::new("Fix target")
        .execute(&mut env)
        .await
        .expect("answer/edit loop executes");

    match output.result {
        ModelResult::Edit { applied, error, .. } => {
            assert!(applied);
            assert!(error.is_none());
        }
        _ => panic!("Expected applied Edit result"),
    }
}

#[tokio::test]
async fn fsm_turn_semantic_blocked_outcome_aborts_and_rolls_back() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let blocked_response = format!(
        "Cannot complete the requested change.\n\
        <blocked_{MARKER} reason=\"Precondition violated: target API deprecated\" />"
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(blocked_response)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .unwrap();
    let mut policy = GitTransactionPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let exec = SingleExecution::new(options);
    let execution = exec
        .execute(
            &mut policy,
            "Refactor compute",
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("workflow execution succeeds");

    assert_eq!(
        execution.blocked,
        Some("Precondition violated: target API deprecated".to_string())
    );
    assert_eq!(
        provider.remaining(),
        0,
        "blocked turn must not trigger retry loops"
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "pub fn compute() -> i32 { 0 }\n",
        "working tree must remain clean after semantic block"
    );
}

#[tokio::test]
async fn plan_semantic_recovery_lifecycle_end_to_end() {
    let (_dir, repo_root) = init_repo();

    let foundation = PlanItem {
        id: "1".to_string(),
        title: "Foundation".to_string(),
        status: PlanItemStatus::Done,
        files: vec!["src/foundation.rs".to_string()],
        ..Default::default()
    };
    let blocked_step = PlanItem {
        id: "2".to_string(),
        title: "Complex Endpoint".to_string(),
        status: PlanItemStatus::Blocked,
        files: vec!["src/endpoint.rs".to_string()],
        ..Default::default()
    };
    let dependent_step = PlanItem {
        id: "3".to_string(),
        title: "Integration Test".to_string(),
        status: PlanItemStatus::Todo,
        files: vec!["tests/integration.rs".to_string()],
        depends_on: vec!["2".to_string()],
        ..Default::default()
    };

    let plan = Plan {
        id: "recovery-plan".to_string(),
        title: "Recovery Plan".to_string(),
        created_at: 10,
        updated_at: 10,
        concept: vec![],
        items: vec![foundation.clone(), blocked_step.clone(), dependent_step.clone()],
        ..Default::default()
    };

    // 1. Save plan to storage
    tauqe_core::plan::storage::PlanStorage::save_plan(&repo_root, &plan)
        .expect("save initial plan");

    // 2. Invariant #c-2: Done is immutable (regression of Done step is rejected)
    let mut illegal_mutation = plan.clone();
    illegal_mutation.items[0].status = PlanItemStatus::Todo;
    let err = tauqe_core::plan::storage::PlanStorage::save_plan(&repo_root, &illegal_mutation)
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("Cannot regress status of completed step '1'"));

    // 3. Invariant #c-4: DAG readiness checks before replanning
    match plan.evaluate_scope(Some("3")) {
        PlanScopeEvaluation::BlockedByDependencies(deps) => {
            assert_eq!(deps.len(), 1);
            assert!(deps[0].contains("#3"));
        }
        other => panic!("Expected BlockedByDependencies, got {:?}", other),
    }
    assert_eq!(
        plan.evaluate_scope(Some("2")),
        PlanScopeEvaluation::StepBlocked
    );

    // 4. Invariant #c-3: Sub-tree grafting replaces blocked step 2 and remaps dependents
    let replacement = vec![
        PlanItem {
            id: "2.1".to_string(),
            title: "Data model".to_string(),
            status: PlanItemStatus::Todo,
            files: vec!["src/model.rs".to_string()],
            ..Default::default()
        },
        PlanItem {
            id: "2.2".to_string(),
            title: "Handler split".to_string(),
            status: PlanItemStatus::Todo,
            files: vec!["src/handler.rs".to_string()],
            depends_on: vec!["2.1".to_string()],
            ..Default::default()
        },
    ];

    let grafted = tauqe_core::plan::storage::PlanStorage::graft_subtree(
        &repo_root,
        "recovery-plan",
        "2",
        replacement,
    )
    .expect("graft subtree over blocked step");

    assert_eq!(grafted.items.len(), 4);
    assert_eq!(grafted.items[0].id, "1");
    assert_eq!(grafted.items[0].status, PlanItemStatus::Done);
    assert_eq!(grafted.items[1].id, "2.1");
    assert_eq!(grafted.items[2].id, "2.2");
    assert_eq!(grafted.items[3].id, "3");

    // Dependent tail step 3 is automatically remapped to exit step "2.2"
    let remapped_dep = grafted.find_item("3").expect("find step 3");
    assert_eq!(remapped_dep.depends_on, vec!["2.2".to_string()]);

    // 5. Invariant #c-4 & #c-5: DAG readiness evaluation on the updated plan
    match grafted.evaluate_scope(None) {
        PlanScopeEvaluation::Ready(leaves) => {
            let ids: Vec<&str> = leaves.iter().map(|l| l.id.as_str()).collect();
            assert_eq!(ids, vec!["2.1", "2.2", "3"]);
        }
        other => panic!("Expected Ready, got {:?}", other),
    }

    match grafted.evaluate_scope(Some("3")) {
        PlanScopeEvaluation::BlockedByDependencies(deps) => {
            assert_eq!(deps.len(), 1);
            assert!(deps[0].contains("#3"));
        }
        other => panic!("Expected BlockedByDependencies on step 3, got {:?}", other),
    }

    // Advance 2.1 to Done
    let mut progressed = grafted.clone();
    progressed.update_item_status("2.1", PlanItemStatus::Done);
    tauqe_core::plan::storage::PlanStorage::save_plan(&repo_root, &progressed)
        .expect("save progress");

    match progressed.evaluate_scope(Some("2.2")) {
        PlanScopeEvaluation::Ready(leaves) => {
            assert_eq!(leaves.len(), 1);
            assert_eq!(leaves[0].id, "2.2");
        }
        other => panic!("Expected Ready for 2.2, got {:?}", other),
    }

    // Advance 2.2 to Done
    progressed.update_item_status("2.2", PlanItemStatus::Done);
    tauqe_core::plan::storage::PlanStorage::save_plan(&repo_root, &progressed)
        .expect("save progress");

    match progressed.evaluate_scope(Some("3")) {
        PlanScopeEvaluation::Ready(leaves) => {
            assert_eq!(leaves.len(), 1);
            assert_eq!(leaves[0].id, "3");
        }
        other => panic!("Expected Ready for 3, got {:?}", other),
    }
}
