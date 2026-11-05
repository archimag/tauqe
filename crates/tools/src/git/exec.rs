//! Bounded wall-clock execution of Git children.
//!
//! Every Git invocation of the substrate runs through this module, so no child
//! outlives the turn that spawned it. A Git command is never only Git: a commit
//! that runs the repository hooks (`GitHooks::Run`) executes arbitrary user
//! code, and a `commit.gpgsign` configuration waits for a signing agent that may
//! be blocked on a passphrase prompt. Waiting for such a child without a
//! deadline occupies a blocking thread for as long as it chooses to run — on a
//! current-thread runtime the only executor thread — and freezes provider
//! streaming, RPC handling, and cancellation for the whole process.
//!
//! The discipline mirrors the toolchain executor (`toolchain::exec`): the child
//! leads its own process group, one wall-clock budget covers the wait and the
//! output drain, the caller's cancellation signal races the natural exit, and
//! expiry tears the whole group down instead of abandoning descendants.

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::cmd::{git_command, spawn_pipe_drain};

/// Wall-clock budget of a Git invocation that names none of its own.
pub const DEFAULT_GIT_TIMEOUT_SECS: u64 = 300;

/// Shortest pause between two observations of a running Git child.
const POLL_FLOOR: Duration = Duration::from_millis(2);

/// Longest pause between two observations of a running Git child.
///
/// The pause backs off from [`POLL_FLOOR`] to this ceiling, so a short command
/// reports its completion without the latency a fixed ceiling would add, while
/// a long one never spins.
const POLL_CEILING: Duration = Duration::from_millis(50);

/// Grace period a Git child receives between the polite termination request
/// and the forceful kill.
///
/// Git removes the lock files it created from the termination handler it
/// installs, so a child that is killed outright leaves `.git/index.lock`
/// behind and every later invocation of the same repository fails on it. The
/// grace period is the window in which the child runs that handler before the
/// group is torn down.
pub const TERMINATION_GRACE: Duration = Duration::from_millis(300);

/// The polite termination signal: it lets the child clean up after itself.
#[cfg(unix)]
const SIGTERM: i32 = 15;

/// The signal that carries no payload and only probes group membership.
#[cfg(unix)]
const SIG_EXISTENCE: i32 = 0;

/// The wall-clock budget and the cancellation signal a Git child runs under.
pub struct GitDeadline {
    timeout_secs: u64,
    cancel: Option<watch::Receiver<bool>>,
}

impl Default for GitDeadline {
    fn default() -> Self {
        Self {
            timeout_secs: DEFAULT_GIT_TIMEOUT_SECS,
            cancel: None,
        }
    }
}

impl GitDeadline {
    /// Builds a deadline that bounds the child under the supplied budget.
    ///
    /// A budget of zero expires at once; a caller that names a budget owns the
    /// value it declares.
    pub fn with_timeout(timeout_secs: u64) -> Self {
        Self {
            timeout_secs,
            cancel: None,
        }
    }

    /// Builds a deadline that follows the turn's cancellation signal under the
    /// default wall-clock budget.
    pub fn from_cancel(cancel: watch::Receiver<bool>) -> Self {
        Self {
            timeout_secs: DEFAULT_GIT_TIMEOUT_SECS,
            cancel: Some(cancel),
        }
    }
    /// Attaches the caller's cancellation signal to the deadline.
    ///
    /// The budget is resolved from the project configuration and the signal
    /// from the turn that runs the command, so the two arrive separately and
    /// combine here instead of at every call site.
    pub fn with_cancel(mut self, cancel: Option<watch::Receiver<bool>>) -> Self {
        self.cancel = cancel;
        self
    }

    /// The wall-clock budget in seconds.
    pub(crate) fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }

    /// Reports whether the caller cancelled the turn this child belongs to.
    fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|cancel| *cancel.borrow())
    }

    /// Reports whether the wall-clock budget measured from `started` is spent.
    fn is_spent(&self, started: Instant) -> bool {
        started.elapsed() >= Duration::from_secs(self.timeout_secs)
    }
}

/// How a bounded Git child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitEnd {
    /// The child exited on its own and its status was collected.
    Exited,
    /// The wall-clock budget expired and the process group was killed.
    TimedOut,
    /// The turn was cancelled and the process group was killed.
    Cancelled,
}

/// What a bounded Git child produced.
pub(super) struct GitOutput {
    /// How the child ended.
    pub(super) end: GitEnd,
    /// The exit code Git reported, present only when the child exited on its own.
    pub(super) code: Option<i32>,
    /// The bytes the child wrote to its standard output.
    pub(super) stdout: Vec<u8>,
    /// The bytes the child wrote to its standard error, or the reason it
    /// produced no answer at all.
    pub(super) stderr: Vec<u8>,
}

impl GitOutput {
    /// Renders why the invocation was torn down, or `None` when the child
    /// exited on its own.
    pub(super) fn killed_reason(&self, args: &[&str], timeout_secs: u64) -> Option<String> {
        let command = render_command(args);
        match self.end {
            GitEnd::Exited => None,
            GitEnd::TimedOut => Some(format!(
                "git command `{command}` exceeded its wall-clock budget of {timeout_secs}s and was killed"
            )),
            GitEnd::Cancelled => Some(format!("git command `{command}` was cancelled")),
        }
    }
}

/// Runs a Git child under the supplied budget.
///
/// `stdin_payload` is the only input a Git child receives: when it is present
/// the child starts with a piped standard input and the payload is written to
/// it, which is how `check-ignore` reads its candidate list. Without a payload
/// the child receives no input at all, so a command can never consume the
/// daemon's own request stream.
pub(super) fn run_bounded(
    dir: Option<&Path>,
    args: &[&str],
    deadline: &GitDeadline,
    stdin_payload: Option<&[u8]>,
) -> GitOutput {
    let display = render_command(args);
    let started = Instant::now();

    let mut command = git_command(dir);
    command.args(args);
    let input = if stdin_payload.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    command.stdin(input);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    isolate_process_group(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return GitOutput {
                end: GitEnd::Exited,
                code: None,
                stdout: Vec::new(),
                stderr: err.to_string().into_bytes(),
            };
        }
    };

    let stdin_writer = stdin_payload.map(|payload| {
        let payload = payload.to_vec();
        spawn_stdin_writer(child.stdin.take(), payload)
    });
    let stdout_drain = spawn_pipe_drain(child.stdout.take());
    let stderr_drain = spawn_pipe_drain(child.stderr.take());

    let waited = wait_under_budget(&mut child, deadline, started, &display);
    let (end, code, probe_reason) = match waited {
        Wait::Exited(status) => (GitEnd::Exited, status.code(), None),
        Wait::TimedOut => (GitEnd::TimedOut, None, None),
        Wait::Cancelled => (GitEnd::Cancelled, None, None),
        Wait::Unobservable(err) => (GitEnd::Exited, None, Some(err.to_string())),
    };

    let drained = collect_output(stdout_drain, stderr_drain, deadline, started, &display);
    let Some((stdout, drained_stderr)) = drained else {
        // A descendant inherited the pipes and never exited. The invocation
        // window closed, so the readers are abandoned instead of waiting for a
        // process that may never finish.
        let end = if deadline.is_cancelled() {
            GitEnd::Cancelled
        } else {
            GitEnd::TimedOut
        };
        return GitOutput {
            end,
            code: None,
            stdout: Vec::new(),
            stderr: b"a descendant process held the output pipes open".to_vec(),
        };
    };

    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }

    GitOutput {
        end,
        code,
        stdout,
        stderr: probe_reason
            .map(String::into_bytes)
            .unwrap_or(drained_stderr),
    }
}

/// How the wait for a Git child ended.
enum Wait {
    /// The child exited and its status was collected.
    Exited(std::process::ExitStatus),
    /// The wall-clock budget expired; the child was torn down.
    TimedOut,
    /// The turn was cancelled; the child was torn down.
    Cancelled,
    /// The status probe failed, so the child was torn down unobserved.
    Unobservable(std::io::Error),
}

/// Waits for the child to exit under the budget.
///
/// Expiry or cancellation tears the process group down through [`terminate`],
/// so the child receives the polite signal first and the group has the grace
/// period to release its locks before anything is killed outright.
fn wait_under_budget(
    child: &mut Child,
    deadline: &GitDeadline,
    started: Instant,
    rendered: &str,
) -> Wait {
    let mut interval = POLL_FLOOR;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Wait::Exited(status),
            Ok(None) => {}
            Err(err) => {
                terminate(child);
                tracing::warn!(
                    command = %rendered,
                    reason = %err,
                    "git child status probe failed; its process group was wound down"
                );
                return Wait::Unobservable(err);
            }
        }
        if deadline.is_cancelled() {
            terminate(child);
            tracing::warn!(
                command = %rendered,
                "git child was cancelled; its process group was wound down"
            );
            return Wait::Cancelled;
        }
        if deadline.is_spent(started) {
            terminate(child);
            tracing::warn!(
                command = %rendered,
                budget_secs = deadline.timeout_secs(),
                "git child exceeded its wall-clock budget; its process group was wound down"
            );
            return Wait::TimedOut;
        }
        std::thread::sleep(interval);
        interval = (interval * 2).min(POLL_CEILING);
    }
}

/// Waits for both output pipes to reach EOF under the invocation window.
///
/// The direct child can exit while a descendant it spawned keeps the inherited
/// pipes open. The wait is bounded by the same budget and cancellation signal
/// that bound the child, so a cancelled or overdue turn is released instead of
/// waiting for a process that may never exit.
fn collect_output(
    stdout_drain: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stderr_drain: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    deadline: &GitDeadline,
    started: Instant,
    rendered: &str,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut interval = POLL_FLOOR;
    loop {
        if stdout_drain.is_finished() && stderr_drain.is_finished() {
            return Some((collect_drain(stdout_drain), collect_drain(stderr_drain)));
        }
        if deadline.is_cancelled() || deadline.is_spent(started) {
            tracing::warn!(
                command = %rendered,
                "abandoned a git child pipe a descendant kept open"
            );
            return None;
        }
        std::thread::sleep(interval);
        interval = (interval * 2).min(POLL_CEILING);
    }
}

/// Collects the bytes one drain thread read.
fn collect_drain(handle: std::thread::JoinHandle<std::io::Result<Vec<u8>>>) -> Vec<u8> {
    match handle.join() {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => {
            tracing::warn!(reason = %err, "failed to drain a git child pipe");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!("a git child pipe drain thread panicked");
            Vec::new()
        }
    }
}

/// Feeds the child's standard input from a dedicated thread.
///
/// `check-ignore` streams its verdicts while it consumes the candidate list, so
/// writing the list on the calling thread would deadlock as soon as the echoed
/// verdicts filled the child's output pipe. A command that ignores its input
/// leaves a broken pipe behind, which stays a diagnostic: the child already
/// answered.
fn spawn_stdin_writer(
    pipe: Option<std::process::ChildStdin>,
    data: Vec<u8>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(mut pipe) = pipe else {
            return;
        };
        let _ = pipe.write_all(&data);
        let _ = pipe.flush();
    })
}

/// Winds the child and its process group down in two phases and reaps it.
///
/// The whole group first receives `SIGTERM`, which is what lets Git and the
/// hooks it runs flush their state and remove the lock files they created, and
/// the group then has `TERMINATION_GRACE` to disappear. Only a group that
/// outlives that window is killed outright.
///
/// The group is signalled while the direct child is still unreaped, so the
/// leader's pid cannot have been recycled and every descendant the child
/// spawned is addressed together with it. A platform without process groups
/// kills the direct child alone.
fn terminate(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        if wind_down_group(child, pid) {
            return;
        }
    }
    crate::toolchain::exec::kill_process_group(pid);
    let _ = child.kill();
    let _ = child.wait();
}

/// Asks the whole process group to wind down and waits out the grace period.
///
/// Returns `true` when the group is gone and the direct child is reaped, so no
/// forceful signal is needed, and `false` when a member outlived the grace
/// period. The group is probed only after the leader is reaped, because a
/// zombie leader keeps its group alive.
#[cfg(unix)]
fn wind_down_group(child: &mut Child, pid: u32) -> bool {
    if !signal_process_group(pid, SIGTERM) {
        // The group is already gone, so the leader is at most a zombie waiting
        // to be reaped and a signal would only address a recycled pid.
        let _ = child.try_wait();
        return true;
    }

    let started = Instant::now();
    let mut interval = POLL_FLOOR;
    let mut reaped = false;
    loop {
        if !reaped {
            match child.try_wait() {
                Ok(Some(_)) => reaped = true,
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        group = pid,
                        reason = %err,
                        "git child status probe failed while its group was winding down"
                    );
                    return false;
                }
            }
        }
        if reaped && !signal_process_group(pid, SIG_EXISTENCE) {
            return true;
        }
        let remaining = TERMINATION_GRACE.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(interval.min(remaining));
        interval = (interval * 2).min(POLL_CEILING);
    }
}

/// Signals every member of the process group led by `pid`.
///
/// Returns `true` when the signal reached at least one member, and `false`
/// when the group has no member left or the caller may not signal it.
#[cfg(unix)]
fn signal_process_group(pid: u32, signal: i32) -> bool {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let Ok(target) = i32::try_from(pid) else {
        return false;
    };
    // A negative pid addresses the whole process group, and the signalling
    // process itself must never become the target.
    if target <= 1 {
        return false;
    }
    unsafe { kill(-target, signal) == 0 }
}

/// Places the child in its own process group, so expiry or cancellation tears
/// down everything the child spawned.
#[cfg(unix)]
fn isolate_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Leaves the child where the platform cannot isolate it.
#[cfg(not(unix))]
fn isolate_process_group(_command: &mut Command) {}

/// Renders an invocation the way the diagnostics name it.
fn render_command(args: &[&str]) -> String {
    format!("git {}", args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fast_invocation_round_trips_its_standard_output() {
        let outcome = run_bounded(None, &["--version"], &GitDeadline::default(), None);

        assert_eq!(outcome.end, GitEnd::Exited);
        assert_eq!(outcome.code, Some(0));
        assert!(
            String::from_utf8_lossy(&outcome.stdout).contains("git version"),
            "stdout: {}",
            String::from_utf8_lossy(&outcome.stdout)
        );
    }

    #[test]
    fn a_child_that_cannot_spawn_reports_a_failed_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-directory");

        let outcome = run_bounded(
            Some(missing.as_path()),
            &["--version"],
            &GitDeadline::default(),
            None,
        );

        assert_eq!(outcome.end, GitEnd::Exited);
        assert_eq!(outcome.code, None);
        assert!(!outcome.stderr.is_empty(), "the reason must be reported");
    }

    #[test]
    fn the_deadline_constructors_carry_the_documented_budgets() {
        assert_eq!(
            GitDeadline::default().timeout_secs(),
            DEFAULT_GIT_TIMEOUT_SECS
        );
        let cancelled = GitDeadline::from_cancel(watch::channel(false).1);
        assert_eq!(cancelled.timeout_secs(), DEFAULT_GIT_TIMEOUT_SECS);
        assert_eq!(GitDeadline::with_timeout(7).timeout_secs(), 7);
    }
    /// Spawns a shell fixture that leads its own process group.
    ///
    /// The fixture leads a group of its own, so the two-phase teardown can
    /// signal it without reaching the test harness.
    #[cfg(unix)]
    fn spawn_group_fixture(dir: &Path, script: &str) -> Child {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command.current_dir(dir);
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        isolate_process_group(&mut command);
        command.spawn().expect("the fixture shell must start")
    }

    /// Waits until the fixture published a file, or reports that it never did.
    #[cfg(unix)]
    fn wait_for_file(path: &Path) -> bool {
        for _ in 0..200 {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Spawns a shell that holds a lock file the way Git holds
    /// `.git/index.lock`, and returns the child together with the lock path.
    ///
    /// The shell installs the termination handler Git installs, so a polite
    /// signal lets it remove the file while a forceful one leaves it behind.
    #[cfg(unix)]
    fn spawn_lock_holder(dir: &Path) -> (Child, std::path::PathBuf) {
        let lock = dir.join("index.lock");
        let child = spawn_group_fixture(
            dir,
            "trap 'rm -f index.lock; exit 0' TERM; : > index.lock; sleep 5 & wait",
        );
        assert!(wait_for_file(&lock), "the fixture never published its lock");
        (child, lock)
    }

    /// A child that receives the polite signal removes the lock it holds and
    /// exits inside the grace period, so no forceful kill is needed.
    #[cfg(unix)]
    #[test]
    fn a_polite_termination_lets_the_child_remove_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, lock) = spawn_lock_holder(dir.path());

        terminate(&mut child);

        assert!(
            !lock.exists(),
            "the child was killed before its termination handler could remove the lock"
        );
    }

    /// A child that ignores the polite signal is killed only after the grace
    /// period, and it is reaped so no zombie survives the teardown.
    #[cfg(unix)]
    #[test]
    fn a_child_that_ignores_the_polite_signal_is_killed_after_the_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        // A signal the shell ignores is inherited across `exec`, so neither the
        // shell nor the `sleep` it runs reacts to the polite request.
        let mut child = spawn_group_fixture(
            dir.path(),
            "trap '' TERM; : > ready; while :; do sleep 0.05; done",
        );
        assert!(
            wait_for_file(&dir.path().join("ready")),
            "the fixture never installed its handler"
        );

        let started = Instant::now();
        terminate(&mut child);

        assert!(
            started.elapsed() >= TERMINATION_GRACE,
            "the child was killed before its grace period expired: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(child.try_wait(), Ok(Some(_))),
            "the forceful termination left the child unreaped"
        );
    }

    /// An expired budget winds the group down politely, so the child removes
    /// the lock that would otherwise block every later invocation.
    #[cfg(unix)]
    #[test]
    fn an_expired_budget_lets_the_child_remove_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, lock) = spawn_lock_holder(dir.path());

        let outcome = wait_under_budget(
            &mut child,
            &GitDeadline::with_timeout(0),
            Instant::now(),
            "git status",
        );

        assert!(matches!(outcome, Wait::TimedOut), "expected a timeout");
        assert!(!lock.exists(), "the expired budget left the lock behind");
    }

    /// A cancelled turn winds the group down politely as well, so the lock is
    /// released instead of being orphaned by an outright kill.
    #[cfg(unix)]
    #[test]
    fn a_cancelled_invocation_lets_the_child_remove_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, lock) = spawn_lock_holder(dir.path());
        let (_cancel_tx, cancel_rx) = watch::channel(true);

        let outcome = wait_under_budget(
            &mut child,
            &GitDeadline::with_timeout(600).with_cancel(Some(cancel_rx)),
            Instant::now(),
            "git status",
        );

        assert!(
            matches!(outcome, Wait::Cancelled),
            "expected a cancellation"
        );
        assert!(
            !lock.exists(),
            "the cancelled invocation left the lock behind"
        );
    }
}
