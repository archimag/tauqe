//! Deterministic end-to-end integration tests for the core FSM task cycles.
//!
//! Every test drives the real turn pipeline against a scripted
//! [`MockLlmProvider`], exercising discovery, edit staging, patch-retry
//! healing, and toolchain verification healing without any network access or
//! manual context assembly.

use std::path::Path;

use tauqe_core::context::ContextManager;
use tauqe_core::history::HistoryManager;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::providers::{ChatMessage, LlmProvider, StreamEvent};
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

/// Project declaration that makes a healing fixture verify with the compiler.
///
/// The harness carries no knowledge of a compiler, so a fixture that expects a
/// verification failure must declare its lifecycle explicitly. `cargo check`
/// reports the mismatched return types of the broken library, and `--quiet`
/// keeps the report free of build progress noise.
const VERIFY_DECLARATION: &str = "\
[models]
primary = [\"openrouter:mock/model\"]

[project.verify]
command = \"cargo\"
args = [\"check\", \"--quiet\"]
";

/// Writes the verification declaration into a healing fixture.
fn declare_verification(repo_root: &Path) {
    std::fs::write(repo_root.join("tauqe.toml"), VERIFY_DECLARATION)
        .expect("write the project verification declaration");
}

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

/// Returns the repository map body rendered into a recorded system message.
fn repo_map_block(content: &str) -> &str {
    let start = content
        .find("<repo_map>\n")
        .expect("the system prompt must render the repository map block")
        + "<repo_map>\n".len();
    let end = start
        + content[start..]
            .find("\n</repo_map>")
            .expect("the repository map block must be closed");
    &content[start..end]
}

/// Returns the head of a system message: everything rendered before the
/// context block.
///
/// The core contract and the frozen repository map live here, so this region
/// must stay byte-identical from one round of a turn to the next.
fn stable_system_prefix(content: &str) -> &str {
    match content.find("## Project Context") {
        Some(index) => &content[..index],
        None => content,
    }
}

/// Returns the rendered context block without its `<context>` delimiters.
fn rendered_context_body(content: &str) -> &str {
    let start = content
        .find("<context>\n")
        .expect("the system prompt must render the context block")
        + "<context>\n".len();
    let end = start
        + content[start..]
            .find("</context>")
            .expect("the context block must be closed");
    &content[start..end]
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
        intent: tauqe_protocol::TurnIntent::InteractiveRepl,
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
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
        intent: tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { edits, outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert_eq!(edits.len(), 1);
        }
        other => panic!("expected staged edits, got {other:?}"),
    }
    assert!(!output.assistant_text.contains("Patch Retry"));
}

#[tokio::test]
async fn answer_edit_loop_accepts_answer_without_edits() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
        intent: tauqe_protocol::TurnIntent::InteractiveRepl,
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
async fn answer_edit_loop_discovers_context_via_repo_search() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src dir");
    std::fs::write(
        repo_root.join("src/helper.rs"),
        "pub const ANSWER: i32 = 42;\n",
    )
    .expect("write helper fixture");
    commit_fixture(&repo_root, &["target.rs", "src/helper.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let search_discovery = format!(
        "Searching for answer constant.\n<repo_search_{MARKER} path=\"src\" regex=\"pub const ANSWER\" />"
    );
    let file_discovery = format!(
        "Found definition, requesting file.\n<context_request_{MARKER} path=\"src/helper.rs\" access=\"read_only\" />"
    );
    let edit = edit_block(
        "Use the helper constant",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider = MockLlmProvider::scripted([
        MockResponse::text(search_discovery),
        MockResponse::text(file_discovery),
        MockResponse::text(edit),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = TaskEnv {
            intent: tauqe_protocol::TurnIntent::InteractiveRepl,
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
        AnswerEditLoop::new("Find and use the answer constant")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(&output.result, ModelResult::Edit { edits, .. } if !edits.is_empty()));
    assert_eq!(provider.requests().len(), 3);
    let second_request = &provider.requests()[1];
    assert!(second_request.messages[1]
        .content
        .contains("<search_feedback path=\"src\" regex=\"pub const ANSWER\">"));
    assert!(second_request.messages[1]
        .content
        .contains("src/helper.rs: 1 match"));
    let third_request = &provider.requests()[2];
    assert!(third_request.messages[0]
        .content
        .contains("pub const ANSWER: i32 = 42;"));
}

#[tokio::test]
async fn answer_edit_loop_discovers_requested_context() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
            intent: tauqe_protocol::TurnIntent::InteractiveRepl,
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
async fn answer_edit_loop_discovers_the_directory_outline() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::create_dir_all(repo_root.join("src/nested")).expect("create nested directory");
    std::fs::write(
        repo_root.join("src/nested/helper.rs"),
        "pub const ANSWER: i32 = 42;\n",
    )
    .expect("write helper fixture");
    commit_fixture(&repo_root, &["target.rs", "src/nested/helper.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!("I need the repository layout.\n<repo_dirs_{MARKER} />");
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
    assert!(follow_up.messages[1]
        .content
        .contains("<repo_dirs_feedback>"));
    assert!(follow_up.messages[1].content.contains("src\n  nested"));
}

#[tokio::test]
async fn answer_edit_loop_discovers_a_targeted_repo_map() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/alpha.rs"), "pub fn alpha() {}\n")
        .expect("write alpha fixture");
    commit_fixture(&repo_root, &["target.rs", "src/alpha.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!("Outlining the subsystem.\n<repo_map_{MARKER} path=\"src\" />");
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
    assert!(follow_up.messages[1]
        .content
        .contains("<repomap_feedback path=\"src\">"));
    assert!(follow_up.messages[1].content.contains("pub fn alpha"));
}

#[tokio::test]
async fn structured_turn_discovers_a_targeted_repo_map() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/alpha.rs"), "pub fn alpha() {}\n")
        .expect("write alpha fixture");
    commit_fixture(&repo_root, &["target.rs", "src/alpha.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = serde_json::json!({
        "message": "Outlining the subsystem.",
        "changes": [],
        "context_requests": [],
        "repo_maps": ["src"],
    })
    .to_string();
    let edit = serde_json::json!({
        "message": "Use the helper constant.",
        "changes": [{
            "op": "replace",
            "path": "target.rs",
            "to": "",
            "old_text": "pub fn compute() -> i32 { 0 }",
            "new_text": "pub fn compute() -> i32 { 42 }",
            "content": ""
        }],
        "context_requests": [],
        "outcome": { "status": "success", "summary": "Constant applied", "message": "" },
    })
    .to_string();

    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::edits();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Use the helper constant")
            .execute(&mut env)
            .await
            .expect("structured turn executes")
    };

    assert!(matches!(&output.result, ModelResult::Edit { edits, .. } if !edits.is_empty()));
    assert_eq!(provider.requests().len(), 2);
    let follow_up = &provider.requests()[1];
    assert!(follow_up.messages[1]
        .content
        .contains("<repomap_feedback path=\"src\">"));
}

#[tokio::test]
async fn answer_edit_loop_heals_failed_patch_application() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
        intent: tauqe_protocol::TurnIntent::InteractiveRepl,
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
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
        intent: tauqe_protocol::TurnIntent::InteractiveRepl,
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
    declare_verification(&repo_root);
    commit_fixture(&repo_root, &["Cargo.toml", "src/lib.rs", "tauqe.toml"]);

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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(
                outcome.applied,
                "healing should apply and commit the repaired edits"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }

    let healed =
        std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read healed library");
    assert!(healed.contains("pub fn alpha() -> i32 { 1 }"));
    assert!(healed.contains("pub fn beta() -> i32 { 2 }"));
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn git_workflow_verifies_and_heals_when_verify_tag_omitted_by_model() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("Cargo.toml"), HEAL_MANIFEST).expect("write manifest");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/lib.rs"), HEAL_LIB).expect("write broken library");
    declare_verification(&repo_root);
    commit_fixture(&repo_root, &["Cargo.toml", "src/lib.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.clone());

    // First attempt: repair alpha ONLY, and DO NOT emit <verify>.
    // Without default verification, broken code would be committed! With auto-verify,
    // cargo check fails because beta is still broken, triggering healing.
    let fix_alpha_no_verify = edit_block(
        "Repair alpha return type",
        "src/lib.rs",
        "pub fn alpha() -> i32 { \"alpha\" }",
        "pub fn alpha() -> i32 { 1 }",
    );
    // Healing attempt: repair beta, also without <verify>.
    let fix_beta_no_verify = edit_block(
        "Repair beta return type",
        "src/lib.rs",
        "pub fn beta() -> i32 { \"beta\" }",
        "pub fn beta() -> i32 { 2 }",
    );
    let provider = MockLlmProvider::scripted([
        MockResponse::text(fix_alpha_no_verify),
        MockResponse::text(fix_beta_no_verify),
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
            "Fix return types in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(
                outcome.applied,
                "auto-verification should heal and apply edits"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }

    let healed =
        std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read healed library");
    assert!(healed.contains("pub fn alpha() -> i32 { 1 }"));
    assert!(healed.contains("pub fn beta() -> i32 { 2 }"));
    assert_eq!(
        provider.requests().len(),
        2,
        "must trigger verification healing pass despite missing verify tag"
    );
}

#[tokio::test]
async fn git_workflow_runs_no_compiler_without_a_verification_declaration() {
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

    // The repository carries a build manifest but declares no `[project.verify]`
    // command, so the harness must not infer a compiler from the manifest: the
    // change set stays unverified and the turn commits it without a healing run.
    let fix_alpha = edit_block(
        "Repair alpha only",
        "src/lib.rs",
        "pub fn alpha() -> i32 { \"alpha\" }",
        "pub fn alpha() -> i32 { 1 }",
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(fix_alpha)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol;
    let protocol = protocol.with_turn_marker(MARKER).unwrap();
    let (stream_tx, cancel_rx) = event_channel();

    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Fix alpha in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(outcome.error.is_none());
            assert!(
                outcome.applied,
                "an undeclared repository applies its edits without guessing a compiler"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }

    assert_eq!(
        provider.requests().len(),
        1,
        "an undeclared verification lifecycle must not trigger a healing round"
    );
}

#[tokio::test]
async fn git_workflow_heals_when_verification_response_has_text_only() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("Cargo.toml"), HEAL_MANIFEST).expect("write manifest");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/lib.rs"), HEAL_LIB).expect("write broken library");
    declare_verification(&repo_root);
    commit_fixture(&repo_root, &["Cargo.toml", "src/lib.rs", "tauqe.toml"]);

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
    // The first verification retry response gives only text explanation without any edits.
    let text_only_retry =
        "I see that beta also has a mismatched return type. Let me fix it.".to_string();
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
        MockResponse::text(fix_alpha),
        MockResponse::text(text_only_retry),
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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(
                outcome.applied,
                "healing should survive text-only retry and apply the repaired edits"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }

    let healed =
        std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read healed library");
    assert!(healed.contains("pub fn alpha() -> i32 { 1 }"));
    assert!(healed.contains("pub fn beta() -> i32 { 2 }"));
    assert_eq!(provider.requests().len(), 3);
    // The third request must contain the user prompt informing the model that edits were missing.
    assert!(provider.requests()[2].messages[1]
        .content
        .contains("did not include any code edits"));
}

#[tokio::test]
async fn scratchpad_from_discovery_reaches_verification_healing() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("Cargo.toml"), HEAL_MANIFEST).expect("write manifest");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/lib.rs"), HEAL_LIB).expect("write broken library");
    std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n")
        .expect("write helper fixture");
    declare_verification(&repo_root);
    commit_fixture(
        &repo_root,
        &["Cargo.toml", "src/lib.rs", "helper.rs", "tauqe.toml"],
    );

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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } if outcome.applied
    ));
    assert_eq!(provider.requests().len(), 3);
    let healing_request = &provider.requests()[2];
    assert!(healing_request.messages[0].content.contains("<scratchpad>"));
    assert!(healing_request.messages[0]
        .content
        .contains("Hypothesis: repair both return types"));
}

#[tokio::test]
async fn fsm_turn_respects_cancellation() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
        tauqe_protocol::TurnIntent::InteractiveRepl,
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
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
async fn scratchpad_persists_between_proposal_rounds() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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

fn event_channel_with_rx() -> (
    mpsc::Sender<StreamEvent>,
    mpsc::Receiver<StreamEvent>,
    watch::Receiver<bool>,
) {
    let (stream_tx, rx) = mpsc::channel(512);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    (stream_tx, rx, cancel_rx)
}

#[tokio::test]
async fn answer_discussing_protocol_tags_never_starts_edits() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .unwrap();
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
        if matches!(
            ev,
            StreamEvent::EditStarted | StreamEvent::EditFileStarted { .. }
        ) {
            saw_edit_started = true;
        }
    }
    assert!(
        !saw_edit_started,
        "Conversational text discussing tags should not emit edit events"
    );
}

#[tokio::test]
async fn terminal_error_outcome_blocks_without_applying_edits() {
    let (_dir, repo_root) = init_repo();
    let mut context = ContextManager::new(repo_root.clone());
    let mut history = HistoryManager::new(repo_root.clone());

    let provider = MockLlmProvider::scripted([MockResponse::text(format!(
        "I cannot proceed.\n<error_{MARKER} message=\"Missing dependencies\" />"
    ))]);
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
        ModelResult::Edit { outcome, .. } => {
            assert!(!outcome.applied);
            assert_eq!(outcome.error, Some("Missing dependencies".to_string()));
        }
        _ => panic!("Expected unapplied Edit result with error"),
    }
}

#[tokio::test]
async fn terminal_success_outcome_labels_applied_edits() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .unwrap();
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .unwrap();
    let mut history = HistoryManager::new(repo_root.clone());

    let edits = edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );

    let provider = MockLlmProvider::scripted([MockResponse::text(format!(
        "{edits}\n<success_{MARKER} summary=\"Operation completed successfully\" />"
    ))]);
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
        ModelResult::Edit { outcome, .. } => {
            assert!(outcome.applied);
            assert!(outcome.error.is_none());
        }
        _ => panic!("Expected applied Edit result"),
    }
}

#[tokio::test]
async fn fsm_turn_semantic_blocked_outcome_aborts_and_rolls_back() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        items: vec![
            foundation.clone(),
            blocked_step.clone(),
            dependent_step.clone(),
        ],
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

    // 4. Wave Horizon Planning replaces uncompleted horizon while preserving Done foundation
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
        PlanItem {
            id: "3".to_string(),
            title: "Integration Test".to_string(),
            status: PlanItemStatus::Todo,
            files: vec!["tests/integration.rs".to_string()],
            depends_on: vec!["2.2".to_string()],
            ..Default::default()
        },
    ];

    let recovered = tauqe_core::plan::storage::PlanStorage::replace_uncompleted_horizon(
        &repo_root,
        "recovery-plan",
        replacement,
        tauqe_protocol::ArchitectTier::new(
            tauqe_protocol::ModelRef::openrouter("test/architect"),
            None,
        ),
        Some("2".to_string()),
    )
    .expect("replace uncompleted horizon over blocked step");

    // The reprojected step is persisted with the horizon.
    assert_eq!(recovered.last_replanned_step.as_deref(), Some("2"));

    assert_eq!(recovered.items.len(), 4);
    assert_eq!(recovered.items[0].id, "1");
    assert_eq!(recovered.items[0].status, PlanItemStatus::Done);
    assert_eq!(recovered.items[1].id, "2.1");
    assert_eq!(recovered.items[2].id, "2.2");
    assert_eq!(recovered.items[3].id, "3");

    // Dependent tail step 3 depends on step "2.2"
    let remapped_dep = recovered.find_item("3").expect("find step 3");
    assert_eq!(remapped_dep.depends_on, vec!["2.2".to_string()]);

    // 5. Invariant #c-4 & #c-5: DAG readiness evaluation on the updated plan
    match recovered.evaluate_scope(None) {
        PlanScopeEvaluation::Ready(leaves) => {
            let ids: Vec<&str> = leaves.iter().map(|l| l.id.as_str()).collect();
            assert_eq!(ids, vec!["2.1", "2.2", "3"]);
        }
        other => panic!("Expected Ready, got {:?}", other),
    }

    match recovered.evaluate_scope(Some("3")) {
        PlanScopeEvaluation::BlockedByDependencies(deps) => {
            assert_eq!(deps.len(), 1);
            assert!(deps[0].contains("#3"));
        }
        other => panic!("Expected BlockedByDependencies on step 3, got {:?}", other),
    }

    // Advance 2.1 to Done
    let mut progressed = recovered.clone();
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

/// Streaming a Structured Output discussion response must keep the REPL clean:
/// only the human-readable `message` reaches the interface, while the JSON
/// scaffolding, control fields, and Markdown fences stay hidden.
#[tokio::test]
async fn discussion_turn_streams_clean_markdown_without_json_leakage() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    // Pre-satisfy the discussion's context request so discovery never re-runs
    // the proposal and the single scripted response is consumed exactly once.
    context
        .add_file("target.rs", ContextAccess::ReadOnly)
        .expect("add reference file to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let message = "Отличный план! Давайте уточним инварианты.";
    let discussion_json = serde_json::json!({
        "message": message,
        "plan_update": null,
        "context_requests": [
            { "path": "target.rs", "access": "read_only" }
        ],
        "user_language": "Russian"
    })
    .to_string();

    let provider = MockLlmProvider::scripted([MockResponse::text(discussion_json)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::discussion();

    let plan_prompt = "# Plan Discussion: [discussion-plan] Refine plan\n";
    let intent = tauqe_protocol::TurnIntent::PlanDiscussion;

    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, mut stream_rx, cancel_rx) = event_channel_with_rx();

    let outcome = tauqe_core::workflow::fsm::run_fsm_turn(
        plan_prompt,
        intent,
        &provider,
        &model,
        &mut context,
        &mut history,
        &protocol,
        &mut policy,
        "git",
        stream_tx,
        cancel_rx,
        &options,
    )
    .await
    .expect("discussion turn executes");

    let mut streamed = String::new();
    while let Ok(event) = stream_rx.try_recv() {
        if let StreamEvent::TextDelta(delta) = event {
            streamed.push_str(&delta);
        }
    }

    assert_eq!(
        streamed, message,
        "only the human-readable message must reach the REPL stream"
    );
    for forbidden in ['{', '}', '"'] {
        assert!(
            !streamed.contains(forbidden),
            "protocol JSON scaffolding must never leak into the REPL stream: {streamed:?}"
        );
    }
    for forbidden in [
        "message",
        "plan_update",
        "context_requests",
        "user_language",
    ] {
        assert!(
            !streamed.contains(forbidden),
            "control metadata must never leak into the REPL stream: {streamed:?}"
        );
    }

    assert!(matches!(outcome.result, ModelResult::Answer { .. }));
    assert_eq!(outcome.assistant_text.trim(), message);
}
/// A plan discussion renders the real context layers instead of a global
/// read-only view: an editable file stays editable, a reference file stays
/// read-only, and a file the discussion discovers as editable renders with the
/// access it requested.
#[tokio::test]
async fn plan_discussion_renders_the_real_context_access_layers() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::write(
        repo_root.join("reference.rs"),
        "pub const LIMIT: i32 = 7;\n",
    )
    .expect("write reference fixture");
    std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n")
        .expect("write helper fixture");
    commit_fixture(&repo_root, &["target.rs", "reference.rs", "helper.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    context
        .add_file("reference.rs", ContextAccess::ReadOnly)
        .expect("add reference to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = serde_json::json!({
        "message": "Запрашиваю константу.",
        "plan_update": null,
        "context_requests": [
            { "path": "helper.rs", "access": "editable" }
        ],
        "user_language": "Russian"
    })
    .to_string();
    let answer = serde_json::json!({
        "message": "Инварианты обсуждены.",
        "plan_update": null,
        "context_requests": [],
        "user_language": "Russian"
    })
    .to_string();

    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(answer)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::discussion();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let outcome = tauqe_core::workflow::fsm::run_fsm_turn(
        "# Plan Discussion: [access-plan] Refine plan\n",
        tauqe_protocol::TurnIntent::PlanDiscussion,
        &provider,
        &model,
        &mut context,
        &mut history,
        &protocol,
        &mut policy,
        "git",
        stream_tx,
        cancel_rx,
        &options,
    )
    .await
    .expect("plan discussion turn executes");

    assert!(matches!(outcome.result, ModelResult::Answer { .. }));
    assert_eq!(provider.requests().len(), 2);

    // A layout may distribute the context bodies through the transcript, so
    // every message of the request is inspected instead of the head alone.
    let rendered = |index: usize| -> String {
        let messages = &provider.requests()[index].messages;
        let mut joined = String::new();
        for message in messages {
            joined.push_str(&message.content);
            joined.push('\n');
        }
        joined
    };

    let first = rendered(0);
    assert!(first.contains("path=\"target.rs\" access=\"editable\""));
    assert!(first.contains("path=\"reference.rs\" access=\"read_only\""));

    let second = rendered(1);
    assert!(second.contains("path=\"target.rs\" access=\"editable\""));
    assert!(second.contains("path=\"reference.rs\" access=\"read_only\""));
    assert!(
        second.contains("path=\"helper.rs\" access=\"editable\""),
        "a discovered file must keep the access the discussion requested: {second}"
    );
}

/// A discussion turn must route the extracted `plan_update` into control
/// commands and persist it, activating the resulting plan, rather than
/// surfacing any structured metadata in the REPL.
#[tokio::test]
async fn discussion_turn_persists_plan_update_and_active_marker() {
    let (_dir, repo_root) = init_repo();

    let mut context = ContextManager::new(repo_root.clone());
    let mut history = HistoryManager::new(repo_root.clone());

    let message = "План сохранён и активирован.";
    let discussion_json = serde_json::json!({
        "message": message,
        "plan_update": {
            "action": "save",
            "id": "discussion-plan",
            "title": "Discussion Plan",
            "description": "Plan produced by a discussion turn",
            "concept": [
                {
                    "id": "c-1",
                    "title": "Discussion invariant",
                    "details": "Responses stream clean Markdown only",
                    "children": []
                }
            ],
            "items": [
                {
                    "id": "1",
                    "title": "First step",
                    "details": "Concrete work",
                    "status": "todo",
                    "files": [],
                    "readonly_files": [],
                    "concept_refs": ["c-1"],
                    "depends_on": [],
                    "children": []
                }
            ]
        },
        "context_requests": [],
        "user_language": "Russian"
    })
    .to_string();

    let provider = MockLlmProvider::scripted([MockResponse::text(discussion_json)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::discussion();
    let plan_prompt = "# Plan Discussion: [discussion-plan] Refine plan\n";

    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, mut stream_rx, cancel_rx) = event_channel_with_rx();

    let outcome = tauqe_core::workflow::fsm::run_fsm_turn(
        plan_prompt,
        tauqe_protocol::TurnIntent::PlanDiscussion,
        &provider,
        &model,
        &mut context,
        &mut history,
        &protocol,
        &mut policy,
        "git",
        stream_tx,
        cancel_rx,
        &options,
    )
    .await
    .expect("discussion turn executes");

    let mut streamed = String::new();
    while let Ok(event) = stream_rx.try_recv() {
        if let StreamEvent::TextDelta(delta) = event {
            streamed.push_str(&delta);
        }
    }
    assert_eq!(streamed, message);
    assert!(matches!(outcome.result, ModelResult::Answer { .. }));

    let plan = tauqe_core::plan::storage::PlanStorage::load_plan(&repo_root, "discussion-plan")
        .expect("load plan")
        .expect("plan_update must be persisted from a discussion turn");
    assert_eq!(plan.title, "Discussion Plan");
    assert_eq!(plan.concept.len(), 1);
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].concept_refs, vec!["c-1".to_string()]);

    let active = tauqe_core::plan::storage::PlanStorage::load_active_id(&repo_root)
        .expect("load active plan id");
    assert_eq!(active.as_deref(), Some("discussion-plan"));
}

/// A plan discussion is a distinct typed turn intent, and the structured
/// discussion protocol it selects hides code editing while keeping plan
/// authoring. The protocol choice itself is resolved by the server from that
/// intent, never from the prompt text.
#[test]
fn plan_discussion_intent_carries_the_dialogue_surface() {
    let intent = tauqe_protocol::TurnIntent::PlanDiscussion;
    assert!(intent.is_plan_discussion());
    assert!(!intent.is_interactive_repl());
    assert!(!intent.is_isolated_step());

    // The core-level contract the server's protocol resolver relies on: a
    // plan discussion carries the whole dialogue surface, so the structured
    // transport derives the edit mode and composes the registry schema,
    // where the plan payload is already declared.
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::edits();
    assert!(protocol.supports(tauqe_protocol::LlmCapability::FileEdits));
    assert!(protocol.supports(tauqe_protocol::LlmCapability::ToolchainVerify));

    let granted = protocol
        .capability_set()
        .with(tauqe_protocol::LlmCapability::PlanUpdate);
    let narrowed = intent.narrow_capabilities(&granted);
    assert!(narrowed.contains(tauqe_protocol::LlmCapability::PlanUpdate));
    assert!(narrowed.contains(tauqe_protocol::LlmCapability::PlanCreation));
    assert!(narrowed.contains(tauqe_protocol::LlmCapability::FileEdits));

    match protocol.response_format(&[], tauqe_protocol::PatchStrategy::SearchReplace) {
        Some(tauqe_core::providers::ResponseFormat::JsonSchema { json_schema, .. }) => {
            let schema = &json_schema.schema;
            assert!(schema["properties"].get("changes").is_some());
            assert!(schema["properties"].get("plan_update").is_some());
        }
        other => panic!("the discussion surface must compose the registry schema, got {other:?}"),
    }
}

/// A Structured Output turn must carry the model's scratchpad notes into the
/// next proposal round, exactly like the XML control tag.
#[tokio::test]
async fn structured_turn_persists_scratchpad_between_proposal_rounds() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n")
        .expect("write helper fixture");
    commit_fixture(&repo_root, &["target.rs", "helper.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = serde_json::json!({
        "message": "Recording working notes.",
        "changes": [],
        "context_requests": ["helper.rs"],
        "scratchpad": "Finding: helper.rs holds ANSWER",
    })
    .to_string();
    let edit = serde_json::json!({
        "message": "Use the helper constant.",
        "changes": [{
            "op": "replace",
            "path": "target.rs",
            "to": "",
            "old_text": "pub fn compute() -> i32 { 0 }",
            "new_text": "pub fn compute() -> i32 { 42 }",
            "content": ""
        }],
        "context_requests": [],
        "outcome": { "status": "success", "summary": "Constant applied", "message": "" },
    })
    .to_string();

    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::edits();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Use the helper constant")
            .execute(&mut env)
            .await
            .expect("structured turn executes")
    };

    assert!(matches!(&output.result, ModelResult::Edit { edits, .. } if !edits.is_empty()));
    assert_eq!(provider.requests().len(), 2);
    let follow_up = &provider.requests()[1];
    assert!(follow_up.messages[0].content.contains("<scratchpad>"));
    assert!(follow_up.messages[0]
        .content
        .contains("Finding: helper.rs holds ANSWER"));
}

/// A structured `outcome` reporting `error` must terminate the turn as an
/// unapplied edit carrying the diagnostic, exactly like the XML `<error>` tag.
#[tokio::test]
async fn structured_turn_terminates_on_terminal_error_outcome() {
    let (_dir, repo_root) = init_repo();
    let mut context = ContextManager::new(repo_root.clone());
    let mut history = HistoryManager::new(repo_root.clone());

    let response = serde_json::json!({
        "message": "I cannot proceed.",
        "changes": [],
        "context_requests": [],
        "outcome": { "status": "error", "summary": "", "message": "Missing dependency" },
    })
    .to_string();

    let provider = MockLlmProvider::scripted([MockResponse::text(response)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::edits();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Do something impossible")
            .execute(&mut env)
            .await
            .expect("structured turn executes")
    };

    match output.result {
        ModelResult::Edit { outcome, edits, .. } => {
            assert!(!outcome.applied);
            assert!(edits.is_empty());
            assert_eq!(outcome.error.as_deref(), Some("Missing dependency"));
        }
        other => panic!("expected a terminated turn, got {other:?}"),
    }
    assert_eq!(
        provider.requests().len(),
        1,
        "a terminal error outcome must not trigger retry loops"
    );
}

/// A structured `blocked` field must surface as a semantic blocker that aborts
/// the turn without retries, exactly like the XML `<blocked>` tag.
#[tokio::test]
async fn structured_turn_reports_blocked_outcome() {
    let (_dir, repo_root) = init_repo();
    let mut context = ContextManager::new(repo_root.clone());
    let mut history = HistoryManager::new(repo_root.clone());

    let response = serde_json::json!({
        "message": "Cannot proceed.",
        "changes": [],
        "context_requests": [],
        "blocked": {
            "is_blocked": true,
            "reason": "Missing table",
            "details": "users table absent"
        },
    })
    .to_string();

    let provider = MockLlmProvider::scripted([MockResponse::text(response)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::edits();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Refactor the persistence layer")
            .execute(&mut env)
            .await
            .expect("structured turn executes")
    };

    assert_eq!(
        output.blocked,
        Some("Missing table\n\nusers table absent".to_string())
    );
    assert_eq!(
        provider.remaining(),
        0,
        "a semantic blocker must not trigger retry loops"
    );
}

/// Runs one answer/edit turn over `target.rs` and reports its normalized result,
/// the number of provider requests, and the file content after the turn.
///
/// The helper is protocol-agnostic: it borrows the protocol as a trait object, so
/// the same scenario can be replayed through either wire format.
async fn run_parity_turn(
    protocol: &dyn PromptProtocol,
    response: String,
) -> (ModelResult, usize, String) {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let provider = MockLlmProvider::scripted([MockResponse::text(response)]);
    let model = ModelRef::openrouter("mock/model");
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::default();
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol,
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Change compute to return 42")
            .execute(&mut env)
            .await
            .expect("turn executes")
    };

    let content =
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture");
    (output.result, provider.requests().len(), content)
}

/// Both wire formats are architectural peers: one and the same bounded turn,
/// driven by the same loop and apply policy, must reach the same validated
/// outcome whether the model speaks marker-protected XML or Structured Output
/// JSON.
#[tokio::test]
async fn xml_and_structured_protocols_are_interchangeable_peers() {
    let xml_protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (xml_result, xml_requests, xml_content) = run_parity_turn(
        xml_protocol.as_ref(),
        edit_block(
            "Change compute to 42",
            "target.rs",
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ),
    )
    .await;

    let structured_protocol = tauqe_core::prompt::json::JsonPromptProtocol::edits();
    let structured_response = serde_json::json!({
        "message": "Change compute to 42",
        "changes": [{
            "op": "replace",
            "path": "target.rs",
            "to": "",
            "old_text": "pub fn compute() -> i32 { 0 }",
            "new_text": "pub fn compute() -> i32 { 42 }",
            "content": ""
        }],
        "context_requests": [],
        "outcome": { "status": "success", "summary": "Change compute to 42", "message": "" }
    })
    .to_string();
    let (structured_result, structured_requests, structured_content) =
        run_parity_turn(&structured_protocol, structured_response).await;

    // Neither format costs an extra proposal round.
    assert_eq!(xml_requests, 1);
    assert_eq!(structured_requests, 1);

    // Both turns apply a verified edit and leave byte-identical file content.
    assert!(
        matches!(xml_result, ModelResult::Edit { outcome, .. } if outcome.applied),
        "the XML turn must apply its staged edit"
    );
    assert!(
        matches!(structured_result, ModelResult::Edit { outcome, .. } if outcome.applied),
        "the structured turn must apply its staged edit"
    );
    assert_eq!(xml_content, "pub fn compute() -> i32 { 42 }\n");
    assert_eq!(structured_content, xml_content);
}

/// A discovery round must extend the cached prompt prefix instead of
/// reshaping it.
///
/// The head of the system message (the core contract and the frozen repository
/// map) is rendered byte-identically, the requested file extends
/// the context block in insertion order, and the discovery feedback appends to
/// the user message rather than being woven into the head.
#[tokio::test]
async fn discovery_round_extends_the_stable_prompt_prefix() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(provider.requests().len(), 2);

    let first_system = &provider.requests()[0].messages[0].content;
    let second_system = &provider.requests()[1].messages[0].content;

    // The head of the system message never moves, so the provider reuses its
    // cached prefix on the second round.
    assert_eq!(
        stable_system_prefix(second_system),
        stable_system_prefix(first_system),
        "the head of the system message must stay byte-identical across rounds"
    );

    // The repository map is frozen: it still lists the file the round added,
    // instead of being regenerated without it.
    assert_eq!(
        repo_map_block(second_system),
        repo_map_block(first_system),
        "the repository map block must be frozen for the whole turn"
    );
    assert!(repo_map_block(first_system).contains("helper.rs"));

    // The requested file extends the context block, so every earlier file keeps
    // its position and its tokens.
    let first_context = rendered_context_body(first_system);
    let second_context = rendered_context_body(second_system);
    assert!(
        second_context.starts_with(first_context),
        "discovery must append to the context block instead of reshaping it"
    );
    assert!(second_context.contains("path=\"helper.rs\" access=\"read_only\""));

    // Discovery feedback appends to the user message, never to the head.
    assert!(provider.requests()[1].messages[1]
        .content
        .starts_with(&provider.requests()[0].messages[1].content));
}

/// Multiple consecutive discovery rounds must preserve the static head and
/// monotonically grow the reusable prompt cache prefix as new files enter
/// context under `line_anchor` strategy.
#[tokio::test]
async fn multi_round_discovery_preserves_and_monotonically_grows_cached_prefix() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::write(repo_root.join("a.rs"), "pub const A: i32 = 1;\n").expect("write a");
    std::fs::write(repo_root.join("b.rs"), "pub const B: i32 = 2;\n").expect("write b");
    std::fs::write(repo_root.join("c.rs"), "pub const C: i32 = 3;\n").expect("write c");
    commit_fixture(&repo_root, &["target.rs", "a.rs", "b.rs", "c.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let req_a =
        format!("Need a.rs.\n<context_request_{MARKER} path=\"a.rs\" access=\"read_only\" />");
    let req_b =
        format!("Need b.rs.\n<context_request_{MARKER} path=\"b.rs\" access=\"read_only\" />");
    let req_c =
        format!("Need c.rs.\n<context_request_{MARKER} path=\"c.rs\" access=\"read_only\" />");
    let edit = edit_block(
        "Use constants",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );

    let provider = MockLlmProvider::scripted([
        MockResponse::text(req_a),
        MockResponse::text(req_b),
        MockResponse::text(req_c),
        MockResponse::text(edit),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let mut options = strategy_options("line_anchor");
    options.session_id = Some("test-session-cache-invariants".to_string());
    options.max_discovery_rounds = 5;
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
        AnswerEditLoop::new("Find constants and update target")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        4,
        "must execute exactly 3 discovery rounds followed by 1 proposal round"
    );

    // 1. Session ID is preserved across all rounds to maintain provider KV-cache routing
    for (i, req) in requests.iter().enumerate() {
        assert_eq!(
            req.session_id.as_deref(),
            Some("test-session-cache-invariants"),
            "round {i} must preserve the session_id"
        );
    }

    // 2. Static system head (contract, frozen repo map) is byte-identical
    //    across all rounds
    let baseline_head = stable_system_prefix(&requests[0].messages[0].content);
    let baseline_repo_map = repo_map_block(&requests[0].messages[0].content);
    for (i, req) in requests.iter().enumerate() {
        let head = stable_system_prefix(&req.messages[0].content);
        assert_eq!(
            head, baseline_head,
            "round {i} must have identical static system head"
        );
        let rmap = repo_map_block(&req.messages[0].content);
        assert_eq!(
            rmap, baseline_repo_map,
            "round {i} must preserve the frozen repo map"
        );
    }

    // 3. Rendered context body extends strictly at the tail on each discovery round
    for i in 0..requests.len() - 1 {
        let ctx_cur = rendered_context_body(&requests[i].messages[0].content);
        let ctx_next = rendered_context_body(&requests[i + 1].messages[0].content);
        assert!(
            ctx_next.starts_with(ctx_cur),
            "round {} context must be a strict prefix extension of round {}",
            i + 1,
            i
        );
        assert!(
            ctx_next.len() > ctx_cur.len(),
            "round {} context must be strictly larger than round {}",
            i + 1,
            i
        );
    }

    // 4. The divergence between two consecutive rounds is confined to the tail
    //    that follows the context block: the session history renders after the
    //    context so the divergence point of two consecutive turns falls as
    //    late as possible, and within the turn the whole warmed context stays
    //    inside the shared prefix, which grows with every discovery round.
    let mut prev_prefix_len = 0;
    for i in 1..requests.len() {
        let prev_sys = &requests[i - 1].messages[0].content;
        let cur_sys = &requests[i].messages[0].content;
        let shared_len = common_prefix_len(prev_sys, cur_sys);
        let context_close = prev_sys
            .find("</context>")
            .expect("the previous round must close its context block");

        assert!(
            shared_len >= context_close,
            "round {i} shared prefix must cover the whole previous context block (shared: {shared_len}, context closes at: {context_close})"
        );
        assert!(
            shared_len > prev_prefix_len,
            "cached prefix length must monotonically grow: round {i} shared {shared_len} <= prev {prev_prefix_len}"
        );
        prev_prefix_len = shared_len;
    }

    // 5. User prompt preserves base task prompt prefix across rounds
    let base_prompt = &requests[0].messages[1].content;
    for (i, req) in requests.iter().enumerate().skip(1) {
        assert!(
            req.messages[1].content.starts_with(base_prompt),
            "round {i} user prompt must preserve base prompt prefix"
        );
    }
}

/// The turn scratchpad is volatile: it appears as the last block of the system
/// message, behind the every-round-stable context and plan blocks, and never
/// shifts the cached prefix.
#[tokio::test]
async fn scratchpad_renders_in_the_volatile_tail() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
        .expect("marker-bound protocol");
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

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(provider.requests().len(), 2);

    let first_system = &provider.requests()[0].messages[0].content;
    let second_system = &provider.requests()[1].messages[0].content;

    assert_eq!(
        stable_system_prefix(second_system),
        stable_system_prefix(first_system),
        "carrying the scratchpad forward must not move the cached head"
    );
    assert!(!first_system.contains("## Turn Scratchpad"));

    let scratchpad = second_system
        .find("## Turn Scratchpad (Working Memory)")
        .expect("the scratchpad must reach the next round");
    let context_block = second_system
        .find("## Project Context")
        .expect("the context block must be rendered");
    assert!(
        scratchpad > context_block,
        "the scratchpad belongs in the volatile tail, after the context block"
    );
    assert!(second_system[scratchpad..].contains("Finding: helper.rs holds ANSWER"));
}

/// A patch retry reports its diagnostics in the user message and leaves the
/// system message untouched, so the retry round hits the cache instead of
/// paying for a full reprocessing.
#[tokio::test]
async fn patch_retry_appends_diagnostics_without_reshaping_the_system_prompt() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Change compute to return 42")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(provider.requests().len(), 2);

    assert_eq!(
        provider.requests()[1].messages[0].content,
        provider.requests()[0].messages[0].content,
        "a patch retry must not reshape the cached system prompt"
    );
    assert!(provider.requests()[1].messages[1]
        .content
        .contains("Failed files and errors:"));
    assert!(!provider.requests()[0].messages[1]
        .content
        .contains("Failed files and errors:"));
}

/// Manifest of a self-contained Cargo fixture used by the formatting tests.
const FORMAT_MANIFEST: &str = "\
[package]
name = \"tauqe-fsm-format-fixture\"
version = \"0.0.0\"
edition = \"2021\"

[workspace]
";

/// Broken fixture source whose mismatched return type the scripted turn repairs.
const FORMAT_LIB: &str = "pub fn compute() -> i32 { \"compute\" }\n";

/// Search text of the scripted repair.
const FORMAT_SEARCH: &str = "pub fn compute() -> i32 { \"compute\" }";

/// Replacement text of the scripted repair.
const FORMAT_REPLACE: &str = "pub fn compute() -> i32 { 7 }";

/// Builds the repository fixture shared by the formatting tests.
#[cfg(unix)]
fn format_fixture(formatter: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("Cargo.toml"), FORMAT_MANIFEST).expect("write manifest");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/lib.rs"), FORMAT_LIB).expect("write broken library");
    std::fs::write(repo_root.join("tauqe.toml"), formatter).expect("write formatter registry");
    commit_fixture(&repo_root, &["Cargo.toml", "src/lib.rs", "tauqe.toml"]);
    (_dir, repo_root)
}

/// Runs one verified repair turn over the formatting fixture.
#[cfg(unix)]
async fn run_format_turn(repo_root: &std::path::Path) -> (ModelResult, usize) {
    let mut context = ContextManager::new(repo_root.to_path_buf());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.to_path_buf());

    let repair = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Repair compute return type",
            "src/lib.rs",
            FORMAT_SEARCH,
            FORMAT_REPLACE
        ),
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(repair)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Repair the mismatched return type in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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

    (execution.result, provider.requests().len())
}

/// A failing in-memory formatter keeps the authored content, so the declared
/// verification gate still passes, the failure never re-prompts the model, and the
/// verified code reaches the single turn commit.
#[cfg(unix)]
#[tokio::test]
async fn git_workflow_discards_formatter_failure_and_commits_verified_code() {
    let declaration = format!(
        r#"
[models]
primary = ["openrouter:mock/model"]

[project.verify]
command = "grep"
args = ["-q", "{repair}", "src/lib.rs"]

[project.format]
command = "sh"
args = ["-c", "printf garbage; exit 1"]
"#,
        repair = FORMAT_REPLACE
    );
    let (_dir, repo_root) = format_fixture(&declaration);

    let (result, requests) = run_format_turn(&repo_root).await;

    match result {
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(
                outcome.applied,
                "verified code must be committed even when a formatter fails"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }
    assert_eq!(
        requests, 1,
        "a formatter failure must never re-prompt the model"
    );

    let restored = std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read library");
    assert!(
        restored.contains(FORMAT_REPLACE),
        "the verified repair must survive the formatting failure: {restored:?}"
    );
    assert!(
        !restored.contains("garbage"),
        "the failed formatter output must be discarded: {restored:?}"
    );
}

/// A successful in-memory formatter rewrites the staged content exactly once, and
/// both the verified repair and the rewritten text reach the single Git commit.
#[cfg(unix)]
#[tokio::test]
async fn git_workflow_commits_in_memory_formatter_output() {
    let declaration = format!(
        r#"
[models]
primary = ["openrouter:mock/model"]

[project.verify]
command = "grep"
args = ["-q", "{repair}", "src/lib.rs"]

[project.format]
command = "sh"
args = ["-c", "cat; echo '// formatted'"]
"#,
        repair = FORMAT_REPLACE
    );
    let (_dir, repo_root) = format_fixture(&declaration);

    let (result, requests) = run_format_turn(&repo_root).await;

    match result {
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(outcome.applied, "a formatted turn must still commit");
        }
        other => panic!("expected applied edits, got {other:?}"),
    }
    assert_eq!(requests, 1);

    let formatted = std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read library");
    assert!(
        formatted.contains(FORMAT_REPLACE),
        "the verified repair must be preserved by formatting: {formatted:?}"
    );
    assert!(
        formatted.trim_end().ends_with("// formatted"),
        "the formatter output must be committed: {formatted:?}"
    );
    assert_eq!(
        formatted.matches("// formatted").count(),
        1,
        "formatting must run once, on the staging contour: {formatted:?}"
    );
}

/// Discovery navigation is Git-only and exclusion-aware: a search and a listing
/// that address a service directory report nothing, so vendored or generated
/// files never reach the prompt.
#[tokio::test]
async fn discovery_ignores_excluded_paths() {
    let (_dir, repo_root) = init_repo();
    std::fs::create_dir_all(repo_root.join("node_modules/pkg")).expect("create node_modules dir");
    std::fs::write(
        repo_root.join("node_modules/pkg/index.js"),
        "module.exports = 42;\n",
    )
    .expect("write vendored module");
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    commit_fixture(&repo_root, &["node_modules/pkg/index.js", "target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!(
        "Inspecting the vendored tree.\n\
        <repo_search_{MARKER} path=\"node_modules\" regex=\"module\\.exports\" />\n\
        <dir_list_{MARKER} path=\"node_modules\" recursive=\"true\" />"
    );
    let edit = edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Change compute to return 42")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(provider.requests().len(), 2);

    let feedback = &provider.requests()[1].messages[1].content;
    assert!(feedback.contains("<search_feedback path=\"node_modules\""));
    assert!(
        feedback.contains("No matches found."),
        "excluded directories must never be searched: {feedback}"
    );
    assert!(feedback.contains("<dir_list_feedback path=\"node_modules\""));
    assert!(
        feedback.contains("No tracked files under `node_modules`"),
        "excluded directories must never be listed: {feedback}"
    );
    assert!(!feedback.contains("index.js"));
}

/// A shallow listing describes one folder, while a recursive listing of the same
/// prefix reports every tracked file underneath it.
#[tokio::test]
async fn dir_list_reports_shallow_children_then_recursive_files() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::create_dir_all(repo_root.join("src/nested")).expect("create nested directory");
    std::fs::write(repo_root.join("src/main.rs"), "pub fn main() {}\n").expect("write main");
    std::fs::write(
        repo_root.join("src/nested/module.rs"),
        "pub fn module() {}\n",
    )
    .expect("write module");
    commit_fixture(
        &repo_root,
        &["target.rs", "src/main.rs", "src/nested/module.rs"],
    );

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let shallow = format!("Listing the folder.\n<dir_list_{MARKER} path=\"src\" />");
    let recursive =
        format!("Listing the subtree.\n<dir_list_{MARKER} path=\"src\" recursive=\"true\" />");
    let edit = edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider = MockLlmProvider::scripted([
        MockResponse::text(shallow),
        MockResponse::text(recursive),
        MockResponse::text(edit),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Change compute to return 42")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(provider.requests().len(), 3, "each listing costs one round");

    let shallow_feedback = &provider.requests()[1].messages[1].content;
    assert!(shallow_feedback.contains("<dir_list_feedback path=\"src\" recursive=\"false\">"));
    assert!(shallow_feedback.contains("main.rs"));
    assert!(shallow_feedback.contains("nested/"));
    assert!(
        !shallow_feedback.contains("module.rs"),
        "a shallow listing must not enumerate nested files: {shallow_feedback}"
    );

    let recursive_feedback = &provider.requests()[2].messages[1].content;
    assert!(recursive_feedback.contains("<dir_list_feedback path=\"src\" recursive=\"true\">"));
    assert!(
        recursive_feedback.contains("nested/module.rs"),
        "a recursive listing must report nested files: {recursive_feedback}"
    );
}

/// A wide listing degrades into a capped summary and names the omitted entries
/// instead of flooding the context budget in silence.
#[tokio::test]
async fn dir_list_truncates_at_the_entry_cap() {
    let (_dir, repo_root) = init_repo();
    std::fs::create_dir_all(repo_root.join("many")).expect("create many directory");
    let mut staged: Vec<String> = Vec::new();
    for index in 0..105 {
        let path = format!("many/file_{index:03}.txt");
        std::fs::write(repo_root.join(&path), format!("entry {index}\n")).expect("write entry");
        staged.push(path);
    }
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    staged.push("target.rs".to_string());
    let paths: Vec<&str> = staged.iter().map(|path| path.as_str()).collect();
    commit_fixture(&repo_root, &paths);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery =
        format!("Listing the wide folder.\n<dir_list_{MARKER} path=\"many\" recursive=\"true\" />");
    let edit = edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Change compute to return 42")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));

    let feedback = &provider.requests()[1].messages[1].content;
    assert!(
        feedback.contains("file_099.txt"),
        "a listing names entries relative to the requested prefix: {feedback}"
    );
    assert!(
        !feedback.contains("file_100.txt"),
        "the listing must stop at the entry cap: {feedback}"
    );
    assert!(
        feedback.contains("[5 more entries omitted"),
        "truncation must be reported explicitly: {feedback}"
    );
}

/// The compact outline names the repository root files and marks every file the
/// pinned context layer anchored, so conventions are visible without searching.
#[tokio::test]
async fn repo_dirs_lists_root_files_and_marks_pinned_conventions() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("README.md"), "# Fixture\n").expect("write readme");
    std::fs::write(repo_root.join("LICENSE"), "Fixture license\n").expect("write license");
    std::fs::write(repo_root.join("Conventions.md"), "# Conventions\n").expect("write conventions");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/main.rs"), "pub fn main() {}\n").expect("write main");
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::write(
        repo_root.join("tauqe.toml"),
        "[models]\nprimary = [\"openrouter:mock/model\"]\n\n[project.verify]\ncommand = \"true\"\n\n[context]\npinned = [\"Conventions.md\"]\n",
    )
    .expect("write project configuration");
    commit_fixture(
        &repo_root,
        &[
            "README.md",
            "LICENSE",
            "Conventions.md",
            "src/main.rs",
            "target.rs",
            "tauqe.toml",
        ],
    );

    let mut context = ContextManager::new(repo_root.clone());
    assert_eq!(
        context.pinned_patterns(),
        &["Conventions.md".to_string()],
        "the pinned pattern must come from the project configuration"
    );
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!("Outlining the repository.\n<repo_dirs_{MARKER} />");
    let edit = edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Change compute to return 42")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));

    let feedback = &provider.requests()[1].messages[1].content;
    assert!(feedback.contains("<repo_dirs_feedback>"));
    assert!(
        feedback.contains("README.md"),
        "root files must be listed explicitly: {feedback}"
    );
    assert!(
        feedback.contains("LICENSE"),
        "root files must be listed explicitly: {feedback}"
    );
    assert!(
        feedback.contains("Conventions.md [pinned]"),
        "pinned files must be marked: {feedback}"
    );
    assert!(
        feedback.contains("\nsrc\n"),
        "the folder tree must name the subsystem folder: {feedback}"
    );
    assert!(
        !feedback.contains("main.rs"),
        "an unpinned nested file stays hidden behind its folder: {feedback}"
    );
}

/// The authoritative configuration chapter travels through a `doc_request`
/// round as volatile feedback, so the model answers configuration questions
/// from documentation instead of inferring an answer from partial source
/// memory, and the cached prefix stays untouched.
#[tokio::test]
async fn doc_request_config_returns_authoritative_configuration_documentation() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!(
        "Checking the authoritative configuration reference.\n\
        <doc_request_{MARKER} topic=\"config\" />"
    );
    let edit = edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Where is the context budget declared?")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(
        provider.requests().len(),
        2,
        "one documentation round must be enough"
    );

    let first_system = &provider.requests()[0].messages[0].content;
    let second_system = &provider.requests()[1].messages[0].content;
    let feedback = &provider.requests()[1].messages[1].content;

    // The chapter itself reaches the model as a documentation block.
    assert!(feedback.contains("<system_documentation>"));
    assert!(feedback.contains("Configuration and Project Preparation"));
    assert!(feedback.contains("files_token_budget"));
    assert!(feedback.contains("Why credentials are never a project setting"));

    // The documentation is volatile tail content: it never widens the cached
    // system prefix, and the head of that prefix stays byte-identical.
    const CHAPTER_ONLY: &str = "Why credentials are never a project setting";
    assert!(!first_system.contains(CHAPTER_ONLY));
    assert!(!second_system.contains(CHAPTER_ONLY));
    assert_eq!(
        stable_system_prefix(second_system),
        stable_system_prefix(first_system),
        "a documentation round must not reshape the cached prefix"
    );
}

/// The static anchor that makes `tauqe.toml` authoritative is rendered in the
/// head of the system message, before the context block, and it keeps its
/// exact bytes from one round of a turn to the next.
#[tokio::test]
async fn tauqe_toml_anchor_renders_in_the_stable_prefix_before_the_context_block() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(provider.requests().len(), 2);

    let first_system = &provider.requests()[0].messages[0].content;
    let second_system = &provider.requests()[1].messages[0].content;

    const ANCHOR: &str = "Environment behavior is declared in `tauqe.toml` at the repository root";
    for system in [first_system, second_system] {
        let anchor = system
            .find(ANCHOR)
            .expect("the system prompt must anchor the project configuration file");
        let context_block = system
            .find("## Project Context")
            .expect("the context block must be rendered");
        assert!(
            anchor < context_block,
            "the configuration anchor belongs to the static head"
        );
        assert!(
            system.contains("Self-Knowledge"),
            "the anchor must route configuration questions to the documentation request"
        );
    }

    assert_eq!(
        stable_system_prefix(second_system),
        stable_system_prefix(first_system),
        "the anchored head must stay byte-identical across rounds"
    );
    assert_eq!(
        repo_map_block(second_system),
        repo_map_block(first_system),
        "the frozen repository outline must keep the earlier bytes"
    );
}

/// A discovery finding is not a repository file, so it lands in the turn
/// journal instead of the context block and survives into the patch-retry
/// round: the model never re-learns what the turn already paid for.
#[tokio::test]
async fn discovery_findings_reach_the_patch_retry_round() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery =
        format!("Checking the authoritative git chapter.\n<doc_request_{MARKER} topic=\"git\" />");
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
    let provider = MockLlmProvider::scripted([
        MockResponse::text(discovery),
        MockResponse::text(broken),
        MockResponse::text(corrected),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Change compute to return 42")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(provider.requests().len(), 3);

    let retry = &provider.requests()[2].messages[1].content;
    assert!(retry.contains("<discovery_journal>"));
    assert!(retry.contains("<system_documentation>"));

    // The message keeps one fixed order: task, journal, phase diagnostics.
    let task_at = retry.find("Change compute to return 42").expect("task");
    let journal_at = retry.find("<discovery_journal>").expect("journal");
    let diagnostics_at = retry
        .find("Failed files and errors:")
        .expect("repair diagnostics");
    assert!(task_at < journal_at);
    assert!(journal_at < diagnostics_at);
}

/// Verification healing replays the whole turn journal, so a documentation
/// chapter requested during Discovery is still available when the compiler
/// rejects the first attempt.
#[tokio::test]
async fn discovery_findings_reach_verification_healing() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("Cargo.toml"), HEAL_MANIFEST).expect("write manifest");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/lib.rs"), HEAL_LIB).expect("write broken library");
    declare_verification(&repo_root);
    commit_fixture(&repo_root, &["Cargo.toml", "src/lib.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery =
        format!("Consulting the workflow chapter.\n<doc_request_{MARKER} topic=\"workflow\" />");
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
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Fix the mismatched return types in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } if outcome.applied
    ));
    assert_eq!(provider.requests().len(), 3);

    let healing = &provider.requests()[2].messages[1].content;
    assert!(healing.contains("<discovery_journal>"));
    assert_eq!(
        healing.matches("<system_documentation>").count(),
        1,
        "the journal must not duplicate an identical finding"
    );

    let journal_at = healing.find("<discovery_journal>").expect("journal");
    let diagnostics_at = healing
        .find("Please fix the errors")
        .expect("healing diagnostics");
    assert!(
        journal_at < diagnostics_at,
        "the compiler diagnostics must stay behind the journal"
    );
}

/// The journal is append-only and de-duplicated: re-requesting the same
/// documentation chapter never inflates the turn message.
#[tokio::test]
async fn repeated_discovery_findings_are_not_duplicated() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let first = format!("Reading the git chapter.\n<doc_request_{MARKER} topic=\"git\" />");
    let second = format!("Re-reading the git chapter.\n<doc_request_{MARKER} topic=\"git\" />");
    let edit = edit_block(
        "Change compute to 42",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider = MockLlmProvider::scripted([
        MockResponse::text(first),
        MockResponse::text(second),
        MockResponse::text(edit),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Change compute to return 42")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(provider.requests().len(), 3);

    let proposal = &provider.requests()[2].messages[1].content;
    assert_eq!(
        proposal.matches("<system_documentation>").count(),
        1,
        "an identical repeated finding must be recorded once: {proposal}"
    );
}

/// Writes the `tauqe.toml` declaration a project command test exercises.
#[cfg(unix)]
fn write_project_config(repo_root: &Path, declaration: &str) {
    std::fs::write(repo_root.join("tauqe.toml"), declaration)
        .expect("write the project declaration");
}

/// Returns the repository-relative paths touched by the commit at HEAD.
#[cfg(unix)]
fn commit_paths(repo_root: &Path) -> Vec<String> {
    tauqe_core::git::cmd::run_git(
        Some(repo_root),
        &["show", "--name-only", "--pretty=format:", "HEAD"],
    )
    .expect("read the committed paths")
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(str::to_string)
    .collect()
}

/// Counts the commits reachable from HEAD.
#[cfg(unix)]
fn commit_count(repo_root: &Path) -> usize {
    tauqe_core::git::cmd::run_git(Some(repo_root), &["rev-list", "--count", "HEAD"])
        .expect("count the commits")
        .trim()
        .parse()
        .expect("parse the commit count")
}

/// Returns the porcelain status lines of the working copy.
#[cfg(unix)]
fn worktree_status(repo_root: &Path) -> Vec<String> {
    tauqe_core::git::cmd::run_git(Some(repo_root), &["status", "--porcelain"])
        .expect("read the working copy status")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// In-memory formatting is a pure stream: the staged text reaches the declared
/// `[project.format]` command on its standard input and the rewritten text comes
/// back on its standard output, while the file on disk keeps its bytes.
///
/// This is the first phase of the project modification lifecycle, so it must
/// run before any byte reaches disk and before verification sees the change.
#[cfg(unix)]
#[tokio::test]
async fn in_memory_format_rewrites_staged_text_without_touching_disk() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("src.rs"), "pub fn value() -> i32 {0}\n")
        .expect("write source fixture");
    write_project_config(
        &repo_root,
        "[project.format]\ncommand = \"sh\"\nargs = ['-c', 'tr a-z A-Z']\n",
    );
    commit_fixture(&repo_root, &["src.rs", "tauqe.toml"]);
    let on_disk_before =
        std::fs::read_to_string(repo_root.join("src.rs")).expect("read the source fixture");

    let outcome = tauqe_core::toolchain::format_staged_content(
        &repo_root,
        "pub fn value() -> i32 {0}\n",
        None,
    )
    .await
    .expect("the repository declares an in-memory formatter");

    assert!(outcome.formatted, "error: {:?}", outcome.error);
    assert_eq!(outcome.content, "PUB FN VALUE() -> I32 {0}\n");
    assert_eq!(
        std::fs::read_to_string(repo_root.join("src.rs")).expect("read the source fixture"),
        on_disk_before,
        "in-memory formatting must never write the staged content to disk"
    );
}

/// The disk post-hook phase runs the declared `[project.post_hook]` command.
///
/// Hooks execute strictly after the patch reached disk and before verification,
/// so generated sources are present when the check suite runs.
#[cfg(unix)]
#[tokio::test]
async fn disk_post_hook_runs_the_declared_project_command() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("src.rs"), "pub fn value() -> i32 {0}\n")
        .expect("write source fixture");
    write_project_config(
        &repo_root,
        "[project.post_hook]\ncommand = \"sh\"\nargs = ['-c', 'printf \"generated\\n\" > generated.txt']\n",
    );
    commit_fixture(&repo_root, &["src.rs", "tauqe.toml"]);

    let response = tauqe_core::toolchain::run_post_hooks(&repo_root, None)
        .await
        .expect("the declared post-hook runs");

    assert!(response.success);
    assert_eq!(response.executed_hooks.len(), 1);
    assert_eq!(response.executed_hooks[0].name, "post_hook");
    assert!(response.executed_hooks[0].success);
    assert_eq!(
        std::fs::read_to_string(repo_root.join("generated.txt"))
            .expect("read the generated artifact"),
        "generated\n"
    );
}

/// Verification runs the declared `[project.verify]` command line directly and
/// reports typed diagnostics that render into the clean, deterministic report a
/// verification-healing round consumes.
///
/// The report carries the offending file, line, and message instead of a
/// terminal transcript, and it is free of ANSI escapes, so the healing round
/// reasons about the location rather than the compiler banner.
#[cfg(unix)]
#[tokio::test]
async fn project_verification_reports_structured_diagnostics_for_healing() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("src.rs"), "pub fn value() -> i32 {0}\n")
        .expect("write source fixture");
    write_project_config(
        &repo_root,
        concat!(
            "[models]\n",
            "primary = [\"openrouter:mock/model\"]\n",
            "\n",
            "[project.verify]\n",
            "command = \"sh\"\n",
            "args = ['-c', 'printf \"src/lib.rs:3:5: error: mismatched types\\n\" >&2; exit 1']\n",
        ),
    );
    commit_fixture(&repo_root, &["src.rs", "tauqe.toml"]);

    let (ok, results) =
        tauqe_core::toolchain::run_verification_pipeline(&repo_root, &[], None).await;

    assert!(!ok, "the failing verification must report a failure");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].diagnostics.len(), 1);
    let report = tauqe_protocol::format_diagnostics(&results[0].diagnostics);
    assert_eq!(report, "src/lib.rs:3:5: error: mismatched types");
    assert!(
        !report.contains('\u{1b}'),
        "the healing input must stay free of terminal escapes"
    );
}

/// The declared disk post-hook runs strictly after the patch reaches disk and
/// before verification, so the artifacts it produces join the turn's change set
/// and land in the single atomic commit next to the edited source.
#[cfg(unix)]
#[tokio::test]
async fn post_hook_artifacts_join_the_single_turn_commit() {
    let (_dir, repo_root) = init_repo();
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(
        repo_root.join("src/lib.rs"),
        "pub fn alpha() -> i32 { \"alpha\" }\n",
    )
    .expect("write broken library");
    write_project_config(
        &repo_root,
        concat!(
            "[models]\n",
            "primary = [\"openrouter:mock/model\"]\n",
            "\n",
            "[project.verify]\n",
            "command = \"grep\"\n",
            "args = [\"-q\", \"pub fn alpha() -> i32 { 1 }\", \"src/lib.rs\"]\n",
            "\n",
            "[project.post_hook]\n",
            "command = \"sh\"\n",
            "args = ['-c', 'printf \"generated\\n\" > generated.txt']\n",
        ),
    );
    commit_fixture(&repo_root, &["src/lib.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let repair = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Repair alpha return type",
            "src/lib.rs",
            "pub fn alpha() -> i32 { \"alpha\" }",
            "pub fn alpha() -> i32 { 1 }",
        ),
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(repair)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let baseline = commit_count(&repo_root);
    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Repair the mismatched return type in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(outcome.applied, "the verified change set must be committed");
        }
        other => panic!("expected applied edits, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 1);

    let mut paths = commit_paths(&repo_root);
    paths.sort();
    assert_eq!(
        paths,
        vec!["generated.txt".to_string(), "src/lib.rs".to_string()],
        "the post-hook artifact must land in the same commit as the edit"
    );
    assert_eq!(
        commit_count(&repo_root),
        baseline + 1,
        "the turn must produce exactly one commit"
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("generated.txt")).expect("read the artifact"),
        "generated\n"
    );

    let status = worktree_status(&repo_root);
    assert!(
        status.is_empty(),
        "the turn must not leave unaccounted files behind: {status:?}"
    );
}

/// An artifact produced during the initial attempt stays in the change set when
/// verification fails and the turn heals, so the repaired source and the latest
/// artifact content converge into one squashed commit.
#[cfg(unix)]
#[tokio::test]
async fn post_hook_artifacts_survive_healing_into_one_commit() {
    let (_dir, repo_root) = init_repo();
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/lib.rs"), HEAL_LIB).expect("write broken library");
    write_project_config(
        &repo_root,
        concat!(
            "[models]\n",
            "primary = [\"openrouter:mock/model\"]\n",
            "\n",
            "[project.verify]\n",
            "command = \"grep\"\n",
            "args = [\"-q\", \"pub fn beta() -> i32 { 2 }\", \"src/lib.rs\"]\n",
            "\n",
            "[project.post_hook]\n",
            "command = \"sh\"\n",
            "args = ['-c', 'printf \"run\\n\" >> generated.txt']\n",
        ),
    );
    commit_fixture(&repo_root, &["src/lib.rs", "tauqe.toml"]);

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
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let baseline = commit_count(&repo_root);
    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Fix the mismatched return types in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(
                outcome.applied,
                "healing must apply and commit the repaired change set"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 2);

    let mut paths = commit_paths(&repo_root);
    paths.sort();
    assert_eq!(
        paths,
        vec!["generated.txt".to_string(), "src/lib.rs".to_string()],
        "the artifact of the healed turn must stay in the single commit"
    );
    assert_eq!(
        commit_count(&repo_root),
        baseline + 1,
        "healing must still produce exactly one squashed commit"
    );
    assert_eq!(
        tauqe_core::git::cmd::run_git(Some(&repo_root), &["show", "HEAD:generated.txt"])
            .expect("read the committed artifact"),
        "run\nrun\n",
        "the artifact rewritten by the healing round must be committed"
    );

    let healed =
        std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read healed library");
    assert!(healed.contains("pub fn alpha() -> i32 { 1 }"));
    assert!(healed.contains("pub fn beta() -> i32 { 2 }"));

    let status = worktree_status(&repo_root);
    assert!(
        status.is_empty(),
        "the turn must not leave unaccounted files behind: {status:?}"
    );
}

/// A verification command that refreshes a companion artifact while it checks
/// the tree leaves that path inside the turn's single atomic commit, and the
/// working copy is clean afterwards.
///
/// A real project check behaves exactly like this: it regenerates a lock file or
/// a snapshot while it verifies, and the artifact it produced must be committed
/// with the edit instead of surviving as untracked residue.
#[cfg(unix)]
#[tokio::test]
async fn verification_generated_lock_file_joins_the_single_turn_commit() {
    let (_dir, repo_root) = init_repo();
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(
        repo_root.join("src/lib.rs"),
        "pub fn compute() -> i32 { \"compute\" }\n",
    )
    .expect("write broken library");
    write_project_config(
        &repo_root,
        concat!(
            "[models]\n",
            "primary = [\"openrouter:mock/model\"]\n",
            "\n",
            "[project.verify]\n",
            "command = \"sh\"\n",
            "args = ['-c', 'echo lockfile > Cargo.lock; grep -q \"pub fn compute() -> i32 { 7 }\" src/lib.rs']\n",
        ),
    );
    commit_fixture(&repo_root, &["src/lib.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let repair = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Repair compute return type",
            "src/lib.rs",
            "pub fn compute() -> i32 { \"compute\" }",
            "pub fn compute() -> i32 { 7 }",
        ),
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(repair)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let baseline = commit_count(&repo_root);
    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Repair the mismatched return type in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(outcome.applied, "the verified change set must be committed");
            let mut changed = outcome.changed_files.clone();
            changed.sort();
            assert_eq!(
                changed,
                vec!["Cargo.lock".to_string(), "src/lib.rs".to_string()],
                "the artifact produced by verification must join the change set"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }
    assert_eq!(
        provider.requests().len(),
        1,
        "the declared verification must pass on the first attempt"
    );

    let mut paths = commit_paths(&repo_root);
    paths.sort();
    assert_eq!(
        paths,
        vec!["Cargo.lock".to_string(), "src/lib.rs".to_string()],
        "the verification artifact must land in the same commit as the edit"
    );
    assert_eq!(
        commit_count(&repo_root),
        baseline + 1,
        "the turn must produce exactly one commit"
    );
    assert_eq!(
        tauqe_core::git::cmd::run_git(Some(&repo_root), &["show", "HEAD:Cargo.lock"])
            .expect("read the committed lock file"),
        "lockfile\n"
    );
    let status = worktree_status(&repo_root);
    assert!(
        status.is_empty(),
        "the turn must not leave unaccounted files behind: {status:?}"
    );
}

/// `tauqe.toml` declaration of the `probe` diagnostic command shared by the
/// project-command tests.
///
/// The command reports one line on its standard output and one on its standard
/// error, so a test observes that both streams reach the model together with
/// the exit status. It edits nothing, which keeps it a pure diagnostic step.
#[cfg(unix)]
const PROBE_DECLARATION: &str = "\
[models]
primary = [\"openrouter:mock/model\"]

[project.verify]
command = \"true\"

[project.commands.probe]
command = \"sh\"
args = ['-c', 'printf \"probe stdout\\n\"; printf \"probe stderr\\n\" >&2']
";

/// Creates the repository fixture shared by the project-command tests.
///
/// The fixture carries one editable target, one reference file the model may
/// request, and the declared `probe` diagnostic command.
#[cfg(unix)]
fn diagnostic_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n")
        .expect("write helper fixture");
    write_project_config(&repo_root, PROBE_DECLARATION);
    commit_fixture(&repo_root, &["target.rs", "helper.rs", "tauqe.toml"]);
    (_dir, repo_root)
}

/// A diagnostic project command runs strictly inside the bounded turn: its exit
/// status, standard output, and standard error land in the turn journal instead
/// of ending the turn, the next round widens the context, and the final round
/// commits the edit through the Git transaction.
#[cfg(unix)]
#[tokio::test]
async fn diagnostic_command_feedback_reaches_the_next_rounds_and_is_committed() {
    let (_dir, repo_root) = diagnostic_fixture();

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let probe =
        format!("Inspecting the project diagnostics.\n<custom_tool_{MARKER} name=\"probe\" />");
    let discovery = format!(
        "Requesting the constant definition.\n<context_request_{MARKER} path=\"helper.rs\" access=\"read_only\" />"
    );
    let edit = edit_block(
        "Use the helper constant",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider = MockLlmProvider::scripted([
        MockResponse::text(probe),
        MockResponse::text(discovery),
        MockResponse::text(edit),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let baseline = commit_count(&repo_root);
    let mut policy = GitTransactionPolicy::new();
    let options = WorkflowOptions {
        max_discovery_rounds: 4,
        ..WorkflowOptions::default()
    };
    let exec = SingleExecution::new(options);
    let execution = exec
        .execute(
            &mut policy,
            "Use the helper constant",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(outcome.applied, "the verified change set must be committed");
            assert_eq!(outcome.changed_files, vec!["target.rs".to_string()]);
            assert!(
                outcome.commit_hash.is_some(),
                "the turn must commit its change set"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }

    // One call per round: the tool round, the widened-context round, and the
    // proposal that finally carries the edit.
    assert_eq!(provider.requests().len(), 3);

    // The command answer rides in the journal, appended behind the task.
    let after_probe = &provider.requests()[1].messages[1].content;
    let task_at = after_probe
        .find("Use the helper constant")
        .expect("task statement");
    let journal_at = after_probe
        .find("<discovery_journal>")
        .expect("journal tail");
    assert!(task_at < journal_at);
    assert!(after_probe.contains("<project_command_feedback name=\"probe\" success=\"true\">"));
    assert!(after_probe.contains("stdout:"));
    assert!(after_probe.contains("probe stdout"));
    assert!(after_probe.contains("stderr:"));
    assert!(after_probe.contains("probe stderr"));

    // The widened context and the journal both survive into the final round.
    let final_request = &provider.requests()[2];
    assert!(final_request.messages[0]
        .content
        .contains("pub const ANSWER: i32 = 42;"));
    assert!(final_request.messages[1]
        .content
        .contains("<project_command_feedback name=\"probe\""));

    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "pub fn compute() -> i32 { 42 }\n"
    );
    assert_eq!(
        commit_count(&repo_root),
        baseline + 1,
        "one bounded turn produces exactly one commit"
    );
    assert_eq!(commit_paths(&repo_root), vec!["target.rs".to_string()]);
    assert!(
        worktree_status(&repo_root).is_empty(),
        "the turn must not leave unaccounted files behind"
    );
}

/// Diagnostic execution and context discovery draw from one shared round
/// budget: a second invocation after the budget is spent terminates the turn
/// deterministically instead of widening it without limit.
#[cfg(unix)]
#[tokio::test]
async fn diagnostic_rounds_consume_the_shared_investigation_budget() {
    let (_dir, repo_root) = diagnostic_fixture();

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let first = format!("Running the diagnostics.\n<custom_tool_{MARKER} name=\"probe\" />");
    let second = format!("Running the diagnostics again.\n<custom_tool_{MARKER} name=\"probe\" />");
    let provider =
        MockLlmProvider::scripted([MockResponse::text(first), MockResponse::text(second)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let mut policy = GitTransactionPolicy::new();
    let options = WorkflowOptions {
        max_discovery_rounds: 1,
        ..WorkflowOptions::default()
    };
    let exec = SingleExecution::new(options);
    let execution = exec
        .execute(
            &mut policy,
            "Run the project diagnostics",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, edits, .. } => {
            assert!(!outcome.applied);
            assert!(edits.is_empty());
            let error = outcome.error.unwrap_or_default();
            assert!(
                error.contains("shared investigation budget"),
                "unexpected error: {error}"
            );
            assert!(error.contains("(1 rounds)"), "unexpected error: {error}");
        }
        other => panic!("expected the exhausted budget to fail the turn, got {other:?}"),
    }

    assert_eq!(provider.requests().len(), 2);
    // The first invocation ran, and its report reached the round that asked
    // for the second one.
    assert!(provider.requests()[1].messages[1]
        .content
        .contains("<project_command_feedback name=\"probe\" success=\"true\">"));
    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "pub fn compute() -> i32 { 0 }\n",
        "an exhausted budget must never touch the working tree"
    );
}

/// An empty reply after a diagnostic round does not lose the command output:
/// the corrective re-ask is composed from the task, the accumulated journal,
/// and the reminder, in that order.
#[cfg(unix)]
#[tokio::test]
async fn empty_reply_after_a_diagnostic_round_keeps_the_command_feedback() {
    let (_dir, repo_root) = diagnostic_fixture();

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let probe =
        format!("Inspecting the project diagnostics.\n<custom_tool_{MARKER} name=\"probe\" />");
    let edit = edit_block(
        "Use the helper constant",
        "target.rs",
        "pub fn compute() -> i32 { 0 }",
        "pub fn compute() -> i32 { 42 }",
    );
    let provider = MockLlmProvider::scripted([
        MockResponse::text(probe),
        MockResponse::text(""),
        MockResponse::text(edit),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions {
        max_discovery_rounds: 4,
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
        AnswerEditLoop::new("Use the helper constant")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(provider.requests().len(), 3);

    // Task first, journal next, corrective reminder last.
    let corrective = &provider.requests()[2].messages[1].content;
    let task_at = corrective
        .find("Use the helper constant")
        .expect("task statement");
    let journal_at = corrective
        .find("<project_command_feedback name=\"probe\"")
        .expect("command feedback");
    let reminder_at = corrective
        .find("Your previous reply was empty")
        .expect("corrective reminder");
    assert!(task_at < journal_at);
    assert!(journal_at < reminder_at);
}

/// Returns the context layer that currently owns `path`.
fn context_layer(context: &ContextManager, path: &str) -> Option<tauqe_protocol::ContextLayer> {
    context
        .get_state()
        .items
        .into_iter()
        .find(|item| item.path == path)
        .map(|item| item.layer)
}

/// Collects the paths currently held by the developer-owned user layer.
fn user_layer_paths(context: &ContextManager) -> Vec<String> {
    context
        .get_state()
        .items
        .into_iter()
        .filter(|item| item.layer == tauqe_protocol::ContextLayer::User)
        .map(|item| item.path)
        .collect()
}

/// A conversational turn ends without touching the accumulated research: the
/// file discovery gathered keeps the transient auto layer, and the
/// developer-owned file keeps the user layer.
#[tokio::test]
async fn auto_context_survives_a_text_answer_turn() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
    let answer = "The helper module declares ANSWER as 42, so the task needs no code change.";
    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(answer)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Explain the helper constant")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(output.result, ModelResult::Answer { .. }));
    assert_eq!(provider.requests().len(), 2);

    // A text answer closes the turn without discarding the research it paid
    // for: the discovered file keeps the auto layer it was added to.
    assert_eq!(
        context_layer(&context, "helper.rs"),
        Some(tauqe_protocol::ContextLayer::Auto)
    );
    assert_eq!(context.auto_files(), vec!["helper.rs".to_string()]);

    // The developer-owned file keeps the user layer it was added to.
    assert_eq!(
        context_layer(&context, "target.rs"),
        Some(tauqe_protocol::ContextLayer::User)
    );
}

/// A turn whose change set never matches leaves the accumulated research in
/// place: the auto layer survives the patch-retry failure untouched and the
/// working tree stays clean.
#[tokio::test]
async fn auto_context_survives_an_unapplied_patch_failure() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
    let broken = edit_block(
        "Broken attempt",
        "target.rs",
        "this text does not exist in the file",
        "irrelevant replacement",
    );
    let broken_retry = edit_block(
        "Broken retry",
        "target.rs",
        "this text still does not exist in the file",
        "irrelevant replacement",
    );
    let provider = MockLlmProvider::scripted([
        MockResponse::text(discovery),
        MockResponse::text(broken),
        MockResponse::text(broken_retry),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions {
        max_retries: 1,
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
        AnswerEditLoop::new("Use the helper constant")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    match &output.result {
        ModelResult::Edit { outcome, edits, .. } => {
            assert!(!outcome.applied, "an unmatched change set must not apply");
            assert!(edits.is_empty());
            assert!(outcome
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Failed to apply edits after 1 retries"));
        }
        other => panic!("expected a failed change set, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 3);

    assert_eq!(
        context_layer(&context, "helper.rs"),
        Some(tauqe_protocol::ContextLayer::Auto)
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "pub fn compute() -> i32 { 0 }\n"
    );
}

/// A verification failure rolls the working tree back but keeps the context:
/// the files discovery gathered stay available for the next attempt.
#[cfg(unix)]
#[tokio::test]
async fn auto_context_survives_a_failed_verification() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n")
        .expect("write helper fixture");
    std::fs::write(
        repo_root.join("tauqe.toml"),
        "[models]\nprimary = [\"openrouter:mock/model\"]\n\n[project.verify]\ncommand = \"sh\"\nargs = ['-c', 'exit 1']\n",
    )
    .expect("write the project verification declaration");
    commit_fixture(&repo_root, &["target.rs", "helper.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let discovery = format!(
        "I need the constant definitions.\n<context_request_{MARKER} path=\"helper.rs\" access=\"read_only\" />"
    );
    let edit = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Use the helper constant",
            "target.rs",
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ),
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let mut policy = GitTransactionPolicy::new();
    let options = WorkflowOptions {
        max_retries: 0,
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
        AnswerEditLoop::new("Use the helper constant")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    match &output.result {
        ModelResult::Edit { outcome, .. } => {
            assert!(!outcome.applied, "a failed verification must not commit");
            assert!(outcome
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Code verification failed"));
        }
        other => panic!("expected a failed verification, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 2);

    // The rolled-back working tree is restored, and the research survives.
    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "pub fn compute() -> i32 { 0 }\n"
    );
    assert_eq!(
        context_layer(&context, "helper.rs"),
        Some(tauqe_protocol::ContextLayer::Auto)
    );
}

/// A committed change set clears the transient auto layer while the
/// developer-owned user layer keeps its files.
#[tokio::test]
async fn successful_commit_clears_the_auto_layer() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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

    assert!(matches!(
        &output.result,
        ModelResult::Edit { outcome, .. } if outcome.applied
    ));
    assert_eq!(provider.requests().len(), 2);

    // A successful change set is the only event that shrinks the context.
    assert!(
        !context.contains("helper.rs").unwrap_or(true),
        "the committed turn must drop the transient auto layer"
    );
    assert!(context.auto_files().is_empty());
    assert_eq!(user_layer_paths(&context), vec!["target.rs".to_string()]);
}

/// Editing a discovered file never promotes it into the user layer: after the
/// turn commits, the user layer still holds exactly what the developer added.
#[tokio::test]
async fn a_turn_never_widens_the_user_layer() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
        "I need to change the constant.\n<context_request_{MARKER} path=\"helper.rs\" access=\"editable\" />"
    );
    let edit = edit_block(
        "Raise the constant",
        "helper.rs",
        "pub const ANSWER: i32 = 42;",
        "pub const ANSWER: i32 = 43;",
    );
    let provider =
        MockLlmProvider::scripted([MockResponse::text(discovery), MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
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
        AnswerEditLoop::new("Raise the helper constant")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { outcome, .. } if outcome.applied
    ));
    assert_eq!(
        std::fs::read_to_string(repo_root.join("helper.rs")).expect("read helper fixture"),
        "pub const ANSWER: i32 = 43;\n"
    );

    // The edited file was discovered during the turn, so it belongs to the
    // transient auto layer and never to the developer-owned user layer.
    assert_eq!(user_layer_paths(&context), vec!["target.rs".to_string()]);
    assert!(context.auto_files().is_empty());
}

/// A post-hook that corrupts `tauqe.toml` invalidates the environment the turn
/// runs in: phase zero of the verification pipeline rejects the change set, the
/// turn fails, and the Git transaction restores the pre-turn working tree.
#[cfg(unix)]
#[tokio::test]
async fn post_hook_corrupting_tauqe_toml_fails_verification_and_rolls_back() {
    let (_dir, repo_root) = init_repo();
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(
        repo_root.join("src/lib.rs"),
        "pub fn compute() -> i32 { \"compute\" }\n",
    )
    .expect("write broken library");

    // The declared hook leaves the project without a valid environment, so the
    // engine must refuse to verify the change set it produced.
    let declaration = r#"
[models]
primary = ["openrouter:mock/model"]

[project.verify]
command = "grep"
args = ["-q", "pub fn compute() -> i32 { 7 }", "src/lib.rs"]

[project.post_hook]
command = "sh"
args = ['-c', 'printf "[models]\nprimary = []\n" > tauqe.toml']
"#;
    std::fs::write(repo_root.join("tauqe.toml"), declaration)
        .expect("write the project declaration");
    commit_fixture(&repo_root, &["src/lib.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let repair = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Repair compute return type",
            "src/lib.rs",
            "pub fn compute() -> i32 { \"compute\" }",
            "pub fn compute() -> i32 { 7 }",
        ),
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(repair)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let mut policy = GitTransactionPolicy::new();
    let options = WorkflowOptions {
        max_retries: 0,
        ..WorkflowOptions::default()
    };
    let exec = SingleExecution::new(options);
    let execution = exec
        .execute(
            &mut policy,
            "Repair the mismatched return type in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(
                !outcome.applied,
                "a corrupted environment must never be committed"
            );
            let error = outcome.error.unwrap_or_default();
            assert!(
                error.contains("tauqe.toml"),
                "the failure must name the corrupted environment file: {error}"
            );
        }
        other => panic!("expected a failed verification, got {other:?}"),
    }

    assert_eq!(
        std::fs::read_to_string(repo_root.join("tauqe.toml")).expect("read the declaration"),
        declaration,
        "the rollback must restore the pre-turn declaration"
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read the library"),
        "pub fn compute() -> i32 { \"compute\" }\n",
        "the rollback must restore the pre-turn source"
    );
}

/// A post-hook that keeps `tauqe.toml` valid preserves the harness environment,
/// so phase zero passes and the turn commits the hook's rewrite of the
/// declaration together with the repaired source.
#[cfg(unix)]
#[tokio::test]
async fn post_hook_preserving_a_valid_tauqe_toml_commits_the_change_set() {
    let (_dir, repo_root) = init_repo();
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(
        repo_root.join("src/lib.rs"),
        "pub fn compute() -> i32 { \"compute\" }\n",
    )
    .expect("write broken library");

    // The hook rewrites the declaration without changing its meaning, so the
    // environment stays sound and the verification gate must not refuse it.
    let declaration = r##"
[models]
primary = ["openrouter:mock/model"]

[project.verify]
command = "grep"
args = ["-q", "pub fn compute() -> i32 { 7 }", "src/lib.rs"]

[project.post_hook]
command = "sh"
args = ['-c', 'printf "# self-preserved\n" >> tauqe.toml']
"##;
    std::fs::write(repo_root.join("tauqe.toml"), declaration)
        .expect("write the project declaration");
    commit_fixture(&repo_root, &["src/lib.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let repair = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Repair compute return type",
            "src/lib.rs",
            "pub fn compute() -> i32 { \"compute\" }",
            "pub fn compute() -> i32 { 7 }",
        ),
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(repair)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let baseline = commit_count(&repo_root);
    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Repair the mismatched return type in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(
                outcome.applied,
                "a valid environment must let the change set commit"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }
    assert_eq!(
        provider.requests().len(),
        1,
        "a sound declaration must verify on the first attempt"
    );

    let preserved = std::fs::read_to_string(repo_root.join("tauqe.toml"))
        .expect("read the rewritten declaration");
    assert!(
        preserved.starts_with(declaration),
        "the preserved declaration must keep its meaning: {preserved:?}"
    );
    assert!(
        preserved.ends_with("# self-preserved\n"),
        "the hook's rewrite must reach the committed tree: {preserved:?}"
    );

    let mut paths = commit_paths(&repo_root);
    paths.sort();
    assert_eq!(
        paths,
        vec!["src/lib.rs".to_string(), "tauqe.toml".to_string()],
        "the valid rewrite of the declaration joins the single turn commit"
    );
    assert_eq!(
        commit_count(&repo_root),
        baseline + 1,
        "the turn must produce exactly one commit"
    );
    assert!(
        worktree_status(&repo_root).is_empty(),
        "the turn must not leave unaccounted files behind"
    );
}

/// Builds the workflow options a project declares through `[develop]
/// patch_strategy`, pinning the semantic prompt layout.
///
/// The options are decoded from a declaration exactly as the daemon decodes
/// `tauqe.toml`, so a test observes the same single source of truth the
/// project configuration provides. The declaration also pins the semantic
/// block order: the line-anchor and cache-invariant tests read the rendered
/// context block out of the system message and measure the divergence of
/// consecutive heads, so they state the layout they analyze instead of
/// inheriting the intent-selected default of an interactive turn.
fn strategy_options(strategy: &str) -> WorkflowOptions {
    let declaration =
        format!("[develop]\npatch_strategy = \"{strategy}\"\nprompt_layout = \"semantic\"\n");
    let config: tauqe_core::config::AppConfig =
        toml::from_str(&declaration).expect("parse the patch strategy declaration");
    WorkflowOptions::from_app_config(&config)
}
/// Builds one anchored replacement of an inclusive line range.
///
/// Both bounds are copied from the anchors the context block rendered for the
/// same line indices, so the proposal addresses the lines exactly as the model
/// sees them.
fn anchored_change(path: &str, start: u16, end: u16, content: &str) -> serde_json::Value {
    serde_json::json!({
        "op": "replace",
        "path": path,
        "start": tauqe_core::edits::anchor::encode_anchor(path, start),
        "end": tauqe_core::edits::anchor::encode_anchor(path, end),
        "content": content,
    })
}

/// The declared `line_anchor` strategy reaches both halves of the turn: the
/// context block renders the opaque anchor of every line, and the change set
/// names its ranges through those anchors.
///
/// Disjoint ranges are applied from the bottom of the file upwards, so a lower
/// replacement that grows the file never shifts the lines a higher range already
/// addressed.
#[tokio::test]
async fn line_anchor_turn_replaces_disjoint_ranges_bottom_to_top() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "alpha\nbeta\ngamma\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    // The lower range absorbs one line into three, while the higher range still
    // addresses the line index of the original snapshot.
    let response = serde_json::json!({
        "message": "Rewrite the middle line and the tail.",
        "changes": [
            anchored_change("target.rs", 1, 1, "beta one\nbeta two\nbeta three\n"),
            anchored_change("target.rs", 2, 2, "GAMMA\n"),
        ],
        "context_requests": [],
    })
    .to_string();

    let provider = MockLlmProvider::scripted([MockResponse::text(response)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::edits();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = strategy_options("line_anchor");
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Rewrite the middle line and the tail")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { outcome, .. } if outcome.applied
    ));
    assert_eq!(
        provider.requests().len(),
        1,
        "one anchored change set must converge on the first attempt"
    );

    // The declaration is the single source of truth for both halves: the very
    // key that stages the ranges is what rendered their anchors into the prompt.
    let system = &provider.requests()[0].messages[0].content;
    let first = tauqe_core::edits::anchor::encode_anchor("target.rs", 0);
    let second = tauqe_core::edits::anchor::encode_anchor("target.rs", 1);
    assert!(system.contains(&format!("{first} | alpha")));
    assert!(system.contains(&format!("{second} | beta")));
    assert!(!system.contains("\nalpha\nbeta\n"));

    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "alpha\nbeta one\nbeta two\nbeta three\nGAMMA\n"
    );
}

/// Two anchored ranges that share a line would make the result depend on the
/// order of application, so the whole change set is rejected before a byte
/// moves.
///
/// The retry receives the diagnostic, and the turn fails only once the retry
/// budget is spent, which keeps the failure inside the bounded turn instead of
/// reaching the working tree.
#[tokio::test]
async fn overlapping_anchor_ranges_are_rejected_before_any_byte_moves() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "alpha\nbeta\ngamma\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    // The first range already claims the line the second one re-addresses.
    let response = serde_json::json!({
        "message": "Rewrite the overlapping block.",
        "changes": [
            anchored_change("target.rs", 1, 2, "BLOCK\n"),
            anchored_change("target.rs", 2, 2, "TAIL\n"),
        ],
        "context_requests": [],
    })
    .to_string();

    let provider = MockLlmProvider::scripted([
        MockResponse::text(response.clone()),
        MockResponse::text(response),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::edits();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions {
        max_retries: 1,
        ..strategy_options("line_anchor")
    };
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Rewrite the overlapping block")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    match &output.result {
        ModelResult::Edit { outcome, edits, .. } => {
            assert!(!outcome.applied, "an overlapping change set must not apply");
            assert!(edits.is_empty());
            let error = outcome.error.clone().unwrap_or_default();
            assert!(
                error.contains("overlaps the range"),
                "the diagnostic must name the collision: {error}"
            );
            assert!(
                error.contains("disjoint line ranges"),
                "the diagnostic must state the invariant: {error}"
            );
        }
        other => panic!("expected a rejected change set, got {other:?}"),
    }
    assert_eq!(
        provider.requests().len(),
        2,
        "the retry budget must be spent on the rejected change set"
    );
    let retry = &provider.requests()[1].messages[1].content;
    assert!(retry.contains("Failed files and errors:"));
    assert!(retry.contains("target.rs"));

    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "alpha\nbeta\ngamma\n",
        "a rejected change set must never reach the working tree"
    );
}

/// An anchor outside the codec alphabet names no line at all, so the change set
/// is rejected with a precise diagnostic that the patch retry hands back to the
/// model.
///
/// The corrected proposal addresses the very same line through the anchor the
/// context block rendered, and the turn then converges normally.
#[tokio::test]
async fn invalid_anchor_token_triggers_a_patch_retry_that_repairs_the_range() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("target.rs"), "alpha\nbeta\ngamma\n")
        .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    // `0` is deliberately absent from the anchor alphabet, so this token cannot
    // have been copied from the context block.
    let broken = serde_json::json!({
        "message": "Rewrite the middle line.",
        "changes": [{
            "op": "replace",
            "path": "target.rs",
            "start": "kR70",
            "end": "kR70",
            "content": "BETA\n",
        }],
        "context_requests": [],
    })
    .to_string();
    let corrected = serde_json::json!({
        "message": "Rewrite the middle line.",
        "changes": [anchored_change("target.rs", 1, 1, "BETA\n")],
        "context_requests": [],
    })
    .to_string();

    let provider =
        MockLlmProvider::scripted([MockResponse::text(broken), MockResponse::text(corrected)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::json::JsonPromptProtocol::edits();
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = strategy_options("line_anchor");
    let (stream_tx, cancel_rx) = event_channel();

    let output = {
        let mut env = build_env(
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            stream_tx,
            cancel_rx,
            &options,
        );
        AnswerEditLoop::new("Rewrite the middle line")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { outcome, .. } if outcome.applied
    ));
    assert!(
        output.assistant_text.contains("Patch Retry"),
        "an undecodable anchor must run the patch retry loop: {:?}",
        output.assistant_text
    );
    assert_eq!(
        provider.requests().len(),
        2,
        "the retry must be spent on the malformed range"
    );
    let retry = &provider.requests()[1].messages[1].content;
    assert!(retry.contains("Failed files and errors:"));
    assert!(retry.contains("target.rs"));

    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "alpha\nBETA\ngamma\n"
    );
}

/// Builds the workflow options a project declares through `[develop]
/// prompt_layout`.
///
/// The options are decoded from a declaration exactly as the daemon decodes
/// `tauqe.toml`, so a test observes the same single source of truth the project
/// configuration provides: one key decides the block order of every prompt the
/// turn assembles.
fn layout_options(layout: &str) -> WorkflowOptions {
    let declaration = format!("[develop]\nprompt_layout = \"{layout}\"\n");
    let config: tauqe_core::config::AppConfig =
        toml::from_str(&declaration).expect("parse the prompt layout declaration");
    WorkflowOptions::from_app_config(&config)
}

/// The layout policy a project declares reaches the prompt the FSM assembles:
/// the same bounded turn composes the semantic block order or the chronological
/// transcript exactly as declared, and a configuration without a declaration
/// follows the turn intent.
#[tokio::test]
async fn declared_prompt_layout_reaches_the_prompt_assembly() {
    let default_config_options =
        WorkflowOptions::from_app_config(&tauqe_core::config::AppConfig::default());
    for (case, options, chronological) in [
        ("declared semantic", layout_options("semantic"), false),
        (
            "declared chronological",
            layout_options("chronological"),
            true,
        ),
        ("intent default", default_config_options, true),
    ] {
        let (_dir, repo_root) = init_repo();
        std::fs::write(
            repo_root.join("target.rs"),
            "pub fn compute() -> i32 { 0 }\n",
        )
        .expect("write target fixture");
        commit_fixture(&repo_root, &["target.rs"]);

        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let edit = edit_block(
            "Change compute to 42",
            "target.rs",
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        );
        let provider = MockLlmProvider::scripted([MockResponse::text(edit)]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
            .with_turn_marker(MARKER)
            .expect("marker-bound protocol");
        let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
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
            AnswerEditLoop::new("Change compute to return 42")
                .execute(&mut env)
                .await
                .expect("answer/edit loop executes")
        };

        assert!(
            matches!(
                &output.result,
                ModelResult::Edit { outcome, .. } if outcome.applied
            ),
            "the {case} turn must apply its edit"
        );
        let requests = provider.requests();
        assert_eq!(
            requests.len(),
            1,
            "the {case} turn must converge in a single proposal round"
        );
        let messages = &requests[0].messages;
        let system = &messages[0].content;

        assert!(
            system.contains("## Core System Contract"),
            "the {case} head must render the system contract"
        );
        if chronological {
            assert!(
                !system.contains("## Project Context"),
                "the {case} head must not carry the context block"
            );
            assert!(
                !system.contains("{ 0 }"),
                "the {case} head must not carry the file body"
            );
            assert!(
                messages.len() > 2,
                "the {case} transcript must interleave the context body"
            );
            let body_in_transcript = messages.iter().skip(1).any(|message| {
                message.content.contains("path=\"target.rs\"") && message.content.contains("{ 0 }")
            });
            assert!(
                body_in_transcript,
                "the {case} transcript must carry the file body"
            );
            assert!(
                messages
                    .iter()
                    .any(|message| message.content.contains("<context_manifest>")),
                "the {case} tail must render the context manifest"
            );
        } else {
            assert!(
                system.contains("## Project Context"),
                "the {case} system message must carry the context block"
            );
            assert!(
                system.contains("{ 0 }"),
                "the {case} system message must carry the file body"
            );
            assert_eq!(
                messages.len(),
                2,
                "the semantic policy must compose system and user messages only"
            );
        }
        let last = messages
            .last()
            .map(|message| message.content.as_str())
            .unwrap_or_default();
        assert!(
            last.ends_with("Change compute to return 42"),
            "the {case} sequence must close with the developer message: {last:?}"
        );
    }
}

/// A plan discussion resolves its prompt layout exactly as the interactive REPL
/// does: with the default configuration it builds the chronological transcript,
/// so the head every dialogue shares stays byte-stable while the context bodies
/// travel through the transcript instead of the head.
#[tokio::test]
async fn plan_discussion_turn_assembles_the_chronological_transcript() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let provider = MockLlmProvider::scripted([MockResponse::text(
        "The invariants are settled; no edits are required.",
    )]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    let options = WorkflowOptions::from_app_config(&tauqe_core::config::AppConfig::default());
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
        env.intent = tauqe_protocol::TurnIntent::PlanDiscussion;
        AnswerEditLoop::new("Refine the plan invariants")
            .execute(&mut env)
            .await
            .expect("plan discussion turn executes")
    };

    assert!(matches!(output.result, ModelResult::Answer { .. }));
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);

    let messages = &requests[0].messages;
    let head = &messages[0].content;
    assert!(
        !head.contains("## Project Context"),
        "the shared head must not carry the context block"
    );
    assert!(
        !head.contains("{ 0 }"),
        "the shared head must not carry a file body"
    );
    assert!(
        messages
            .iter()
            .skip(1)
            .any(|message| message.content.contains("path=\"target.rs\"")
                && message.content.contains("{ 0 }")),
        "the transcript must carry the context body"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.content.contains("<context_manifest>")),
        "the tail must render the context manifest"
    );
    assert_eq!(
        messages.last().map(|message| message.content.as_str()),
        Some("Refine the plan invariants")
    );
}

/// Returns the length in bytes of the common prefix of two rendered strings.
///
/// The helper measures how much of an earlier prompt a later round reproduced
/// byte for byte: a turn that only appends material keeps the shared prefix
/// growing monotonically instead of reshaping what it already sent.
fn common_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

/// Returns the tail user message of a chronological transcript.
///
/// The transcript closes with the developer task message, so the message in
/// front of it is the volatile tail that carries the regime delta, the active
/// plan, the context manifest, and the turn scratchpad.
fn tail_message(messages: &[ChatMessage]) -> &str {
    messages
        .len()
        .checked_sub(2)
        .and_then(|index| messages.get(index))
        .map(|message| message.content.as_str())
        .unwrap_or_default()
}

/// Returns the developer task message that closes a transcript.
fn task_message(messages: &[ChatMessage]) -> &str {
    messages
        .last()
        .map(|message| message.content.as_str())
        .unwrap_or_default()
}

/// Returns the regime delta block that leads the tail message.
///
/// The block opens the tail message and ends where the first volatile tail
/// block begins, so two rounds compare the delta alone instead of the whole
/// tail. A transcript without a leading delta reports an empty block.
fn regime_delta(tail: &str) -> &str {
    const HEADER: &str = "## Regime Delta\n";
    if !tail.starts_with(HEADER) {
        return "";
    }
    let rest = &tail[HEADER.len()..];
    let end = [
        "## Active Task Plan Context",
        "## Project Context Manifest",
        "## Turn Scratchpad (Working Memory)",
    ]
    .iter()
    .filter_map(|heading| rest.find(heading))
    .min()
    .unwrap_or(rest.len());
    &tail[..HEADER.len() + end]
}

/// The regime delta leads the tail of every dialogue round: it stays strictly in
/// front of the context manifest and the turn scratchpad, it survives discovery
/// and a patch retry byte for byte, and the round diagnostics land behind it in
/// the developer task message.
#[tokio::test]
async fn dialogue_tail_leads_with_the_regime_delta_across_rounds() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
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
    let provider = MockLlmProvider::scripted([
        MockResponse::text(discovery),
        MockResponse::text(broken),
        MockResponse::text(corrected),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let mut policy = tauqe_core::workflow::policy::DirectApplyPolicy::new();
    // Единая голова диалога требует транскрипта: при semantic-layout дельта
    // режима сливается с сообщением задачи и хвостового сообщения не существует.
    // Тест заявляет макет, который анализирует, вместо неявного дефолта.
    let options = layout_options("chronological");
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
        AnswerEditLoop::new("Change compute to return 42")
            .execute(&mut env)
            .await
            .expect("answer/edit loop executes")
    };

    assert!(matches!(
        &output.result,
        ModelResult::Edit { edits, .. } if !edits.is_empty()
    ));
    assert_eq!(
        provider.requests().len(),
        3,
        "discovery, the patch retry, and the corrected proposal cost one round each"
    );

    let requests = provider.requests();
    let rounds: Vec<(&str, &str)> = requests
        .iter()
        .map(|request| {
            (
                tail_message(&request.messages),
                task_message(&request.messages),
            )
        })
        .collect();

    let first_delta = regime_delta(rounds[0].0);
    assert!(
        !first_delta.is_empty(),
        "every dialogue round states the regime delta: {}",
        rounds[0].0
    );
    for (index, (tail, task)) in rounds.iter().enumerate() {
        assert!(
            tail.starts_with("## Regime Delta"),
            "round {index} tail must open with the delta: {tail}"
        );
        assert_eq!(
            regime_delta(tail),
            first_delta,
            "round {index} must reproduce the delta byte for byte"
        );
        assert_eq!(
            tail.matches("## Regime Delta").count(),
            1,
            "round {index} must carry exactly one delta block"
        );
        assert!(
            common_prefix_len(rounds[0].0, tail) >= first_delta.len(),
            "round {index} shared prefix must cover the whole delta"
        );
        assert!(
            !task.contains("## Regime Delta"),
            "the delta belongs in the tail, never in the task message"
        );
    }

    // The tail keeps one fixed order behind the delta: the context manifest,
    // then the turn scratchpad.
    let tail = rounds[2].0;
    let delta_end = regime_delta(tail).len();
    let manifest_at = tail
        .find("<context_manifest>")
        .expect("the tail renders the context manifest");
    let scratchpad_at = tail
        .find("## Turn Scratchpad (Working Memory)")
        .expect("the scratchpad discovery gathered survives the turn");
    assert!(delta_end <= manifest_at, "the delta must lead the manifest");
    assert!(
        manifest_at < scratchpad_at,
        "the manifest must precede the scratchpad"
    );

    // Round diagnostics append to the task message, behind the turn journal.
    assert!(!rounds[0].1.contains("Failed files and errors:"));
    // The broken attempt costs one round and the retry that carries its
    // diagnostics costs the next one, so the repair prompt is the third.
    let before_retry = rounds[1].1;
    assert!(
        !before_retry.contains("Failed files and errors:"),
        "the retry diagnostics belong to the retry round: {before_retry}"
    );
    let retry = rounds[2].1;
    assert!(
        retry.contains("Failed files and errors:"),
        "the patch retry must report its diagnostics: {retry}"
    );
    assert!(
        !rounds[2].0.contains("Failed files and errors:"),
        "diagnostics never reshape the tail"
    );
}

/// Verification healing repeats the same turn, so the regime delta that leads
/// the tail message must survive every additional round byte for byte while the
/// compiler diagnostics travel in the task message.
#[tokio::test]
async fn verification_healing_reproduces_the_regime_delta_byte_for_byte() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(repo_root.join("Cargo.toml"), HEAL_MANIFEST).expect("write manifest");
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(repo_root.join("src/lib.rs"), HEAL_LIB).expect("write broken library");
    declare_verification(&repo_root);
    commit_fixture(&repo_root, &["Cargo.toml", "src/lib.rs", "tauqe.toml"]);

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
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let mut policy = GitTransactionPolicy::new();
    // Healing повторяет тот же оборот: макет обязан быть тем же транскриптом,
    // иначе байт-стабильность дельты проверялась бы на другом порядке блоков.
    let exec = SingleExecution::new(layout_options("chronological"));
    let execution = exec
        .execute(
            &mut policy,
            "Fix the mismatched return types in src/lib.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        ModelResult::Edit { outcome, .. } if outcome.applied
    ));
    assert_eq!(
        provider.requests().len(),
        2,
        "the first attempt must fail verification and heal"
    );

    let requests = provider.requests();
    let rounds: Vec<(&str, &str)> = requests
        .iter()
        .map(|request| {
            (
                tail_message(&request.messages),
                task_message(&request.messages),
            )
        })
        .collect();

    let first_delta = regime_delta(rounds[0].0);
    assert!(
        !first_delta.is_empty(),
        "the healing round repeats the same regime delta: {}",
        rounds[0].0
    );
    for (index, (tail, task)) in rounds.iter().enumerate() {
        assert!(
            tail.starts_with("## Regime Delta"),
            "round {index} tail must open with the delta: {tail}"
        );
        assert_eq!(
            regime_delta(tail),
            first_delta,
            "round {index} must reproduce the delta byte for byte"
        );
        assert_eq!(tail.matches("## Regime Delta").count(), 1);
        assert!(common_prefix_len(rounds[0].0, tail) >= first_delta.len());
        assert!(!task.contains("## Regime Delta"));
    }
    assert!(
        rounds[1].1.contains("Please fix the errors"),
        "the healing diagnostics stay behind the delta: {}",
        rounds[1].1
    );
}

/// A plan discussion is an editing dialogue: a change it proposes travels the
/// full transactional path — in-memory staging, verified toolchain run, and
/// exactly one atomic commit — while the intent keeps authorizing plan
/// mutations through the discussion plan mode.
#[cfg(unix)]
#[tokio::test]
async fn plan_discussion_edit_runs_the_transactional_path() {
    let (_dir, repo_root) = init_repo();
    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(
        repo_root.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 0 }\n",
    )
    .expect("write library");
    write_project_config(
        &repo_root,
        concat!(
            "[models]\n",
            "primary = [\"openrouter:mock/model\"]\n",
            "\n",
            "[project.verify]\n",
            "command = \"grep\"\n",
            "args = [\"-q\", \"pub fn alpha() -> i32 { 1 }\", \"src/lib.rs\"]\n",
        ),
    );
    commit_fixture(&repo_root, &["src/lib.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add library to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let repair = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Raise the alpha constant",
            "src/lib.rs",
            "pub fn alpha() -> i32 { 0 }",
            "pub fn alpha() -> i32 { 1 }",
        ),
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(repair)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let baseline = commit_count(&repo_root);
    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Raise the alpha constant",
            tauqe_protocol::TurnIntent::PlanDiscussion,
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("plan discussion turn executes");

    match execution.result {
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(
                outcome.applied,
                "a discussion edit must pass staging, verification, and the git transaction"
            );
            assert_eq!(outcome.changed_files, vec!["src/lib.rs".to_string()]);
            assert!(
                outcome.commit_hash.is_some(),
                "the change set must be committed"
            );
        }
        other => panic!("expected applied edits, got {other:?}"),
    }
    assert_eq!(
        provider.requests().len(),
        1,
        "the declared verification must pass on the first attempt"
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read library"),
        "pub fn alpha() -> i32 { 1 }\n"
    );
    assert_eq!(
        commit_paths(&repo_root),
        vec!["src/lib.rs".to_string()],
        "the discussion edit must be the only committed path"
    );
    assert_eq!(
        commit_count(&repo_root),
        baseline + 1,
        "one bounded turn produces exactly one commit"
    );
    let status = worktree_status(&repo_root);
    assert!(
        status.is_empty(),
        "the turn must not leave unaccounted files behind: {status:?}"
    );
}

/// A failed verification inside a plan discussion rolls the working tree back to
/// its pre-turn checkpoint, so a discussion edit never leaves a half-applied
/// change behind.
#[cfg(unix)]
#[tokio::test]
async fn plan_discussion_edit_rolls_back_a_failed_verification() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    write_project_config(
        &repo_root,
        concat!(
            "[models]\n",
            "primary = [\"openrouter:mock/model\"]\n",
            "\n",
            "[project.verify]\n",
            "command = \"sh\"\n",
            "args = ['-c', 'exit 1']\n",
        ),
    );
    commit_fixture(&repo_root, &["target.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let edit = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Change compute to 42",
            "target.rs",
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ),
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(edit)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let baseline = commit_count(&repo_root);
    let mut policy = GitTransactionPolicy::new();
    let options = WorkflowOptions {
        max_retries: 0,
        ..WorkflowOptions::default()
    };
    let exec = SingleExecution::new(options);
    let execution = exec
        .execute(
            &mut policy,
            "Change compute to return 42",
            tauqe_protocol::TurnIntent::PlanDiscussion,
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("plan discussion turn executes");

    match execution.result {
        ModelResult::Edit { outcome, .. } => {
            assert!(!outcome.applied, "a failed verification must not commit");
            let error = outcome.error.unwrap_or_default();
            assert!(
                error.contains("Code verification failed"),
                "unexpected error: {error}"
            );
        }
        other => panic!("expected a failed verification, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "pub fn compute() -> i32 { 0 }\n",
        "the rollback must restore the pre-turn source"
    );
    assert_eq!(
        commit_count(&repo_root),
        baseline,
        "a rolled-back turn produces no commit"
    );
    let status = worktree_status(&repo_root);
    assert!(
        status.is_empty(),
        "the rollback must leave a clean working tree: {status:?}"
    );
}

/// A semantic block inside a plan discussion ends the turn before any byte
/// reaches disk, exactly like an interactive block.
#[tokio::test]
async fn plan_discussion_blocked_outcome_rolls_back_its_working_tree() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    commit_fixture(&repo_root, &["target.rs"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    let blocked_response = format!(
        "Cannot revise the plan invariant.\n\
        <blocked_{MARKER} reason=\"Plan invariant conflicts with the approved design\" />"
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(blocked_response)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    let (stream_tx, cancel_rx) = event_channel();

    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Refine the plan invariant",
            tauqe_protocol::TurnIntent::PlanDiscussion,
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("plan discussion turn executes");

    assert_eq!(
        execution.blocked,
        Some("Plan invariant conflicts with the approved design".to_string())
    );
    assert_eq!(
        provider.remaining(),
        0,
        "a blocked discussion must not trigger retry loops"
    );
    assert_eq!(
        std::fs::read_to_string(repo_root.join("target.rs")).expect("read target fixture"),
        "pub fn compute() -> i32 { 0 }\n",
        "the working tree must remain clean after a semantic block"
    );
}

/// A review dialogue runs as an interactive turn, so an edit it proposes travels
/// the same transactional path as any other dialogue edit: staging, verification,
/// and exactly one atomic commit.
#[cfg(unix)]
#[tokio::test]
async fn review_dialogue_edit_runs_the_transactional_path() {
    let (_dir, repo_root) = init_repo();
    std::fs::write(
        repo_root.join("target.rs"),
        "pub fn compute() -> i32 { 0 }\n",
    )
    .expect("write target fixture");
    write_project_config(
        &repo_root,
        concat!(
            "[models]\n",
            "primary = [\"openrouter:mock/model\"]\n",
            "\n",
            "[project.verify]\n",
            "command = \"grep\"\n",
            "args = [\"-q\", \"pub fn compute() -> i32 { 42 }\", \"target.rs\"]\n",
        ),
    );
    commit_fixture(&repo_root, &["target.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("target.rs", ContextAccess::Editable)
        .expect("add target to context");
    let mut history = HistoryManager::new(repo_root.clone());

    // The review dialogue is an interactive turn by construction, and that very
    // surface authorizes the edit a finding asks for.
    let intent = tauqe_protocol::TurnIntent::InteractiveRepl;
    let protocol = tauqe_core::prompt::xml::XmlPromptProtocol
        .with_turn_marker(MARKER)
        .expect("marker-bound protocol");
    assert!(
        intent
            .narrow_capabilities(&protocol.capability_set())
            .contains(tauqe_protocol::LlmCapability::FileEdits),
        "a review dialogue must keep the file-edit surface"
    );

    let repair = format!(
        "{edits}\n<verify_{MARKER} target=\"check\" on_success=\"silent\" />",
        edits = edit_block(
            "Address the finding",
            "target.rs",
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ),
    );
    let provider = MockLlmProvider::scripted([MockResponse::text(repair)]);
    let model = ModelRef::openrouter("mock/model");
    let (stream_tx, cancel_rx) = event_channel();

    let baseline = commit_count(&repo_root);
    let mut policy = GitTransactionPolicy::new();
    let exec = SingleExecution::new(WorkflowOptions::default());
    let execution = exec
        .execute(
            &mut policy,
            "Address the review finding in target.rs",
            intent,
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            stream_tx,
            cancel_rx,
        )
        .await
        .expect("review dialogue turn executes");

    match execution.result {
        ModelResult::Edit { outcome, .. } => {
            assert!(
                outcome.error.is_none(),
                "unexpected error: {:?}",
                outcome.error
            );
            assert!(
                outcome.applied,
                "a review-dialogue edit must pass staging, verification, and the git transaction"
            );
            assert_eq!(outcome.changed_files, vec!["target.rs".to_string()]);
        }
        other => panic!("expected applied edits, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        commit_paths(&repo_root),
        vec!["target.rs".to_string()],
        "the review-dialogue edit must be the only committed path"
    );
    assert_eq!(
        commit_count(&repo_root),
        baseline + 1,
        "one bounded turn produces exactly one commit"
    );
    let status = worktree_status(&repo_root);
    assert!(
        status.is_empty(),
        "the turn must not leave unaccounted files behind: {status:?}"
    );
}
