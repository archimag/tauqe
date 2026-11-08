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

use super::cmd::{git_command, spawn_pipe_drain, PipeDrain};

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
///
/// The window is sized for a loaded host, not for an idle one. Signal
/// delivery, the scheduling of the process that installed the handler, and
/// the cleanup that handler runs are all wall-clock costs the child pays
/// before its locks are gone, and a window that only covers an idle machine
/// leaves the lock behind exactly when the host is busy. A turn that is
/// already aborting pays the window once, while a surviving lock fails every
/// later invocation of the same repository.
pub const TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// Window in which an exited Git child's output pipes may still reach EOF.
///
/// The direct child can exit while a descendant it spawned keeps the inherited
/// pipes open, so the drain is bounded on its own instead of inheriting whatever
/// remains of the invocation budget. The window starts when the drain starts,
/// which is why a child that used almost all of its budget still delivers its
/// complete output, and it is short enough that a lingering descendant can never
/// hold the turn.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Diagnostic appended to the standard error of an invocation whose output pipes
/// a descendant kept open.
const ABANDONED_DRAIN_NOTE: &[u8] = b"a descendant process held the output pipes open";

/// The polite termination signal: it lets the child clean up after itself.
#[cfg(unix)]
const SIGTERM: i32 = 15;

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
    /// The bytes the child wrote to its standard error, the reason it produced
    /// no answer at all, and — when a descendant kept the pipes open past the
    /// drain window — the diagnostic of the abandoned drain.
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
///
/// How the child ended comes from the wait alone. A descendant that inherits the
/// output pipes and outlives the child delays the drain, never the verdict: the
/// exit code and the bytes the child already produced survive, and the abandoned
/// drain is appended to the standard error as a diagnostic.
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

    // The drain annotates the invocation; it never reclassifies it. How the
    // child ended was established by the wait, and a descendant that keeps the
    // inherited pipes open can neither turn a clean exit into a kill nor
    // discard the output the child already produced.
    let drained = collect_output(stdout_drain, stderr_drain, deadline, &display);

    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }

    let mut stderr = match probe_reason {
        Some(reason) => reason.into_bytes(),
        None => drained.stderr,
    };
    if drained.abandoned {
        if !stderr.is_empty() && !stderr.ends_with(b"\n") {
            stderr.push(b'\n');
        }
        stderr.extend_from_slice(ABANDONED_DRAIN_NOTE);
    }

    GitOutput {
        end,
        code,
        stdout: drained.stdout,
        stderr,
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

/// What an exited Git child's output pipes delivered.
struct DrainedOutput {
    /// The bytes the child wrote to its standard output.
    stdout: Vec<u8>,
    /// The bytes the child wrote to its standard error.
    stderr: Vec<u8>,
    /// Whether a descendant kept a pipe open past the drain window.
    abandoned: bool,
}

/// Waits for both output pipes and collects what they delivered.
///
/// The direct child can exit while a descendant it spawned keeps the inherited
/// pipes open. The wait is bounded by the caller's cancellation signal and by
/// [`DRAIN_GRACE`] measured from this call, so a cancelled turn is released at
/// once and an exited child's output is never held hostage by a descendant. The
/// bytes the readers already delivered come back in every case, so an abandoned
/// drain annotates the invocation instead of replacing its verdict or its
/// output.
fn collect_output(
    stdout_drain: PipeDrain,
    stderr_drain: PipeDrain,
    deadline: &GitDeadline,
    rendered: &str,
) -> DrainedOutput {
    let started = Instant::now();
    let mut interval = POLL_FLOOR;
    let abandoned = loop {
        if stdout_drain.is_finished() && stderr_drain.is_finished() {
            break false;
        }
        if deadline.is_cancelled() || started.elapsed() >= DRAIN_GRACE {
            tracing::warn!(
                command = %rendered,
                "abandoned a git child pipe a lingering descendant kept open"
            );
            break true;
        }
        std::thread::sleep(interval);
        interval = (interval * 2).min(POLL_CEILING);
    };

    DrainedOutput {
        stdout: stdout_drain.collect(),
        stderr: stderr_drain.collect(),
        abandoned,
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
/// Every group signal is sent while the direct child is still unreaped. An
/// unreaped child, live or zombie, keeps its pid reserved, so the kernel cannot
/// recycle that pid into a new process group that a negative-pid signal would
/// then destroy. A reap taken earlier to learn whether the group emptied would
/// release exactly that anchor. A platform without process groups kills the
/// direct child alone.
fn terminate(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    wind_down_group(child, pid);
    #[cfg(not(unix))]
    {
        crate::toolchain::exec::kill_process_group(pid);
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Completes the Unix teardown of the child and its process group.
///
/// The group receives `SIGTERM` first, so Git and the hooks it runs can flush
/// their state and remove the lock files they created. A signal that reaches no
/// member ends the teardown at once: nothing is left to wind down, and no group
/// signal follows the reap of the leader.
///
/// Otherwise the teardown spends the whole `TERMINATION_GRACE` window without
/// reaping the leader, kills the group outright, and reaps the leader only
/// after that forceful signal. The leader stays unreaped throughout, so its pid
/// still reserves its process group and the negative-pid kill cannot reach an
/// unrelated group that may have inherited a recycled pid.
///
/// The fast exit for a group that empties inside the window is deliberately
/// given up: observing it required reaping the leader, because a zombie leader
/// keeps its group alive and masks any probe of the group. The forceful signal
/// is therefore decided blind, which is harmless to a group whose only member
/// is the zombie leader.
#[cfg(unix)]
fn wind_down_group(child: &mut Child, pid: u32) {
    if !signal_process_group(pid, SIGTERM) {
        // No member of the group could be signalled, so nothing is left to
        // wind down. Only the leader is left to be reaped.
        let _ = child.kill();
        let _ = child.wait();
        return;
    }

    std::thread::sleep(TERMINATION_GRACE);
    // The leader is still unreaped here, so its pid cannot have been recycled
    // into an unrelated process group that the negative-pid signal would then
    // destroy.
    crate::toolchain::exec::kill_process_group(pid);
    let _ = child.kill();
    let _ = child.wait();
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

/// Places the child in its own process group and lets it receive the polite
/// termination request.
///
/// A group only winds down gracefully when its members can act on `SIGTERM`.
/// Signal dispositions and the signal mask are inherited across `fork` and
/// `exec`, so a parent that ignores or blocks that signal — a supervisor, a
/// Git hook, or a test harness — silently silences every child it starts,
/// including the processes a repository hook runs. The teardown would then
/// fall through to the forceful kill without the cleanup window the grace
/// period exists for, and the lock files Git removes from its own termination
/// handler would survive. The child therefore starts with the default
/// `SIGTERM` disposition and an unblocked `SIGTERM`; a script stays free to
/// override either with its own `trap`.
///
/// The hook runs in the forked child before `exec`, so it confines itself to
/// async-signal-safe calls that touch nothing but the child's own state.
#[cfg(unix)]
fn isolate_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
    // SAFETY: `pre_exec` requires a closure that is safe to run between `fork`
    // and `exec`. `signal` and `sigprocmask` are async-signal-safe, they
    // allocate nothing, and they modify only the child's own signal state,
    // which is exactly what the graceful teardown depends on.
    unsafe {
        command.pre_exec(|| {
            let _ = libc::signal(libc::SIGTERM, libc::SIG_DFL);
            let mut unblocked = std::mem::zeroed::<libc::sigset_t>();
            let _ = libc::sigemptyset(&mut unblocked);
            let _ = libc::sigaddset(&mut unblocked, libc::SIGTERM);
            let _ = libc::sigprocmask(libc::SIG_UNBLOCK, &unblocked, std::ptr::null_mut());
            Ok(())
        });
    }
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
    ///
    /// The fixture idles in a loop of short foreground sleeps instead of
    /// waiting for a background job, so the shell regains control at least
    /// every 50 ms and runs its handler well inside the grace window. A shell
    /// that defers a trap until the job it waits for completes would otherwise
    /// spend the whole window inside `wait` and be killed before its handler
    /// could remove the lock.
    #[cfg(unix)]
    fn spawn_lock_holder(dir: &Path) -> (Child, std::path::PathBuf) {
        let lock = dir.join("index.lock");
        let child = spawn_group_fixture(
            dir,
            "trap 'rm -f index.lock; exit 0' TERM; : > index.lock; while :; do sleep 0.05; done",
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
    /// A descendant that outlives the polite window is killed by the forceful
    /// group signal, and the leader stays unreaped until after that signal.
    ///
    /// The leader exits on `SIGTERM` at once, so only its zombie keeps the
    /// process group reserved. The descendant ignores the polite signal and
    /// keeps writing a heartbeat, and must stop exactly when the forceful
    /// signal reaches the group after the grace window; the teardown then reaps
    /// the leader, so no zombie survives it.
    #[cfg(unix)]
    #[test]
    fn a_live_descendant_is_killed_after_the_grace_period_and_the_leader_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = dir.path().join("heartbeat.log");
        let mut child = spawn_group_fixture(
            dir.path(),
            "trap 'exit 0' TERM; sh -c 'trap \"\" TERM; while :; do echo tick >> heartbeat.log; sleep 0.05; done' & : > ready; while :; do sleep 0.05; done",
        );
        assert!(
            wait_for_file(&dir.path().join("ready")),
            "the fixture never started"
        );
        assert!(
            wait_for_file(&heartbeat),
            "the descendant never wrote its heartbeat"
        );

        let started = Instant::now();
        terminate(&mut child);
        let elapsed = started.elapsed();

        assert!(
            elapsed >= TERMINATION_GRACE,
            "the descendant was killed before its polite window expired: {elapsed:?}"
        );
        assert!(
            matches!(child.try_wait(), Ok(Some(_))),
            "the teardown left the leader unreaped"
        );

        // A write that was already in flight when the descendant died lands
        // before the baseline, so the comparison observes only writes the
        // teardown failed to stop.
        std::thread::sleep(Duration::from_millis(150));
        let size_at_kill = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);
        std::thread::sleep(TERMINATION_GRACE);
        let size_later = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert_eq!(
            size_at_kill, size_later,
            "the descendant kept writing after the forceful group signal"
        );
    }
}
