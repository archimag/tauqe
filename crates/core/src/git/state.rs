use anyhow::{Context, Result};
use std::collections::BTreeMap;
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

/// Working-copy changes reported by `git status --porcelain`.
///
/// The map is keyed by the repository-relative path of each file and carries its
/// two-letter status code. Ignored paths never appear, because Git omits them
/// from `--porcelain`, so an artifact directory listed in `.gitignore` cannot
/// leak into a change set.
pub type WorktreeSnapshot = BTreeMap<String, String>;

/// Captures the current status of the working copy.
///
/// A snapshot is the reference point for detecting files an external project
/// step produced: a path whose status changed or that appeared afterwards
/// belongs to the change set, and a path that disappeared was removed by it.
/// Reading the status is a Git subprocess, so it runs through the blocking
/// helper and degrades to an empty snapshot instead of aborting a turn whose
/// repository temporarily cannot be inspected.
pub fn snapshot_worktree(dir: &Path) -> WorktreeSnapshot {
    match run_git(Some(dir), &["status", "--porcelain"]) {
        Ok(output) => parse_worktree_status(&output),
        Err(err) => {
            tracing::warn!("Failed to snapshot the working copy status: {}", err);
            WorktreeSnapshot::new()
        }
    }
}

/// Parses `git status --porcelain` output into a snapshot.
///
/// Each line starts with two status characters followed by the path. A rename
/// reports both sides, and the destination is the path that exists afterwards,
/// so it is the one recorded here.
pub fn parse_worktree_status(output: &str) -> WorktreeSnapshot {
    let mut snapshot = WorktreeSnapshot::new();

    for line in output.lines() {
        let mut chars = line.chars();
        let status: String = chars.by_ref().take(2).collect();
        let rest = chars.as_str().trim_start();
        if rest.is_empty() {
            continue;
        }
        let path = match rest.split_once(" -> ") {
            Some((_, to)) => to.trim(),
            None => rest,
        };
        let path = path.trim_matches('"');
        if !path.is_empty() {
            snapshot.insert(path.to_string(), status.trim().to_string());
        }
    }

    snapshot
}

/// Computes the paths a project step created, rewrote, or removed.
///
/// The comparison is deterministic and sorted, so the reported change set never
/// depends on the order Git listed the entries.
pub fn changed_worktree_paths(before: &WorktreeSnapshot, after: &WorktreeSnapshot) -> Vec<String> {
    let mut changed = Vec::new();
    for (path, status) in after {
        if before.get(path).map(String::as_str) != Some(status.as_str()) {
            changed.push(path.clone());
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changed.push(path.clone());
        }
    }
    changed.sort();
    changed.dedup();
    changed
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
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
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
        let dir = tempfile::tempdir().unwrap();
        init_repository(Some(dir.path()), true).unwrap();

        let state = get_repository_state(Some(dir.path()));
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        std::fs::write(path.join("tracked.txt"), "content\n").unwrap();
        init_repository(Some(path), true).unwrap();

        let files = list_repository_files(Some(path)).unwrap();
        assert!(files.iter().any(|file| file == "tracked.txt"));
    }

    #[test]
    fn test_worktree_snapshot_detects_created_modified_and_deleted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        std::fs::write(path.join("tracked.txt"), "content\n").unwrap();
        std::fs::write(path.join("ignored.txt"), "ignored\n").unwrap();
        std::fs::write(path.join(".gitignore"), "ignored.txt\n").unwrap();
        init_repository(Some(path), true).unwrap();

        let before = snapshot_worktree(path);
        assert!(
            before.is_empty(),
            "a clean tree reports nothing: {before:?}"
        );

        std::fs::write(path.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(path.join("created.txt"), "created\n").unwrap();
        std::fs::write(path.join("ignored.txt"), "still ignored\n").unwrap();

        let after = snapshot_worktree(path);
        assert_eq!(
            changed_worktree_paths(&before, &after),
            vec!["created.txt".to_string(), "tracked.txt".to_string()]
        );

        std::fs::remove_file(path.join("tracked.txt")).unwrap();
        let removed = snapshot_worktree(path);
        assert_eq!(
            changed_worktree_paths(&after, &removed),
            vec!["tracked.txt".to_string()]
        );
    }

    #[test]
    fn test_worktree_status_parsing_reads_renames_and_quoted_paths() {
        // Every porcelain line starts with the two status characters, so a
        // quoted path still carries its own status prefix.
        let snapshot = parse_worktree_status(concat!(
            " M src/lib.rs\n",
            "?? generated.rs\n",
            "R  old.rs -> new.rs\n",
            "?? \"sp ace.rs\"\n",
            "R  \"old name.rs\" -> \"new name.rs\"\n",
        ));

        assert_eq!(snapshot.get("src/lib.rs").map(String::as_str), Some("M"));
        assert_eq!(snapshot.get("generated.rs").map(String::as_str), Some("??"));
        assert_eq!(snapshot.get("new.rs").map(String::as_str), Some("R"));
        assert!(!snapshot.contains_key("old.rs"));
        assert_eq!(snapshot.get("sp ace.rs").map(String::as_str), Some("??"));
        // A rename keeps the destination, which is the path that exists
        // afterwards, and drops the quoted source.
        assert_eq!(snapshot.get("new name.rs").map(String::as_str), Some("R"));
        assert!(!snapshot.contains_key("old name.rs"));
    }
}
