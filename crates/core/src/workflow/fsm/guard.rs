//! Transactional finalization and rollback for the FSM bounded turn.
//!
//! The turn engine persists validated changes exclusively through an
//! [`ApplyPolicy`]. This module owns the transactional lifecycle: applying a
//! proposal into the working tree, finalizing it into a commit, rolling it
//! back on failure, and applying the turn's plan mutations once the turn
//! succeeds.

use std::path::Path;

use tauqe_protocol::{EditOutcome, EditProposal, ModelResult};
use tokio::sync::{mpsc, watch};

use crate::context::ContextManager;
use crate::git::state::{changed_worktree_paths, snapshot_worktree};
use crate::providers::StreamEvent;
use crate::workflow::policy::ApplyPolicy;

use super::FsmTurnOutcome;

/// Rolls back an open transaction, logging a failure instead of discarding it.
///
/// A failed rollback leaves the working tree modified while the caller reports
/// that the turn was reverted, so the discrepancy must stay visible rather than
/// being silently dropped. Returns whether the working tree was actually
/// restored.
pub(super) async fn rollback_or_log(policy: &mut dyn ApplyPolicy, repo_root: &Path) -> bool {
    match policy.rollback(repo_root).await {
        Ok(rolled_back) => rolled_back,
        Err(err) => {
            tracing::error!(
                "Failed to roll back the turn transaction; the working tree may still be modified: {}",
                err
            );
            false
        }
    }
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
    cancel_rx: watch::Receiver<bool>,
    armed: bool,
}

impl<'a> TransactionRollbackGuard<'a> {
    pub(super) fn new(
        policy: &'a mut dyn ApplyPolicy,
        context_manager: &'a mut ContextManager,
        repo_root: &'a Path,
        cancel_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            policy,
            context_manager,
            repo_root,
            cancel_rx,
            armed: false,
        }
    }

    pub(super) fn arm(&mut self) {
        self.armed = true;
    }

    pub(super) async fn rollback(&mut self) -> bool {
        self.armed = false;
        let rolled_back = rollback_or_log(self.policy, self.repo_root).await;
        // Clearing auto context recomputes hierarchical pinned files through
        // disk checks, so the sweep runs off the async executor thread.
        crate::blocking::run_blocking(|| {
            self.context_manager.clear_auto();
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
            .apply_temporary(self.repo_root, self.context_manager, proposal)
            .await?;

        for artifact in self.run_post_hook().await? {
            if !changed_files.contains(&artifact) {
                changed_files.push(artifact);
            }
        }

        Ok(changed_files)
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

        let response = crate::toolchain::run_post_hooks(repo_root, Some(&self.cancel_rx))
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
    if should_apply_plans {
        apply_turn_plans(repo_root, plans, mode).await;
    }
    FsmTurnOutcome {
        assistant_text,
        result,
        is_cancelled: false,
        blocked: None,
        has_terminal_success: false,
        plan_saved,
        skip_reason: None,
    }
}

async fn apply_turn_plans(
    repo_root: &Path,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
    mode: crate::plan::storage::PlanMutationMode,
) {
    if plans.is_empty() {
        return;
    }
    let root = repo_root.to_path_buf();
    let plans_vec = plans.to_vec();
    if let Err(err) = crate::blocking::run_blocking_async(move || {
        for plan_update in &plans_vec {
            let outcome = crate::plan::storage::PlanStorage::apply_discussion_plan_update_with_mode(
                &root,
                plan_update,
                mode,
            );
            for err in &outcome.errors {
                tracing::warn!("Plan update rejected: {}", err);
            }
            for plan in outcome.applied {
                let _ = crate::plan::storage::PlanStorage::save_active_id(&root, Some(&plan.id));
            }
        }
    })
    .await
    {
        tracing::error!("Failed to apply turn plans: {}", err);
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
    // Auto-layer cleanup performs filesystem metadata checks, so the sweep
    // runs off the async executor thread.
    crate::blocking::run_blocking(|| {
        context_manager.clear_auto();
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
        let mut guard = TransactionRollbackGuard::new(&mut policy, &mut context, &root, cancel_rx);

        let changed = guard.apply_temporary(&replace_edit()).await.unwrap();

        assert_eq!(changed, vec!["a.rs".to_string()]);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");
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
        let mut guard = TransactionRollbackGuard::new(&mut policy, &mut context, &root, cancel_rx);

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
        let mut guard = TransactionRollbackGuard::new(&mut policy, &mut context, &root, cancel_rx);

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
}
