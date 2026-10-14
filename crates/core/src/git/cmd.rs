use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

use crate::config::load_git_config;

pub const DEFAULT_AUTHOR_NAME: &str = "Developer";
pub const DEFAULT_AUTHOR_EMAIL: &str = "developer@tauqe.dev";

/// Executes a raw Git command in the given repository directory.
pub fn run_git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    // Shelling out to `git` can block for hundreds of milliseconds on a large
    // repository. Run it through the blocking helper so the async worker pool
    // keeps its capacity instead of stalling other tasks.
    crate::blocking::run_blocking(|| {
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
    })
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

/// Commits staged changes under the repository's developer identity.
///
/// The identity is probed with a single `git var GIT_AUTHOR_IDENT` call. When
/// Git can resolve a complete `user.name`/`user.email` identity the commit
/// inherits it verbatim; otherwise both built-in defaults are injected in one
/// shot. This replaces the previous pair of `git config user.name` and
/// `git config user.email` lookups, which cost two extra processes per commit.
///
/// Pre-commit and commit-msg hooks are bypassed with `--no-verify` only when
/// the repository's `[git].bypass_hooks` setting is enabled. It defaults to
/// enabled so an autonomous turn never stalls on user hooks, but a project can
/// opt in to running its hooks on every Tauqe commit through `tauqe.toml`.
pub fn commit_with_developer_author(dir: &Path, message: &str, allow_empty: bool) -> Result<()> {
    let bypass_hooks = load_git_config(dir).bypass_hooks;
    let identity_resolved = run_git(Some(dir), &["var", "GIT_AUTHOR_IDENT"]).is_ok();

    // Keep the override strings alive until `run_git` consumes the argument
    // vector, since the vector borrows them as `&str`.
    let name_override = format!("user.name={DEFAULT_AUTHOR_NAME}");
    let email_override = format!("user.email={DEFAULT_AUTHOR_EMAIL}");

    let mut args = Vec::new();
    if !identity_resolved {
        args.extend_from_slice(&[
            "-c",
            name_override.as_str(),
            "-c",
            email_override.as_str(),
        ]);
    }
    args.push("commit");
    if allow_empty {
        args.push("--allow-empty");
    }
    args.extend_from_slice(&["-m", message]);
    if bypass_hooks {
        args.push("--no-verify");
    }

    run_git(Some(dir), &args).context("Failed to commit staged changes")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        run_git(Some(dir), &["init"]).unwrap();
        run_git(Some(dir), &["config", "user.name", "Test"]).unwrap();
        run_git(Some(dir), &["config", "user.email", "test@test.com"]).unwrap();
        std::fs::write(dir.join("file.txt"), "content").unwrap();
        run_git(Some(dir), &["add", "file.txt"]).unwrap();
    }

    #[cfg(unix)]
    fn install_failing_pre_commit_hook(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let hooks = dir.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bypass_hooks_default_skips_failing_pre_commit_hook() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        install_failing_pre_commit_hook(root);

        // No project config means the default bypass stays on, so the failing
        // hook must not block the commit.
        assert!(commit_with_developer_author(root, "msg", false).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn disabled_bypass_hooks_runs_pre_commit_hook() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        install_failing_pre_commit_hook(root);
        std::fs::write(root.join("tauqe.toml"), "[git]\nbypass_hooks = false\n").unwrap();

        // With the bypass disabled the hook runs and its failure blocks the
        // commit.
        assert!(commit_with_developer_author(root, "msg", false).is_err());
    }
}
