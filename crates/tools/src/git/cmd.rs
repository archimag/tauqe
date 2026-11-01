use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::git::GitHooks;

pub const DEFAULT_AUTHOR_NAME: &str = "Developer";
pub const DEFAULT_AUTHOR_EMAIL: &str = "developer@tauqe.dev";

/// Environment variables that redirect a Git child away from the repository its
/// working directory names.
///
/// A daemon launched from a Git hook or a scripted environment inherits them,
/// and each one silently overrides a decision the substrate believes it made by
/// passing the repository directory down. The identity variables
/// (`GIT_AUTHOR_*`, `GIT_COMMITTER_*`) are deliberately kept: the commit path
/// probes and supplements the identity they carry.
const REDIRECTED_GIT_ENVIRONMENT: &[&str] = &[
    // Repository, work tree, index, ref namespace, and object store.
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    // Injected configuration, which can rewrite hooks, identity, and repository
    // paths behind the caller's back.
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
];

/// Prefixes of the numbered `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>`
/// configuration-injection variables.
const REDIRECTED_GIT_ENVIRONMENT_PREFIXES: &[&str] = &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

/// Builds a `git` command scoped to the supplied repository directory.
///
/// Every Git child of this crate starts here, so the repository resolution
/// rests on the working directory alone. The redirection family declared above
/// is removed from the child environment only: the identity variables the
/// commit path probes stay inherited, together with this process's own
/// environment.
fn git_command(dir: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(d) = dir {
        command.current_dir(d);
    }
    for key in REDIRECTED_GIT_ENVIRONMENT {
        command.env_remove(*key);
    }
    // The numbered configuration family carries no fixed name of its own, so it
    // is matched by prefix against the inherited environment.
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        let injected = REDIRECTED_GIT_ENVIRONMENT_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(*prefix));
        if injected {
            command.env_remove(&key);
        }
    }
    command
}

/// Executes a raw Git command in the given repository directory.
pub fn run_git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    // Shelling out to `git` can block for hundreds of milliseconds on a large
    // repository. Run it through the blocking helper so the async worker pool
    // keeps its capacity instead of stalling other tasks.
    crate::blocking::run_blocking(|| {
        let output = git_command(dir).args(args).output()?;
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
///
/// The child is spawned through the shared `git_command` helper, so it inherits
/// the same sanitized environment as every other Git call.
fn check_ignore_batch(dir: &Path, paths: &[String]) -> Result<Vec<String>> {
    let mut child = git_command(Some(dir))
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
/// A path the change set removed — a deleted file or the source of a move — has
/// no working-tree file left, and `git add` rejects a path Git cannot match as an
/// unmatched pathspec. Such a path is recorded through `git rm --cached`
/// instead, which carries the removal of a tracked file into the index without
/// asking for a file that is deliberately gone. `--ignore-unmatch` keeps a path
/// that was never tracked — a draft that never reached Git, or a file created
/// and deleted inside one turn — from aborting the staging call.
///
/// The answer comes from Git rather than from a probed guess about the index, so
/// a move lands as one change: the destination is added and the source is
/// removed in the same commit.
pub fn stage_files(dir: &Path, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let mut present = Vec::new();
    let mut removed = Vec::new();

    for path in files {
        if dir.join(path).exists() {
            present.push(path.as_str());
        } else {
            removed.push(path.as_str());
        }
    }

    if !present.is_empty() {
        let mut add_args = vec!["add", "--"];
        add_args.extend_from_slice(&present);
        run_git(Some(dir), &add_args)?;
    }

    if !removed.is_empty() {
        let mut remove_args = vec!["rm", "--cached", "--ignore-unmatch", "-f", "-q", "--"];
        remove_args.extend_from_slice(&removed);
        run_git(Some(dir), &remove_args)?;
    }

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
/// `hooks` carries the hook policy resolved by the caller that owns the project
/// configuration. The substrate never reads `tauqe.toml` itself, so the decision
/// whether an autonomous commit may stall on a user hook arrives from outside;
/// [`GitHooks::Bypass`] is the harness default, which lets a turn commit the
/// state it has already verified.
pub fn commit_with_developer_author(
    dir: &Path,
    message: &str,
    allow_empty: bool,
    hooks: GitHooks,
) -> Result<()> {
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
    if hooks.bypasses() {
        args.push("--no-verify");
    }

    run_git(Some(dir), &args).context("Failed to commit staged changes")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitHooks;

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
    fn bypass_policy_skips_a_failing_pre_commit_hook() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        install_failing_pre_commit_hook(root);

        // The bypass policy is the harness default, so a failing project hook
        // must not block the commit of a verified change set.
        assert!(commit_with_developer_author(root, "msg", false, GitHooks::Bypass).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn run_policy_executes_the_pre_commit_hook() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        install_failing_pre_commit_hook(root);

        // A project that resolved the policy to `Run` sees its hook execute, so
        // the hook failure blocks the commit.
        assert!(commit_with_developer_author(root, "msg", false, GitHooks::Run).is_err());
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
    #[test]
    fn stage_files_records_a_move_as_a_removal_and_an_addition() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::create_dir_all(root.join("src/old")).unwrap();
        std::fs::write(root.join("src/old/moved.rs"), "fn moved() {}\n").unwrap();
        run_git(Some(root), &["add", "src/old/moved.rs"]).unwrap();
        commit_with_developer_author(root, "add the moved file", false, GitHooks::Bypass).unwrap();

        // The move takes the file out of its source directory and puts it into
        // the destination, leaving the source directory with no file inside it.
        std::fs::create_dir_all(root.join("src/new")).unwrap();
        std::fs::write(root.join("src/new/moved.rs"), "fn moved() {}\n").unwrap();
        std::fs::remove_dir_all(root.join("src/old")).unwrap();

        stage_files(
            root,
            &[
                "src/old/moved.rs".to_string(),
                "src/new/moved.rs".to_string(),
            ],
        )
        .unwrap();

        // A path that never reached the disk and never entered the index is
        // ignored instead of aborting the staging call.
        stage_files(root, &["src/ghost.rs".to_string()]).unwrap();

        // The index now differs from HEAD exactly by the removal of the source
        // and the addition of the destination, which is what the commit records.
        let staged = run_git(
            Some(root),
            &["diff", "--cached", "--name-status", "--no-renames"],
        )
        .unwrap();
        assert!(
            staged.contains("D\tsrc/old/moved.rs"),
            "the removal must reach the index: {staged}"
        );
        assert!(
            staged.contains("A\tsrc/new/moved.rs"),
            "the destination must reach the index: {staged}"
        );
    }
    #[test]
    fn every_git_child_carries_the_redirection_removals() {
        let command = git_command(None);
        let removals: Vec<&str> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(key, _)| key.to_str())
            .collect();

        // The declared list is the contract of the helper: a name that drops
        // out of it silently re-opens the redirection it was added to close.
        for key in REDIRECTED_GIT_ENVIRONMENT {
            assert!(
                removals.contains(key),
                "{key} must be removed from the child environment"
            );
        }
    }
}
