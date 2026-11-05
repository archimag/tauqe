//! Git transactions, resolved against the harness configuration.
//!
//! The subprocess plumbing — checkpoints, step commits, squash, crash recovery,
//! diff and repository-state queries — is deterministic substrate and lives in
//! `tauqe_tools::git`. The harness-specific inputs of that substrate are the
//! declarations this project owns: the hook policy, whether a Tauqe commit
//! bypasses the repository's own `pre-commit` and `commit-msg` hooks, and the
//! wall-clock budget every Git command runs under. Both are declared in
//! `tauqe.toml`, which the substrate must not read, so this module resolves them
//! into a [`GitHooks`] decision and a [`GitDeadline`], and hands those decisions
//! to every committing entry point.
//!
//! The checkpoint capture and the step commit also observe the turn's cancellation
//! signal, so a repository hook or a signing agent that holds the commit is
//! released the moment the developer cancels. Finalization and rollback stay
//! cancel-blind: they settle an outcome the turn already decided and complete
//! under the wall-clock budget.
//!
//! Both transactional entry points additionally gate the repository index: Git
//! refuses every command that writes the index while another process holds
//! `.git/index.lock`, so the gate waits out a lock that clears on its own and
//! reports a lock that outlives the bounded backoff as a typed refusal instead
//! of failing the transaction halfway through.
//!
//! Everything else is a re-export: the module keeps the paths the cognitive
//! layers already address while the substrate crate owns the implementation.

pub use tauqe_tools::git::{GitDeadline, GitHooks};

use std::path::Path;
use std::time::{Duration, Instant};

use tauqe_tools::git::checkpoint::{index_availability, IndexAvailability};
use tokio::sync::watch;

/// Resolves the hook policy a commit inside `dir` follows.
///
/// The declaration is read from the project configuration of the directory the
/// commit targets, so a turn never inherits the settings of whatever process it
/// happens to run in. An absent or unreadable configuration keeps the harness
/// default, which bypasses the hooks.
fn hooks_for(dir: &Path) -> GitHooks {
    if crate::config::load_git_config(dir).bypass_hooks {
        GitHooks::Bypass
    } else {
        GitHooks::Run
    }
}

/// Resolves the wall-clock budget of every Git command a turn runs against
/// `dir`, together with the cancellation signal of that turn.
///
/// The declaration is read from the project configuration of the directory the
/// command targets, exactly as the hook policy is, so a turn never inherits the
/// budget of whatever process it happens to run in. An absent or unreadable
/// configuration keeps the harness default.
fn git_deadline(dir: &Path, cancel: Option<watch::Receiver<bool>>) -> GitDeadline {
    let timeout_secs = crate::config::load_git_config(dir).timeout_secs;
    GitDeadline::with_timeout(timeout_secs).with_cancel(cancel)
}
/// Refuses a transaction step that begins after the turn was cancelled.
///
/// The capture and the step commit both open with repository probes, and the
/// substrate answers a probe it could not run the way that probe answers for a
/// repository without history: a failed `rev-parse HEAD` reads as an unborn base
/// and a failed status reads as a clean tree. A probe torn down by the turn's own
/// cancellation therefore describes a repository that does not exist, so the
/// refusal runs before the first probe and no recorded base of a cancelled capture
/// can ever reach a rollback.
///
/// A cancellation that arrives once the step is under way needs no refusal here:
/// the substrate fails the child it interrupts, the command reports the
/// cancellation, and the marker the capture already wrote is resolved by crash
/// recovery, which distinguishes a capture that committed from one that did not.
fn refuse_cancelled(cancel: Option<&watch::Receiver<bool>>, step: &str) -> anyhow::Result<()> {
    if cancel.is_some_and(|cancel| *cancel.borrow()) {
        anyhow::bail!("the {step} was not started: the turn was cancelled");
    }
    Ok(())
}

/// Number of index observations one transactional entry makes before it reports
/// the repository index as unavailable.
const INDEX_GATE_OBSERVATIONS: usize = 5;

/// First pause between two observations of a locked index.
///
/// The pause doubles after every observation, so the whole wait stays within a
/// fraction of a second: a lock another process holds for a moment clears on
/// its own, while a lock a crashed writer left behind is reclaimed by the
/// substrate instead of being waited out.
const INDEX_GATE_INITIAL_PAUSE: Duration = Duration::from_millis(25);

/// Refusal reported when the repository index stays locked by another process.
///
/// The refusal is typed rather than textual, so a caller distinguishes a busy
/// index from a failed transaction without inspecting an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IndexGateError {
    /// A fresh lock outlived every observation of the gate.
    #[error("the Git index stayed locked by another process for {elapsed_ms} ms")]
    Locked {
        /// Wall-clock time the gate spent waiting for the lock, in milliseconds.
        elapsed_ms: u128,
    },
    /// The turn was cancelled before the index became available.
    #[error("the Git index was not inspected: the turn was cancelled")]
    Cancelled,
}

/// Awaits the availability of the repository index before a transactional
/// operation that has to lock it.
///
/// A checkpoint commit, a step commit, and a project verification all open with
/// Git commands that write the index, and Git refuses every one of them while
/// another process holds `.git/index.lock`. The gate therefore observes the
/// index before the first such command: a lock no invocation may still own is
/// reclaimed by the substrate, a lock that clears on its own is waited out with
/// a bounded exponential pause, and a lock that outlives the whole gate is
/// reported as [`IndexGateError::Locked`] instead of failing the transaction
/// halfway through.
///
/// The lock location and the staleness rule stay in the substrate, which is the
/// only layer allowed to address `.git`; this gate owns the retry policy and the
/// typed refusal.
pub fn ensure_index_available(
    dir: &Path,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<(), IndexGateError> {
    let deadline = git_deadline(dir, cancel.cloned());
    let started = Instant::now();
    let mut pause = INDEX_GATE_INITIAL_PAUSE;

    for observation in 1..=INDEX_GATE_OBSERVATIONS {
        // A cancelled turn stops waiting at once instead of holding the caller
        // for the rest of the backoff.
        if cancel.is_some_and(|cancel| *cancel.borrow()) {
            return Err(IndexGateError::Cancelled);
        }
        match index_availability(dir, &deadline) {
            IndexAvailability::Free | IndexAvailability::Reclaimed => return Ok(()),
            IndexAvailability::Held(age) => {
                tracing::debug!(
                    observation,
                    held_for = ?age,
                    "the Git index is locked by another process; waiting for it"
                );
            }
        }
        if observation < INDEX_GATE_OBSERVATIONS {
            std::thread::sleep(pause);
            pause = pause.saturating_mul(2);
        }
    }

    Err(IndexGateError::Locked {
        elapsed_ms: started.elapsed().as_millis(),
    })
}

pub mod checkpoint {
    //! Checkpoints, step commits, squash, and crash recovery.
    //!
    //! The committing entry points resolve the hook policy and the wall-clock
    //! budget of the target repository, and delegate the transaction itself to
    //! the substrate. The capture and the step commit additionally observe the
    //! turn's cancellation signal, which they hand to every Git child they run.
    use std::path::Path;

    use tokio::sync::watch;

    pub use tauqe_tools::git::checkpoint::{
        cleanup_orphaned_staging_files, ensure_tauqe_ignored, is_tauqe_commit,
        recover_interrupted_workflow, unwind_checkpoint_if_present, CheckpointInfo,
        CHECKPOINT_PREFIX, STEP_PREFIX, TAUQE_CO_AUTHOR_TRAILER,
    };

    /// Creates a safe checkpoint of uncommitted changes before any AI modification.
    ///
    /// `cancel` is the turn's cancellation signal: a capture that begins after the
    /// turn was cancelled is refused, and the signal bounds every Git child the
    /// capture runs, so a repository hook holding the commit is released as soon as
    /// the developer cancels. `None` keeps the capture cancel-blind, which is what a
    /// caller outside a bounded turn expects. A locked repository index is
    /// waited out by the index gate before the capture starts.
    pub fn create_checkpoint(
        dir: &Path,
        cancel: Option<&watch::Receiver<bool>>,
    ) -> anyhow::Result<CheckpointInfo> {
        super::refuse_cancelled(cancel, "checkpoint capture")?;
        // The capture commits, and Git refuses a commit while another process
        // holds the index, so the gate runs before the first repository probe.
        super::ensure_index_available(dir, cancel)?;
        let deadline = super::git_deadline(dir, cancel.cloned());
        tauqe_tools::git::checkpoint::create_checkpoint_with_deadline(
            dir,
            super::hooks_for(dir),
            &deadline,
        )
    }

    /// Records an intermediate step commit for the part of a turn already applied.
    ///
    /// `cancel` follows the same contract as the capture: a step commit that begins
    /// after the turn was cancelled is refused, and the signal bounds every Git
    /// child the commit runs. A locked repository index is waited out by the
    /// index gate before the commit starts.
    pub fn create_step_commit(
        dir: &Path,
        changed_files: &[String],
        step_name: &str,
        cancel: Option<&watch::Receiver<bool>>,
    ) -> anyhow::Result<String> {
        super::refuse_cancelled(cancel, "step commit")?;
        // The commit writes the index, so a lock another process holds would
        // fail it halfway through; the gate settles that before it starts.
        super::ensure_index_available(dir, cancel)?;
        let deadline = super::git_deadline(dir, cancel.cloned());
        tauqe_tools::git::checkpoint::create_step_commit_with_deadline(
            dir,
            changed_files,
            step_name,
            super::hooks_for(dir),
            &deadline,
        )
    }

    /// Squashes the intermediate step commits into one AI commit.
    pub fn finalize_ai_commit(
        dir: &Path,
        checkpoint: &CheckpointInfo,
        changed_files: &[String],
        commit_message: &str,
    ) -> anyhow::Result<String> {
        let deadline = super::git_deadline(dir, None);
        tauqe_tools::git::checkpoint::finalize_ai_commit_with_deadline(
            dir,
            checkpoint,
            changed_files,
            commit_message,
            super::hooks_for(dir),
            &deadline,
        )
    }

    /// Creates an isolated AI commit for a turn that opened no checkpoint.
    pub fn create_ai_commit(
        dir: &Path,
        changed_files: &[String],
        commit_message: &str,
    ) -> anyhow::Result<String> {
        let deadline = super::git_deadline(dir, None);
        tauqe_tools::git::checkpoint::create_ai_commit_with_deadline(
            dir,
            changed_files,
            commit_message,
            super::hooks_for(dir),
            &deadline,
        )
    }

    /// Restores the working tree to its pre-turn state.
    pub fn restore_checkpoint(dir: &Path, checkpoint: &CheckpointInfo) -> anyhow::Result<()> {
        let deadline = super::git_deadline(dir, None);
        tauqe_tools::git::checkpoint::restore_checkpoint_with_deadline(dir, checkpoint, &deadline)
    }
}

pub mod cmd {
    //! Subprocess plumbing: staging paths and committing under the developer identity.

    use std::path::Path;

    pub use tauqe_tools::git::cmd::{
        run_git, run_git_check_ignore, stage_files, DEFAULT_AUTHOR_EMAIL, DEFAULT_AUTHOR_NAME,
    };

    /// Commits staged changes under the repository's developer identity.
    pub fn commit_with_developer_author(
        dir: &Path,
        message: &str,
        allow_empty: bool,
    ) -> anyhow::Result<()> {
        let deadline = super::git_deadline(dir, None);
        tauqe_tools::git::cmd::commit_with_developer_author_with_deadline(
            dir,
            message,
            allow_empty,
            super::hooks_for(dir),
            &deadline,
        )
    }
}

pub mod diff {
    //! Commit listings, cumulative diffs, and squashing.

    use std::path::Path;

    pub use tauqe_tools::git::diff::{
        find_last_non_ai_commit, get_commit_diff, get_commits_ahead, get_cumulative_diff,
        get_cumulative_file_diffs, get_diff, get_diff_stat, CommitSummary,
    };

    /// Squashes every commit ahead of `base_ref` into one developer-authored commit.
    pub fn squash_to_single_commit(
        dir: &Path,
        base_ref: &str,
        message: &str,
    ) -> anyhow::Result<String> {
        tauqe_tools::git::diff::squash_to_single_commit(
            dir,
            base_ref,
            message,
            super::hooks_for(dir),
        )
    }
}

pub mod state {
    //! Repository state, tracked-file listings, and working-copy snapshots.

    use std::path::Path;

    use tauqe_protocol::RepositoryState;

    pub use tauqe_tools::git::state::{
        changed_worktree_paths, detect_upstream_branch, get_repository_state, gitignored_paths,
        is_git_repository, list_repository_files, parse_worktree_status, snapshot_worktree,
        WorktreeSnapshot,
    };

    /// Initializes a repository and records the initial commit when asked.
    pub fn init_repository(
        dir: Option<&Path>,
        initial_commit: bool,
    ) -> anyhow::Result<RepositoryState> {
        // Without a directory the command runs in the process working directory,
        // which is therefore where the hook policy is declared.
        let policy_root = dir.unwrap_or_else(|| Path::new("."));
        tauqe_tools::git::state::init_repository(dir, initial_commit, super::hooks_for(policy_root))
    }
}

pub use checkpoint::{
    create_ai_commit, create_checkpoint, create_step_commit, finalize_ai_commit, is_tauqe_commit,
    restore_checkpoint, CheckpointInfo, CHECKPOINT_PREFIX, STEP_PREFIX, TAUQE_CO_AUTHOR_TRAILER,
};
pub use cmd::run_git;
pub use diff::{
    find_last_non_ai_commit, get_commit_diff, get_commits_ahead, get_cumulative_diff,
    get_cumulative_file_diffs, get_diff, get_diff_stat, squash_to_single_commit, CommitSummary,
};
pub use state::{
    detect_upstream_branch, get_repository_state, init_repository, is_git_repository,
    list_repository_files,
};

#[cfg(test)]
mod tests {
    use super::{
        create_checkpoint, create_step_commit, ensure_index_available, hooks_for, run_git,
        GitHooks, IndexGateError,
    };

    /// The hook policy is the one harness-specific input of the Git substrate, so
    /// its resolution is pinned here: the substrate crate receives the decision
    /// and never reads the project configuration itself.
    #[test]
    fn the_hook_policy_follows_the_project_declaration() {
        let dir = tempfile::tempdir().unwrap();

        // An absent declaration keeps the harness default: an autonomous commit
        // must not stall on a user hook.
        assert_eq!(hooks_for(dir.path()), GitHooks::Bypass);

        // A project that opts in runs its hooks on every Tauqe commit.
        std::fs::write(
            dir.path().join("tauqe.toml"),
            "[git]\nbypass_hooks = false\n",
        )
        .unwrap();
        assert_eq!(hooks_for(dir.path()), GitHooks::Run);

        // An explicit opt-in reproduces the default.
        std::fs::write(
            dir.path().join("tauqe.toml"),
            "[git]\nbypass_hooks = true\n",
        )
        .unwrap();
        assert_eq!(hooks_for(dir.path()), GitHooks::Bypass);
    }

    /// The budget of a Git command is resolved from the same declaration the
    /// hook policy comes from, so a project that narrows it sees the commit the
    /// checkpoint runs torn down at that budget instead of stalling the turn.
    #[cfg(unix)]
    #[test]
    fn the_resolved_budget_bounds_the_checkpoint_commit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::init_repository(Some(root), true).unwrap();

        // A dirty tree forces the capture through a commit, and the run policy
        // lets the repository hook execute inside that commit.
        std::fs::write(root.join("user_work.txt"), "uncommitted work").unwrap();
        std::fs::write(
            root.join("tauqe.toml"),
            "[git]\nbypass_hooks = false\ntimeout_secs = 1\n",
        )
        .unwrap();
        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).unwrap();

        let started = std::time::Instant::now();
        let error = create_checkpoint(root, None).expect_err("the hook outlives the budget");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("exceeded its wall-clock budget"),
            "error: {rendered}"
        );
        assert!(rendered.contains("git commit"), "error: {rendered}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "the budget did not release the caller: {:?}",
            started.elapsed()
        );
    }

    /// Resolves the path Git addresses the index lock of `dir` through.
    ///
    /// The lock does not always live at `<repository>/.git/index.lock`: a linked
    /// worktree keeps its own index, so the path is asked from Git instead of
    /// being assumed.
    fn index_lock_path(dir: &std::path::Path) -> std::path::PathBuf {
        let resolved = run_git(Some(dir), &["rev-parse", "--git-path", "index.lock"])
            .expect("the repository resolves its index lock path");
        dir.join(resolved.trim())
    }

    /// A fresh lock may belong to a process that still runs, so the gate waits it
    /// out with its bounded backoff and then reports the typed refusal instead of
    /// letting a transaction fail halfway through.
    #[test]
    fn the_index_gate_refuses_a_lock_that_never_clears() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::init_repository(Some(root), true).unwrap();
        let lock = index_lock_path(root);
        std::fs::write(&lock, "").unwrap();

        let error = ensure_index_available(root, None)
            .expect_err("a lock that never clears must be refused");

        assert!(matches!(error, IndexGateError::Locked { .. }), "{error:?}");
        assert!(
            lock.exists(),
            "the gate never removes a lock it may not own"
        );
    }

    /// A lock that outlived every invocation that could own it is reclaimed by
    /// the substrate, so the gate reports the index as available instead of
    /// waiting for a writer that no longer exists.
    #[test]
    fn the_index_gate_reclaims_a_lock_no_invocation_may_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::init_repository(Some(root), true).unwrap();
        // A zero budget dates the staleness rule immediately: any lock older
        // than the termination grace period can no longer belong to a running
        // invocation.
        std::fs::write(root.join("tauqe.toml"), "[git]\ntimeout_secs = 0\n").unwrap();
        let lock = index_lock_path(root);
        std::fs::write(&lock, "").unwrap();
        std::thread::sleep(
            tauqe_tools::git::exec::TERMINATION_GRACE + std::time::Duration::from_millis(50),
        );

        assert!(
            ensure_index_available(root, None).is_ok(),
            "a stale lock must be reclaimed by the gate"
        );
        assert!(
            !lock.exists(),
            "the reclaimed lock must leave the repository"
        );
    }

    /// A cancelled turn never waits for a locked index.
    #[test]
    fn the_index_gate_reports_a_cancelled_turn_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::init_repository(Some(root), true).unwrap();
        std::fs::write(index_lock_path(root), "").unwrap();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(true);

        let error = ensure_index_available(root, Some(&cancel_rx))
            .expect_err("a cancelled turn never waits for the lock");

        assert_eq!(error, IndexGateError::Cancelled);
    }

    /// The capture refuses to open a transaction while another process holds the
    /// index, so no marker records a capture that could not have committed.
    #[test]
    fn a_checkpoint_is_refused_while_the_index_stays_locked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::init_repository(Some(root), true).unwrap();
        std::fs::write(root.join("notes.txt"), "uncommitted work").unwrap();
        std::fs::write(index_lock_path(root), "").unwrap();

        let error = create_checkpoint(root, None)
            .expect_err("a held lock must refuse the checkpoint capture");
        let rendered = format!("{error:#}");

        assert!(rendered.contains("locked"), "error: {rendered}");
        assert!(
            !root.join(".tauqe/active-transaction.json").exists(),
            "the refusal must come before the capture writes its marker"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt")).unwrap(),
            "uncommitted work"
        );
    }

    /// A step commit needs the very index the gate protects.
    #[test]
    fn a_step_commit_is_refused_while_the_index_stays_locked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::init_repository(Some(root), true).unwrap();
        std::fs::write(root.join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(index_lock_path(root), "").unwrap();

        let error = create_step_commit(root, &["a.rs".to_string()], "apply step", None)
            .expect_err("a held lock must refuse the step commit");

        assert!(format!("{error:#}").contains("locked"), "{error:#}");
    }
}
