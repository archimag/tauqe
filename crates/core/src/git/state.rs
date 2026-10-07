use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tauqe_protocol::RepositoryState;

use super::cmd::{commit_with_developer_author, run_git};

/// Checks whether the specified directory is inside a Git repository work tree.
pub fn is_git_repository(dir: Option<&Path>) -> bool {
    run_git(dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|out| out.trim() == "true")
        .unwrap_or(false)
}

/// Initializes a new Git repository and optionally creates an initial commit.
pub fn init_repository(dir: Option<&Path>, initial_commit: bool) -> Result<RepositoryState> {
    run_git(dir, &["init"]).context("Failed to run git init")?;

    let repo_state = get_repository_state(dir);
    let repo_dir = Path::new(&repo_state.root);

    if initial_commit {
        let _ = run_git(Some(repo_dir), &["add", "-A"]);
        let status = run_git(Some(repo_dir), &["status", "--porcelain"]).unwrap_or_default();
        let allow_empty = status.trim().is_empty();
        commit_with_developer_author(repo_dir, "chore: initial commit", allow_empty)?;
    }

    Ok(get_repository_state(dir))
}

/// Inspects current repository root, branch, HEAD revision, and dirty status.
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

/// Lists all tracked files in the repository.
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
    fn test_is_git_repository_and_init() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        assert!(!is_git_repository(Some(path)));

        let state = init_repository(Some(path), true).unwrap();
        assert!(is_git_repository(Some(path)));
        assert_ne!(state.head, "unknown");
    }

    #[test]
    fn test_list_repository_files() {
        let files = list_repository_files(None).unwrap();
        assert!(!files.is_empty());
    }
}
