//! Git-backed transactional substrate.
//!
//! Every operation here runs the `git` executable — `add`, `rm --cached`,
//! `check-ignore`, `commit`, `reset`, `log`, `diff` — so the crate carries no
//! libgit2 binding and never writes the object database itself. Git stays the
//! single source of truth for repository state, and the deterministic
//! transaction guarantees (pre-edit checkpoint, intermediate step commits,
//! squash, crash recovery) are expressed entirely through those commands.
//!
//! The one harness-specific input of the substrate is the hook policy: whether a
//! Tauqe commit bypasses the repository's own `pre-commit` and `commit-msg`
//! hooks. The declaration lives in `tauqe.toml`, which this crate must not read,
//! so [`GitHooks`] carries the resolved decision into every committing entry
//! point while the cognitive crate owns the declaration.
//!
//! Every invocation runs under a wall-clock budget. A Git child is never only
//! Git: a commit that runs the repository hooks executes arbitrary user code,
//! and a signing configuration waits for an agent that may be blocked on a
//! passphrase prompt. [`exec`] therefore gives every child its own process
//! group, bounds the wait and the output drain with one budget, races the
//! caller's cancellation signal against the natural exit, and tears the whole
//! group down on expiry. [`cmd::run_git`] carries
//! [`exec::DEFAULT_GIT_TIMEOUT_SECS`] with no call-site change, while
//! [`cmd::run_git_with_deadline`] carries an explicit [`GitDeadline`].

pub mod checkpoint;
pub mod cmd;
pub mod diff;
pub mod exec;
pub mod state;

pub use exec::GitDeadline;

/// Whether a Tauqe commit runs the repository's own Git hooks.
///
/// An autonomous turn commits the state it has just verified, so a user hook
/// that reformats, lints, or rejects the commit would stall a turn the harness
/// already proved correct. Bypassing is therefore the harness default, and a
/// project opts into running its hooks through `[git].bypass_hooks = false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHooks {
    /// Pass `--no-verify`, skipping `pre-commit` and `commit-msg`.
    Bypass,
    /// Let the repository hooks run on every Tauqe commit.
    Run,
}

impl GitHooks {
    /// Reports whether a commit must bypass the repository hooks.
    pub fn bypasses(self) -> bool {
        matches!(self, Self::Bypass)
    }
}
