//! End-to-end proof that a bounded turn resolves its project configuration once.
//!
//! A declared task runner answers the harness through a subprocess dump, so
//! every redundant resolution spawns a process and re-parses the same
//! deterministic manifest. The turn below passes through both repetition points
//! of a bounded turn — a patch retry inside staging and a verification-healing
//! round — while an instrumented runner stub counts how often it was dumped.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tauqe_core::context::ContextManager;
use tauqe_core::control::cache::TurnInputCache;
use tauqe_core::control::context::{CancellationToken, EventSink, LoopContext};
use tauqe_core::control::prompt::PromptBuilder;
use tauqe_core::control::turn::{run_turn_loop, TurnLoopConfig};
use tauqe_core::prompt::json::JsonPromptProtocol;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::workflow::git_policy::GitTransactionPolicy;
use tauqe_core::workflow::policy::DirectApplyPolicy;
use tauqe_core::workflow::research::journal::TurnResearchJournal;
use tauqe_protocol::{CapabilityExpr, ContextAccess, LlmCapability, ModelRef};

/// Writes an instrumented `just` stub into `bin_dir` and returns its counter.
///
/// The stub answers every invocation with the declarative dump of a runner that
/// exports no recipe, and appends one line to the counter beside it, so the test
/// observes the dump as a real subprocess instead of an in-process call count.
fn write_runner_stub(bin_dir: &Path) -> PathBuf {
    std::fs::create_dir_all(bin_dir).expect("create the stub directory");
    let counter = bin_dir.join("dump-count");
    let stub = bin_dir.join("just");
    let script = format!(
        "#!/bin/sh\nprintf 'dump\\n' >> '{counter}'\nprintf '%s\\n' '{{\"recipes\":{{}}}}'\n",
        counter = counter.display()
    );
    std::fs::write(&stub, script).expect("write the runner stub");
    let mut permissions = std::fs::metadata(&stub)
        .expect("read the stub permissions")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&stub, permissions).expect("make the stub executable");
    counter
}

/// Counts the dumps the runner stub answered.
fn dump_count(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .map(|contents| contents.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

/// Creates a temporary Git repository with an initial commit.
fn init_repo() -> (tempfile::TempDir, PathBuf) {
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

/// Builds a single-edit XML response carrying one search/replace hunk.
fn edit_payload(summary: &str, path: &str, search: &str, replace: &str) -> String {
    serde_json::json!({
        "message": summary,
        "action": {
            "kind": "edit",
            "changes": [
                {
                    "op": "replace",
                    "path": path,
                    "old_text": search,
                    "new_text": replace,
                    "to": "",
                    "start": "",
                    "end": "",
                    "content": ""
                }
            ],
            "commit_message": summary,
            "message": "",
            "reason": ""
        }
    })
    .to_string()
}

/// Serializes the tests that install an instrumented task runner on `PATH`.
///
/// A declared task runner is executed by name, so its executable is resolved
/// through the process-wide `PATH`. Two such tests running at the same time
/// would each overwrite the other's entry and let one stub answer for the other
/// test's turn, counting a dump nobody spent. The lock keeps that ownership
/// exclusive for the whole body of each test.
async fn path_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static PATH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    PATH_LOCK.lock().await
}

#[tokio::test]
async fn a_bounded_turn_dumps_the_task_runner_once_across_retries_and_healing() {
    let _guard = path_lock().await;
    let (_repo_dir, repo_root) = init_repo();
    let stub_dir = tempfile::tempdir().expect("create the stub directory");
    let counter = write_runner_stub(stub_dir.path());

    let mut paths = vec![stub_dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::set_var("PATH", std::env::join_paths(paths).expect("compose PATH"));

    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(
        repo_root.join("src/lib.rs"),
        "pub fn alpha() -> i32 { \"alpha\" }\npub fn beta() -> i32 { \"beta\" }\n",
    )
    .expect("write broken library");
    std::fs::write(
        repo_root.join("tauqe.toml"),
        concat!(
            "[models]\n",
            "repl = [\"openrouter:mock/model\"]\n",
            "\n",
            "[project]\n",
            "runner = \"just\"\n",
            "\n",
            "[project.verify]\n",
            "command = \"grep\"\n",
            "args = [\"-q\", \"pub fn beta() -> i32 { 2 }\", \"src/lib.rs\"]\n",
        ),
    )
    .expect("write the project declaration");
    commit_fixture(&repo_root, &["src/lib.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add the library to context");

    let broken = edit_payload(
        "Broken attempt",
        "src/lib.rs",
        "this text does not exist in the file",
        "irrelevant replacement",
    );
    let repair_alpha = edit_payload(
        "Repair alpha return type",
        "src/lib.rs",
        "pub fn alpha() -> i32 { \"alpha\" }",
        "pub fn alpha() -> i32 { 1 }",
    );
    let repair_beta = edit_payload(
        "Repair beta return type",
        "src/lib.rs",
        "pub fn beta() -> i32 { \"beta\" }",
        "pub fn beta() -> i32 { 2 }",
    );
    let provider = MockLlmProvider::scripted([
        MockResponse::text(broken),
        MockResponse::text(repair_alpha),
        MockResponse::text(repair_beta),
    ]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = JsonPromptProtocol::edits();

    let cancel = CancellationToken::new();
    let events = EventSink::noop();
    let mut cache = TurnInputCache::default();
    let mut journal = TurnResearchJournal::default();
    let mut policy = GitTransactionPolicy::new();
    let caps = tauqe_core::control::turn::turn_capability_expr();
    let prompt_builder = PromptBuilder::new(&provider, &model, &protocol, &mut cache);
    let mut ctx = LoopContext::new(
        &cancel,
        &events,
        &repo_root,
        &mut context,
        &mut journal,
        &mut policy,
        caps,
        prompt_builder,
    );

    let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
        .await
        .expect("the bounded turn executes");

    assert!(outcome.is_success());
    let success = outcome.into_success().expect("success");
    assert_eq!(success.value.modified_paths, vec!["src/lib.rs"]);
    assert!(success.value.commit_hash.is_some());
    assert!(
        success.value.commit_error.is_none(),
        "unexpected commit error: {:?}",
        success.value.commit_error
    );

    assert_eq!(
        provider.requests().len(),
        3,
        "the turn must spend one patch retry and one healing round"
    );
    assert_eq!(
        dump_count(&counter),
        1,
        "the declared task runner must be dumped once for the whole bounded turn"
    );

    let healed = std::fs::read_to_string(repo_root.join("src/lib.rs")).expect("read the library");
    assert!(healed.contains("pub fn alpha() -> i32 { 1 }"));
    assert!(healed.contains("pub fn beta() -> i32 { 2 }"));
}

#[tokio::test]
async fn an_answer_only_verification_round_reuses_the_frozen_configuration() {
    let _guard = path_lock().await;
    let (_repo_dir, repo_root) = init_repo();
    let stub_dir = tempfile::tempdir().expect("create the stub directory");
    let counter = write_runner_stub(stub_dir.path());

    let mut paths = vec![stub_dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::set_var("PATH", std::env::join_paths(paths).expect("compose PATH"));

    std::fs::create_dir_all(repo_root.join("src")).expect("create src directory");
    std::fs::write(
        repo_root.join("src/lib.rs"),
        "pub fn alpha() -> i32 { 1 }\n",
    )
    .expect("write the library");
    std::fs::write(
        repo_root.join("tauqe.toml"),
        concat!(
            "[models]\n",
            "repl = [\"openrouter:mock/model\"]\n",
            "\n",
            "[project]\n",
            "runner = \"just\"\n",
            "\n",
            "[project.verify]\n",
            "command = \"true\"\n",
        ),
    )
    .expect("write the project declaration");
    commit_fixture(&repo_root, &["src/lib.rs", "tauqe.toml"]);

    let mut context = ContextManager::new(repo_root.clone());
    context
        .add_file("src/lib.rs", ContextAccess::Editable)
        .expect("add the library to context");

    let answer = "Nothing to change here.";
    let provider = MockLlmProvider::scripted([MockResponse::text(answer)]);
    let model = ModelRef::openrouter("mock/model");
    let protocol = JsonPromptProtocol::edits();

    let cancel = CancellationToken::new();
    let events = EventSink::noop();
    let mut cache = TurnInputCache::default();
    let mut journal = TurnResearchJournal::default();
    let mut policy = DirectApplyPolicy::new();
    let caps = CapabilityExpr::set([
        CapabilityExpr::leaf(LlmCapability::TextMessage),
        CapabilityExpr::leaf(LlmCapability::EditReplace),
        CapabilityExpr::leaf(LlmCapability::ContextRequest),
    ]);
    let prompt_builder = PromptBuilder::new(&provider, &model, &protocol, &mut cache);
    let mut ctx = LoopContext::new(
        &cancel,
        &events,
        &repo_root,
        &mut context,
        &mut journal,
        &mut policy,
        caps,
        prompt_builder,
    );

    let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
        .await
        .expect("the bounded turn executes");

    assert!(outcome.is_success());
    let success = outcome.into_success().expect("success");
    assert!(
        success.value.modified_paths.is_empty(),
        "an answer-only turn touches no paths"
    );
    assert!(
        success.response.has(LlmCapability::TextMessage),
        "an answer-only turn retains text response"
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "the answer-only turn must spend a single proposal round"
    );
    assert_eq!(
        dump_count(&counter),
        1,
        "the answer round must reuse the frozen configuration"
    );
}
