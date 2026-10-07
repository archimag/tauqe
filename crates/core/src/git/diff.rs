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

/// Parses cumulative diff into structured per-file diff blocks.
pub fn get_cumulative_file_diffs(
    dir: &Path,
    base_ref: &str,
) -> Result<Vec<tauqe_protocol::GitSquashFileDiff>> {
    let raw_diff = get_cumulative_diff(dir, base_ref)?;
    let mut files = Vec::new();
    if raw_diff.trim().is_empty() {
        return Ok(files);
    }

    let mut current_path = String::new();
    let mut current_diff = String::new();

    for line in raw_diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if !current_path.is_empty() {
                files.push(tauqe_protocol::GitSquashFileDiff {
                    path: current_path,
                    diff: current_diff,
                });
                current_diff = String::new();
            }
            if let Some(idx) = rest.rfind(" b/") {
                let b_path = &rest[idx + 3..];
                current_path = b_path.trim_matches('"').to_string();
            } else {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 2 {
                    let b_path = parts[1].strip_prefix("b/").unwrap_or(parts[1]);
                    current_path = b_path.trim_matches('"').to_string();
                } else {
                    current_path = "unknown".to_string();
                }
            }
            current_diff.push_str(line);
            current_diff.push('\n');
        } else {
            current_diff.push_str(line);
            current_diff.push('\n');
        }
    }

    if !current_path.is_empty() {
        files.push(tauqe_protocol::GitSquashFileDiff {
            path: current_path,
            diff: current_diff,
        });
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
