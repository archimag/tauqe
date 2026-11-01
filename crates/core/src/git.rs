//! Git transactions, resolved against the harness configuration.
//!
//! The subprocess plumbing — checkpoints, step commits, squash, crash recovery,
//! diff and repository-state queries — is deterministic substrate and lives in
//! `tauqe_tools::git`. The single harness-specific input of that substrate is
//! the hook policy: whether a Tauqe commit bypasses the repository's own
//! `pre-commit` and `commit-msg` hooks. The declaration is `[git].bypass_hooks`
//! inside `tauqe.toml`, which the substrate must not read, so this module
//! resolves it into a [`GitHooks`] decision and hands that decision to every
//! committing entry point.
//!
//! Everything else is a re-export: the module keeps the paths the cognitive
//! layers already address while the substrate crate owns the implementation.

pub use tauqe_tools::git::GitHooks;

use std::path::Path;

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

pub mod checkpoint {
    //! Checkpoints, step commits, squash, and crash recovery.
    //!
    //! The committing entry points resolve the hook policy of the target
    //! repository and delegate the transaction itself to the substrate.

    use std::path::Path;

    pub use tauqe_tools::git::checkpoint::{
        cleanup_orphaned_staging_files, ensure_tauqe_ignored, is_tauqe_commit,
        recover_interrupted_workflow, restore_checkpoint, unwind_checkpoint_if_present,
        CheckpointInfo, CHECKPOINT_PREFIX, STEP_PREFIX, TAUQE_CO_AUTHOR_TRAILER,
    };

    /// Creates a safe checkpoint of uncommitted changes before any AI modification.
    pub fn create_checkpoint(dir: &Path) -> anyhow::Result<CheckpointInfo> {
        tauqe_tools::git::checkpoint::create_checkpoint(dir, super::hooks_for(dir))
    }

    /// Records an intermediate step commit for the part of a turn already applied.
    pub fn create_step_commit(
        dir: &Path,
        changed_files: &[String],
        step_name: &str,
    ) -> anyhow::Result<String> {
        tauqe_tools::git::checkpoint::create_step_commit(
            dir,
            changed_files,
            step_name,
            super::hooks_for(dir),
        )
    }

    /// Squashes the intermediate step commits into one AI commit.
    pub fn finalize_ai_commit(
        dir: &Path,
        checkpoint: &CheckpointInfo,
        changed_files: &[String],
        summary: &str,
    ) -> anyhow::Result<String> {
        tauqe_tools::git::checkpoint::finalize_ai_commit(
            dir,
            checkpoint,
            changed_files,
            summary,
            super::hooks_for(dir),
        )
    }

    /// Creates an isolated AI commit for a turn that opened no checkpoint.
    pub fn create_ai_commit(
        dir: &Path,
        changed_files: &[String],
        summary: &str,
    ) -> anyhow::Result<String> {
        tauqe_tools::git::checkpoint::create_ai_commit(
            dir,
            changed_files,
            summary,
            super::hooks_for(dir),
        )
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
        tauqe_tools::git::cmd::commit_with_developer_author(
            dir,
            message,
            allow_empty,
            super::hooks_for(dir),
        )
    }
}

pub mod diff {
    //! Commit listings, cumulative diffs, and squashing.

    use std::path::Path;

    pub use tauqe_tools::git::diff::{
        find_last_non_ai_commit, get_commits_ahead, get_cumulative_diff, get_cumulative_file_diffs,
        get_diff, get_diff_stat, CommitSummary,
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
    find_last_non_ai_commit, get_commits_ahead, get_cumulative_diff, get_cumulative_file_diffs,
    get_diff, get_diff_stat, squash_to_single_commit, CommitSummary,
};
pub use state::{
    detect_upstream_branch, get_repository_state, init_repository, is_git_repository,
    list_repository_files,
};

#[cfg(test)]
mod tests {
    use super::{hooks_for, GitHooks};

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
}
