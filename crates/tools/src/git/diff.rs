use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::checkpoint::is_tauqe_commit;
use super::cmd::{commit_with_developer_author, run_git};
use crate::git::GitHooks;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSummary {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

/// Finds the most recent non-AI commit hash preceding the current session of AI commits.
/// Returns Ok(Some(hash)) if a base non-AI commit was found, or Ok(None) if not.
pub fn find_last_non_ai_commit(dir: &Path) -> Result<Option<String>> {
    let output = match run_git(
        Some(dir),
        &[
            "log",
            "-n",
            "50",
            "--format=%H\x1f%s\x1f%an <%ae>\x1f%(trailers:key=Co-authored-by)\x1f%b\x1e",
        ],
    ) {
        Ok(out) => out,
        Err(_) => return Ok(None),
    };

    let mut found_ai_commit = false;

    for entry in output.split('\x1e') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        let parts: Vec<&str> = entry.splitn(5, '\x1f').collect();
        if parts.len() < 5 {
            continue;
        }

        let hash = parts[0].trim();
        let subject = parts[1].trim();
        let author = parts[2].trim();
        let trailers = parts[3].trim();
        let body = parts[4].trim();

        let is_ai = is_tauqe_commit(subject, body, author, trailers);

        if is_ai {
            found_ai_commit = true;
        } else if found_ai_commit {
            // First non-AI commit preceding the contiguous AI commits session
            return Ok(Some(hash.to_string()));
        }
    }

    Ok(None)
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

/// Renders the moving revision range every cumulative query addresses.
///
/// The queries that report the current state of the repository read the range
/// this way. A query that pairs two listings resolves the same range into a
/// [`PinnedRange`] first and renders the two hashes instead, so no pairing
/// straddles a moving endpoint.
fn moving_range(base_ref: &str) -> String {
    format!("{}..HEAD", base_ref.trim())
}

/// Runs `git diff` over a rendered revision range with the supplied options.
///
/// One runner keeps every cumulative query on the same plumbing, so the
/// moving-range and pinned-range callers differ only in the range they render.
fn diff_range(dir: &Path, range: &str, options: &[&str]) -> Result<String> {
    let mut args = vec!["diff"];
    args.extend_from_slice(options);
    args.push(range);
    run_git(Some(dir), &args)
}

/// Retrieves cumulative diff between `base_ref` and HEAD.
pub fn get_cumulative_diff(dir: &Path, base_ref: &str) -> Result<String> {
    diff_range(dir, &moving_range(base_ref), &[])
}

/// Retrieves git diff --stat summary between `base_ref` and HEAD.
pub fn get_diff_stat(dir: &Path, base_ref: &str) -> Result<String> {
    let stat = diff_range(dir, &moving_range(base_ref), &["--stat"])?;
    let last_line = stat
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| "0 files changed".to_string());
    Ok(last_line)
}

/// Resolves one endpoint of a range to the immutable commit hash it names.
///
/// The `^{commit}` peel keeps the answer a commit — an annotated tag resolves
/// to the commit it points at — and makes an endpoint that does not resolve
/// fail with Git's own diagnostic naming the revision that was addressed.
fn resolve_revision(dir: &Path, revision: &str) -> Result<String> {
    let peeled = format!("{revision}^{{commit}}");
    let resolved = run_git(Some(dir), &["rev-parse", peeled.as_str()])?;
    let hash = resolved.trim();
    if hash.is_empty() {
        bail!("git did not resolve the revision {revision}");
    }
    Ok(hash.to_string())
}

/// The immutable commit pair a cumulative change set is read at.
///
/// `base_revision` keeps the spelling the caller used, so the drift check asks
/// the same question the pinning asked: whether that endpoint still names the
/// hash the evidence was computed over.
#[derive(Debug)]
struct PinnedRange {
    /// The revision the base endpoint was named by.
    base_revision: String,
    /// The commit hash the base endpoint resolved to.
    base: String,
    /// The commit hash `HEAD` resolved to.
    head: String,
}

impl PinnedRange {
    /// Renders the pair as the two-hash revision range Git reads.
    fn rev_range(&self) -> String {
        format!("{}..{}", self.base, self.head)
    }
}

/// Resolves both endpoints of a cumulative range into one pinned commit pair.
///
/// Both hashes are resolved before either listing runs, so the two listings of
/// a pairing address the same commits even when a ref moves while they run.
fn pin_range(dir: &Path, base_ref: &str) -> Result<PinnedRange> {
    let base_revision = base_ref.trim().to_string();
    let base = resolve_revision(dir, base_revision.as_str())?;
    let head = resolve_revision(dir, "HEAD")?;
    Ok(PinnedRange {
        base_revision,
        base,
        head,
    })
}

/// A refused pairing: an endpoint no longer names the pinned commit.
#[derive(Debug)]
struct RangeDrift {
    /// The revision that moved, spelled the way it was addressed.
    endpoint: String,
    /// The commit the evidence was computed over.
    pinned: String,
    /// The commit the endpoint names now.
    observed: String,
}

impl std::fmt::Display for RangeDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Refusing to report the cumulative diff: the endpoint {} moved from the pinned commit {} to {} while the file diffs were computed",
            self.endpoint, self.pinned, self.observed
        )
    }
}

impl std::error::Error for RangeDrift {}

/// Re-asserts that both endpoints still name the pinned commits.
///
/// A pairing is truthful only while the commits it read are still the commits
/// the endpoints address: a ref that moved while the listings ran, or right
/// after them, describes a snapshot nobody asked for. The drift travels as a
/// typed refusal rather than a warning, so the squash preview reports the
/// movement instead of presenting a stale file list.
fn assert_range_pinned(dir: &Path, pinned: &PinnedRange) -> Result<()> {
    let endpoints = [
        (pinned.base_revision.as_str(), pinned.base.as_str()),
        ("HEAD", pinned.head.as_str()),
    ];
    for (revision, expected) in endpoints {
        let observed = resolve_revision(dir, revision)?;
        if observed != expected {
            return Err(anyhow::Error::new(RangeDrift {
                endpoint: revision.to_string(),
                pinned: expected.to_string(),
                observed,
            }));
        }
    }
    Ok(())
}

/// Lists the paths a rendered revision range changed, exactly as they exist on
/// disk.
///
/// `--name-only -z` terminates each path with a NUL and disables Git's C-style
/// path quoting, so a non-ASCII name, an embedded double quote, or a backslash
/// survives instead of arriving as an escaped string. Git emits these entries in
/// the same order as the patch blocks of the same revision range, which is what
/// lets the per-file diffs be paired with an unquoted name.
fn range_paths(dir: &Path, range: &str) -> Result<Vec<String>> {
    let output = diff_range(dir, range, &["--name-only", "-z"])?;
    Ok(output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| path.to_string())
        .collect())
}

/// Extracts the destination path from a `diff --git a/<old> b/<new>` header.
///
/// This is a degraded fallback, used only when the NUL-separated listing and the
/// patch disagree on the number of files: a header may still carry Git's
/// C-style quoting, so the listing is always preferred when it is available.
fn header_path(rest: &str) -> Option<String> {
    if let Some(idx) = rest.rfind(" b/") {
        return Some(rest[idx + 3..].trim_matches('"').to_string());
    }
    let parts: Vec<&str> = rest.split_whitespace().collect();
    let candidate = parts.get(1)?;
    Some(
        candidate
            .strip_prefix("b/")
            .unwrap_or(candidate)
            .trim_matches('"')
            .to_string(),
    )
}

/// Pairs the per-file patch blocks with the verbatim paths of one pinned range.
///
/// Paths are taken from the NUL-separated `--name-only` listing rather than from
/// the block headers, so a file whose name Git would escape under the default
/// `core.quotePath=true` is reported under its real on-disk name. Both listings
/// address the two pinned hashes, so neither observes a ref that moved between
/// them: the block count and the path count describe one snapshot and the
/// positional pairing cannot shift a name onto another file's block.
fn pair_file_diffs(
    dir: &Path,
    pinned: &PinnedRange,
) -> Result<Vec<tauqe_protocol::GitSquashFileDiff>> {
    let range = pinned.rev_range();
    let raw_diff = diff_range(dir, &range, &[])?;
    if raw_diff.trim().is_empty() {
        return Ok(Vec::new());
    }

    let paths = range_paths(dir, &range)?;
    let mut files = Vec::new();
    let mut next_path = 0;

    for line in raw_diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = match paths.get(next_path) {
                Some(path) => path.clone(),
                None => {
                    tracing::warn!(
                        "git diff reported more file blocks than paths; falling back to the block header"
                    );
                    header_path(rest).unwrap_or_else(|| "unknown".to_string())
                }
            };
            next_path += 1;
            files.push(tauqe_protocol::GitSquashFileDiff {
                path,
                diff: String::new(),
            });
        }
        if let Some(block) = files.last_mut() {
            block.diff.push_str(line);
            block.diff.push('\n');
        }
    }

    // Over one pinned pair the two listings always agree, so a leftover path
    // names a format the pairing does not understand rather than a moved ref.
    // It is reported instead of disappearing from the file list.
    if next_path < paths.len() {
        tracing::warn!(
            unreported = paths.len() - next_path,
            "git diff reported fewer file blocks than paths over one pinned range"
        );
    }

    Ok(files)
}

/// Computes the pairing of one pinned range and re-asserts its endpoints.
///
/// The pairing itself reads a fixed snapshot, so it stays mutually consistent
/// whatever the repository does meanwhile. The answer is only truthful while
/// the endpoints still name that snapshot, which is why the drift check runs
/// over the whole computation before the result reaches a caller.
fn file_diffs_of_pinned_range(
    dir: &Path,
    pinned: &PinnedRange,
) -> Result<Vec<tauqe_protocol::GitSquashFileDiff>> {
    let files = pair_file_diffs(dir, pinned)?;
    assert_range_pinned(dir, pinned)?;
    Ok(files)
}

/// Parses cumulative diff into structured per-file diff blocks.
///
/// The range is resolved into one pinned commit pair first, so the patch blocks
/// and the names paired with them describe the same repository snapshot: a
/// commit or ref update landing between the two listings shifts neither the
/// block count nor the name reported for a block. An endpoint that does not
/// resolve fails with Git's own diagnostic, and an endpoint that moved while
/// the listings ran is a typed refusal naming the drift. The public signature
/// and the shape of a file diff stay unchanged, so the squash preview handler
/// keeps calling this entry point.
pub fn get_cumulative_file_diffs(
    dir: &Path,
    base_ref: &str,
) -> Result<Vec<tauqe_protocol::GitSquashFileDiff>> {
    let pinned = pin_range(dir, base_ref)?;
    file_diffs_of_pinned_range(dir, &pinned)
}

/// Squashes all commits ahead of `base_ref` into a single conceptual commit via soft-reset.
///
/// `hooks` is the hook policy the squashed commit follows.
pub fn squash_to_single_commit(
    dir: &Path,
    base_ref: &str,
    message: &str,
    hooks: GitHooks,
) -> Result<String> {
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

    // 2. Commit the staged state solely as developer without AI co-authorship
    commit_with_developer_author(dir, trimmed_msg, false, hooks)?;

    let short_hash = run_git(Some(dir), &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();

    Ok(short_hash)
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
            if let Some(target_path) = target_path {
                fallback_args.push("--");
                fallback_args.push(target_path);
            }
            run_git(Some(dir), &fallback_args)
        }
    }
}

/// Retrieves the diff a single commit introduced against its parent.
///
/// The caller names the revision, so the answer is scoped to exactly one
/// commit: a step commit reports what that step delivered instead of the whole
/// session ahead of a base. The plumbing is `diff-tree --root`, which also
/// renders a commit that has no parent — the first commit of a repository — as
/// the addition of its whole tree, where a diff against a missing parent would
/// fail instead.
pub fn get_commit_diff(dir: &Path, commit: &str) -> Result<String> {
    let commit = commit.trim();
    if commit.is_empty() {
        bail!("Commit cannot be empty");
    }
    run_git(
        Some(dir),
        &["diff-tree", "--root", "--no-commit-id", "--patch", commit],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cmd::commit_with_developer_author;
    use crate::git::GitHooks;

    #[test]
    fn test_cumulative_file_diffs_report_paths_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(Some(root), &["init", "-b", "master"]).unwrap();
        run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
        run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        run_git(Some(root), &["add", "base.txt"]).unwrap();
        commit_with_developer_author(root, "chore: init", false, GitHooks::Bypass).unwrap();
        let base = run_git(Some(root), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        // Names Git escapes under the default `core.quotePath=true`, listed in
        // the byte order Git reports them.
        let names = ["back\\slash.rs", "quote\"d.rs", "отчёт.rs"];
        for name in names {
            std::fs::write(root.join(name), "content\n").unwrap();
        }
        run_git(Some(root), &["add", "-A"]).unwrap();
        commit_with_developer_author(root, "feat: add names", false, GitHooks::Bypass).unwrap();

        let files = get_cumulative_file_diffs(root, &base).unwrap();
        let reported: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            reported, names,
            "each block must carry the path Git listed for it"
        );
        for file in &files {
            assert!(
                file.diff.starts_with("diff --git "),
                "the block keeps its header: {}",
                file.diff
            );
            assert!(
                file.diff.contains("+content"),
                "the block keeps its body: {}",
                file.diff
            );
        }
    }

    /// Creates a repository whose branch holds one commit and returns its hash.
    fn init_repo_with_one_commit(root: &Path) -> String {
        run_git(Some(root), &["init", "-b", "master"]).unwrap();
        run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
        run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        run_git(Some(root), &["add", "base.txt"]).unwrap();
        commit_with_developer_author(root, "chore: init", false, GitHooks::Bypass).unwrap();
        run_git(Some(root), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
    }

    /// Commits the whole working-tree state under the supplied message.
    fn commit_worktree(root: &Path, message: &str) {
        run_git(Some(root), &["add", "-A"]).unwrap();
        commit_with_developer_author(root, message, false, GitHooks::Bypass).unwrap();
    }

    /// Both listings of a pairing address one pinned commit pair, so a ref that
    /// moves afterwards cannot shift a name onto another block or add a file the
    /// pinned snapshot never held.
    #[test]
    fn test_pinned_pairing_describes_one_snapshot_after_head_advanced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let base = init_repo_with_one_commit(root);
        std::fs::write(root.join("first.rs"), "fn first() {}\n").unwrap();
        std::fs::write(root.join("second.rs"), "fn second() {}\n").unwrap();
        commit_worktree(root, "feat: add two files");

        let pinned = pin_range(root, &base).unwrap();

        // The repository advances after the pair was pinned, so the new commit
        // belongs to a later state and must not enter the pinned snapshot.
        std::fs::write(root.join("later.rs"), "fn later() {}\n").unwrap();
        commit_worktree(root, "feat: add a later file");

        let files = pair_file_diffs(root, &pinned).unwrap();
        let reported: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();

        assert_eq!(
            reported,
            ["first.rs", "second.rs"],
            "the block and path counts describe the pinned snapshot"
        );
        for file in &files {
            assert!(
                file.diff.starts_with("diff --git "),
                "the block keeps its header: {}",
                file.diff
            );
            assert!(
                !file.diff.contains("later.rs"),
                "a later commit must not enter the pinned block: {}",
                file.diff
            );
        }
    }

    /// The snapshot is reported only while the endpoints still name it, so a
    /// ref that moved during the pairing is refused instead of presented as the
    /// current squash scope.
    #[test]
    fn test_moved_endpoint_refuses_the_pinned_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let base = init_repo_with_one_commit(root);
        std::fs::write(root.join("first.rs"), "fn first() {}\n").unwrap();
        commit_worktree(root, "feat: add a file");

        let pinned = pin_range(root, &base).unwrap();
        std::fs::write(root.join("later.rs"), "fn later() {}\n").unwrap();
        commit_worktree(root, "feat: move HEAD");
        let moved = run_git(Some(root), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        let error = file_diffs_of_pinned_range(root, &pinned)
            .expect_err("a moved HEAD must refuse the pairing");
        let rendered = format!("{error:#}");

        assert!(rendered.contains("HEAD moved"), "error: {rendered}");
        assert!(rendered.contains(&pinned.head), "error: {rendered}");
        assert!(rendered.contains(&moved), "error: {rendered}");
    }

    /// An endpoint that cannot be resolved fails with Git's diagnostic instead
    /// of pairing against a range that does not exist.
    #[test]
    fn test_unresolved_base_ref_fails_the_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo_with_one_commit(root);

        let error = get_cumulative_file_diffs(root, "no-such-revision")
            .expect_err("a non-resolving base_ref must fail");
        let rendered = format!("{error:#}");

        assert!(
            rendered.contains("no-such-revision"),
            "the diagnostic must name the revision: {rendered}"
        );
        assert!(
            rendered.contains("git command failed"),
            "the refusal comes from Git: {rendered}"
        );
    }
    #[test]
    fn test_commit_diff_is_scoped_to_the_named_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(Some(root), &["init", "-b", "master"]).unwrap();
        run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
        run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
        std::fs::write(root.join("step.rs"), "fn step() {}\n").unwrap();
        run_git(Some(root), &["add", "-A"]).unwrap();
        commit_with_developer_author(root, "feat: add the step", false, GitHooks::Bypass).unwrap();
        let first = run_git(Some(root), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        // The first commit has no parent, so its whole tree is reported as
        // added instead of collapsing into an empty diff.
        let first_diff = get_commit_diff(root, &first).unwrap();
        assert!(first_diff.contains("+fn step() {}"), "{first_diff}");

        std::fs::write(root.join("step.rs"), "fn step() -> u8 { 1 }\n").unwrap();
        std::fs::write(root.join("other.rs"), "fn other() {}\n").unwrap();
        run_git(Some(root), &["add", "-A"]).unwrap();
        commit_with_developer_author(root, "feat: change the step", false, GitHooks::Bypass)
            .unwrap();
        let second = run_git(Some(root), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        // The later commit is answered against its own parent only, so the
        // earlier commit keeps its own unchanged answer.
        let second_diff = get_commit_diff(root, &second).unwrap();
        assert!(
            second_diff.contains("-fn step() {}") && second_diff.contains("+fn step() -> u8 { 1 }"),
            "{second_diff}"
        );
        assert!(second_diff.contains("+fn other() {}"), "{second_diff}");
        assert_eq!(get_commit_diff(root, &first).unwrap(), first_diff);

        // An empty or unknown revision is refused instead of answering a diff
        // of something the caller never named.
        assert!(get_commit_diff(root, "   ").is_err());
        assert!(get_commit_diff(root, "refs/does-not-exist").is_err());
    }
}
