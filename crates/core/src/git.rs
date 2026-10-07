pub mod checkpoint;
pub mod cmd;
pub mod diff;
pub mod state;

pub use checkpoint::{
    create_ai_commit, create_checkpoint, create_step_commit, finalize_ai_commit,
    is_tauqe_commit, restore_checkpoint, undo_last_ai_commit, CheckpointInfo,
    CHECKPOINT_PREFIX, STEP_PREFIX, TAUQE_CO_AUTHOR_TRAILER,
};
pub use cmd::run_git;
pub use diff::{
    find_last_non_ai_commit, get_commits_ahead, get_cumulative_diff,
    get_cumulative_file_diffs, get_diff, get_diff_stat, squash_to_single_commit,
    CommitSummary,
};
pub use state::{
    detect_upstream_branch, get_repository_state, init_repository,
    is_git_repository, list_repository_files,
};

#[cfg(test)]
mod tests {
    use super::*;

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

        // Verify that squash commit has no AI co-author and undo is safely refused
        let undo_res = undo_last_ai_commit(root);
        assert!(undo_res.is_err());
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
        let init_hash = run_git(Some(root), &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // No AI commits yet
        assert_eq!(find_last_non_ai_commit(root).unwrap(), None);

        // 2. AI step 1
        std::fs::write(root.join("step1.txt"), "step1").unwrap();
        create_ai_commit(root, &["step1.txt".to_string()], "feat: add step 1").unwrap();

        // 3. AI step 2
        std::fs::write(root.join("step2.txt"), "step2").unwrap();
        create_ai_commit(root, &["step2.txt".to_string()], "feat: add step 2").unwrap();

        // Check author and co-author on AI commit
        let author = run_git(Some(root), &["log", "-1", "--format=%an <%ae>"]).unwrap();
        assert_eq!(author.trim(), "Alice Developer <alice@dev.org>");
        let trailers = run_git(Some(root), &["log", "-1", "--format=%(trailers:key=Co-authored-by)"]).unwrap();
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

        let squashed_hash = squash_to_single_commit(root, &init_hash, "feat: complete feature").unwrap();
        assert!(!squashed_hash.is_empty());

        // Squashed commit has Alice as author and NO Co-authored-by trailer
        let squashed_author = run_git(Some(root), &["log", "-1", "--format=%an <%ae>"]).unwrap();
        assert_eq!(squashed_author.trim(), "Alice Developer <alice@dev.org>");
        let squashed_trailers = run_git(Some(root), &["log", "-1", "--format=%(trailers:key=Co-authored-by)"]).unwrap();
        assert!(!squashed_trailers.contains("Tauqe AI"));

        // No more AI commits in history
        assert_eq!(find_last_non_ai_commit(root).unwrap(), None);

        // Undo on squashed commit is refused
        assert!(undo_last_ai_commit(root).is_err());
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

        let cp = create_checkpoint(root).unwrap();
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

        let cp = create_checkpoint(root).unwrap();
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

        let cp = create_checkpoint(root).unwrap();

        // Step 1
        std::fs::write(root.join("file1.txt"), "modified 1").unwrap();
        create_step_commit(root, &["file1.txt".to_string()], "step 1").unwrap();

        // Step 2
        std::fs::write(root.join("file2.txt"), "modified 2").unwrap();
        create_step_commit(root, &["file2.txt".to_string()], "step 2").unwrap();

        // Intermediate log has 3 commits (init, step 1, step 2)
        let count: usize = run_git(Some(root), &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(count, 3);

        // Finalize (squash)
        let files = vec!["file1.txt".to_string(), "file2.txt".to_string()];
        let final_hash = finalize_ai_commit(root, &cp, &files, "Squashed AI feature").unwrap();
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
        assert_eq!(std::fs::read_to_string(root.join("file1.txt")).unwrap(), "modified 1");
        assert_eq!(std::fs::read_to_string(root.join("file2.txt")).unwrap(), "modified 2");
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

        let cp = create_checkpoint(root).unwrap();
        assert!(cp.created_checkpoint_commit);

        std::fs::write(root.join("file.txt"), "step 1 mod").unwrap();
        create_step_commit(root, &["file.txt".to_string()], "step 1").unwrap();

        std::fs::write(root.join("file.txt"), "step 2 mod").unwrap();
        create_step_commit(root, &["file.txt".to_string()], "step 2").unwrap();

        // Restore checkpoint on failure
        restore_checkpoint(root, &cp).unwrap();

        assert_eq!(std::fs::read_to_string(root.join("file.txt")).unwrap(), "initial");
        assert_eq!(std::fs::read_to_string(root.join("user.txt")).unwrap(), "user dirty");

        let log = run_git(Some(root), &["log", "--oneline"]).unwrap();
        assert!(!log.contains("tauqe-step:"));
    }

    #[test]
    fn test_undo_last_ai_commit_preserves_uncommitted_user_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(Some(root), &["init"]).unwrap();
        run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
        run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
        std::fs::write(root.join("file.txt"), "initial").unwrap();
        run_git(Some(root), &["add", "file.txt"]).unwrap();
        run_git(Some(root), &["commit", "-m", "init"]).unwrap();

        std::fs::write(root.join("file.txt"), "ai content").unwrap();
        let commit_hash =
            create_ai_commit(root, &["file.txt".to_string()], "AI change").unwrap();

        std::fs::write(root.join("uncommitted.txt"), "keep me").unwrap();

        let res = undo_last_ai_commit(root).unwrap();
        assert_eq!(res.undone_commit, commit_hash);
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "initial"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("uncommitted.txt")).unwrap(),
            "keep me"
        );
    }
}
