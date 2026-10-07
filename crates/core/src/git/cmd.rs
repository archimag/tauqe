use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

pub const DEFAULT_AUTHOR_NAME: &str = "Developer";
pub const DEFAULT_AUTHOR_EMAIL: &str = "developer@tauqe.dev";

/// Executes a raw Git command in the given repository directory.
pub fn run_git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
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

/// Stages the specified list of files into the Git index.
pub fn stage_files(dir: &Path, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let mut add_args = vec!["add", "--"];
    for f in files {
        add_args.push(f.as_str());
    }
    run_git(Some(dir), &add_args)?;
    Ok(())
}

/// Commits staged changes preserving developer author identity and signing configuration.
pub fn commit_with_developer_author(dir: &Path, message: &str, allow_empty: bool) -> Result<()> {
    let has_name = run_git(Some(dir), &["config", "user.name"]).is_ok();
    let has_email = run_git(Some(dir), &["config", "user.email"]).is_ok();

    let mut args = Vec::new();
    if !has_name {
        args.extend_from_slice(&["-c", "user.name=Developer"]);
    }
    if !has_email {
        args.extend_from_slice(&["-c", "user.email=developer@tauqe.dev"]);
    }
    args.push("commit");
    if allow_empty {
        args.push("--allow-empty");
    }
    args.extend_from_slice(&["-m", message, "--no-verify"]);

    run_git(Some(dir), &args).context("Failed to commit staged changes")?;
    Ok(())
}
