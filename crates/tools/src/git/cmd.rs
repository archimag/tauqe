use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::git::exec::{self, GitDeadline};
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
pub(super) fn git_command(dir: Option<&Path>) -> Command {
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
///
/// Every invocation carries the default wall-clock budget, so a repository hook
/// or a signing agent can never outlive the turn that started it. A caller that
/// holds the turn's cancellation signal or names its own budget calls
/// [`run_git_with_deadline`].
pub fn run_git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    run_git_with_deadline(dir, args, &GitDeadline::default())
}

/// Executes a raw Git command under the supplied wall-clock budget, returning
/// its standard output on success.
///
/// A child that outlives the budget or observes the caller's cancellation
/// signal is torn down with its process group and reported as a failed
/// invocation, so no Git command can stall the turn that started it.
pub fn run_git_with_deadline(
    dir: Option<&Path>,
    args: &[&str],
    deadline: &GitDeadline,
) -> Result<String> {
    // Shelling out to `git` can block for hundreds of milliseconds on a large
    // repository. Run it through the blocking helper so the async worker pool
    // keeps its capacity instead of stalling other tasks.
    crate::blocking::run_blocking(|| {
        let outcome = exec::run_bounded(dir, args, deadline, None);
        if let Some(reason) = outcome.killed_reason(args, deadline.timeout_secs()) {
            bail!("{reason}");
        }
        match outcome.code {
            Some(0) => Ok(String::from_utf8_lossy(&outcome.stdout).to_string()),
            _ => bail!(
                "git command failed: {}",
                String::from_utf8_lossy(&outcome.stderr)
            ),
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
    let deadline = GitDeadline::default();
    run_git_check_ignore_with_deadline(dir, paths, &deadline)
}

/// Reports which of the supplied paths repository ignore rules exclude, under
/// the supplied wall-clock budget.
pub fn run_git_check_ignore_with_deadline(
    dir: &Path,
    paths: &[String],
    deadline: &GitDeadline,
) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    crate::blocking::run_blocking(|| {
        let mut ignored = Vec::new();
        for batch in check_ignore_batches(paths) {
            ignored.extend(check_ignore_batch(dir, batch, deadline)?);
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
/// The child runs under the same bounded executor as every other Git call: its
/// candidate list is written on a dedicated thread while its verdicts are
/// drained on another, so a command that stops reading its standard input is
/// torn down at the invocation budget instead of blocking the caller's write
/// forever. `check-ignore` echoes one verdict per candidate, so writing the
/// whole list before reading would deadlock as soon as the echoed verdicts
/// filled the child's stdout pipe.
///
/// The child is spawned through the shared `git_command` helper, so it inherits
/// the same sanitized environment as every other Git call.
fn check_ignore_batch(dir: &Path, paths: &[String], deadline: &GitDeadline) -> Result<Vec<String>> {
    let args = ["check-ignore", "-z", "--stdin"];
    let payload = candidate_payload(paths);
    let outcome = exec::run_bounded(Some(dir), &args, deadline, Some(payload.as_slice()));

    if let Some(reason) = outcome.killed_reason(&args, deadline.timeout_secs()) {
        bail!("{reason}");
    }

    let code = outcome.code.unwrap_or(-1);
    if code != 0 && code != 1 {
        bail!(
            "git check-ignore failed with exit code {code}: {}",
            String::from_utf8_lossy(&outcome.stderr).trim()
        );
    }
    if code == 1 {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&outcome.stdout)
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.to_string())
        .collect())
}

/// Renders the candidate list as the NUL-separated payload `check-ignore` reads
/// from its standard input.
///
/// NUL separation keeps a path containing a space, a quote, or a newline
/// intact.
fn candidate_payload(paths: &[String]) -> Vec<u8> {
    let mut payload = Vec::new();
    for path in paths {
        payload.extend_from_slice(path.as_bytes());
        payload.push(0);
    }
    payload
}

/// Bytes the drain thread reads from a child pipe before it publishes them.
const PIPE_CHUNK_BYTES: usize = 16 * 1024;

/// A child pipe drained on its own thread.
///
/// The bytes are published into a shared buffer as the reader consumes them, so
/// a reader that is abandoned — because a descendant inherited the pipe and kept
/// it open past the invocation window — still reports everything the child wrote
/// before the window closed.
pub(super) struct PipeDrain {
    reader: std::thread::JoinHandle<std::io::Result<()>>,
    delivered: Arc<Mutex<Vec<u8>>>,
}

impl PipeDrain {
    /// Reports whether the reader reached its end on its own.
    pub(super) fn is_finished(&self) -> bool {
        self.reader.is_finished()
    }

    /// Collects the bytes the reader delivered.
    ///
    /// A reader that reached its end is joined, so a failed or panicked drain
    /// stays a diagnostic. A reader that a descendant still holds open is never
    /// joined: the bytes it already published come back instead, and the reader
    /// is abandoned with the buffer it keeps filling.
    pub(super) fn collect(self) -> Vec<u8> {
        if self.reader.is_finished() {
            match self.reader.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(reason = %err, "failed to drain a git child pipe");
                }
                Err(_) => {
                    tracing::warn!("a git child pipe drain thread panicked");
                }
            }
        }
        match self.delivered.lock() {
            Ok(mut delivered) => std::mem::take(&mut *delivered),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

/// Drains a child pipe to EOF on its own thread.
///
/// Reading a pipe independently of the child's exit keeps a command that emits
/// more than a pipe buffer holds from blocking on its own output, and it lets
/// the bounded executor observe the child it started while the pipes drain.
pub(super) fn spawn_pipe_drain<R>(pipe: Option<R>) -> PipeDrain
where
    R: Read + Send + 'static,
{
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&delivered);
    let reader = std::thread::spawn(move || -> std::io::Result<()> {
        let Some(mut pipe) = pipe else {
            return Ok(());
        };
        let mut chunk = [0u8; PIPE_CHUNK_BYTES];
        loop {
            let read = pipe.read(&mut chunk)?;
            if read == 0 {
                return Ok(());
            }
            match sink.lock() {
                Ok(mut buffer) => buffer.extend_from_slice(&chunk[..read]),
                Err(poisoned) => poisoned.into_inner().extend_from_slice(&chunk[..read]),
            }
        }
    });
    PipeDrain { reader, delivered }
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
    let deadline = GitDeadline::default();
    stage_files_with_deadline(dir, files, &deadline)
}

/// Stages the specified list of files under the supplied wall-clock budget.
pub fn stage_files_with_deadline(
    dir: &Path,
    files: &[String],
    deadline: &GitDeadline,
) -> Result<()> {
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
        run_git_with_deadline(Some(dir), &add_args, deadline)?;
    }

    if !removed.is_empty() {
        let mut remove_args = vec!["rm", "--cached", "--ignore-unmatch", "-f", "-q", "--"];
        remove_args.extend_from_slice(&removed);
        run_git_with_deadline(Some(dir), &remove_args, deadline)?;
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
    let deadline = GitDeadline::default();
    commit_with_developer_author_with_deadline(dir, message, allow_empty, hooks, &deadline)
}

/// Commits staged changes under the repository's developer identity, under the
/// supplied wall-clock budget.
///
/// The budget is the only difference from [`commit_with_developer_author`]: a
/// repository hook or a signing agent that waits on a passphrase prompt is torn
/// down at the budget instead of occupying the caller.
pub fn commit_with_developer_author_with_deadline(
    dir: &Path,
    message: &str,
    allow_empty: bool,
    hooks: GitHooks,
    deadline: &GitDeadline,
) -> Result<()> {
    let identity_resolved =
        run_git_with_deadline(Some(dir), &["var", "GIT_AUTHOR_IDENT"], deadline).is_ok();

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

    run_git_with_deadline(Some(dir), &args, deadline).context("Failed to commit staged changes")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitHooks;

    #[cfg(unix)]
    use std::time::{Duration, Instant};

    fn init_repo(dir: &Path) {
        run_git(Some(dir), &["init"]).unwrap();
        run_git(Some(dir), &["config", "user.name", "Test"]).unwrap();
        run_git(Some(dir), &["config", "user.email", "test@test.com"]).unwrap();
        std::fs::write(dir.join("file.txt"), "content").unwrap();
        run_git(Some(dir), &["add", "file.txt"]).unwrap();
    }

    #[cfg(unix)]
    fn install_failing_pre_commit_hook(dir: &Path) {
        install_hook(dir, "#!/bin/sh\nexit 1\n");
    }

    /// Writes an executable `pre-commit` hook into the repository.
    #[cfg(unix)]
    fn install_hook(dir: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;

        let hooks = dir.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, script).unwrap();
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

    #[test]
    fn a_git_invocation_round_trips_its_standard_output() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);

        let output = run_git(Some(root), &["rev-parse", "--is-inside-work-tree"]).unwrap();

        assert_eq!(output.trim(), "true");
    }

    #[test]
    fn a_failing_exit_status_keeps_the_git_command_failed_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);

        let error = run_git(Some(root), &["rev-parse", "--verify", "no-such-revision"])
            .expect_err("an unresolvable revision fails the invocation");

        assert!(
            error.to_string().contains("git command failed"),
            "error: {error}"
        );
    }

    /// Hook that announces its start and then sleeps far beyond any test budget.
    #[cfg(unix)]
    const SLEEPING_HOOK: &str = "#!/bin/sh\necho started > hook-started.log\nsleep 30\n";

    /// Hook whose grandchild keeps a heartbeat alive while the hook itself sleeps.
    #[cfg(unix)]
    const HEARTBEAT_HOOK: &str = "#!/bin/sh\nsh -c 'while true; do echo tick >> heartbeat.log; sleep 0.1; done' &\nsleep 30\n";

    /// How long a heartbeat fixture is observed after the process-group kill.
    ///
    /// The fixture appends one tick every 100 ms, so a writer that survived the
    /// kill adds several ticks inside this window and changes the log size.
    #[cfg(unix)]
    const HEARTBEAT_WINDOW: Duration = Duration::from_millis(400);

    /// A repository hook that never returns is torn down at the invocation
    /// budget, together with every descendant it spawned.
    #[cfg(unix)]
    #[test]
    fn a_sleeping_hook_exceeds_the_budget_and_its_descendants_stop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        install_hook(root, HEARTBEAT_HOOK);

        let started = Instant::now();
        let error = run_git_with_deadline(
            Some(root),
            &["commit", "-m", "hooked"],
            &GitDeadline::with_timeout(1),
        )
        .expect_err("the hook outlives the one-second budget");

        assert!(
            error.to_string().contains("exceeded its wall-clock budget"),
            "error: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the budget did not release the turn: {:?}",
            started.elapsed()
        );

        let heartbeat = root.join("heartbeat.log");
        assert!(heartbeat.exists(), "the hook's grandchild never wrote");
        let size_at_kill = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);
        std::thread::sleep(HEARTBEAT_WINDOW);
        let size_later = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert_eq!(
            size_at_kill, size_later,
            "the grandchild kept writing after the process-group kill"
        );
    }

    /// A flipped cancellation receiver kills the child instead of waiting the
    /// repository hook out.
    #[cfg(unix)]
    #[test]
    fn a_cancelled_invocation_kills_the_child_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        install_hook(root, SLEEPING_HOOK);

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let deadline = GitDeadline::from_cancel(cancel_rx);
        let path = root.to_path_buf();
        let started = Instant::now();
        let turn = std::thread::spawn(move || {
            run_git_with_deadline(
                Some(path.as_path()),
                &["commit", "-m", "cancelled"],
                &deadline,
            )
        });

        // Cancel only once the hook is demonstrably running, so the prompt
        // return proves the teardown instead of a slow start.
        let marker = root.join("hook-started.log");
        let mut hook_started = false;
        for _ in 0..100 {
            if marker.exists() {
                hook_started = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(hook_started, "the hook never started");
        let _ = cancel_tx.send(true);

        let error = turn
            .join()
            .unwrap()
            .expect_err("a cancelled invocation reports no answer");

        assert!(
            error.to_string().contains("was cancelled"),
            "error: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the cancellation did not release the turn: {:?}",
            started.elapsed()
        );
    }

    /// Hook that succeeds at once and leaves a descendant holding the inherited
    /// output pipes behind.
    ///
    /// Git runs a hook with its standard output redirected to its own standard
    /// error, so the marker the hook prints travels on the stderr pipe that the
    /// descendant keeps open.
    #[cfg(unix)]
    const PIPE_HOLDING_HOOK: &str = "#!/bin/sh\necho hook-output\nsleep 5 &\nexit 0\n";

    /// A command that exited on its own keeps its exit code and its output when
    /// a descendant holds the inherited pipes open.
    ///
    /// The hook exits successfully and leaves a background descendant behind, so
    /// the direct child exits with status 0 well inside its budget while the
    /// pipes stay open. The wait owns the verdict: the invocation must stay
    /// `Exited` with its code intact, and every byte the child wrote before the
    /// drain window closed must survive the abandoned drain.
    #[cfg(unix)]
    #[test]
    fn a_clean_exit_keeps_its_output_when_a_descendant_holds_the_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        install_hook(root, PIPE_HOLDING_HOOK);

        let started = Instant::now();
        let outcome = exec::run_bounded(
            Some(root),
            &["commit", "-m", "hooked"],
            &GitDeadline::with_timeout(1),
            None,
        );

        assert_eq!(
            outcome.end,
            exec::GitEnd::Exited,
            "a command that exited on its own is never reported as killed"
        );
        assert_eq!(outcome.code, Some(0));
        assert!(
            String::from_utf8_lossy(&outcome.stdout).contains("hooked"),
            "the output of the exited child must survive the drain: {}",
            String::from_utf8_lossy(&outcome.stdout)
        );
        assert!(
            String::from_utf8_lossy(&outcome.stderr).contains("hook-output"),
            "the bytes written before the drain window closed must survive: {}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the abandoned drain did not release the caller: {:?}",
            started.elapsed()
        );
    }
}
