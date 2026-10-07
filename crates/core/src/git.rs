use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use tauqe_protocol::{GitUndoResult, RepositoryState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSummary {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

pub fn get_repository_state(dir: Option<&Path>) -> RepositoryState {
    let root = match run_git(dir, &["rev-parse", "--show-toplevel"]) {
        Ok(out) => out.trim().to_string(),
        Err(_) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            return RepositoryState {
                root: cwd.display().to_string(),
                branch: "unknown".to_string(),
                head: "unknown".to_string(),
                dirty: false,
            };
        }
    };

    let repo_dir = Path::new(&root);

    let branch = run_git(Some(repo_dir), &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string())
        .trim()
        .to_string();

    let head = run_git(Some(repo_dir), &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();

    let status = run_git(Some(repo_dir), &["status", "--porcelain"]).unwrap_or_default();
    let dirty = !status.trim().is_empty();

    RepositoryState {
        root,
        branch,
        head,
        dirty,
    }
}

pub fn list_repository_files(dir: Option<&Path>) -> Result<Vec<String>> {
    let output = run_git(dir, &["ls-files"])?;
    let files = output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(files)
}

/// Detects the upstream base branch for the repository using convention over configuration:
/// 1. If explicit upstream branch is specified in config and exists in git, use it.
/// 2. If current branch tracks an upstream (@{upstream}), use it.
/// 3. Standard fallback conventions: origin/main, origin/master, main, master.
pub fn detect_upstream_branch(dir: &Path, configured_upstream: Option<&str>) -> Option<String> {
    if let Some(cfg) = configured_upstream {
        let trimmed = cfg.trim();
        if !trimmed.is_empty() && run_git(Some(dir), &["rev-parse", "--verify", trimmed]).is_ok() {
            return Some(trimmed.to_string());
        }
    }

    // Check tracked upstream branch
    if let Ok(upstream) = run_git(
        Some(dir),
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
    ) {
        let trimmed = upstream.trim();
        if !trimmed.is_empty() && trimmed != "@{upstream}" {
            return Some(trimmed.to_string());
        }
    }

    let current_branch = run_git(Some(dir), &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();

    // Check standard candidate references (skipping the current branch itself)
    let candidates = ["origin/main", "origin/master", "main", "master"];
    for candidate in candidates {
        if candidate == current_branch {
            continue;
        }
        if run_git(Some(dir), &["rev-parse", "--verify", candidate]).is_ok() {
            return Some(candidate.to_string());
        }
    }

    None
}

/// Lists all commits on the current branch ahead of `base_ref`.
pub fn get_commits_ahead(dir: &Path, base_ref: &str) -> Result<Vec<CommitSummary>> {
    let rev_range = format!("{}..HEAD", base_ref.trim());
    let output = run_git(
        Some(dir),
        &[
            "log",
            "--format=%h\x1f%an\x1f%ad\x1f%s",
            "--date=short",
            &rev_range,
        ],
    )?;

    let mut commits = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\x1f').collect();
        if parts.len() >= 4 {
            commits.push(CommitSummary {
                hash: parts[0].to_string(),
                author: parts[1].to_string(),
                date: parts[2].to_string(),
                subject: parts[3].to_string(),
            });
        }
    }

    Ok(commits)
}

/// Retrieves cumulative diff between `base_ref` and HEAD.
pub fn get_cumulative_diff(dir: &Path, base_ref: &str) -> Result<String> {
    let rev_range = format!("{}..HEAD", base_ref.trim());
    run_git(Some(dir), &["diff", &rev_range])
}

/// Squashes all commits ahead of `base_ref` into a single conceptual commit via soft-reset.
pub fn squash_to_single_commit(dir: &Path, base_ref: &str, message: &str) -> Result<String> {
    let trimmed_msg = message.trim();
    if trimmed_msg.is_empty() {
        bail!("Commit message cannot be empty");
    }

    let commits = get_commits_ahead(dir, base_ref)?;
    if commits.is_empty() {
        bail!(
            "No commits to squash: HEAD is already at or behind {}",
            base_ref
        );
    }

    // 1. Soft-reset to base_ref. Keeps all cumulative changes staged in index.
    run_git(Some(dir), &["reset", "--soft", base_ref.trim()])
        .with_context(|| format!("Failed to soft reset to {}", base_ref))?;

    // 2. Commit the staged state
    let has_author = run_git(Some(dir), &["config", "user.name"]).is_ok();
    if has_author {
        run_git(Some(dir), &["commit", "-m", trimmed_msg, "--no-verify"])
            .context("Failed to create squashed commit")?;
    } else {
        run_git(
            Some(dir),
            &[
                "-c",
                "user.name=Developer",
                "-c",
                "user.email=developer@tauqe.dev",
                "commit",
                "-m",
                trimmed_msg,
                "--no-verify",
            ],
        )
        .context("Failed to create squashed commit")?;
    }

    let short_hash = run_git(Some(dir), &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();

    Ok(short_hash)
}

/// Checkpoint information representing saved pre-edit developer state.
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub created_checkpoint_commit: bool,
    pub previous_head: String,
}

/// Creates a safe checkpoint of uncommitted changes before applying AI modifications.
///
/// If the repository contains dirty changes, they are temporarily committed into an
/// isolated checkpoint commit. This preserves user changes from ever being overwritten
/// and cleanly isolates the subsequent AI commit in Git history.
pub fn create_checkpoint(dir: &Path) -> Result<CheckpointInfo> {
    let head = run_git(Some(dir), &["rev-parse", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string())
        .trim()
        .to_string();

    let status = run_git(Some(dir), &["status", "--porcelain"]).unwrap_or_default();
    let is_dirty = !status.trim().is_empty();

    if !is_dirty {
        return Ok(CheckpointInfo {
            created_checkpoint_commit: false,
            previous_head: head,
        });
    }

    // Stage and commit uncommitted user changes into a checkpoint commit
    run_git(Some(dir), &["add", "-A"])
        .context("Failed to stage uncommitted changes for checkpoint")?;

    run_git(
        Some(dir),
        &[
            "commit",
            "-m",
            "tauqe-checkpoint: uncommitted user changes",
            "--no-verify",
        ],
    )
    .context("Failed to commit checkpoint")?;

    let new_head = run_git(Some(dir), &["rev-parse", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();

    Ok(CheckpointInfo {
        created_checkpoint_commit: true,
        previous_head: new_head,
    })
}

/// Restores working tree back to pre-checkpoint state (used on workflow failure/cancellation).
pub fn restore_checkpoint(dir: &Path, checkpoint: &CheckpointInfo) -> Result<()> {
    // 1. Reset working tree to checkpoint base commit (instantly wiping any intermediate step commits)
    run_git(Some(dir), &["reset", "--hard", &checkpoint.previous_head])?;
    let _ = run_git(Some(dir), &["clean", "-fd"]);

    // 2. If a checkpoint commit was created, uncommit it so user modifications are restored
    if checkpoint.created_checkpoint_commit {
        let head_subject = run_git(Some(dir), &["log", "-1", "--format=%s"])
            .unwrap_or_default()
            .trim()
            .to_string();
        if head_subject.starts_with("tauqe-checkpoint:") {
            // Mixed reset: uncommits the checkpoint, leaving user modifications in working tree
            run_git(Some(dir), &["reset", "HEAD~1"])?;
        }
    }
    Ok(())
}

/// Creates an intermediate step commit during edit execution or verification auto-healing.
pub fn create_step_commit(dir: &Path, changed_files: &[String], step_name: &str) -> Result<String> {
    if changed_files.is_empty() {
        return Ok(String::new());
    }

    let mut add_args = vec!["add", "--"];
    for f in changed_files {
        add_args.push(f.as_str());
    }
    run_git(Some(dir), &add_args)?;

    let commit_message = format!("tauqe-step: {}", step_name.trim());
    run_git(
        Some(dir),
        &[
            "-c",
            "user.name=Tauqe AI",
            "-c",
            "user.email=ai@tauqe.dev",
            "commit",
            "-m",
            &commit_message,
            "--no-verify",
        ],
    )?;

    let short_hash = run_git(Some(dir), &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();

    Ok(short_hash)
}

/// Squashes all intermediate step commits into a single final AI commit.
///
/// If a temporary checkpoint commit exists at the base, it is unwound prior to committing
/// so user uncommitted files remain in the working tree and only `changed_files` are recorded.
pub fn finalize_ai_commit(
    dir: &Path,
    checkpoint: &CheckpointInfo,
    changed_files: &[String],
    summary: &str,
) -> Result<String> {
    if changed_files.is_empty() {
        bail!("No changed files to commit");
    }

    // 1. Soft-reset back to checkpoint base (keeps all step changes staged in the index)
    run_git(Some(dir), &["reset", "--soft", &checkpoint.previous_head])
        .context("Failed to soft reset to checkpoint base")?;

    // 2. If checkpoint commit was created, unwind it so user uncommitted changes stay in working tree
    if checkpoint.created_checkpoint_commit {
        let head_subject = run_git(Some(dir), &["log", "-1", "--format=%s"])
            .unwrap_or_default()
            .trim()
            .to_string();
        if head_subject.starts_with("tauqe-checkpoint:") {
            run_git(Some(dir), &["reset", "HEAD~1"])
                .context("Failed to unwind temporary checkpoint before finalizing AI commit")?;
        }
    }

    // 3. Stage touched files
    let mut add_args = vec!["add", "--"];
    for f in changed_files {
        add_args.push(f.as_str());
    }
    run_git(Some(dir), &add_args)?;

    let commit_message = format!("{}\n\nGenerated by Tauqe AI", summary.trim());

    // 4. Commit with explicit author metadata
    run_git(
        Some(dir),
        &[
            "-c",
            "user.name=Tauqe AI",
            "-c",
            "user.email=ai@tauqe.dev",
            "commit",
            "-m",
            &commit_message,
            "--no-verify",
        ],
    )?;

    let short_hash = run_git(Some(dir), &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();

    Ok(short_hash)
}

/// Creates an isolated AI commit containing only the files modified by the model.
///
/// If a temporary checkpoint commit exists at HEAD, it is unwound (mixed reset)
/// prior to staging changed files, ensuring user uncommitted files remain in the
/// working tree and only `changed_files` are recorded into the AI commit.
pub fn create_ai_commit(dir: &Path, changed_files: &[String], summary: &str) -> Result<String> {
    if changed_files.is_empty() {
        bail!("No changed files to commit");
    }

    // If HEAD is a temporary checkpoint commit, unwind it so user dirty changes remain in working tree
    let head_subject = run_git(Some(dir), &["log", "-1", "--format=%s"])
        .unwrap_or_default()
        .trim()
        .to_string();
    if head_subject.starts_with("tauqe-checkpoint:") {
        run_git(Some(dir), &["reset", "HEAD~1"])
            .context("Failed to unwind temporary checkpoint before creating AI commit")?;
    }

    // Stage touched files
    let mut add_args = vec!["add", "--"];
    for f in changed_files {
        add_args.push(f.as_str());
    }
    run_git(Some(dir), &add_args)?;

    let commit_message = format!("{}\n\nGenerated by Tauqe AI", summary.trim());

    // Commit with explicit author metadata
    run_git(
        Some(dir),
        &[
            "-c",
            "user.name=Tauqe AI",
            "-c",
            "user.email=ai@tauqe.dev",
            "commit",
            "-m",
            &commit_message,
            "--no-verify",
        ],
    )?;

    let short_hash = run_git(Some(dir), &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();

    Ok(short_hash)
}

/// Undoes the last AI commit deterministically.
///
/// If an AI commit is at HEAD, it hard-resets it.
/// If preceded by a user checkpoint commit, it soft-resets the checkpoint so
/// the developer's original dirty changes are fully restored.
pub fn undo_last_ai_commit(dir: &Path) -> Result<GitUndoResult> {
    // 1. Verify that HEAD exists and is an AI commit
    let head_hash = run_git(Some(dir), &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();
    let author = run_git(Some(dir), &["log", "-1", "--format=%an <%ae>"])?
        .trim()
        .to_string();
    let subject = run_git(Some(dir), &["log", "-1", "--format=%s"])?
        .trim()
        .to_string();

    let is_ai_commit = author.contains("Tauqe AI")
        || author.contains("ai@tauqe.dev")
        || subject.starts_with("AI:")
        || run_git(Some(dir), &["log", "-1", "--format=%b"])
            .unwrap_or_default()
            .contains("Generated by Tauqe AI");

    if !is_ai_commit {
        bail!(
            "HEAD commit {} ('{}') was not created by Tauqe AI. Undo refused for safety.",
            head_hash,
            subject
        );
    }

    // 2. Identify files changed in this AI commit before unwinding
    let touched_output = run_git(
        Some(dir),
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    )
    .unwrap_or_default();
    let touched_files: Vec<String> = touched_output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // 3. Mixed reset to HEAD~1 (moves HEAD back without destroying unrelated working tree changes)
    run_git(Some(dir), &["reset", "HEAD~1"]).context("Failed to reset AI commit")?;

    // 4. Revert only the touched files in the working tree to the new HEAD
    for file in &touched_files {
        let exists_in_head =
            run_git(Some(dir), &["cat-file", "-e", &format!("HEAD:{}", file)]).is_ok();
        if exists_in_head {
            let _ = run_git(Some(dir), &["checkout", "HEAD", "--", file]);
        } else {
            let full = dir.join(file);
            let _ = std::fs::remove_file(&full);
        }
    }

    // 5. Check if previous commit was a checkpoint commit
    let prev_subject = run_git(Some(dir), &["log", "-1", "--format=%s"])
        .unwrap_or_default()
        .trim()
        .to_string();

    let mut restored_checkpoint = false;
    if prev_subject.starts_with("tauqe-checkpoint:") {
        // Mixed reset uncommits checkpoint and restores user dirty state into working tree
        run_git(Some(dir), &["reset", "HEAD~1"])?;
        restored_checkpoint = true;
    }

    let new_head = run_git(Some(dir), &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string())
        .trim()
        .to_string();

    let msg = if restored_checkpoint {
        format!(
            "Undid AI commit {} ('{}') and restored original uncommitted changes.",
            head_hash, subject
        )
    } else {
        format!("Undid AI commit {} ('{}').", head_hash, subject)
    };

    Ok(GitUndoResult {
        undone_commit: head_hash,
        restored_checkpoint,
        new_head,
        message: msg,
    })
}

/// Retrieves diff between HEAD~1 and HEAD or working tree changes.
pub fn get_diff(dir: &Path, target_path: Option<&str>) -> Result<String> {
    let mut args = vec!["diff", "HEAD~1..HEAD"];
    if let Some(path) = target_path {
        args.push("--");
        args.push(path);
    }
    match run_git(Some(dir), &args) {
        Ok(out) => Ok(out),
        Err(_) => {
            // Fallback to git diff HEAD
            let mut fallback_args = vec!["diff", "HEAD"];
            if let Some(path) = target_path {
                fallback_args.push("--");
                fallback_args.push(path);
            }
            run_git(Some(dir), &fallback_args)
        }
    }
}

fn run_git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    if let Some(d) = dir {
        command.current_dir(d);
    }
    let output = command.args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        bail!(
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_repository_state_no_panic() {
        let state = get_repository_state(None);
        assert!(!state.root.is_empty());
        assert!(!state.branch.is_empty());
        assert!(!state.head.is_empty());
    }

    #[test]
    fn test_list_repository_files() {
        let files = list_repository_files(None).unwrap();
        assert!(!files.is_empty());
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
