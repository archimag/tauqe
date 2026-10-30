//! Transactional finalization and rollback for the FSM bounded turn.
//!
//! The turn engine persists validated changes exclusively through an
//! [`ApplyPolicy`]. This module owns the transactional lifecycle: applying a
//! proposal into the working tree, finalizing it into a commit, rolling it
//! back on failure, and applying the turn's plan mutations once the turn
//! succeeds.

use std::path::Path;
use std::time::Duration;
use tauqe_protocol::{EditOutcome, EditProposal, ModelResult, ProjectConfig};
use tokio::sync::{mpsc, watch};

use crate::context::ContextManager;
use crate::git::state::{changed_worktree_paths, snapshot_worktree, WorktreeSnapshot};
use crate::providers::StreamEvent;
use crate::workflow::policy::ApplyPolicy;

use super::FsmTurnOutcome;

/// Number of rollback attempts before a transient failure is treated as fatal.
const ROLLBACK_ATTEMPTS: usize = 3;

/// Pause between two rollback attempts, long enough for a transient lock held by
/// an IDE, a language server, or the operating system to clear.
const ROLLBACK_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Outcome of a rollback attempt against the turn transaction.
///
/// The three states are deliberately distinct, because "the tree was restored"
/// and "the rollback did not fail" answer different questions: the first
/// decides whether a context change must be published, the second decides
/// whether the next attempt may start at all. A policy that holds no open
/// transaction — the direct-write policy of the naive workflow — reports
/// [`RollbackOutcome::NoTransaction`], which is not a failure: there is simply
/// nothing it could restore.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RollbackOutcome {
    /// The working tree was restored to its pre-turn state.
    Restored,
    /// The policy held no open transaction, so nothing needed restoring.
    NoTransaction,
    /// Every attempt failed; the working tree may still hold the turn's writes.
    Failed,
}

impl RollbackOutcome {
    /// Whether the working tree was actually restored.
    pub(crate) fn is_restored(self) -> bool {
        matches!(self, Self::Restored)
    }

    /// Whether the rollback failed, leaving the working tree in an unknown
    /// state.
    pub(crate) fn is_failed(self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// Rolls back an open transaction, retrying a transient failure and logging a
/// final one instead of discarding it.
///
/// A failed rollback leaves the working tree modified while the caller reports
/// that the turn was reverted, so the discrepancy must stay visible rather than
/// being silently dropped.
///
/// A rollback can fail for reasons that are not the repository's fault: an IDE,
/// a language server such as `rust-analyzer`, or the operating system may hold a
/// lock on a file for a few milliseconds while it re-reads or re-indexes it. The
/// helper therefore makes `ROLLBACK_ATTEMPTS` attempts with a pause between
/// them, so a transient lock clears on its own instead of turning into a dirty
/// working tree. Only an error that survives the last attempt is reported as a
/// failed rollback.
///
/// The helper is shared beyond the FSM: the execution-mode supervisors call it
/// on their turn-error and escalation paths, so no rollback failure is
/// discarded anywhere in the turn lifecycle.
pub(crate) async fn attempt_rollback(
    policy: &mut dyn ApplyPolicy,
    repo_root: &Path,
) -> RollbackOutcome {
    let mut attempt = 1;
    loop {
        match policy.rollback(repo_root).await {
            Ok(true) => return RollbackOutcome::Restored,
            Ok(false) => return RollbackOutcome::NoTransaction,
            Err(err) if attempt < ROLLBACK_ATTEMPTS => {
                tracing::warn!(attempt, error = %err, "rollback attempt failed; retrying");
                tokio::time::sleep(ROLLBACK_RETRY_DELAY).await;
                attempt += 1;
            }
            Err(err) => {
                tracing::error!(
                    attempts = ROLLBACK_ATTEMPTS,
                    error = %err,
                    "failed to roll back the turn transaction; the working tree may still be modified"
                );
                return RollbackOutcome::Failed;
            }
        }
    }
}

/// Rolls back an open transaction and reports whether the working tree was
/// actually restored.
///
/// A caller that only needs to know whether the rollback failed reads
/// [`attempt_rollback`] directly: a policy without an open transaction restores
/// nothing and therefore reports `false` here without having failed.
pub(crate) async fn rollback_or_log(policy: &mut dyn ApplyPolicy, repo_root: &Path) -> bool {
    attempt_rollback(policy, repo_root).await.is_restored()
}

/// RAII guard that rolls back an open transaction and cleans auto context if
/// dropped before being explicitly disarmed or finalized.
///
/// Ensures that early `?`-returns (e.g. LLM provider errors during verification
/// healing), task cancellations, or panics do not leave dirty working trees,
/// step commits, or transaction markers behind.
pub(super) struct TransactionRollbackGuard<'a> {
    policy: &'a mut dyn ApplyPolicy,
    pub(super) context_manager: &'a mut ContextManager,
    repo_root: &'a Path,
    /// Effective project configuration frozen at the start of the turn.
    project: &'a ProjectConfig,
    cancel_rx: watch::Receiver<bool>,
    armed: bool,
    /// Working-copy status captured once, at the end of the first change set
    /// that reached the disk.
    ///
    /// It is the reference point for the files the turn produced outside the
    /// proposal: a repair round, a declared post-hook, or the toolchain
    /// rewriting a file while it verified the tree.
    worktree_baseline: Option<WorktreeSnapshot>,
}

impl<'a> TransactionRollbackGuard<'a> {
    pub(super) fn new(
        policy: &'a mut dyn ApplyPolicy,
        context_manager: &'a mut ContextManager,
        repo_root: &'a Path,
        project: &'a ProjectConfig,
        cancel_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            policy,
            context_manager,
            repo_root,
            project,
            cancel_rx,
            armed: false,
            worktree_baseline: None,
        }
    }

    pub(super) fn arm(&mut self) {
        self.armed = true;
    }

    pub(super) async fn rollback(&mut self) -> bool {
        self.armed = false;
        let rolled_back = rollback_or_log(self.policy, self.repo_root).await;
        // The sweep only drops context paths that no longer exist on disk, so
        // it runs off the async executor thread. The auto layer is deliberately
        // kept: only a successful commit may shrink the context.
        crate::blocking::run_blocking(|| {
            self.context_manager.prune_missing_files();
        });
        rolled_back
    }

    pub(super) async fn finalize(
        &mut self,
        changed_files: &[String],
        summary: &str,
    ) -> (Option<String>, Option<String>) {
        self.armed = false;
        // Clearing auto context recomputes hierarchical pinned files through
        // disk checks, so the sweep runs off the async executor thread.
        crate::blocking::run_blocking(|| self.context_manager.clear_auto());
        finalize_applied(self.policy, self.repo_root, changed_files, summary).await
    }

    /// Writes the validated change set to disk and returns every file it touched.
    ///
    /// The write is followed immediately by the declared disk-side post-hook, so
    /// generated sources, refreshed manifests, and migrations are present before
    /// the verification command runs. The files the hook produced are folded into
    /// the returned change set, which makes them part of verification and of the
    /// final atomic commit instead of an untracked residue.
    pub(super) async fn apply_temporary(
        &mut self,
        proposal: &EditProposal,
    ) -> anyhow::Result<Vec<String>> {
        let mut changed_files = self
            .policy
            .apply_temporary(self.repo_root, self.project, self.context_manager, proposal)
            .await?;

        for artifact in self.run_post_hook().await? {
            if !changed_files.contains(&artifact) {
                changed_files.push(artifact);
            }
        }

        // The working copy is baselined exactly once, right after the first
        // change set reached the disk. Every path that differs from it later on
        // was produced by the turn itself, so capturing it once — rather than on
        // every healing round — keeps the differential from drifting forward and
        // silently dropping an artifact the toolchain already created.
        if self.worktree_baseline.is_none() {
            let repo_root = self.repo_root;
            self.worktree_baseline = Some(crate::blocking::run_blocking(|| {
                snapshot_worktree(repo_root)
            }));
        }

        Ok(changed_files)
    }

    /// Reports every working-copy path that changed since the transaction began.
    ///
    /// The differential is read from Git itself, so a file the toolchain rewrote
    /// is detected exactly like a repair-round edit, and `.gitignore` keeps
    /// generated artifacts out of both snapshots. The result is sorted and
    /// de-duplicated, which makes the resulting change set deterministic across
    /// healing rounds.
    pub(super) fn worktree_changes(&self) -> Vec<String> {
        let Some(baseline) = self.worktree_baseline.as_ref() else {
            return Vec::new();
        };
        let repo_root = self.repo_root;
        let after = crate::blocking::run_blocking(|| snapshot_worktree(repo_root));
        changed_worktree_paths(baseline, &after)
    }

    /// Runs the declared disk-side post-hook and reports the files it produced.
    ///
    /// The working copy is snapshotted immediately before the hook starts and
    /// again once it exited successfully, so the difference is exactly the set of
    /// files the project step created, rewrote, or removed, with `.gitignore`
    /// respected through Git itself. A hook that fails or is interrupted aborts
    /// the change set: committing a tree whose generator failed would record
    /// artifacts nobody can reproduce.
    async fn run_post_hook(&self) -> anyhow::Result<Vec<String>> {
        let repo_root = self.repo_root;
        let before = crate::blocking::run_blocking(|| snapshot_worktree(repo_root));

        let response = crate::toolchain::run_post_hooks_with_config(
            repo_root,
            self.project,
            Some(&self.cancel_rx),
        )
        .await
        .map_err(|err| anyhow::anyhow!("the project post-hook could not run: {err}"))?;
        if !response.success {
            let reason = response
                .message
                .as_deref()
                .unwrap_or("the project post-hook reported no reason");
            anyhow::bail!("the project post-hook failed: {reason}");
        }
        if response.executed_hooks.is_empty() {
            return Ok(Vec::new());
        }

        let after = crate::blocking::run_blocking(|| snapshot_worktree(repo_root));
        let artifacts = changed_worktree_paths(&before, &after);
        if !artifacts.is_empty() {
            tracing::info!(
                files = artifacts.len(),
                "project post-hook produced artifacts"
            );
        }
        Ok(artifacts)
    }
}

impl Drop for TransactionRollbackGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            // `Drop` must never perform I/O. It can run on a Tokio worker
            // thread (for example when the turn future is aborted), and a
            // blocking `git` subprocess, path canonicalization, or filesystem
            // walk would stall the executor. The checkpoint already wrote the
            // transaction marker to disk, so `recover_interrupted_workflow`
            // unwinds the dangling transaction deterministically on the next
            // daemon startup.
            tracing::error!(
                "TransactionRollbackGuard dropped while armed; leaving the open Git transaction to crash recovery"
            );
        }
    }
}

/// Finalizes an applied turn and surfaces a commit failure as a diagnostic.
///
/// The validated changes already sit in the working tree, so the turn stays
/// `applied`, but a Git transaction that could not be finalized must not be
/// hidden behind a silent `commit_hash: None`. The failure is logged and
/// returned so the caller can attach it to `ModelResult::Edit.error`.
async fn finalize_applied(
    policy: &mut dyn ApplyPolicy,
    repo_root: &Path,
    changed_files: &[String],
    summary: &str,
) -> (Option<String>, Option<String>) {
    match policy.apply_final(repo_root, changed_files, summary).await {
        Ok(commit_hash) => (commit_hash, None),
        Err(err) => {
            let message = format!(
                "Changes were applied but the Git transaction could not be finalized: {err}"
            );
            tracing::error!("{}", message);
            (None, Some(message))
        }
    }
}

/// Applies the turn's plan updates and packages the outcome.
///
/// History recording is deliberately excluded here: it runs once per bounded
/// turn in [`super::run_fsm_turn`], so repeated proposal rounds never duplicate
/// history entries.
///
/// Invariant: Plan mutations are applied strictly when the turn succeeds without
/// rollback or error (`applied == true` for edits, or `ModelResult::Answer`).
pub(super) async fn finish_turn(
    assistant_text: String,
    result: ModelResult,
    repo_root: &Path,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
    mode: crate::plan::storage::PlanMutationMode,
) -> FsmTurnOutcome {
    let should_apply_plans = match &result {
        ModelResult::Answer { .. } => true,
        ModelResult::Edit { outcome, .. } => outcome.applied,
    };
    // A turn "saved" a plan when it carried an explicit `save` action and the
    // plan mutations were actually applied. Surfacing the flag alongside the
    // turn result lets the server mark the emitted `plan/updated` event as a
    // creation instead of re-deriving it from storage.
    let plan_saved = should_apply_plans
        && plans
            .iter()
            .any(|plan| plan.action == tauqe_protocol::DiscussionPlanAction::Save);
    let plan_errors = if should_apply_plans {
        apply_turn_plans(repo_root, plans, mode).await
    } else {
        Vec::new()
    };
    FsmTurnOutcome {
        assistant_text,
        result,
        is_cancelled: false,
        blocked: None,
        abort: None,
        has_terminal_success: false,
        plan_saved,
        plan_errors,
        skip_reason: None,
    }
}

/// Applies the turn's plan mutations and returns every rejection the storage
/// engine reported.
///
/// A rejection is a diagnostic rather than a failed turn: the change set it
/// travelled with may be valid while the plan payload is not, so the reasons
/// travel back to the caller instead of being aborted on here. They reach the
/// session history and the developer, so a refused mutation is never a silent
/// journal trace the model can never learn from.
async fn apply_turn_plans(
    repo_root: &Path,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
    mode: crate::plan::storage::PlanMutationMode,
) -> Vec<String> {
    if plans.is_empty() {
        return Vec::new();
    }
    let root = repo_root.to_path_buf();
    let plans_vec = plans.to_vec();
    match crate::blocking::run_blocking_async(move || {
        let mut errors = Vec::new();
        for plan_update in &plans_vec {
            let outcome = crate::plan::storage::PlanStorage::apply_discussion_plan_update_with_mode(
                &root,
                plan_update,
                mode,
            );
            for err in &outcome.errors {
                tracing::warn!("Plan update rejected: {}", err);
            }
            errors.extend(outcome.errors);
            for plan in outcome.applied {
                let _ = crate::plan::storage::PlanStorage::save_active_id(&root, Some(&plan.id));
            }
        }
        errors
    })
    .await
    {
        Ok(errors) => errors,
        Err(err) => {
            // The offload itself failed, so every mutation of this turn was
            // left unapplied: the turn reports that as one reason instead of
            // letting the plans disappear silently.
            tracing::error!("Failed to apply turn plans: {}", err);
            vec![format!(
                "The turn's plan mutations could not be applied: {err}"
            )]
        }
    }
}

/// Finalizes a turn terminated by an explicit semantic `<blocked>` signal.
///
/// The step transaction is rolled back to its pre-step checkpoint and the
/// transient auto context is cleared. The coder's report is preserved on the
/// outcome so the caller can move the step to `Blocked`; no patch retry or
/// healing cycle runs, because a block asserts that the step preconditions
/// cannot be satisfied as stated.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_blocked_turn(
    policy: &mut dyn ApplyPolicy,
    context_manager: &mut ContextManager,
    stream_tx: &mpsc::Sender<StreamEvent>,
    repo_root: &Path,
    reason: String,
    accumulated_text: String,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
    mode: crate::plan::storage::PlanMutationMode,
) -> FsmTurnOutcome {
    tracing::warn!(reason = %reason, "bounded turn blocked");
    rollback_or_log(policy, repo_root).await;
    // The sweep only drops context paths that no longer exist on disk and runs
    // off the async executor thread. A blocked step keeps the auto layer: the
    // coder's research stays available for the controlled replanning cycle.
    crate::blocking::run_blocking(|| {
        context_manager.prune_missing_files();
    });
    let ctx_state = context_manager.get_state();
    let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
    let _ = stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary: "Execution blocked".to_string(),
        edits: Vec::new(),
        proposal: None,
        outcome: EditOutcome {
            applied: false,
            error: Some(reason.clone()),
            changed_files: Vec::new(),
            commit_hash: None,
        },
    };
    let mut outcome = finish_turn(accumulated_text, result, repo_root, plans, mode).await;
    outcome.blocked = Some(reason);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::git_policy::GitTransactionPolicy;
    use crate::workflow::policy::DirectApplyPolicy;
    use tauqe_protocol::{ContextAccess, EditOperation};

    /// Creates a repository holding an editable file and an optional post-hook
    /// declaration.
    fn setup_repo(config: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&root), true).unwrap();
        std::fs::write(root.join("a.rs"), "one\n").unwrap();

        let mut files = vec!["a.rs".to_string()];
        if let Some(config) = config {
            std::fs::write(root.join("tauqe.toml"), config).unwrap();
            files.push("tauqe.toml".to_string());
        }
        crate::git::cmd::stage_files(&root, &files).unwrap();
        crate::git::cmd::commit_with_developer_author(&root, "Add fixtures", false).unwrap();
        (dir, root)
    }

    /// Builds the single replacement the guard applies in these tests.
    fn replace_edit() -> EditProposal {
        EditProposal {
            summary: "update a.rs".to_string(),
            edits: vec![EditOperation::Replace {
                path: "a.rs".to_string(),
                old_text: "one\n".to_string(),
                new_text: "two\n".to_string(),
            }],
        }
    }

    #[tokio::test]
    async fn applies_edits_without_a_declared_post_hook() {
        let (_dir, root) = setup_repo(None);
        let mut context = ContextManager::new(root.clone());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        let mut policy = GitTransactionPolicy::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = crate::toolchain::effective_project_config(&root, None).await;
        let mut guard =
            TransactionRollbackGuard::new(&mut policy, &mut context, &root, &project, cancel_rx);

        let changed = guard.apply_temporary(&replace_edit()).await.unwrap();

        assert_eq!(changed, vec!["a.rs".to_string()]);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");
    }

    #[tokio::test]
    async fn worktree_changes_join_files_produced_after_the_baseline() {
        let (_dir, root) = setup_repo(None);
        let mut context = ContextManager::new(root.clone());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        let mut policy = GitTransactionPolicy::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = crate::toolchain::effective_project_config(&root, None).await;
        let mut guard =
            TransactionRollbackGuard::new(&mut policy, &mut context, &root, &project, cancel_rx);

        // Before any change set reaches the disk there is nothing to compare.
        assert!(guard.worktree_changes().is_empty());

        guard.apply_temporary(&replace_edit()).await.unwrap();

        // A verification command rewrites a tracked file and produces a new one,
        // so both must join the change set even though the model proposed
        // neither of them.
        std::fs::write(root.join("Cargo.lock"), "generated\n").unwrap();
        std::fs::write(root.join("snapshot.txt"), "generated\n").unwrap();

        let mut total = vec!["a.rs".to_string()];
        for path in guard.worktree_changes() {
            if !total.contains(&path) {
                total.push(path);
            }
        }

        assert_eq!(
            total,
            vec![
                "a.rs".to_string(),
                "Cargo.lock".to_string(),
                "snapshot.txt".to_string()
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_hook_artifacts_join_the_change_set() {
        let (_dir, root) = setup_repo(Some(
            "[project.post_hook]\ncommand = \"sh\"\nargs = ['-c', 'printf \"generated\\n\" > generated.rs']\n",
        ));
        let mut context = ContextManager::new(root.clone());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        let mut policy = GitTransactionPolicy::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = crate::toolchain::effective_project_config(&root, None).await;
        let mut guard =
            TransactionRollbackGuard::new(&mut policy, &mut context, &root, &project, cancel_rx);

        let changed = guard.apply_temporary(&replace_edit()).await.unwrap();

        assert!(changed.contains(&"a.rs".to_string()), "{changed:?}");
        assert!(changed.contains(&"generated.rs".to_string()), "{changed:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("generated.rs")).unwrap(),
            "generated\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failing_post_hook_aborts_the_change_set() {
        let (_dir, root) = setup_repo(Some(
            "[project.post_hook]\ncommand = \"sh\"\nargs = ['-c', 'printf \"generator crashed\\n\" >&2; exit 3']\n",
        ));
        let mut context = ContextManager::new(root.clone());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        let mut policy = GitTransactionPolicy::new();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let project = crate::toolchain::effective_project_config(&root, None).await;
        let mut guard =
            TransactionRollbackGuard::new(&mut policy, &mut context, &root, &project, cancel_rx);

        let err = guard.apply_temporary(&replace_edit()).await.unwrap_err();

        assert!(
            err.to_string().contains("post-hook"),
            "unexpected reason: {err}"
        );
        assert!(
            err.to_string().contains("generator crashed"),
            "the hook output must reach the diagnostic: {err}"
        );
    }
    /// Policy whose rollback fails a fixed number of times before it succeeds,
    /// standing in for a file lock that another process releases shortly.
    ///
    /// Every phase other than `rollback` delegates to [`DirectApplyPolicy`], so
    /// the double satisfies the whole `ApplyPolicy` contract instead of leaving
    /// a panicking branch that the rollback helper never exercises.
    struct TransientLockPolicy {
        inner: DirectApplyPolicy,
        failures_left: usize,
        attempts: usize,
    }

    impl TransientLockPolicy {
        fn new(failures_left: usize) -> Self {
            Self {
                inner: DirectApplyPolicy::new(),
                failures_left,
                attempts: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl ApplyPolicy for TransientLockPolicy {
        async fn staging(
            &mut self,
            repo_root: &Path,
            project: &ProjectConfig,
            context_manager: &ContextManager,
            edits: &[EditOperation],
            staged_state: Option<&crate::edits::StagedEditsState>,
            cancel_rx: Option<&watch::Receiver<bool>>,
        ) -> crate::edits::StageEditsResult {
            self.inner
                .staging(
                    repo_root,
                    project,
                    context_manager,
                    edits,
                    staged_state,
                    cancel_rx,
                )
                .await
        }

        async fn apply_temporary(
            &mut self,
            repo_root: &Path,
            project: &ProjectConfig,
            context_manager: &mut ContextManager,
            proposal: &EditProposal,
        ) -> anyhow::Result<Vec<String>> {
            self.inner
                .apply_temporary(repo_root, project, context_manager, proposal)
                .await
        }

        async fn apply_final(
            &mut self,
            repo_root: &Path,
            changed_files: &[String],
            summary: &str,
        ) -> anyhow::Result<Option<String>> {
            self.inner
                .apply_final(repo_root, changed_files, summary)
                .await
        }

        async fn rollback(&mut self, _repo_root: &Path) -> anyhow::Result<bool> {
            self.attempts += 1;
            if self.failures_left == 0 {
                return Ok(true);
            }
            self.failures_left -= 1;
            anyhow::bail!("the working tree is locked by another process")
        }
    }

    #[tokio::test]
    async fn rollback_retries_a_transient_lock_until_it_clears() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = TransientLockPolicy::new(2);

        assert!(rollback_or_log(&mut policy, dir.path()).await);
        assert_eq!(policy.attempts, ROLLBACK_ATTEMPTS);
    }

    #[tokio::test]
    async fn rollback_reports_failure_once_every_attempt_is_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = TransientLockPolicy::new(ROLLBACK_ATTEMPTS);

        assert!(!rollback_or_log(&mut policy, dir.path()).await);
        assert_eq!(policy.attempts, ROLLBACK_ATTEMPTS);
    }
}
