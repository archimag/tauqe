//! Tests for the Git transaction lifecycle owned by `crate::git::checkpoint`.
//!
//! Every case drives the real `git` executable through the checkpoint, step
//! commit, squash, recovery, and staging-artifact sweeps, so the guarantees a
//! bounded turn relies on are verified against the repository state it really
//! produces.

use super::*;
use crate::git::exec::{GitDeadline, TERMINATION_GRACE};
use crate::git::GitHooks;
use std::fs;

/// Process id of an already reaped child, which is provably no longer running.
///
/// `git` is a hard prerequisite of this crate's suite, so the dead writer a
/// reclamation case needs is a short-lived child of this process.
#[cfg(unix)]
fn dead_process_id() -> u32 {
    let mut child = std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("git is installed");
    let pid = child.id();
    child.wait().expect("the child exits on its own");
    pid
}

/// A child process that keeps running until its standard input is dropped.
///
/// A reclamation case needs a writer that is provably alive and provably not
/// this process. `read` blocks on the piped standard input, so the child stays
/// in the process table until the test drops it.
#[cfg(unix)]
fn live_child() -> std::process::Child {
    std::process::Command::new("sh")
        .arg("-c")
        .arg("read line")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("a POSIX shell is installed")
}

/// Rewrites the open marker's writer attribution to the legacy unattributed form.
///
/// The crash-simulation cases recover a marker this test process wrote itself:
/// they exercise the unwind and refusal mechanics, not the writer-ownership
/// gate, which treats a live writer as an owner. Clearing the attribution routes
/// them through the path an unattributed legacy marker takes, on every platform.
fn forget_marker_writer(path: &std::path::Path) {
    let mut checkpoint = read_transaction_marker(path).expect("the fixture opens a transaction");
    checkpoint.writer_pid = None;
    write_transaction_marker(path, &checkpoint).unwrap();
}

#[test]
fn test_recover_interrupted_workflow_clean_repo() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_none());
}

#[test]
fn test_cleanup_orphaned_staging_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();

    let tmp_file = path.join("orphan.rs.tauqe-tmp");
    fs::write(&tmp_file, "temporary").unwrap();

    let existing_orig = path.join("existing.rs");
    fs::write(&existing_orig, "existing content").unwrap();
    let existing_bak = path.join("existing.rs.tauqe-bak");
    fs::write(&existing_bak, "stale backup").unwrap();

    let missing_bak = path.join("missing.rs.tauqe-bak");
    fs::write(&missing_bak, "original content to restore").unwrap();

    let sub_dir = path.join("subdir");
    fs::create_dir_all(&sub_dir).unwrap();
    let sub_tmp = sub_dir.join("sub.rs.tauqe-tmp");
    fs::write(&sub_tmp, "sub temporary").unwrap();

    let git_dir = path.join(".git");
    fs::create_dir_all(&git_dir).unwrap();
    let git_tmp = git_dir.join("internal.tauqe-tmp");
    fs::write(&git_tmp, "do not touch").unwrap();

    let cleaned = cleanup_orphaned_staging_files(path);
    assert_eq!(cleaned, 4);

    assert!(!tmp_file.exists());
    assert!(!existing_bak.exists());
    assert_eq!(
        fs::read_to_string(&existing_orig).unwrap(),
        "existing content"
    );
    assert!(!missing_bak.exists());
    assert_eq!(
        fs::read_to_string(path.join("missing.rs")).unwrap(),
        "original content to restore"
    );
    assert!(!sub_tmp.exists());
    assert!(git_tmp.exists());
}

#[cfg(unix)]
#[test]
fn test_cleanup_reclaims_only_siblings_whose_writer_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();

    // A writer that is still running: this test process.
    let own_pid = std::process::id();
    let own_tmp_name = format!("own.rs.tauqe-tmp.{own_pid}.1");
    let own_tmp = path.join(&own_tmp_name);
    fs::write(&own_tmp, "own staged").unwrap();
    let own_original = path.join("own.rs");
    fs::write(&own_original, "own original").unwrap();
    let own_backup_name = format!("own.rs.tauqe-bak.{own_pid}.2");
    let own_backup = path.join(&own_backup_name);
    fs::write(&own_backup, "own backup").unwrap();

    // A writer that is still running: a child held across the sweep.
    let mut child = live_child();
    let child_pid = child.id();
    let child_tmp_name = format!("child.rs.tauqe-tmp.{child_pid}.3");
    let child_tmp = path.join(&child_tmp_name);
    fs::write(&child_tmp, "child staged").unwrap();
    let child_backup_name = format!("child_missing.rs.tauqe-bak.{child_pid}.4");
    let child_backup = path.join(&child_backup_name);
    fs::write(&child_backup, "child original").unwrap();

    // A writer that is provably gone.
    let dead_pid = dead_process_id();
    let dead_tmp_name = format!("dead.rs.tauqe-tmp.{dead_pid}.5");
    let dead_tmp = path.join(&dead_tmp_name);
    fs::write(&dead_tmp, "dead staged").unwrap();
    let dead_backup_name = format!("dead_missing.rs.tauqe-bak.{dead_pid}.6");
    let dead_backup = path.join(&dead_backup_name);
    fs::write(&dead_backup, "dead original").unwrap();

    // A name of the earlier fixed-name policy records no writer at all.
    let legacy_tmp = path.join("legacy.rs.tauqe-tmp");
    fs::write(&legacy_tmp, "legacy staged").unwrap();
    let legacy_backup = path.join("legacy_missing.rs.tauqe-bak");
    fs::write(&legacy_backup, "legacy original").unwrap();

    let cleaned = cleanup_orphaned_staging_files(path);

    let _ = child.kill();
    let _ = child.wait();

    // Only the dead-writer and legacy siblings are reclaimed.
    assert_eq!(cleaned, 4);

    // A live writer keeps its staged content, its backup, and its original.
    assert!(own_tmp.exists());
    assert!(own_backup.exists());
    assert!(own_original.exists());
    assert!(child_tmp.exists());
    assert!(child_backup.exists());
    assert!(!path.join("child_missing.rs").exists());

    // A dead writer's artifacts are reclaimed exactly as before.
    assert!(!dead_tmp.exists());
    assert!(!dead_backup.exists());
    assert_eq!(
        fs::read_to_string(path.join("dead_missing.rs")).unwrap(),
        "dead original"
    );

    // A legacy name is reclaimed exactly as before.
    assert!(!legacy_tmp.exists());
    assert!(!legacy_backup.exists());
    assert_eq!(
        fs::read_to_string(path.join("legacy_missing.rs")).unwrap(),
        "legacy original"
    );
}

#[test]
fn test_recover_interrupted_workflow_cleans_orphaned_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let tmp_file = path.join("orphan.rs.tauqe-tmp");
    fs::write(&tmp_file, "staged").unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_some());
    assert!(recovery
        .unwrap()
        .contains("Cleaned up 1 orphaned staging artifact(s)"));
    assert!(!tmp_file.exists());
}

#[test]
fn test_checkpoint_ignores_and_preserves_tauqe_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // Simulate internal .tauqe directory with credentials and history
    let tauqe_dir = path.join(".tauqe");
    fs::create_dir_all(&tauqe_dir).unwrap();
    let creds_file = tauqe_dir.join("credentials.toml");
    fs::write(&creds_file, "openrouter_key = \"secret\"").unwrap();
    let history_file = tauqe_dir.join("history.jsonl");
    fs::write(&history_file, "{\"type\":\"turn\",\"prompt\":\"test\"}\n").unwrap();

    // Root credentials file should also be ignored and preserved
    let root_creds = path.join("credentials.toml");
    fs::write(&root_creds, "root_key = \"secret\"").unwrap();

    // User uncommitted change in a tracked/project file
    let user_file = path.join("user_code.rs");
    fs::write(&user_file, "fn main() {}").unwrap();

    // 1. Create checkpoint
    let chk = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(chk.created_checkpoint_commit);

    // Verify that .tauqe files and credentials were NOT committed into checkpoint
    let committed_files = crate::git::cmd::run_git(
        Some(path),
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    )
    .unwrap();
    assert!(committed_files.contains("user_code.rs"));
    assert!(!committed_files.contains(".tauqe"));
    assert!(!committed_files.contains("credentials.toml"));

    // 2. AI creates a new file during the turn
    let ai_file = path.join("ai_generated.rs");
    fs::write(&ai_file, "pub fn ai() {}").unwrap();

    // 3. Restore checkpoint (e.g. on error or undo)
    restore_checkpoint(path, &chk).unwrap();

    // User file must be restored
    assert!(user_file.exists());
    assert_eq!(fs::read_to_string(&user_file).unwrap(), "fn main() {}");

    // AI generated file must be cleaned
    assert!(!ai_file.exists());

    // CRITICAL: .tauqe credentials, root credentials and history must STILL exist and be intact
    assert!(creds_file.exists());
    assert_eq!(
        fs::read_to_string(&creds_file).unwrap(),
        "openrouter_key = \"secret\""
    );
    assert!(root_creds.exists());
    assert_eq!(
        fs::read_to_string(&root_creds).unwrap(),
        "root_key = \"secret\""
    );
    assert!(history_file.exists());
    assert_eq!(
        fs::read_to_string(&history_file).unwrap(),
        "{\"type\":\"turn\",\"prompt\":\"test\"}\n"
    );
}

#[test]
fn test_checkpoint_clean_repo_with_only_tauqe_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // Only .tauqe exists (no user code changes)
    let tauqe_dir = path.join(".tauqe");
    fs::create_dir_all(&tauqe_dir).unwrap();
    let creds_file = tauqe_dir.join("credentials.toml");
    fs::write(&creds_file, "key = \"secret\"").unwrap();

    // Checkpoint must consider repo clean (created_checkpoint_commit = false)
    let chk = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(!chk.created_checkpoint_commit);

    // AI generates files
    let ai_file = path.join("ai.rs");
    fs::write(&ai_file, "code").unwrap();

    // Restore checkpoint
    restore_checkpoint(path, &chk).unwrap();

    assert!(!ai_file.exists());
    assert!(creds_file.exists());
}

#[test]
fn test_recover_interrupted_workflow_with_checkpoint_and_steps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // Simulate user uncommitted changes
    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();

    // 1. Checkpoint created
    let chk = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(chk.created_checkpoint_commit);

    // 2. Step commit created
    let ai_file = path.join("ai_work.txt");
    fs::write(&ai_file, "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    // 3. Crash recovery runs
    forget_marker_writer(path);
    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_some());
    let msg = recovery.unwrap();
    assert!(msg.contains("unwound 1 step commit(s)"));
    assert!(msg.contains("restored checkpoint: true"));

    // User uncommitted work must be intact in the working tree
    assert!(user_file.exists());
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
    // Intermediate step modification should be removed
    assert!(!ai_file.exists());
}

#[test]
fn test_recover_interrupted_workflow_unwinds_multiple_steps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();

    let chk = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(chk.created_checkpoint_commit);

    // Two intermediate step commits imitate a healed turn that crashed
    // before its squashed commit could be produced.
    fs::write(path.join("step_one.txt"), "first step").unwrap();
    create_step_commit(
        path,
        &["step_one.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    fs::write(path.join("step_two.txt"), "second step").unwrap();
    create_step_commit(
        path,
        &["step_two.txt".to_string()],
        "apply step 2",
        GitHooks::Bypass,
    )
    .unwrap();

    forget_marker_writer(path);
    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_some());
    let msg = recovery.unwrap();
    assert!(msg.contains("unwound 2 step commit(s)"));
    assert!(msg.contains("restored checkpoint: true"));

    // Every temporary commit is unwound while the developer's work survives
    // in the working tree exactly as before the turn.
    assert!(user_file.exists());
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
    assert!(!path.join("step_one.txt").exists());
    assert!(!path.join("step_two.txt").exists());
}

#[test]
fn test_recover_ignores_manual_step_commit_without_marker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A commit that merely shares the step-commit subject prefix must never
    // be treated as a dangling Tauqe transaction when no marker is present.
    fs::write(path.join("manual.txt"), "manual work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "manual.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "tauqe-step: hand-written by developer",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let head_before = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_none());

    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_before, head_after);
    assert!(path.join("manual.txt").exists());
}

#[test]
fn test_recover_skips_when_no_steps_and_no_checkpoint_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // Simulate an open marker without checkpoint commit (clean repo at start)
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let checkpoint = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Commit(head),
        phase: TransactionPhase::Established,
        writer_pid: None,
    };
    write_transaction_marker(path, &checkpoint).unwrap();

    // User or turn leaves verified changes uncommitted in the working tree
    let dirty_file = path.join("dirty.txt");
    fs::write(&dirty_file, "valuable uncommitted edits").unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_none());
    assert!(read_transaction_marker(path).is_none());
    // Uncommitted edits must remain completely untouched
    assert_eq!(
        fs::read_to_string(&dirty_file).unwrap(),
        "valuable uncommitted edits"
    );
}

#[test]
fn test_recover_refuses_when_range_contains_foreign_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // Open a transaction so the marker records the pre-turn base.
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(!checkpoint.created_checkpoint_commit);

    // A developer commit without the Tauqe trailer lands inside the range.
    fs::write(path.join("foreign.txt"), "foreign work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "foreign.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let head_before = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    forget_marker_writer(path);
    let recovery = recover_interrupted_workflow(path).unwrap();
    let msg = recovery.unwrap_or_default();
    assert!(msg.contains("Skipped crash recovery"));

    // The foreign commit is preserved and HEAD is untouched.
    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_before, head_after);
}

#[test]
fn test_recover_refuses_when_recorded_base_is_not_an_ancestor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let current_branch =
        crate::git::cmd::run_git(Some(path), &["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

    // A commit that lives only on a side branch, recorded as the base of a
    // transaction opened on the branch currently checked out.
    crate::git::cmd::run_git(Some(path), &["checkout", "-b", "side"]).unwrap();
    fs::write(path.join("side.txt"), "side work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "side.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "chore: side work",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let foreign_base = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    crate::git::cmd::run_git(Some(path), &["checkout", current_branch.as_str()]).unwrap();
    let head_before = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let checkpoint = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Commit(foreign_base),
        phase: TransactionPhase::Established,
        writer_pid: None,
    };
    write_transaction_marker(path, &checkpoint).unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    let msg = recovery.unwrap_or_default();
    assert!(msg.contains("not an ancestor of HEAD"));

    // Resetting to a base outside the current history would discard commits
    // that were never part of the transaction, so HEAD must stay put and the
    // marker must survive for a later attempt on the matching branch.
    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_before, head_after);
    assert!(read_transaction_marker(path).is_some());
}

#[test]
fn test_create_checkpoint_refuses_when_ignore_rules_cannot_be_installed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A directory occupying `.git/info/exclude` rejects the atomic replacement
    // that would publish the rules, so no exclusion is installed at all and
    // `git add -A` would see the credential file as an ordinary untracked path
    // and commit the API key.
    let exclude_path = path.join(".git").join("info").join("exclude");
    if exclude_path.is_file() {
        fs::remove_file(&exclude_path).unwrap();
    }
    fs::create_dir_all(&exclude_path).unwrap();

    let tauqe_dir = path.join(".tauqe");
    fs::create_dir_all(&tauqe_dir).unwrap();
    fs::write(
        tauqe_dir.join("credentials.toml"),
        "[openrouter]\napi_key = \"sk-or-v1-secret\"\n",
    )
    .unwrap();

    let err = create_checkpoint(path, GitHooks::Bypass)
        .expect_err("the checkpoint must abort instead of committing the API key");
    assert!(err.to_string().contains("exclude"), "{err}");

    // Nothing was committed and no transaction was opened, so no secret ever
    // reached the object database.
    let subject = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%s"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(subject, "chore: initial commit");
    let tracked = crate::git::cmd::run_git(Some(path), &["ls-files"]).unwrap();
    assert!(!tracked.contains("credentials.toml"), "{tracked}");
    assert!(!tauqe_dir.join("active-transaction.json").exists());
}

#[test]
fn test_create_checkpoint_refuses_when_protected_path_is_not_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // Installing the ignore rules succeeds here, but a project `.gitignore`
    // outranks `.git/info/exclude` and re-includes the credential file, so the
    // rules are in place without actually covering the protected path.
    fs::write(path.join(".gitignore"), "!credentials.toml\n").unwrap();
    fs::write(
        path.join("credentials.toml"),
        "[openrouter]\napi_key = \"sk-or-v1-secret\"\n",
    )
    .unwrap();

    let err = create_checkpoint(path, GitHooks::Bypass)
        .expect_err("a credential file Git does not ignore must abort the checkpoint");
    assert!(err.to_string().contains("credentials.toml"), "{err}");

    let subject = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%s"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(subject, "chore: initial commit");
    let tracked = crate::git::cmd::run_git(Some(path), &["ls-files"]).unwrap();
    assert!(!tracked.contains("credentials.toml"), "{tracked}");
}

#[test]
fn test_ensure_tauqe_ignored_publishes_one_stable_document() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A document an earlier tool left behind: it carries no terminator, so the
    // publication has to add exactly one separator newline before the rules.
    let exclude = path.join(".git").join("info").join("exclude");
    fs::write(&exclude, "*.swp").unwrap();

    ensure_tauqe_ignored(path).unwrap();

    let document = fs::read_to_string(&exclude).unwrap();
    assert!(
        document.starts_with("*.swp\n"),
        "the previous document survives behind one separator newline: {document:?}"
    );
    assert!(
        !document.contains("\n\n"),
        "the separator is one newline, never a blank rule line: {document:?}"
    );
    assert!(document.ends_with('\n'), "{document:?}");

    for rule in tauqe_ignore_rules() {
        let declarations = document
            .lines()
            .filter(|line| declares_rule(line, rule.as_str()))
            .count();
        assert_eq!(
            declarations, 1,
            "the rule {rule} must be declared exactly once: {document:?}"
        );
    }

    // The publication runs through the atomic-write policy, so it leaves no
    // staging sibling beside the document it publishes.
    let leftovers: Vec<String> = fs::read_dir(exclude.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            crate::atomic_write::staging_temp_original_name(name).is_some()
                || crate::atomic_write::staging_backup_original_name(name).is_some()
        })
        .collect();
    assert!(leftovers.is_empty(), "staging artifacts: {leftovers:?}");

    // A repeated installation recognises every rule it wrote, so the document
    // stays byte-identical instead of collecting a second copy.
    let published = fs::read(&exclude).unwrap();
    ensure_tauqe_ignored(path).unwrap();
    ensure_tauqe_ignored(path).unwrap();
    assert_eq!(fs::read(&exclude).unwrap(), published);
}

#[test]
fn test_create_checkpoint_refuses_when_a_staging_marker_path_is_not_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A project `.gitignore` outranks `.git/info/exclude` and re-includes the
    // staging artifacts, so the rules are installed without covering them.
    fs::write(path.join(".gitignore"), "!*.tauqe-tmp*\n").unwrap();
    let orphan = path.join(format!("draft.rs{STAGING_TEMP_MARKER}.4242.1"));
    fs::write(&orphan, "staged content").unwrap();

    let err = create_checkpoint(path, GitHooks::Bypass)
        .expect_err("a staging artifact Git does not ignore must abort the checkpoint");
    assert!(err.to_string().contains(STAGING_TEMP_MARKER), "{err}");

    // Nothing was committed and no transaction was opened, so the artifact the
    // re-included rule exposes never entered the object database.
    let subject = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%s"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(subject, "chore: initial commit");
    let tracked = crate::git::cmd::run_git(Some(path), &["ls-files"]).unwrap();
    assert!(!tracked.contains(STAGING_TEMP_MARKER), "{tracked}");
    assert!(!transaction_marker_path(path).exists());
}

#[test]
fn test_protected_paths_are_proven_on_a_repository_without_harness_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();
    assert!(
        !path.join(".tauqe").exists(),
        "the fixture must start without harness state"
    );

    ensure_tauqe_ignored(path).unwrap();

    // The proof covers the state directory, the credentials, and both staging
    // markers, and it answers for paths that do not exist yet: the rules are
    // pattern matches, not observations of the current tree.
    let probes = protected_paths();
    let ignored = crate::git::cmd::run_git_check_ignore(path, &probes).unwrap();
    for probe in &probes {
        assert!(
            ignored.iter().any(|entry| entry == probe),
            "{probe} must be proven ignored: {ignored:?}"
        );
    }
}

#[test]
fn test_exclusion_document_keeps_bytes_it_cannot_decode_verbatim() {
    let existing = b"# local excludes \xff\n".to_vec();
    let rules = vec!["credentials.toml".to_string()];

    let document = exclusion_document(&existing, &rules).expect("the rule is missing");

    assert!(
        document.starts_with(&existing),
        "the previous bytes must survive the publication"
    );
    assert!(document.ends_with(b"credentials.toml\n"));

    // Once the rule is declared the document is complete, and a repeated
    // installation answers with nothing to write.
    assert!(exclusion_document(&document, &rules).is_none());
}

#[test]
fn test_finalize_and_create_ai_commit_close_an_empty_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A developer's uncommitted work opens a real checkpoint commit, which
    // finalization has to roll back into the working tree.
    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "uncommitted work").unwrap();
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(checkpoint.created_checkpoint_commit);

    // The turn wrote nothing Git tracks, so closing the transaction reports
    // the absence of a commit instead of failing the verified turn.
    let hash = finalize_ai_commit(path, &checkpoint, &[], "draft only", GitHooks::Bypass).unwrap();
    assert!(hash.is_empty());
    assert!(!transaction_marker_path(path).exists());
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "uncommitted work",
        "the developer's work must survive the closed transaction"
    );
    let subject = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%s"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(subject, "chore: initial commit");

    // An isolated commit without a checkpoint closes the same way.
    let hash = create_ai_commit(path, &[], "draft only", GitHooks::Bypass).unwrap();
    assert!(hash.is_empty());
    assert!(!transaction_marker_path(path).exists());
}

#[test]
fn test_transaction_marker_rewrite_publishes_a_complete_document() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let first = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Commit(head.clone()),
        phase: TransactionPhase::Established,
        writer_pid: None,
    };
    write_transaction_marker(path, &first).unwrap();
    let read_back = read_transaction_marker(path).expect("the marker must be readable");
    assert_eq!(read_back, first);

    // A rewrite replaces the previous complete document whole; it never
    // merges into it and never publishes a truncated one.
    let second = CheckpointInfo {
        created_checkpoint_commit: true,
        base: TransactionBase::Commit("0123456789abcdef0123456789abcdef01234567".to_string()),
        phase: TransactionPhase::Established,
        writer_pid: None,
    };
    write_transaction_marker(path, &second).unwrap();
    let read_back = read_transaction_marker(path).expect("the marker must be readable");
    assert_eq!(read_back, second);

    // The rewrite publishes through a synced sibling rename, so no staging
    // artifact may survive it.
    let entries: Vec<String> = std::fs::read_dir(path.join(".tauqe"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().any(|name| name == "active-transaction.json"),
        "the marker must be present: {entries:?}"
    );
    assert!(
        !entries.iter().any(|name| {
            crate::atomic_write::staging_temp_original_name(name).is_some()
                || crate::atomic_write::staging_backup_original_name(name).is_some()
        }),
        "an atomic rewrite must leave no staging artifact behind: {entries:?}"
    );

    // Recovery resolves the atomically written marker and closes a
    // transaction that holds neither a step commit nor a checkpoint commit.
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(head),
            phase: TransactionPhase::Established,
            writer_pid: None,
        },
    )
    .unwrap();
    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_none());
    assert!(read_transaction_marker(path).is_none());
}

#[test]
fn test_concurrent_marker_writes_never_expose_a_torn_document() {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(head.clone()),
            phase: TransactionPhase::Established,
            writer_pid: None,
        },
    )
    .unwrap();

    let written: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let reader = {
        let path = path.to_path_buf();
        let head = head.clone();
        let written = Arc::clone(&written);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // A torn or empty document decodes as `None`, so a readable
                // marker proves the rewrite published a complete one.
                let observed = read_transaction_marker(&path)
                    .expect("the marker always exists and always decodes whole")
                    .base;
                match observed {
                    TransactionBase::Commit(observed_head) => {
                        if observed_head != head {
                            let registered = written.lock().unwrap();
                            assert!(
                                registered.contains(&observed_head),
                                "observed an unregistered marker document: {observed_head}"
                            );
                        }
                    }
                    // The concurrent writers record commit bases only; an
                    // unborn observation still proves a complete document.
                    TransactionBase::Unborn => {}
                }
            }
        })
    };

    let writers: Vec<_> = (0..4)
        .map(|thread_index| {
            let path = path.to_path_buf();
            let written = Arc::clone(&written);
            std::thread::spawn(move || {
                for step in 0..50 {
                    let head = format!("concurrent-{thread_index}-{step}");
                    let marker = CheckpointInfo {
                        created_checkpoint_commit: step % 2 == 0,
                        base: TransactionBase::Commit(head.clone()),
                        phase: TransactionPhase::Established,
                        writer_pid: None,
                    };
                    // Register the document before it can be observed.
                    written.lock().unwrap().insert(head);
                    write_transaction_marker(&path, &marker).unwrap();
                }
            })
        })
        .collect();

    for writer in writers {
        writer.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();

    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(head),
            phase: TransactionPhase::Established,
            writer_pid: None,
        },
    )
    .unwrap();
    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_none());
    assert!(read_transaction_marker(path).is_none());
}

#[cfg(unix)]
#[test]
fn test_recovery_reclaims_state_dir_staging_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // Reclamation claims a sibling only when its writer is provably gone, so
    // the interrupted writes below are attributed to an exited child.
    let pid = dead_process_id();
    let state_dir = path.join(".tauqe");
    fs::create_dir_all(state_dir.join("reviews")).unwrap();
    fs::create_dir_all(state_dir.join("plans")).unwrap();
    fs::create_dir_all(state_dir.join("mockups")).unwrap();

    let credentials = state_dir.join("credentials.toml");
    fs::write(&credentials, "key = \"secret\"").unwrap();
    let history = state_dir.join("history.jsonl");
    fs::write(&history, "{\"type\":\"turn\"}\n").unwrap();
    let review = state_dir.join("reviews").join("review-a.json");
    fs::write(&review, "{\"id\":\"review-a\"}\n").unwrap();

    // Two interrupted atomic replacements and one interrupted sibling
    // commit, all inside the state directory the worktree walk skips.
    let history_orphan = state_dir.join(format!("history.jsonl.tauqe-tmp.{pid}.1"));
    fs::write(&history_orphan, "{\"type\":\"turn\"}").unwrap();
    let plan_orphan = state_dir
        .join("plans")
        .join(format!("plan-a.json.tauqe-tmp.{pid}.2"));
    fs::write(&plan_orphan, "{\"id\":\"plan-a\"}").unwrap();
    let backup = state_dir
        .join("mockups")
        .join(format!("notes.md.tauqe-bak.{pid}.3"));
    fs::write(&backup, "draft notes").unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(
        message.contains("Cleaned up 3 orphaned staging artifact(s)"),
        "{message}"
    );

    assert!(!history_orphan.exists());
    assert!(!plan_orphan.exists());
    assert!(!backup.exists());
    let restored = state_dir.join("mockups").join("notes.md");
    assert_eq!(fs::read_to_string(&restored).unwrap(), "draft notes");

    // Live harness state is never a reclaim candidate.
    let credentials_content = fs::read_to_string(&credentials).unwrap();
    assert_eq!(credentials_content, "key = \"secret\"");
    let history_content = fs::read_to_string(&history).unwrap();
    assert_eq!(history_content, "{\"type\":\"turn\"}\n");
    let review_content = fs::read_to_string(&review).unwrap();
    assert_eq!(review_content, "{\"id\":\"review-a\"}\n");
}

#[cfg(unix)]
#[test]
fn test_crashed_atomic_write_orphans_are_ignored_by_git_and_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A crashed atomic replacement of a worktree file leaves a sibling the
    // installed exclude rules must cover.
    let orphan_name = format!("notes.rs.tauqe-tmp.{}.7", dead_process_id());
    let orphan = path.join(&orphan_name);
    fs::write(&orphan, "staged content").unwrap();

    ensure_tauqe_ignored(path).unwrap();

    let candidates = std::slice::from_ref(&orphan_name);
    let ignored = crate::git::cmd::run_git_check_ignore(path, candidates).unwrap();
    assert!(
        ignored.iter().any(|entry| entry == &orphan_name),
        "the staging suffix must be ignored by Git: {ignored:?}"
    );

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(
        message.contains("Cleaned up 1 orphaned staging artifact(s)"),
        "{message}"
    );
    assert!(!orphan.exists());
}

/// Initializes a repository whose branch carries no commit at all.
///
/// `rev-parse HEAD` fails on such a branch, which is exactly the state the
/// checkpoint, unwind, restore, and recovery paths have to survive.
fn init_unborn_repository(path: &std::path::Path) {
    crate::git::state::init_repository(Some(path), false, GitHooks::Bypass).unwrap();
    assert!(
        head_is_unborn(path),
        "the fixture must start without a commit"
    );
}

/// Reports whether the branch currently checked out has no commit at all.
fn head_is_unborn(path: &std::path::Path) -> bool {
    crate::git::cmd::run_git(Some(path), &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err()
}

#[test]
fn test_recovery_returns_an_unborn_branch_to_the_unborn_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    // The developer's work is the only content the repository holds.
    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();

    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(checkpoint.created_checkpoint_commit);
    // The checkpoint became the repository root commit, so a dirty unborn
    // branch records the checkpoint itself — a resolving commit hash — as
    // its base.
    let TransactionBase::Commit(recorded_base) = &checkpoint.base else {
        panic!("a created checkpoint commit must be the recorded base");
    };
    let resolved =
        crate::git::cmd::run_git(Some(path), &["rev-parse", "--verify", recorded_base]).unwrap();
    assert_eq!(resolved.trim(), recorded_base);
    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    forget_marker_writer(path);
    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("unwound 1 step commit(s)"), "{message}");
    assert!(message.contains("restored checkpoint: true"), "{message}");

    // The branch is unborn again and the index is empty, so the developer's
    // captured work sits in the working tree exactly as before the turn.
    assert!(head_is_unborn(path));
    let status = crate::git::cmd::run_git(Some(path), &["status", "--porcelain"]).unwrap();
    assert!(status.contains("?? user_work.txt"), "{status}");
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
    assert!(!path.join("ai_work.txt").exists());
    assert!(read_transaction_marker(path).is_none());
}

#[test]
fn test_restore_checkpoint_returns_an_unborn_branch_to_the_unborn_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(checkpoint.created_checkpoint_commit);

    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    restore_checkpoint(path, &checkpoint).unwrap();

    assert!(head_is_unborn(path));
    assert!(!transaction_marker_path(path).exists());
    let status = crate::git::cmd::run_git(Some(path), &["status", "--porcelain"]).unwrap();
    assert!(status.contains("?? user_work.txt"), "{status}");
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
    assert!(!path.join("ai_work.txt").exists());
}

#[test]
fn test_finalize_on_an_unborn_branch_squashes_into_one_root_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(checkpoint.created_checkpoint_commit);

    fs::write(path.join("ai_work.txt"), "ai work").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    let hash = finalize_ai_commit(
        path,
        &checkpoint,
        &["ai_work.txt".to_string()],
        "feat: add the generated file",
        GitHooks::Bypass,
    )
    .unwrap();
    assert!(!hash.is_empty());

    // Exactly one commit carries the turn summary on a branch that had no
    // history before the turn: the step commit and the checkpoint are gone.
    let count = crate::git::cmd::run_git(Some(path), &["rev-list", "--count", "HEAD"]).unwrap();
    assert_eq!(count.trim(), "1");
    let subject = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%s"]).unwrap();
    assert_eq!(subject.trim(), "feat: add the generated file");
    let body = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%B"]).unwrap();
    assert!(body.contains(TAUQE_CO_AUTHOR_TRAILER), "{body}");
    let tracked = crate::git::cmd::run_git(Some(path), &["ls-files"]).unwrap();
    assert!(tracked.contains("ai_work.txt"), "{tracked}");
    assert!(!tracked.contains("user_work.txt"), "{tracked}");

    // The developer's pre-turn work was never part of the change set, so it
    // stays in the working tree exactly as it was before the turn.
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
    assert!(!transaction_marker_path(path).exists());
}

#[test]
fn test_marker_decodes_the_legacy_head_literal_as_an_unborn_base() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // Earlier revisions recorded the moving ref `HEAD` whenever `rev-parse`
    // failed on an unborn branch. That marker must keep decoding as the
    // typed unborn base, because an undecodable marker would hide an open
    // transaction.
    let marker = transaction_marker_path(path);
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(
        &marker,
        r#"{"created_checkpoint_commit":false,"previous_head":"HEAD"}"#,
    )
    .unwrap();

    let checkpoint = read_transaction_marker(path).expect("the legacy marker must decode");
    assert!(!checkpoint.created_checkpoint_commit);
    assert_eq!(checkpoint.base, TransactionBase::Unborn);
    // A marker written before the phase existed carries no `phase` key, and the
    // state it records is the settled one.
    assert_eq!(checkpoint.phase, TransactionPhase::Established);
    // A marker written before the writer was recorded proves nothing about
    // ownership, so it decodes as unattributed.
    assert_eq!(checkpoint.writer_pid, None);
}

#[test]
fn test_marker_round_trips_both_base_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A commit base serializes as the hash string under the preserved key.
    let born = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Commit("0123456789abcdef0123456789abcdef01234567".to_string()),
        phase: TransactionPhase::Established,
        writer_pid: None,
    };
    write_transaction_marker(path, &born).unwrap();
    assert_eq!(read_transaction_marker(path), Some(born));
    assert_eq!(
        fs::read_to_string(transaction_marker_path(path)).unwrap(),
        r#"{"created_checkpoint_commit":false,"previous_head":"0123456789abcdef0123456789abcdef01234567","phase":"established","writer_pid":null}"#
    );

    // An unborn base serializes as null under the same key.
    let unborn = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Unborn,
        phase: TransactionPhase::Established,
        writer_pid: None,
    };
    write_transaction_marker(path, &unborn).unwrap();
    assert_eq!(read_transaction_marker(path), Some(unborn));
    assert_eq!(
        fs::read_to_string(transaction_marker_path(path)).unwrap(),
        r#"{"created_checkpoint_commit":false,"previous_head":null,"phase":"established","writer_pid":null}"#
    );

    // The capturing phase carries its own wire value, so the document alone
    // tells an open capture apart from a settled transaction.
    let capturing = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Unborn,
        phase: TransactionPhase::Capturing,
        writer_pid: None,
    };
    write_transaction_marker(path, &capturing).unwrap();
    assert_eq!(read_transaction_marker(path), Some(capturing));
    assert_eq!(
        fs::read_to_string(transaction_marker_path(path)).unwrap(),
        r#"{"created_checkpoint_commit":false,"previous_head":null,"phase":"capturing","writer_pid":null}"#
    );
}

#[test]
fn test_clean_unborn_lifecycle_squashes_steps_into_one_root_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    // The tree is clean and the branch holds no commit, so the transaction
    // opens on the unborn base without creating a checkpoint commit.
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(!checkpoint.created_checkpoint_commit);
    assert_eq!(checkpoint.base, TransactionBase::Unborn);

    fs::write(path.join("step_one.txt"), "first step").unwrap();
    create_step_commit(
        path,
        &["step_one.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();
    fs::write(path.join("step_two.txt"), "second step").unwrap();
    create_step_commit(
        path,
        &["step_two.txt".to_string()],
        "apply step 2",
        GitHooks::Bypass,
    )
    .unwrap();

    let hash = finalize_ai_commit(
        path,
        &checkpoint,
        &["step_one.txt".to_string(), "step_two.txt".to_string()],
        "feat: add the generated files",
        GitHooks::Bypass,
    )
    .unwrap();
    assert!(!hash.is_empty());

    // Soft-resetting to the unborn base deleted the branch ref while the
    // index kept the staged work, so exactly one root commit carries the
    // summary and the trailer.
    let count = crate::git::cmd::run_git(Some(path), &["rev-list", "--count", "HEAD"]).unwrap();
    assert_eq!(count.trim(), "1");
    let subject = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%s"]).unwrap();
    assert_eq!(subject.trim(), "feat: add the generated files");
    let body = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%B"]).unwrap();
    assert!(body.contains(TAUQE_CO_AUTHOR_TRAILER), "{body}");
    assert!(path.join("step_one.txt").exists());
    assert!(path.join("step_two.txt").exists());
    assert!(!transaction_marker_path(path).exists());
}

#[test]
fn test_clean_unborn_recovery_returns_the_branch_to_unborn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(!checkpoint.created_checkpoint_commit);
    assert_eq!(checkpoint.base, TransactionBase::Unborn);

    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    // An untracked file the turn left behind must survive, exactly as it
    // would survive `reset --hard` on a born base.
    fs::write(path.join("scratch.txt"), "untracked scratch").unwrap();

    forget_marker_writer(path);
    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("unwound 1 step commit(s)"), "{message}");

    assert!(head_is_unborn(path));
    assert!(!path.join("ai_work.txt").exists());
    assert_eq!(
        fs::read_to_string(path.join("scratch.txt")).unwrap(),
        "untracked scratch"
    );
    assert!(read_transaction_marker(path).is_none());
}

#[test]
fn test_restore_checkpoint_on_an_unborn_base_removes_the_step_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert_eq!(checkpoint.base, TransactionBase::Unborn);

    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    restore_checkpoint(path, &checkpoint).unwrap();

    assert!(head_is_unborn(path));
    assert!(!path.join("ai_work.txt").exists());
    assert!(!transaction_marker_path(path).exists());
}

#[test]
fn test_unborn_recovery_refuses_when_a_foreign_commit_is_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert_eq!(checkpoint.base, TransactionBase::Unborn);

    // A developer commit without the Tauqe trailer is reachable from HEAD,
    // and the unborn-base proof covers the whole reachable history, so
    // recovery refuses instead of rewriting it.
    fs::write(path.join("foreign.txt"), "foreign work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "foreign.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();

    forget_marker_writer(path);
    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("Skipped crash recovery"), "{message}");

    // The repository is untouched and the marker survives for a later
    // attempt on the proof's terms.
    assert!(!head_is_unborn(path));
    assert!(path.join("foreign.txt").exists());
    assert!(read_transaction_marker(path).is_some());
}

#[test]
fn test_create_checkpoint_records_an_established_marker_on_a_clean_tree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(!checkpoint.created_checkpoint_commit);
    assert_eq!(checkpoint.phase, TransactionPhase::Established);
    // The marker names the process that opened the transaction, so a later
    // reader can tell a live writer from a crashed one.
    assert_eq!(checkpoint.writer_pid, Some(std::process::id()));

    // A clean tree opens the transaction with a single write, and the document
    // it publishes says so.
    let marker = read_transaction_marker(path).expect("the marker must be readable");
    assert_eq!(marker, checkpoint);
    let persisted = fs::read_to_string(transaction_marker_path(path)).unwrap();
    assert!(
        persisted.contains(r#""phase":"established""#),
        "{persisted}"
    );
    assert!(
        persisted.contains(&format!(r#""writer_pid":{}"#, std::process::id())),
        "{persisted}"
    );
}

#[test]
fn test_create_checkpoint_leaves_an_established_marker_after_capturing_a_dirty_tree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    fs::write(path.join("user_work.txt"), "uncommitted work").unwrap();

    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(checkpoint.created_checkpoint_commit);
    assert_eq!(checkpoint.phase, TransactionPhase::Established);
    // The capturing marker and the rewrite that settles it both name the same
    // writer, so a later reader can tell whether that process still runs.
    assert_eq!(checkpoint.writer_pid, Some(std::process::id()));

    // The capturing marker the capture opened with is rewritten once the commit
    // exists, so the document names the commit the capture created instead of
    // the base the capture started from.
    let marker = read_transaction_marker(path).expect("the marker must be readable");
    assert_eq!(marker, checkpoint);
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(marker.base, TransactionBase::Commit(head));
    let subject = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%s"]).unwrap();
    assert!(subject.trim().starts_with(CHECKPOINT_PREFIX), "{subject}");
}

#[test]
fn test_capturing_recovery_before_the_checkpoint_commit_keeps_the_working_tree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let base = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();

    // The capturing marker is durable while the work still sits uncommitted in
    // the working tree, which is the state a crash before the commit leaves.
    // The capture runs `git add -A` before it commits, so the index holds the
    // staged work of that interrupted command.
    ensure_tauqe_ignored(path).unwrap();
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(base.clone()),
            phase: TransactionPhase::Capturing,
            writer_pid: None,
        },
    )
    .unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "-A"]).unwrap();
    let staged =
        crate::git::cmd::run_git(Some(path), &["diff", "--cached", "--name-only"]).unwrap();
    assert!(staged.contains("user_work.txt"), "{staged}");

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(
        message.contains("no checkpoint commit was created"),
        "{message}"
    );

    // Every byte of the developer's work and the revision HEAD names survive.
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_after, base);
    // The index returns to the recorded base, so the crash leaves no staged
    // work behind and the tree reads exactly like the pre-turn shape.
    let staged_after =
        crate::git::cmd::run_git(Some(path), &["diff", "--cached", "--name-only"]).unwrap();
    assert!(
        staged_after.trim().is_empty(),
        "the interrupted capture must leave no staged work: {staged_after}"
    );
    let status = crate::git::cmd::run_git(Some(path), &["status", "--porcelain"]).unwrap();
    assert!(status.contains("?? user_work.txt"), "{status}");
    assert!(read_transaction_marker(path).is_none());
}

#[test]
fn test_capturing_recovery_before_the_checkpoint_commit_on_an_unborn_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();
    // The capture installs the ignore rules and announces itself before it
    // stages anything, so the fixture reproduces the crash window of a capture
    // that staged the whole work and never committed it.
    ensure_tauqe_ignored(path).unwrap();
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Unborn,
            phase: TransactionPhase::Capturing,
            writer_pid: None,
        },
    )
    .unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "-A"]).unwrap();
    let staged = crate::git::cmd::run_git(Some(path), &["ls-files"]).unwrap();
    assert!(staged.contains("user_work.txt"), "{staged}");

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(
        message.contains("no checkpoint commit was created"),
        "{message}"
    );

    // The branch still holds no commit and the developer's file stays untracked.
    assert!(head_is_unborn(path));
    // An unborn base has no commit to compare the index against, so the
    // recovery empties it and the captured file becomes untracked again.
    let staged_after = crate::git::cmd::run_git(Some(path), &["ls-files"]).unwrap();
    assert!(
        staged_after.trim().is_empty(),
        "the interrupted capture must leave no staged work: {staged_after}"
    );
    let status = crate::git::cmd::run_git(Some(path), &["status", "--porcelain"]).unwrap();
    assert!(status.contains("?? user_work.txt"), "{status}");
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
    assert!(read_transaction_marker(path).is_none());
}

#[test]
fn test_capturing_recovery_unwinds_the_checkpoint_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // The capture starts on a tracked file the developer already modified.
    fs::write(path.join("tracked.txt"), "committed content\n").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "tracked.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "chore: add the tracked file",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let base = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    fs::write(path.join("tracked.txt"), "developer edit\n").unwrap();

    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(base.clone()),
            phase: TransactionPhase::Capturing,
            writer_pid: None,
        },
    )
    .unwrap();

    // The capture committed the work but crashed before the marker recorded it.
    crate::git::cmd::run_git(Some(path), &["add", "-A"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        &format!("{CHECKPOINT_PREFIX} uncommitted user changes"),
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let subject = crate::git::cmd::run_git(Some(path), &["log", "-1", "--format=%s"]).unwrap();
    assert!(
        subject.trim().starts_with(CHECKPOINT_PREFIX),
        "the fixture must reproduce the crash window: {subject}"
    );

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(
        message.contains("Recovered interrupted capture"),
        "{message}"
    );

    // The checkpoint commit is unwound: HEAD returns to the recorded base and
    // the captured modification returns to the working tree unstaged.
    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_after, base);
    assert_eq!(
        fs::read_to_string(path.join("tracked.txt")).unwrap(),
        "developer edit\n"
    );
    let status = crate::git::cmd::run_git(Some(path), &["status", "--porcelain"]).unwrap();
    assert!(
        status.lines().any(|line| line == " M tracked.txt"),
        "the captured change must return unstaged: {status}"
    );
    assert!(read_transaction_marker(path).is_none());
}

#[test]
fn test_capturing_recovery_unwinds_the_root_checkpoint_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Unborn,
            phase: TransactionPhase::Capturing,
            writer_pid: None,
        },
    )
    .unwrap();

    // The capture committed the work as the repository root commit and crashed
    // before the marker recorded it.
    crate::git::cmd::run_git(Some(path), &["add", "-A"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        &format!("{CHECKPOINT_PREFIX} uncommitted user changes"),
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    assert!(
        !head_is_unborn(path),
        "the fixture must reproduce the crash window"
    );

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(
        message.contains("Recovered interrupted capture"),
        "{message}"
    );

    // The root checkpoint commit is unwound back to the unborn branch, and the
    // captured files return to the working tree untracked.
    assert!(head_is_unborn(path));
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
    let status = crate::git::cmd::run_git(Some(path), &["status", "--porcelain"]).unwrap();
    assert!(status.contains("?? user_work.txt"), "{status}");
    assert!(read_transaction_marker(path).is_none());
}

#[test]
fn test_capturing_recovery_refuses_when_head_is_a_foreign_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let base = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    fs::write(path.join("user_work.txt"), "uncommitted work").unwrap();
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(base),
            phase: TransactionPhase::Capturing,
            writer_pid: None,
        },
    )
    .unwrap();

    // A developer commit lands after the crash, so HEAD reproduces neither
    // recoverable state of the capture.
    fs::write(path.join("foreign.txt"), "foreign work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "foreign.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();

    let head_before = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("Skipped crash recovery"), "{message}");

    // The foreign commit is preserved, the marker survives, and the developer's
    // uncommitted work is untouched.
    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_before, head_after);
    assert!(path.join("foreign.txt").exists());
    assert!(path.join("user_work.txt").exists());
    assert!(read_transaction_marker(path).is_some());
}

#[test]
fn test_capturing_recovery_refuses_a_checkpoint_commit_of_another_base() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let recorded_base = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(recorded_base),
            phase: TransactionPhase::Capturing,
            writer_pid: None,
        },
    )
    .unwrap();

    // History moves before the capture commits, so the checkpoint commit that
    // reaches HEAD sits on a revision the marker never recorded.
    fs::write(path.join("foreign.txt"), "foreign work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "foreign.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    fs::write(path.join("captured.txt"), "captured work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "-A"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        &format!("{CHECKPOINT_PREFIX} uncommitted user changes"),
        false,
        GitHooks::Bypass,
    )
    .unwrap();

    let head_before = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("Skipped crash recovery"), "{message}");

    // A checkpoint commit of another base authorizes nothing, so history stays
    // untouched and the marker survives.
    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_before, head_after);
    assert!(path.join("captured.txt").exists());
    assert!(read_transaction_marker(path).is_some());
}

#[test]
fn test_clear_transaction_marker_removes_a_marker_and_tolerates_its_absence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A transaction opened on a clean tree leaves a durable marker behind.
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(!checkpoint.created_checkpoint_commit);
    assert!(read_transaction_marker(path).is_some());

    clear_transaction_marker(path).unwrap();
    assert!(!transaction_marker_path(path).exists());
    assert!(read_transaction_marker(path).is_none());

    // Clearing an absent marker is success, so a retried recovery never fails
    // on the state a completed removal leaves behind.
    clear_transaction_marker(path).unwrap();
    assert!(!transaction_marker_path(path).exists());
}

#[cfg(unix)]
#[test]
fn test_clear_transaction_marker_reports_a_denied_removal() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();
    create_checkpoint(path, GitHooks::Bypass).unwrap();
    let marker = transaction_marker_path(path);
    assert!(marker.exists());

    // The unlink is recorded in the `.tauqe` directory, so a directory that
    // refuses the write rejects the removal before any sync could run and the
    // marker survives for a later retry.
    let state_dir = path.join(".tauqe");
    fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let cleared = clear_transaction_marker(path);
    fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(cleared.is_err(), "a refused removal must be reported");
    assert!(marker.exists(), "a refused removal keeps the marker");

    // Once the directory admits the unlink again the marker is durably gone.
    clear_transaction_marker(path).unwrap();
    assert!(!marker.exists());
}

#[cfg(unix)]
#[test]
fn test_settled_recovery_reports_a_denied_marker_clear_and_retries() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A clean tree opens the transaction without a checkpoint commit, so the
    // settled proof range holds exactly the step commit below.
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(!checkpoint.created_checkpoint_commit);
    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();
    let marker = transaction_marker_path(path);
    assert!(marker.exists());

    // The marker records this test process as the writer, so the ownership gate
    // would skip the case; it verifies the settled unwind and the denied clear
    // instead.
    forget_marker_writer(path);

    // The destructive reset reaches the recorded base, but `.tauqe` refuses
    // the unlink that clears the marker afterwards.
    let state_dir = path.join(".tauqe");
    fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let recovery = recover_interrupted_workflow(path).unwrap();
    fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let message = recovery.unwrap_or_default();
    assert!(
        message.contains("transaction marker could not be cleared"),
        "{message}"
    );
    assert!(marker.exists(), "the surviving marker authorizes the retry");
    assert!(!path.join("ai_work.txt").exists());

    // The second run finds the tree already at the recorded base, so it only
    // clears the surviving marker.
    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_none(), "{recovery:?}");
    assert!(!marker.exists());
}

#[test]
fn test_recovery_refuses_a_transaction_whose_writer_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // The test process opens a real transaction and records a step commit, so
    // the marker describes exactly the live state a running turn holds.
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert_eq!(checkpoint.writer_pid, Some(std::process::id()));
    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();
    let head_before = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    let expected = format!(
        "the transaction writer (pid {}) is still running",
        std::process::id()
    );
    assert!(message.contains(&expected), "{message}");

    // Nothing of the live transaction was rewritten: the step commit is still
    // HEAD, its content is still on disk, and the marker survives for a later
    // startup that observes an exited writer.
    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_before, head_after);
    assert!(path.join("ai_work.txt").exists());
    assert!(read_transaction_marker(path).is_some());
}

#[cfg(unix)]
#[test]
fn test_recovery_of_a_dead_writer_runs_the_full_unwind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A transaction attributed to a writer that is provably gone is exactly
    // what a crashed turn leaves behind, so the gate lets the unwind run.
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(head.clone()),
            phase: TransactionPhase::Established,
            writer_pid: Some(dead_process_id()),
        },
    )
    .unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("unwound 1 step commit(s)"), "{message}");

    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_after, head);
    assert!(!path.join("ai_work.txt").exists());
    assert!(read_transaction_marker(path).is_none());
}

#[cfg(unix)]
#[test]
fn test_recovery_resumes_once_the_concurrent_writer_is_reaped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let mut child = live_child();
    let writer_pid = child.id();

    // The marker attributes an open transaction to a process that is still
    // running, which is the shape a second harness must never rewrite.
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(head),
            phase: TransactionPhase::Established,
            writer_pid: Some(writer_pid),
        },
    )
    .unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    let expected = format!("(pid {writer_pid}) is still running");
    assert!(message.contains(&expected), "{message}");
    assert!(read_transaction_marker(path).is_some());
    assert!(path.join("ai_work.txt").exists());

    // Once the writer is reaped, the surviving marker authorizes the unwind.
    let _ = child.kill();
    let _ = child.wait();
    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("unwound 1 step commit(s)"), "{message}");
    assert!(!path.join("ai_work.txt").exists());
    assert!(read_transaction_marker(path).is_none());
}

#[test]
fn test_sweep_skips_git_ignored_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A build or dependency tree is covered by the ignore rules, so no
    // admitted change set can leave a staging sibling inside it.
    fs::write(path.join(".gitignore"), "generated/\n").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", ".gitignore"]).unwrap();
    let ignored_dir = path.join("generated").join("deep");
    fs::create_dir_all(&ignored_dir).unwrap();
    let ignored_orphan = ignored_dir.join("artifact.rs.tauqe-tmp");
    fs::write(&ignored_orphan, "staged").unwrap();

    // A tree the ignore rules do not cover keeps the whole reach of the
    // sweep, at every depth.
    let visible_dir = path.join("src").join("nested");
    fs::create_dir_all(&visible_dir).unwrap();
    let visible_orphan = visible_dir.join("module.rs.tauqe-tmp");
    fs::write(&visible_orphan, "staged").unwrap();
    let root_orphan = path.join("draft.rs.tauqe-tmp");
    fs::write(&root_orphan, "staged").unwrap();

    let cleaned = cleanup_orphaned_staging_files(path);

    assert_eq!(cleaned, 2);
    assert!(!root_orphan.exists(), "the root level is swept");
    assert!(!visible_orphan.exists(), "an admitted tree is swept");
    assert!(ignored_orphan.exists(), "an ignored tree is never entered");
}

#[test]
fn test_create_ai_commit_keeps_a_checkpoint_commit_no_marker_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A commit whose subject carries the checkpoint prefix, written by hand or
    // kept by a developer after a refused recovery. No marker records it, so
    // nothing authorizes removing it from the branch.
    fs::write(path.join("kept.txt"), "kept work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "kept.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        &format!("{CHECKPOINT_PREFIX} kept by hand"),
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let kept = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // The isolated commit lands on top of the kept commit instead of deleting
    // it from the branch.
    fs::write(path.join("ai_work.txt"), "ai work").unwrap();
    let hash = create_ai_commit(
        path,
        &["ai_work.txt".to_string()],
        "feat: add the generated file",
        GitHooks::Bypass,
    )
    .unwrap();
    assert!(!hash.is_empty());

    let parent = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD^"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(parent, kept, "the kept commit must stay in the history");
    assert_eq!(
        fs::read_to_string(path.join("kept.txt")).unwrap(),
        "kept work"
    );
}

#[test]
fn test_unwind_refuses_a_checkpoint_commit_no_marker_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A temporary checkpoint commit sits at HEAD, exactly as a capture leaves
    // it, but none of the markers below records this commit.
    fs::write(path.join("captured.txt"), "captured work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "-A"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        &format!("{CHECKPOINT_PREFIX} uncommitted user changes"),
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let head_before = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let elsewhere = "0123456789abcdef0123456789abcdef01234567".to_string();
    // A settled marker records another commit as the checkpoint it created.
    let settled = CheckpointInfo {
        created_checkpoint_commit: true,
        base: TransactionBase::Commit(elsewhere.clone()),
        phase: TransactionPhase::Established,
        writer_pid: None,
    };
    assert!(!unwind_checkpoint_if_present(path, &settled).unwrap());
    // A capturing marker records a base that is not the parent of HEAD.
    let capturing = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Commit(elsewhere),
        phase: TransactionPhase::Capturing,
        writer_pid: None,
    };
    assert!(!unwind_checkpoint_if_present(path, &capturing).unwrap());
    // An unborn base holds no commit to descend from, so a commit with a
    // parent is never the root that marker describes.
    let unborn = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Unborn,
        phase: TransactionPhase::Established,
        writer_pid: None,
    };
    assert!(!unwind_checkpoint_if_present(path, &unborn).unwrap());

    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_before, head_after);
    assert_eq!(
        fs::read_to_string(path.join("captured.txt")).unwrap(),
        "captured work"
    );
}

#[test]
fn test_unwind_removes_the_checkpoint_commit_its_marker_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let base = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    fs::write(path.join("captured.txt"), "captured work").unwrap();
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(checkpoint.created_checkpoint_commit);

    // HEAD is exactly the checkpoint commit the marker records, so the marker
    // authorizes removing it.
    assert!(unwind_checkpoint_if_present(path, &checkpoint).unwrap());

    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_after, base);
    assert_eq!(
        fs::read_to_string(path.join("captured.txt")).unwrap(),
        "captured work",
        "the captured work returns to the working tree"
    );
}

#[test]
fn test_pinned_evidence_answers_about_the_pinned_commit_after_head_advances() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();
    let base = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // A dirty tree captures the developer's work in a checkpoint commit, which
    // is the commit the evidence below is pinned to.
    fs::write(path.join("user_work.txt"), "uncommitted work").unwrap();
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(checkpoint.created_checkpoint_commit);
    let pinned = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_ne!(pinned, base);

    // A commit lands afterwards, so HEAD no longer names the pinned commit.
    fs::write(path.join("later.txt"), "later work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "later.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();

    let deadline = GitDeadline::default();
    assert_eq!(
        commit_subject(path, &pinned, &deadline),
        format!("{CHECKPOINT_PREFIX} uncommitted user changes")
    );
    assert_eq!(commit_parent(path, &pinned, &deadline), Some(base.clone()));
    // The initial commit is the repository root, so it pins no parent.
    assert_eq!(commit_parent(path, &base, &deadline), None);

    // The readers answer about the commit they are handed, never about the
    // moving `HEAD`, so the drifted commit is read by its own hash.
    let drifted = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_ne!(drifted, pinned);
    assert_eq!(
        commit_subject(path, &drifted, &deadline),
        "fix: developer commit"
    );
    assert_eq!(commit_parent(path, &drifted, &deadline), Some(pinned));
}

#[test]
fn test_assert_head_pinned_detects_a_moved_head() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();
    let pinned = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let deadline = GitDeadline::default();

    // The identity a proof settled on is accepted while HEAD still names it.
    assert_head_pinned(path, &pinned, &deadline).unwrap();

    fs::write(path.join("later.txt"), "later work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "later.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();

    // A commit that landed after the pin refuses the rewrite instead of being
    // silently dropped.
    let error =
        assert_head_pinned(path, &pinned, &deadline).expect_err("a moved HEAD must be refused");
    assert!(error.to_string().contains("HEAD moved"), "{error}");

    // A branch that holds no commit resolves nothing, so the pin fails too.
    let unborn = tempfile::tempdir().unwrap();
    let unborn_path = unborn.path();
    crate::git::state::init_repository(Some(unborn_path), false, GitHooks::Bypass).unwrap();
    assert!(
        assert_head_pinned(unborn_path, &pinned, &deadline).is_err(),
        "an unborn HEAD resolves nothing to re-assert"
    );
}

#[test]
fn test_pinned_unwind_refuses_a_head_that_moved_after_the_proof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A dirty tree captures the developer's work in a checkpoint commit, which
    // the unwind proof pins before the destructive step runs.
    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "uncommitted work").unwrap();
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(checkpoint.created_checkpoint_commit);

    let deadline = GitDeadline::default();
    let pinned = resolve_pinned_unwind(path, &checkpoint, &deadline)
        .expect("the checkpoint commit is authorized for the unwind");

    // A commit lands between the proof and the destructive step.
    fs::write(path.join("later.txt"), "later work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "later.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let drifted = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let staged_before =
        crate::git::cmd::run_git(Some(path), &["diff", "--cached", "--name-only"]).unwrap();

    let error = unwind_pinned_head(path, &pinned, &deadline)
        .expect_err("a HEAD that moved after the proof must refuse the unwind");
    assert!(error.to_string().contains("HEAD moved"), "{error}");
    assert!(
        error.downcast_ref::<HeadDrift>().is_some(),
        "the refusal must stay typed so recovery can report its skip diagnostic"
    );

    // The drifted commit survives at HEAD, and no history, index entry, or
    // working-tree byte was rewritten.
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head, drifted);
    assert_eq!(
        fs::read_to_string(path.join("later.txt")).unwrap(),
        "later work"
    );
    assert_eq!(fs::read_to_string(&user_file).unwrap(), "uncommitted work");
    assert_eq!(
        crate::git::cmd::run_git(Some(path), &["diff", "--cached", "--name-only"]).unwrap(),
        staged_before
    );
    assert!(read_transaction_marker(path).is_some());
}

#[test]
fn test_pinned_unborn_return_refuses_a_head_that_moved_after_the_listing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    // The turn records its work in a step commit on a branch that held no
    // commit before it, which is the state the return below pins.
    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    let deadline = GitDeadline::default();
    let pinned = resolve_pinned_unborn_return(path, &deadline)
        .unwrap()
        .expect("the branch holds a commit to return from");
    assert_eq!(pinned.tracked, vec!["ai_work.txt".to_string()]);

    // A commit lands between the listing and the destructive step.
    fs::write(path.join("later.txt"), "later work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "later.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let drifted = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let error = return_pinned_to_unborn(path, &pinned, &deadline)
        .expect_err("a HEAD that moved after the listing must refuse the return");
    assert!(error.to_string().contains("HEAD moved"), "{error}");

    // The branch still holds the drifted commit, no path was removed, and the
    // index still records both files.
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head, drifted);
    assert_eq!(
        fs::read_to_string(path.join("ai_work.txt")).unwrap(),
        "ai step modification"
    );
    assert_eq!(
        fs::read_to_string(path.join("later.txt")).unwrap(),
        "later work"
    );
    let tracked = crate::git::cmd::run_git(Some(path), &["ls-files"]).unwrap();
    assert!(tracked.contains("ai_work.txt"), "{tracked}");
    assert!(tracked.contains("later.txt"), "{tracked}");
}

/// Writes an executable `pre-commit` hook into the repository.
#[cfg(unix)]
fn install_pre_commit_hook(dir: &std::path::Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;

    let hooks = dir.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    fs::write(&hook, script).unwrap();
    let mut perms = fs::metadata(&hook).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook, perms).unwrap();
}

/// A checkpoint whose commit runs a repository hook must honour the budget it
/// was given: the hook is arbitrary user code and can wait forever.
#[cfg(unix)]
#[test]
fn test_create_checkpoint_with_deadline_bounds_the_hook_the_commit_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A dirty tree forces the capture through a commit, which the run policy
    // lets the repository hook execute.
    fs::write(path.join("user_work.txt"), "uncommitted work").unwrap();
    install_pre_commit_hook(path, "#!/bin/sh\nsleep 30\n");

    let started = std::time::Instant::now();
    let deadline = GitDeadline::with_timeout(1);
    let error = create_checkpoint_with_deadline(path, GitHooks::Run, &deadline)
        .expect_err("the hook outlives the budget");

    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("exceeded its wall-clock budget"),
        "error: {rendered}"
    );
    assert!(rendered.contains("git commit"), "error: {rendered}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "the budget did not release the caller: {:?}",
        started.elapsed()
    );
}

#[cfg(unix)]
#[test]
fn test_recovery_reclaims_the_index_lock_of_a_dead_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    // The writer died between creating the index lock and removing it, which
    // is exactly the state that blocks every later Git command.
    let lock = path.join(".git").join("index.lock");
    fs::write(&lock, "").unwrap();

    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(head),
            phase: TransactionPhase::Established,
            writer_pid: Some(dead_process_id()),
        },
    )
    .unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("1 orphaned Git index lock"), "{message}");
    assert!(!lock.exists());
}

#[test]
fn test_recovery_keeps_the_index_lock_of_a_writer_that_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // The marker names this test process, so the transaction it opened still
    // owns everything it recorded, the index lock included.
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert_eq!(checkpoint.writer_pid, Some(std::process::id()));
    let lock = path.join(".git").join("index.lock");
    fs::write(&lock, "").unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("is still running"), "{message}");
    assert!(lock.exists());
}

#[test]
fn test_recovery_keeps_the_index_lock_no_marker_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let lock = path.join(".git").join("index.lock");
    fs::write(&lock, "").unwrap();

    // A marker written by an earlier revision names no writer, so it proves
    // nothing about the lock and the previous behavior is kept.
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Commit(head),
            phase: TransactionPhase::Established,
            writer_pid: None,
        },
    )
    .unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_none(), "{recovery:?}");
    assert!(lock.exists());
}

#[test]
fn test_recover_stale_index_lock_reclaims_only_a_lock_that_outlived_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();
    let lock = path.join(".git").join("index.lock");
    fs::write(&lock, "").unwrap();

    // A lock younger than the budget of a running invocation may belong to
    // that invocation, so it is kept.
    assert!(!recover_stale_index_lock(
        path,
        &GitDeadline::with_timeout(600)
    ));
    assert!(lock.exists());

    // Once the grace period has passed, a zero budget proves that no
    // invocation can still own the lock.
    std::thread::sleep(TERMINATION_GRACE + std::time::Duration::from_millis(50));
    assert!(recover_stale_index_lock(
        path,
        &GitDeadline::with_timeout(0)
    ));
    assert!(!lock.exists());
}

/// Corrupts the commit object `rev` resolves to.
///
/// The revision keeps resolving to its hash — `rev-parse` answers from the ref
/// and only checks that the object file exists — while every command that
/// parses the object fails. A recovery proof that depends on listing such a
/// revision therefore observes a resolvable `HEAD` and an unestablished range.
fn corrupt_commit_object(dir: &std::path::Path, rev: &str) {
    let hash = crate::git::cmd::run_git(Some(dir), &["rev-parse", rev])
        .unwrap()
        .trim()
        .to_string();
    let (prefix, rest) = hash.split_at(2);
    let object = format!("objects/{prefix}/{rest}");
    let relative =
        crate::git::cmd::run_git(Some(dir), &["rev-parse", "--git-path", object.as_str()])
            .unwrap()
            .trim()
            .to_string();
    let object_path = dir.join(relative);
    assert!(
        object_path.is_file(),
        "the fixture must address a loose commit object: {}",
        object_path.display()
    );
    // Git stores loose objects read-only, so the corrupt payload replaces the
    // file entry instead of overwriting it: withdrawing the entry takes the
    // directory permission, not a write bit on the object itself.
    fs::remove_file(&object_path).unwrap();
    fs::write(&object_path, "corrupted commit object").unwrap();
}

#[test]
fn test_settled_unborn_proof_clears_the_marker_of_a_branch_still_unborn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    // The marker records a transaction that opened on an unborn branch and the
    // branch still holds no commit, so the proof range is legitimately empty
    // and needs no listing at all.
    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Unborn,
            phase: TransactionPhase::Established,
            writer_pid: None,
        },
    )
    .unwrap();

    let recovery = recover_interrupted_workflow(path).unwrap();
    assert!(recovery.is_none(), "{recovery:?}");

    assert!(head_is_unborn(path));
    assert!(read_transaction_marker(path).is_none());
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
}

#[test]
fn test_settled_proof_refuses_when_the_range_cannot_be_listed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // The marker describes a transaction that opened on an unborn branch while
    // the branch now holds a commit, so the proof has to list it. Corrupting
    // that commit keeps HEAD resolvable while `git log` fails to read it.
    let head_before = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "valuable uncommitted edits").unwrap();
    write_transaction_marker(
        path,
        &CheckpointInfo {
            created_checkpoint_commit: false,
            base: TransactionBase::Unborn,
            phase: TransactionPhase::Established,
            writer_pid: None,
        },
    )
    .unwrap();
    corrupt_commit_object(path, "HEAD");

    // The fixture reproduces the exposed state exactly: HEAD still resolves,
    // while the listing of its history fails.
    let resolves =
        crate::git::cmd::run_git(Some(path), &["rev-parse", "--verify", "--quiet", "HEAD"]);
    assert!(resolves.is_ok(), "HEAD must keep resolving: {resolves:?}");
    assert!(
        crate::git::cmd::run_git(Some(path), &["log", "--format=%H", "HEAD"]).is_err(),
        "the listing of a corrupted history must fail"
    );

    let recovery = recover_interrupted_workflow(path).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(message.contains("Skipped crash recovery"), "{message}");
    assert!(message.contains("could not be listed"), "{message}");

    // A proof that cannot be established authorizes nothing: the marker
    // survives for a later attempt, and neither the branch nor the working
    // tree was rewritten.
    assert!(read_transaction_marker(path).is_some());
    let head_after = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_before, head_after);
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "valuable uncommitted edits"
    );
}

/// The reclamation report of a recovery case that found no orphaned artifact.
fn nothing_reclaimed() -> ReclaimedOrphans {
    ReclaimedOrphans {
        staging_files: 0,
        index_lock: false,
    }
}

#[test]
fn test_settled_proof_refuses_a_born_reset_when_the_repository_advanced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    // A clean tree opens the transaction without a checkpoint commit, and the
    // step commit is the verified progress of the interrupted turn.
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert!(!checkpoint.created_checkpoint_commit);
    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    let deadline = GitDeadline::default();
    let pinned = resolve_head(path, &deadline);
    assert!(
        pinned.is_some(),
        "the proof is settled over a resolved commit"
    );

    // A commit lands between the proof and the action it licenses.
    fs::write(path.join("later.txt"), "later work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "later.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let drifted = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let staged_before =
        crate::git::cmd::run_git(Some(path), &["diff", "--cached", "--name-only"]).unwrap();

    let refused = settle_proved_transaction(
        path,
        &checkpoint,
        pinned.as_deref(),
        1,
        nothing_reclaimed(),
        &deadline,
    )
    .unwrap();

    let message = refused.unwrap_or_default();
    assert!(message.contains("Skipped crash recovery"), "{message}");
    assert!(message.contains("HEAD moved"), "{message}");

    // The commit that landed survives, and no history, index entry, or
    // working-tree byte was rewritten.
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head, drifted);
    assert_eq!(
        fs::read_to_string(path.join("later.txt")).unwrap(),
        "later work"
    );
    assert_eq!(
        fs::read_to_string(path.join("ai_work.txt")).unwrap(),
        "ai step modification"
    );
    assert_eq!(
        crate::git::cmd::run_git(Some(path), &["diff", "--cached", "--name-only"]).unwrap(),
        staged_before
    );
    assert!(read_transaction_marker(path).is_some());
}

#[test]
fn test_settled_proof_refuses_the_unborn_return_when_the_repository_advanced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    init_unborn_repository(path);

    // The transaction opens on the unborn base, and the step commit below is
    // the verified progress that would be returned to the unborn state.
    let checkpoint = create_checkpoint(path, GitHooks::Bypass).unwrap();
    assert_eq!(checkpoint.base, TransactionBase::Unborn);
    assert!(!checkpoint.created_checkpoint_commit);
    fs::write(path.join("ai_work.txt"), "ai step modification").unwrap();
    create_step_commit(
        path,
        &["ai_work.txt".to_string()],
        "apply step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    let deadline = GitDeadline::default();
    let pinned = resolve_head(path, &deadline);
    assert!(pinned.is_some(), "the step commit resolves the branch");

    fs::write(path.join("later.txt"), "later work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "later.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let drifted = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let refused = settle_proved_transaction(
        path,
        &checkpoint,
        pinned.as_deref(),
        1,
        nothing_reclaimed(),
        &deadline,
    )
    .unwrap();

    let message = refused.unwrap_or_default();
    assert!(message.contains("Skipped crash recovery"), "{message}");
    assert!(message.contains("HEAD moved"), "{message}");

    // The branch keeps the commit that landed, and the paths the transaction
    // tracked stay in the working tree instead of being removed.
    assert!(!head_is_unborn(path));
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head, drifted);
    assert_eq!(
        fs::read_to_string(path.join("ai_work.txt")).unwrap(),
        "ai step modification"
    );
    assert_eq!(
        fs::read_to_string(path.join("later.txt")).unwrap(),
        "later work"
    );
    assert!(read_transaction_marker(path).is_some());
}

#[test]
fn test_capturing_recovery_refuses_the_unstage_when_the_repository_advanced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    let base = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let user_file = path.join("user_work.txt");
    fs::write(&user_file, "original uncommitted work").unwrap();

    // The capturing marker is durable while the work still sits uncommitted in
    // the working tree, and the interrupted capture already staged it.
    ensure_tauqe_ignored(path).unwrap();
    let checkpoint = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Commit(base.clone()),
        phase: TransactionPhase::Capturing,
        writer_pid: None,
    };
    write_transaction_marker(path, &checkpoint).unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "-A"]).unwrap();

    let deadline = GitDeadline::default();
    let pinned = resolve_head(path, &deadline);
    assert_eq!(pinned.as_deref(), Some(base.as_str()));

    // A commit lands after the crash, so HEAD no longer names the recorded base.
    fs::write(path.join("later.txt"), "later work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "-A"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let drifted = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let status_before = crate::git::cmd::run_git(Some(path), &["status", "--porcelain"]).unwrap();

    let refused = recover_capturing_transaction_pinned(
        path,
        &checkpoint,
        pinned.as_deref(),
        nothing_reclaimed(),
        &deadline,
    )
    .unwrap();

    let message = refused.unwrap_or_default();
    assert!(message.contains("Skipped crash recovery"), "{message}");
    assert!(message.contains("HEAD moved"), "{message}");

    // The commit that landed survives, the developer's bytes stay in the
    // working tree, and the index is left exactly as the crash left it.
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head, drifted);
    assert_eq!(
        fs::read_to_string(&user_file).unwrap(),
        "original uncommitted work"
    );
    assert_eq!(
        crate::git::cmd::run_git(Some(path), &["status", "--porcelain"]).unwrap(),
        status_before
    );
    assert!(read_transaction_marker(path).is_some());
}

#[test]
fn test_capturing_recovery_refuses_the_unwind_when_the_repository_advanced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();

    fs::write(path.join("tracked.txt"), "committed content\n").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "tracked.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "chore: add the tracked file",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let base = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    fs::write(path.join("tracked.txt"), "developer edit\n").unwrap();

    let checkpoint = CheckpointInfo {
        created_checkpoint_commit: false,
        base: TransactionBase::Commit(base),
        phase: TransactionPhase::Capturing,
        writer_pid: None,
    };
    write_transaction_marker(path, &checkpoint).unwrap();

    // The capture committed the work but crashed before the marker recorded it.
    crate::git::cmd::run_git(Some(path), &["add", "-A"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        &format!("{CHECKPOINT_PREFIX} uncommitted user changes"),
        false,
        GitHooks::Bypass,
    )
    .unwrap();

    let deadline = GitDeadline::default();
    let pinned = resolve_head(path, &deadline);
    let checkpoint_commit = pinned.clone().expect("the checkpoint commit resolves");

    // A commit lands on top of the checkpoint commit after the crash.
    fs::write(path.join("later.txt"), "later work").unwrap();
    crate::git::cmd::run_git(Some(path), &["add", "later.txt"]).unwrap();
    crate::git::cmd::commit_with_developer_author(
        path,
        "fix: developer commit",
        false,
        GitHooks::Bypass,
    )
    .unwrap();
    let drifted = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let refused = recover_capturing_transaction_pinned(
        path,
        &checkpoint,
        pinned.as_deref(),
        nothing_reclaimed(),
        &deadline,
    )
    .unwrap();

    let message = refused.unwrap_or_default();
    assert!(message.contains("Skipped crash recovery"), "{message}");
    assert!(message.contains("HEAD moved"), "{message}");

    // The checkpoint commit and the commit that landed on top of it both stay
    // reachable, and the marker authorizes a later startup to redo the proof.
    let head = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head, drifted);
    let parent = crate::git::cmd::run_git(Some(path), &["rev-parse", "HEAD^"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(parent, checkpoint_commit);
    assert_eq!(
        fs::read_to_string(path.join("tracked.txt")).unwrap(),
        "developer edit\n"
    );
    assert!(read_transaction_marker(path).is_some());
}

/// The installed exclude rules cover the artifacts the staging policy writes.
///
/// The candidate names carry the marker constants themselves, so a rule that
/// restated a suffix on its own would stop covering a sibling the atomic-write
/// policy renamed, and the orphan of a crashed write would reach the next
/// checkpoint commit.
#[test]
fn test_staging_sibling_names_are_ignored_by_the_installed_rules() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    crate::git::state::init_repository(Some(path), true, GitHooks::Bypass).unwrap();
    ensure_tauqe_ignored(path).unwrap();

    let pid = std::process::id();
    let candidates = vec![
        format!("source.rs{STAGING_TEMP_MARKER}.{pid}.1"),
        format!("source.rs{STAGING_BACKUP_MARKER}.{pid}.2"),
        // The earlier fixed-name policy left siblings without a writer
        // attribution, and crash recovery still reclaims them.
        format!("source.rs{STAGING_TEMP_MARKER}"),
        format!("source.rs{STAGING_BACKUP_MARKER}"),
    ];

    let ignored = crate::git::cmd::run_git_check_ignore(path, &candidates).unwrap();
    for candidate in &candidates {
        assert!(
            ignored.iter().any(|entry| entry == candidate),
            "{candidate} must be covered by the installed exclude rules: {ignored:?}"
        );
    }
}
