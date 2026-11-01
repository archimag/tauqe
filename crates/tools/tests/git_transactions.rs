//! End-to-end Git transaction tests.
//!
//! Every scenario drives the real `git` executable through the substrate
//! functions, so the checkpoint, step-commit, squash and rollback guarantees are
//! verified against the repository state a bounded turn really produces.

use tauqe_tools::atomic_write::STAGING_TEMP_MARKER;
use tauqe_tools::git::checkpoint::{
    create_ai_commit, create_checkpoint, create_step_commit, ensure_tauqe_ignored,
    finalize_ai_commit, recover_interrupted_workflow, restore_checkpoint,
};
use tauqe_tools::git::cmd::{run_git, run_git_check_ignore};
use tauqe_tools::git::diff::{
    find_last_non_ai_commit, get_commits_ahead, get_cumulative_diff, squash_to_single_commit,
};
use tauqe_tools::git::state::detect_upstream_branch;
use tauqe_tools::git::GitHooks;
/// Process id of an already reaped child, which is provably no longer running.
///
/// `git` is a hard prerequisite of this suite, so the dead writer a reclamation
/// case needs is a short-lived child of this process.
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
#[test]
fn test_detect_upstream_branch_and_squash_to_single_commit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_git(Some(root), &["init", "-b", "master"]).unwrap();
    run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
    run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();

    std::fs::write(root.join("base.txt"), "base content\n").unwrap();
    run_git(Some(root), &["add", "base.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "chore: initial commit"]).unwrap();

    // Create and switch to feature branch
    run_git(Some(root), &["checkout", "-b", "feature/my-task"]).unwrap();

    // Upstream should resolve to master (convention)
    let detected = detect_upstream_branch(root, None);
    assert_eq!(detected.as_deref(), Some("master"));

    // If explicitly configured, respects configuration
    let configured = detect_upstream_branch(root, Some("master"));
    assert_eq!(configured.as_deref(), Some("master"));

    // Make commit 1
    std::fs::write(root.join("f1.txt"), "part 1\n").unwrap();
    run_git(Some(root), &["add", "f1.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "feat: part 1"]).unwrap();

    // Make commit 2
    std::fs::write(root.join("f2.txt"), "part 2\n").unwrap();
    run_git(Some(root), &["add", "f2.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "feat: part 2"]).unwrap();

    // Check ahead commits
    let ahead = get_commits_ahead(root, "master").unwrap();
    assert_eq!(ahead.len(), 2);
    assert_eq!(ahead[0].subject, "feat: part 2");
    assert_eq!(ahead[1].subject, "feat: part 1");

    // Cumulative diff contains both files
    let diff = get_cumulative_diff(root, "master").unwrap();
    assert!(diff.contains("f1.txt"));
    assert!(diff.contains("f2.txt"));

    // Squash into a single commit
    let squashed_hash = squash_to_single_commit(
        root,
        "master",
        "feat(task): implement complete task feature",
        GitHooks::Bypass,
    )
    .unwrap();
    assert!(!squashed_hash.is_empty());

    // Now ahead of master is exactly 1 commit
    let after_ahead = get_commits_ahead(root, "master").unwrap();
    assert_eq!(after_ahead.len(), 1);
    assert_eq!(
        after_ahead[0].subject,
        "feat(task): implement complete task feature"
    );

    // Working tree files are present and match
    assert_eq!(
        std::fs::read_to_string(root.join("f1.txt")).unwrap(),
        "part 1\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("f2.txt")).unwrap(),
        "part 2\n"
    );

    // Verify that the squash commit has no AI co-author trailer.
    let squashed_trailers = run_git(
        Some(root),
        &["log", "-1", "--format=%(trailers:key=Co-authored-by)"],
    )
    .unwrap();
    assert!(!squashed_trailers.contains("Tauqe AI"));
}

#[test]
fn test_find_last_non_ai_commit_and_authorship() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_git(Some(root), &["init", "-b", "master"]).unwrap();
    run_git(Some(root), &["config", "user.name", "Alice Developer"]).unwrap();
    run_git(Some(root), &["config", "user.email", "alice@dev.org"]).unwrap();

    // 1. Initial developer commit
    std::fs::write(root.join("init.txt"), "hello").unwrap();
    run_git(Some(root), &["add", "init.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "chore: init"]).unwrap();
    let init_hash = run_git(Some(root), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // No AI commits yet
    assert_eq!(find_last_non_ai_commit(root).unwrap(), None);

    // 2. AI step 1
    std::fs::write(root.join("step1.txt"), "step1").unwrap();
    create_ai_commit(
        root,
        &["step1.txt".to_string()],
        "feat: add step 1",
        GitHooks::Bypass,
    )
    .unwrap();

    // 3. AI step 2
    std::fs::write(root.join("step2.txt"), "step2").unwrap();
    create_ai_commit(
        root,
        &["step2.txt".to_string()],
        "feat: add step 2",
        GitHooks::Bypass,
    )
    .unwrap();

    // Check author and co-author on AI commit
    let author = run_git(Some(root), &["log", "-1", "--format=%an <%ae>"]).unwrap();
    assert_eq!(author.trim(), "Alice Developer <alice@dev.org>");
    let trailers = run_git(
        Some(root),
        &["log", "-1", "--format=%(trailers:key=Co-authored-by)"],
    )
    .unwrap();
    assert!(trailers.contains("Tauqe AI"));

    // Last non-AI commit should point to init_hash
    let base = find_last_non_ai_commit(root).unwrap();
    assert_eq!(base.as_deref(), Some(init_hash.as_str()));

    // 4. Developer makes a manual fix on top
    std::fs::write(root.join("fix.txt"), "manual fix").unwrap();
    run_git(Some(root), &["add", "fix.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "fix: developer manual tweak"]).unwrap();

    // Base should STILL point back to init_hash before the AI session
    let base_after_manual = find_last_non_ai_commit(root).unwrap();
    assert_eq!(base_after_manual.as_deref(), Some(init_hash.as_str()));

    // 5. Squash all commits ahead of base
    let ahead = get_commits_ahead(root, &init_hash).unwrap();
    assert_eq!(ahead.len(), 3);

    let squashed_hash =
        squash_to_single_commit(root, &init_hash, "feat: complete feature", GitHooks::Bypass)
            .unwrap();
    assert!(!squashed_hash.is_empty());

    // Squashed commit has Alice as author and NO Co-authored-by trailer
    let squashed_author = run_git(Some(root), &["log", "-1", "--format=%an <%ae>"]).unwrap();
    assert_eq!(squashed_author.trim(), "Alice Developer <alice@dev.org>");
    let squashed_trailers = run_git(
        Some(root),
        &["log", "-1", "--format=%(trailers:key=Co-authored-by)"],
    )
    .unwrap();
    assert!(!squashed_trailers.contains("Tauqe AI"));

    // No more AI commits in history
    assert_eq!(find_last_non_ai_commit(root).unwrap(), None);
}

#[test]
fn test_checkpoint_create_and_restore_clean_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_git(Some(root), &["init"]).unwrap();
    run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
    run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
    std::fs::write(root.join("file.txt"), "initial").unwrap();
    run_git(Some(root), &["add", "file.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "init"]).unwrap();

    let cp = create_checkpoint(root, GitHooks::Bypass).unwrap();
    assert!(!cp.created_checkpoint_commit);

    std::fs::write(root.join("file.txt"), "modified").unwrap();
    std::fs::write(root.join("new.txt"), "new").unwrap();

    restore_checkpoint(root, &cp).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "initial"
    );
    assert!(!root.join("new.txt").exists());
}

#[test]
fn test_checkpoint_create_and_restore_dirty_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_git(Some(root), &["init"]).unwrap();
    run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
    run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
    std::fs::write(root.join("file.txt"), "initial").unwrap();
    run_git(Some(root), &["add", "file.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "init"]).unwrap();

    std::fs::write(root.join("user.txt"), "user dirty").unwrap();

    let cp = create_checkpoint(root, GitHooks::Bypass).unwrap();
    assert!(cp.created_checkpoint_commit);

    std::fs::write(root.join("file.txt"), "ai modified").unwrap();
    std::fs::write(root.join("ai_new.txt"), "ai new").unwrap();

    restore_checkpoint(root, &cp).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "initial"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("user.txt")).unwrap(),
        "user dirty"
    );
    assert!(!root.join("ai_new.txt").exists());
}

#[test]
fn test_step_commits_squash_into_single_ai_commit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_git(Some(root), &["init"]).unwrap();
    run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
    run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
    std::fs::write(root.join("file1.txt"), "initial 1").unwrap();
    std::fs::write(root.join("file2.txt"), "initial 2").unwrap();
    run_git(Some(root), &["add", "-A"]).unwrap();
    run_git(Some(root), &["commit", "-m", "init"]).unwrap();

    let cp = create_checkpoint(root, GitHooks::Bypass).unwrap();

    // Step 1
    std::fs::write(root.join("file1.txt"), "modified 1").unwrap();
    create_step_commit(root, &["file1.txt".to_string()], "step 1", GitHooks::Bypass).unwrap();

    // Step 2
    std::fs::write(root.join("file2.txt"), "modified 2").unwrap();
    create_step_commit(root, &["file2.txt".to_string()], "step 2", GitHooks::Bypass).unwrap();

    // Intermediate log has 3 commits (init, step 1, step 2)
    let count: usize = run_git(Some(root), &["rev-list", "--count", "HEAD"])
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(count, 3);

    // Finalize (squash)
    let files = vec!["file1.txt".to_string(), "file2.txt".to_string()];
    let final_hash =
        finalize_ai_commit(root, &cp, &files, "Squashed AI feature", GitHooks::Bypass).unwrap();
    assert!(!final_hash.is_empty());

    // Final log has exactly 2 commits (init + squashed AI commit)
    let final_count: usize = run_git(Some(root), &["rev-list", "--count", "HEAD"])
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(final_count, 2);

    let subject = run_git(Some(root), &["log", "-1", "--format=%s"]).unwrap();
    assert_eq!(subject.trim(), "Squashed AI feature");
    assert_eq!(
        std::fs::read_to_string(root.join("file1.txt")).unwrap(),
        "modified 1"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("file2.txt")).unwrap(),
        "modified 2"
    );
}

#[test]
fn test_step_commits_rollback_cleanly_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_git(Some(root), &["init"]).unwrap();
    run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
    run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
    std::fs::write(root.join("file.txt"), "initial").unwrap();
    run_git(Some(root), &["add", "file.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "init"]).unwrap();

    std::fs::write(root.join("user.txt"), "user dirty").unwrap();

    let cp = create_checkpoint(root, GitHooks::Bypass).unwrap();
    assert!(cp.created_checkpoint_commit);

    std::fs::write(root.join("file.txt"), "step 1 mod").unwrap();
    create_step_commit(root, &["file.txt".to_string()], "step 1", GitHooks::Bypass).unwrap();

    std::fs::write(root.join("file.txt"), "step 2 mod").unwrap();
    create_step_commit(root, &["file.txt".to_string()], "step 2", GitHooks::Bypass).unwrap();

    // Restore checkpoint on failure
    restore_checkpoint(root, &cp).unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "initial"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("user.txt")).unwrap(),
        "user dirty"
    );

    let log = run_git(Some(root), &["log", "--oneline"]).unwrap();
    assert!(!log.contains("tauqe-step:"));
}

#[cfg(unix)]
#[test]
fn test_crashed_atomic_write_orphans_are_ignored_and_reclaimed_by_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_git(Some(root), &["init", "-b", "master"]).unwrap();
    run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
    run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
    std::fs::write(root.join("base.txt"), "base content\n").unwrap();
    run_git(Some(root), &["add", "base.txt"]).unwrap();
    run_git(Some(root), &["commit", "-m", "chore: initial commit"]).unwrap();

    let pid = dead_process_id();

    // The sibling a crashed `write_atomic` leaves next to a worktree target.
    std::fs::create_dir_all(root.join("src")).unwrap();
    let worktree_orphan_name = format!("src/notes.rs{STAGING_TEMP_MARKER}.{pid}.1");
    let worktree_orphan = root.join(&worktree_orphan_name);
    std::fs::write(&worktree_orphan, "staged content").unwrap();

    // The same staging policy applied to a document inside the state directory.
    std::fs::create_dir_all(root.join(".tauqe").join("plans")).unwrap();
    let state_orphan_name = format!(".tauqe/plans/plan-a.json{STAGING_TEMP_MARKER}.{pid}.2");
    let state_orphan = root.join(&state_orphan_name);
    std::fs::write(&state_orphan, "{\"id\":\"plan-a\"}").unwrap();
    let history = root.join(".tauqe").join("history.jsonl");
    std::fs::write(&history, "{\"type\":\"turn\"}\n").unwrap();

    ensure_tauqe_ignored(root).unwrap();

    // The installed exclude rule covers the unified staging suffix, so the next
    // `git add -A` checkpoint can never sweep the worktree orphan into a commit.
    let candidates = std::slice::from_ref(&worktree_orphan_name);
    let ignored = run_git_check_ignore(root, candidates).unwrap();
    assert!(
        ignored.iter().any(|entry| entry == &worktree_orphan_name),
        "the staging suffix must be ignored by Git: {ignored:?}"
    );

    let recovery = recover_interrupted_workflow(root).unwrap();
    let message = recovery.unwrap_or_default();
    assert!(
        message.contains("Cleaned up 2 orphaned staging artifact(s)"),
        "{message}"
    );

    assert!(!worktree_orphan.exists());
    assert!(!state_orphan.exists());
    let history_content = std::fs::read_to_string(&history).unwrap();
    assert_eq!(history_content, "{\"type\":\"turn\"}\n");
}
