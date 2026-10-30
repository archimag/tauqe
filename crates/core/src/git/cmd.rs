use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

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

/// Reports which of the supplied paths repository ignore rules exclude.
///
/// The candidate paths are written to `git check-ignore` through stdin in
/// NUL-separated form, which keeps a path containing a space, a quote, or a
/// newline intact. Both `.gitignore` files and `.git/info/exclude` take part,
/// because the answer comes from Git itself rather than from a locally
/// re-implemented matcher.
///
/// A change set is batched into a handful of invocations rather than one per
/// path, so the whole list costs a few Git subprocesses instead of hundreds.
///
/// `check-ignore` answers with exit code `1` when none of the supplied paths
/// matches an ignore rule, which is a valid negative answer rather than a
/// failure. Any other non-zero code signals a real failure and is reported
/// with Git's diagnostic output.
pub fn run_git_check_ignore(dir: &Path, paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    crate::blocking::run_blocking(|| {
        let mut ignored = Vec::new();
        for batch in check_ignore_batches(paths) {
            ignored.extend(check_ignore_batch(dir, batch)?);
        }
        Ok(ignored)
    })
}

/// Largest candidate payload handed to a single `check-ignore` invocation.
///
/// The bound stays well below the capacity of an OS pipe buffer, so one
/// invocation can never fill the child's pipes on its own, even when a change
/// set grows beyond the sizes a model-bounded turn produces today.
const CHECK_IGNORE_BATCH_BYTES: usize = 32 * 1024;

/// Splits a candidate list into batches that respect [`CHECK_IGNORE_BATCH_BYTES`].
///
/// A single path longer than the whole budget forms a batch of its own. The
/// candidates keep their order, so the verdicts of the batches concatenate
/// into the answer of the whole change set.
fn check_ignore_batches(paths: &[String]) -> Vec<&[String]> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut payload = 0;
    for (index, path) in paths.iter().enumerate() {
        let entry_bytes = path.len() + 1;
        if index > start && payload + entry_bytes > CHECK_IGNORE_BATCH_BYTES {
            batches.push(&paths[start..index]);
            start = index;
            payload = 0;
        }
        payload += entry_bytes;
    }
    if start < paths.len() {
        batches.push(&paths[start..]);
    }
    batches
}

/// Runs one bounded `check-ignore` invocation over the supplied candidates.
///
/// Both child pipes are drained on dedicated threads while the candidate list
/// is still being written. `check-ignore` streams its verdicts as it consumes
/// standard input, so writing the whole list first and reading afterwards
/// would deadlock as soon as the echoed verdicts filled the child's stdout
/// pipe. The toolchain executor drains its own children the same way.
fn check_ignore_batch(dir: &Path, paths: &[String]) -> Result<Vec<String>> {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout_drain = spawn_pipe_drain(child.stdout.take());
    let stderr_drain = spawn_pipe_drain(child.stderr.take());

    // The handle is scoped so that closing it delivers end of input to
    // `check-ignore` before the child is awaited.
    let write_result = {
        let mut stdin = child.stdin.take().context("Failed to open git stdin")?;
        write_candidate_paths(&mut stdin, paths)
    };

    let status = child.wait()?;
    let stdout = join_pipe_drain(stdout_drain)?;
    let stderr = join_pipe_drain(stderr_drain)?;

    let code = status.code().unwrap_or(-1);
    if code != 0 && code != 1 {
        bail!(
            "git check-ignore failed with exit code {code}: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    write_result?;
    if code == 1 {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&stdout)
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.to_string())
        .collect())
}

/// Writes the candidates as a NUL-separated list and closes the stream.
fn write_candidate_paths(stdin: &mut impl Write, paths: &[String]) -> std::io::Result<()> {
    for path in paths {
        stdin.write_all(path.as_bytes())?;
        stdin.write_all(b"\0")?;
    }
    stdin.flush()
}

/// Drains a child pipe to EOF on its own thread.
fn spawn_pipe_drain<R>(pipe: Option<R>) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut buffer)?;
        }
        Ok(buffer)
    })
}

/// Collects the bytes a drain thread read.
fn join_pipe_drain(handle: std::thread::JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    let read = handle
        .join()
        .map_err(|_| anyhow!("a git check-ignore pipe drain thread panicked"))?;
    read.context("Failed to drain a git check-ignore pipe")
}

/// Stages the specified list of files into the Git index.
///
/// If a file does not exist on disk (for example, after a staged deletion or move),
/// it is only passed to `git add` if it is currently tracked in the Git index.
/// Untracked files that were deleted on disk have no representation in Git and
/// would otherwise cause `git add -- <path>` to abort with a fatal pathspec error.
pub fn stage_files(dir: &Path, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let mut files_to_stage = Vec::new();
    let mut missing_on_disk = Vec::new();

    for f in files {
        if dir.join(f).exists() {
            files_to_stage.push(f.as_str());
        } else {
            missing_on_disk.push(f.as_str());
        }
    }

    if !missing_on_disk.is_empty() {
        // Probe the index in batch for missing files
        let mut ls_args = vec!["ls-files", "-z", "--"];
        ls_args.extend_from_slice(&missing_on_disk);
        if let Ok(tracked_output) = run_git(Some(dir), &ls_args) {
            let tracked: std::collections::HashSet<&str> = tracked_output
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect();
            for missing in missing_on_disk {
                if tracked.contains(missing) {
                    files_to_stage.push(missing);
                }
            }
        }
    }

    if files_to_stage.is_empty() {
        return Ok(());
    }

    let mut add_args = vec!["add", "--"];
    add_args.extend_from_slice(&files_to_stage);
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
        args.extend_from_slice(&["-c", name_override.as_str(), "-c", email_override.as_str()]);
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

    #[test]
    fn check_ignore_batches_paths_and_tolerates_a_negative_answer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        run_git(Some(root), &["add", ".gitignore"]).unwrap();

        let matched =
            run_git_check_ignore(root, &["trace.log".to_string(), "file.txt".to_string()]).unwrap();
        assert_eq!(matched, vec!["trace.log".to_string()]);

        // A path no rule matches answers with exit code 1, which stays a
        // successful call here.
        let matched = run_git_check_ignore(root, &["file.txt".to_string()]).unwrap();
        assert!(matched.is_empty());

        // An empty change set probes nothing and reports nothing.
        assert!(run_git_check_ignore(root, &[]).unwrap().is_empty());
    }

    #[test]
    fn check_ignore_drains_a_candidate_list_larger_than_a_pipe_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        run_git(Some(root), &["add", ".gitignore"]).unwrap();

        // More NUL-terminated candidates than an OS pipe buffer holds, with
        // every path ignored. An implementation that wrote the whole list
        // before it read the echoed verdicts would block here forever.
        let candidates: Vec<String> = (0..8_000)
            .map(|index| format!("generated/build/nested/artifact-{index:05}.log"))
            .collect();
        let payload_bytes: usize = candidates.iter().map(|path| path.len() + 1).sum();
        assert!(payload_bytes > 256 * 1024, "payload: {payload_bytes} bytes");
        assert!(
            check_ignore_batches(&candidates).len() > 1,
            "the stress test must cross several batches"
        );

        let matched = run_git_check_ignore(root, &candidates).unwrap();
        assert_eq!(matched.len(), candidates.len());
        assert_eq!(matched, candidates);
    }
}
