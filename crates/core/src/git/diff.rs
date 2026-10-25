use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::checkpoint::is_tauqe_commit;
use super::cmd::{commit_with_developer_author, run_git};

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

/// Retrieves cumulative diff between `base_ref` and HEAD.
pub fn get_cumulative_diff(dir: &Path, base_ref: &str) -> Result<String> {
    let rev_range = format!("{}..HEAD", base_ref.trim());
    run_git(Some(dir), &["diff", &rev_range])
}

/// Retrieves git diff --stat summary between `base_ref` and HEAD.
pub fn get_diff_stat(dir: &Path, base_ref: &str) -> Result<String> {
    let rev_range = format!("{}..HEAD", base_ref.trim());
    let stat = run_git(Some(dir), &["diff", "--stat", &rev_range])?;
    let last_line = stat
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| "0 files changed".to_string());
    Ok(last_line)
}

/// Lists the paths changed between `base_ref` and HEAD exactly as they exist on
/// disk.
///
/// `--name-only -z` terminates each path with a NUL and disables Git's C-style
/// path quoting, so a non-ASCII name, an embedded double quote, or a backslash
/// survives instead of arriving as an escaped string. Git emits these entries in
/// the same order as the patch blocks of the same revision range, which is what
/// lets the per-file diffs be paired with an unquoted name.
fn get_cumulative_paths(dir: &Path, base_ref: &str) -> Result<Vec<String>> {
    let rev_range = format!("{}..HEAD", base_ref.trim());
    let output = run_git(Some(dir), &["diff", "--name-only", "-z", &rev_range])?;
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

/// Parses cumulative diff into structured per-file diff blocks.
///
/// Paths are taken from the NUL-separated `--name-only` listing rather than from
/// the block headers, so a file whose name Git would escape under the default
/// `core.quotePath=true` is reported under its real on-disk name.
pub fn get_cumulative_file_diffs(
    dir: &Path,
    base_ref: &str,
) -> Result<Vec<tauqe_protocol::GitSquashFileDiff>> {
    let raw_diff = get_cumulative_diff(dir, base_ref)?;
    let mut files = Vec::new();
    if raw_diff.trim().is_empty() {
        return Ok(files);
    }

    let paths = get_cumulative_paths(dir, base_ref)?;
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

    Ok(files)
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

    // 2. Commit the staged state solely as developer without AI co-authorship
    commit_with_developer_author(dir, trimmed_msg, false)?;

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
            if let Some(path) = target_path {
                fallback_args.push("--");
                fallback_args.push(path);
            }
            run_git(Some(dir), &fallback_args)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cmd::commit_with_developer_author;

    #[test]
    fn test_cumulative_file_diffs_report_paths_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(Some(root), &["init", "-b", "master"]).unwrap();
        run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
        run_git(Some(root), &["config", "user.email", "test@test.com"]).unwrap();
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        run_git(Some(root), &["add", "base.txt"]).unwrap();
        commit_with_developer_author(root, "chore: init", false).unwrap();
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
        commit_with_developer_author(root, "feat: add names", false).unwrap();

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
}
