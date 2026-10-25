use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tauqe_protocol::RepositoryState;

use super::cmd::{commit_with_developer_author, run_git, run_git_check_ignore};

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

/// Splits a NUL-separated Git listing into its non-empty entries.
///
/// Git emits `-z` lists as bare path names separated by `\0`, which keeps a
/// path containing spaces, quotes, or a newline intact.
fn split_nul_list(output: &str) -> Vec<String> {
    output
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.to_string())
        .collect()
}

/// Returns the supplied paths that Git resolves to a tracked index entry.
///
/// A tracked file stays editable even when its name matches an ignore rule:
/// the rule governs paths Git does not track yet, while a file recorded in the
/// index was committed deliberately. Pathspec magic is disabled with the
/// `:(literal)` prefix, so a name containing `*`, `?`, or `[` is matched
/// verbatim instead of being read as a pattern.
fn tracked_paths(dir: &Path, paths: &[String]) -> Result<BTreeSet<String>> {
    let literals: Vec<String> = paths
        .iter()
        .map(|path| format!(":(literal){path}"))
        .collect();
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend(literals.iter().map(String::as_str));
    let output = run_git(Some(dir), &args)?;
    Ok(split_nul_list(&output).into_iter().collect())
}

/// Collects the supplied paths that repository ignore rules exclude.
///
/// The probe is one batch `git check-ignore` call, so checking a whole change
/// set costs a single Git subprocess instead of one per path. Both
/// `.gitignore` files and `.git/info/exclude` take part, because the answer
/// comes from Git itself rather than from a locally re-implemented matcher.
///
/// Paths already present in the index are subtracted: such a file is tracked,
/// and a rule that happens to match its name does not forbid editing it. The
/// result is sorted and de-duplicated, so callers report a stable diagnostic
/// regardless of the order they supplied.
pub fn gitignored_paths(dir: &Path, paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let ignored = run_git_check_ignore(dir, paths)?;
    if ignored.is_empty() {
        return Ok(Vec::new());
    }

    let tracked = tracked_paths(dir, &ignored)?;
    let mut result: Vec<String> = ignored
        .into_iter()
        .filter(|path| !tracked.contains(path))
        .collect();
    result.sort();
    result.dedup();
    Ok(result)
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
    match run_git(Some(dir), &["status", "--porcelain", "-z"]) {
        Ok(output) => parse_worktree_status(&output),
        Err(err) => {
            tracing::warn!("Failed to snapshot the working copy status: {}", err);
            WorktreeSnapshot::new()
        }
    }
}

/// Parses `git status --porcelain -z` output into a snapshot.
///
/// The NUL-separated form is the machine-readable one: it disables Git's
/// C-style path quoting entirely, so a path containing a non-ASCII character,
/// an embedded double quote, or a backslash arrives byte-for-byte instead of as
/// an escaped string. An escaped rendering is not a path — staging it would
/// demand a file name that does not exist on disk — so the change set has to be
/// built from the literal names.
///
/// Each record carries the two status characters, a separating space, and the
/// path. A rename or copy appends the source path as its own record, which is
/// skipped here: the destination is the path that exists afterwards, so it is
/// the one the snapshot records.
pub fn parse_worktree_status(output: &str) -> WorktreeSnapshot {
    let mut snapshot = WorktreeSnapshot::new();
    let mut skip_source = false;

    for record in output.split('\0') {
        if record.is_empty() {
            continue;
        }
        if skip_source {
            skip_source = false;
            continue;
        }

        let mut chars = record.chars();
        let status: String = chars.by_ref().take(2).collect();
        if status.chars().count() < 2 {
            continue;
        }
        let rest = chars.as_str().strip_prefix(' ').unwrap_or(chars.as_str());
        if rest.is_empty() {
            continue;
        }

        snapshot.insert(rest.to_string(), status.trim().to_string());
        skip_source = status.starts_with('R') || status.starts_with('C');
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
    fn test_gitignored_paths_excludes_tracked_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        std::fs::write(path.join(".gitignore"), "*.log\ntarget/\n").unwrap();
        std::fs::write(path.join("legacy.log"), "tracked\n").unwrap();
        std::fs::write(path.join("src.rs"), "fn main() {}\n").unwrap();
        init_repository(Some(path), true).unwrap();

        // A file whose name matches an ignore rule but is already committed
        // stays editable.
        run_git(Some(path), &["add", "-f", "legacy.log"]).unwrap();
        run_git(Some(path), &["commit", "-m", "chore: track legacy log"]).unwrap();

        std::fs::write(path.join("fresh.log"), "new\n").unwrap();
        std::fs::create_dir_all(path.join("target")).unwrap();
        std::fs::write(path.join("target/binary"), "artifact\n").unwrap();

        let ignored = gitignored_paths(
            path,
            &[
                "legacy.log".to_string(),
                "src.rs".to_string(),
                "fresh.log".to_string(),
                "target/binary".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(
            ignored,
            vec!["fresh.log".to_string(), "target/binary".to_string()]
        );

        // An empty change set probes nothing and reports nothing.
        assert!(gitignored_paths(path, &[]).unwrap().is_empty());
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
    fn test_worktree_status_parsing_reads_renames_and_literal_paths() {
        // Every `-z` record starts with the two status characters and a
        // separating space, and a rename appends its source as the next record.
        let snapshot = parse_worktree_status(concat!(
            " M src/lib.rs\0",
            "?? generated.rs\0",
            "R  new.rs\0old.rs\0",
            "?? sp ace.rs\0",
            "R  new name.rs\0old name.rs\0",
            "?? привет.rs\0",
            "?? quote\"d.rs\0",
            "?? back\\slash.rs\0",
        ));

        assert_eq!(snapshot.get("src/lib.rs").map(String::as_str), Some("M"));
        assert_eq!(snapshot.get("generated.rs").map(String::as_str), Some("??"));
        assert_eq!(snapshot.get("new.rs").map(String::as_str), Some("R"));
        assert!(!snapshot.contains_key("old.rs"));
        assert_eq!(snapshot.get("sp ace.rs").map(String::as_str), Some("??"));
        // A rename keeps the destination, which is the path that exists
        // afterwards, and drops the source record.
        assert_eq!(snapshot.get("new name.rs").map(String::as_str), Some("R"));
        assert!(!snapshot.contains_key("old name.rs"));
        // `-z` disables path quoting, so these names are recorded verbatim
        // instead of as escaped octal text.
        assert_eq!(snapshot.get("привет.rs").map(String::as_str), Some("??"));
        assert_eq!(snapshot.get("quote\"d.rs").map(String::as_str), Some("??"));
        assert_eq!(
            snapshot.get("back\\slash.rs").map(String::as_str),
            Some("??")
        );
    }

    #[test]
    fn test_worktree_snapshot_and_staging_preserve_quoting_sensitive_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        std::fs::write(path.join("tracked.txt"), "content\n").unwrap();
        init_repository(Some(path), true).unwrap();

        let before = snapshot_worktree(path);

        // With the default `core.quotePath=true` a line-oriented listing would
        // escape every one of these names; the NUL-separated snapshot keeps
        // them as the paths that exist on disk.
        let names = ["back\\slash.rs", "quote\"d.rs", "привет.rs"];
        for name in names {
            std::fs::write(path.join(name), "created\n").unwrap();
        }

        let after = snapshot_worktree(path);
        let changed = changed_worktree_paths(&before, &after);
        let mut expected: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        expected.sort();
        assert_eq!(changed, expected);

        // A path reported verbatim is a pathspec Git resolves, so the change set
        // reaches a commit instead of failing the Git transaction.
        crate::git::cmd::stage_files(path, &changed).unwrap();
        crate::git::cmd::commit_with_developer_author(path, "chore: add names", false).unwrap();
        assert!(
            snapshot_worktree(path).is_empty(),
            "the committed change set must leave the working copy clean"
        );

        let committed =
            crate::git::cmd::run_git(Some(path), &["ls-tree", "-z", "--name-only", "HEAD"])
                .unwrap();
        let committed: Vec<&str> = committed
            .split('\0')
            .filter(|name| !name.is_empty())
            .collect();
        for name in names {
            assert!(
                committed.contains(&name),
                "the commit must record {name:?}: {committed:?}"
            );
        }
    }

    #[test]
    fn test_gitignored_paths_honors_repository_exclude_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        std::fs::write(path.join("src.rs"), "fn main() {}\n").unwrap();
        init_repository(Some(path), true).unwrap();

        // `.git/info/exclude` holds the repository's private ignore rules and
        // Git applies them exactly like `.gitignore`, so the probe must see
        // them without a local re-implementation of the matching rules.
        std::fs::write(path.join(".git/info/exclude"), "local-only/\n").unwrap();
        std::fs::create_dir_all(path.join("local-only")).unwrap();
        std::fs::write(path.join("local-only/secret.txt"), "secret\n").unwrap();

        let ignored = gitignored_paths(
            path,
            &["src.rs".to_string(), "local-only/secret.txt".to_string()],
        )
        .unwrap();

        assert_eq!(ignored, vec!["local-only/secret.txt".to_string()]);
    }
}
