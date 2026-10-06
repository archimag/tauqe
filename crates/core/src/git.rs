use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use workbench_protocol::{GitUndoResult, RepositoryState};

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
            "workbench-checkpoint: uncommitted user changes",
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
    if checkpoint.created_checkpoint_commit {
        let head_subject = run_git(Some(dir), &["log", "-1", "--format=%s"])
            .unwrap_or_default()
            .trim()
            .to_string();
        if head_subject.starts_with("workbench-checkpoint:") {
            // Mixed reset: uncommits the checkpoint, leaving user modifications in working tree
            run_git(Some(dir), &["reset", "HEAD~1"])?;
        }
    }
    Ok(())
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
    if head_subject.starts_with("workbench-checkpoint:") {
        run_git(Some(dir), &["reset", "HEAD~1"])
            .context("Failed to unwind temporary checkpoint before creating AI commit")?;
    }

    // Stage touched files
    let mut add_args = vec!["add", "--"];
    for f in changed_files {
        add_args.push(f.as_str());
    }
    run_git(Some(dir), &add_args)?;

    let commit_message = format!("{}\n\nGenerated by Workbench AI", summary.trim());

    // Commit with explicit author metadata
    run_git(
        Some(dir),
        &[
            "-c",
            "user.name=Workbench AI",
            "-c",
            "user.email=ai@workbench.dev",
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

    let is_ai_commit = author.contains("Workbench AI")
        || author.contains("ai@workbench.dev")
        || subject.starts_with("AI:")
        || run_git(Some(dir), &["log", "-1", "--format=%b"])
            .unwrap_or_default()
            .contains("Generated by Workbench AI");

    if !is_ai_commit {
        bail!(
            "HEAD commit {} ('{}') was not created by Workbench AI. Undo refused for safety.",
            head_hash,
            subject
        );
    }

    // 2. Hard reset the AI commit
    run_git(Some(dir), &["reset", "--hard", "HEAD~1"]).context("Failed to reset AI commit")?;

    // 3. Check if previous commit was a checkpoint commit
    let prev_subject = run_git(Some(dir), &["log", "-1", "--format=%s"])
        .unwrap_or_default()
        .trim()
        .to_string();

    let mut restored_checkpoint = false;
    if prev_subject.starts_with("workbench-checkpoint:") {
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
}
