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

use std::path::Path;
use std::time::Duration;

use crate::stop::HardCancellation;

use super::cmd::git_command;

/// Wall-clock budget of a Git invocation that names none of its own.
pub const DEFAULT_GIT_TIMEOUT_SECS: u64 = 300;

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

/// The wall-clock budget and the cancellation signal a Git child runs under.
pub struct GitDeadline {
    timeout_secs: u64,
    cancel: Option<HardCancellation>,
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
    /// Attaches the caller's cancellation signal to the deadline.
    ///
    /// The budget is resolved from the project configuration and the signal
    /// from the turn that runs the command, so the two arrive separately and
    /// combine here instead of at every call site.
    pub fn with_cancel(mut self, cancel: Option<HardCancellation>) -> Self {
        self.cancel = cancel;
        self
    }

    /// The wall-clock budget in seconds.
    pub(crate) fn timeout_secs(&self) -> u64 {
        self.timeout_secs
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
    /// Shutdown of the process group or its pipes was not confirmed.
    Unconfirmed,
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
            GitEnd::Unconfirmed => Some(format!(
                "git command `{command}` could not confirm shutdown: {}",
                String::from_utf8_lossy(&self.stderr)
            )),
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
    let mut command = git_command(dir);
    command.args(args);
    let output = crate::process::run(
        command,
        Duration::from_secs(deadline.timeout_secs()),
        deadline.cancel.as_ref(),
        stdin_payload,
    );
    let mut stderr = output.stderr;
    if let Some(error) = output.error.as_ref() {
        if !stderr.is_empty() && !stderr.ends_with(b"\n") {
            stderr.push(b'\n');
        }
        stderr.extend_from_slice(error.as_bytes());
    }
    GitOutput {
        end: if output.error.is_some() {
            GitEnd::Unconfirmed
        } else if output.cancelled {
            GitEnd::Cancelled
        } else if output.timed_out {
            GitEnd::TimedOut
        } else {
            GitEnd::Exited
        },
        code: output.code,
        stdout: output.stdout,
        stderr,
    }
}

/// Renders an invocation the way the diagnostics name it.
fn render_command(args: &[&str]) -> String {
    format!("git {}", args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stop::StopSource;

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

        assert_eq!(outcome.end, GitEnd::Unconfirmed);
        assert_eq!(outcome.code, None);
        assert!(!outcome.stderr.is_empty(), "the reason must be reported");
    }

    #[test]
    fn the_deadline_constructors_carry_the_documented_budgets() {
        assert_eq!(
            GitDeadline::default().timeout_secs(),
            DEFAULT_GIT_TIMEOUT_SECS
        );
        let cancelled =
            GitDeadline::default().with_cancel(Some(StopSource::new().token().hard_cancellation()));
        assert_eq!(cancelled.timeout_secs(), DEFAULT_GIT_TIMEOUT_SECS);
        assert_eq!(GitDeadline::with_timeout(7).timeout_secs(), 7);
    }
}
