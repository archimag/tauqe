//! Git-backed transactional apply policy for edit workflows.

use crate::config::project::ProjectConfig;

mod operations;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::edits::EditOperation;
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::blocking::StartedOperations;
use operations::GitOperation;

use super::policy::{ApplyPolicy, FinalizationStatus, PolicyRollback};
use crate::context::ContextManager;
use crate::edits::{
    apply_edit_proposal_with_config_and_operations, prepare_staged_edits_with_config,
    StageEditsResult, StagedEditsState,
};
use crate::git::{
    create_ai_commit, create_checkpoint, create_step_commit, finalize_ai_commit,
    restore_checkpoint, CheckpointInfo,
};
use crate::stop::HardCancellation;

/// Transactional apply policy that records every applied change set as a
/// recoverable Git transaction.
///
/// Before any edit touches the working tree the policy captures uncommitted
/// developer work in a temporary checkpoint commit. The applied changes are then
/// written to disk and recorded as an intermediate step commit so the toolchain
/// verification phase runs against a reproducible revision. On success the step
/// commits are squashed into a single permanent AI commit; on failure the
/// working tree is reset back to the pre-edit checkpoint, restoring the
/// developer's original uncommitted state.
///
/// The optional `guard` flag mirrors whether a transaction is currently open.
/// A turn supervisor consults it to avoid force-aborting a workflow task in the
/// middle of a Git transaction, which would otherwise strand a checkpoint or
/// step commit until startup crash recovery runs.
///
/// The optional `cancel` receiver is the turn's cancellation signal. The capture
/// and the step commit observe it, so cancelling the turn releases the commit a
/// repository hook is holding instead of waiting for the hook to finish.
/// Finalization and rollback stay cancel-blind on purpose: both settle an outcome
/// the turn already decided, and completing them under the wall-clock budget is
/// what leaves the working tree in a known state and what makes the deferred
/// force-abort of a stalled turn deterministic instead of open-ended.
#[derive(Clone)]
pub struct GitTransactionPolicy {
    state: Arc<Mutex<GitState>>,
    guard: Option<Arc<AtomicBool>>,
    /// Turn-scoped cancellation signal the capture and the step commit observe.
    cancel: Option<HardCancellation>,
}

#[derive(Default)]
struct GitState {
    operations: StartedOperations,
    publication: StartedOperations,
    checkpoint: Option<CheckpointInfo>,
    capture: GitOperation<CheckpointInfo>,
    step: GitOperation<String>,
    pending_step: Option<Vec<String>>,
    finalization: GitOperation<String>,
    restoration: GitOperation<()>,
    applying: bool,
    closed: bool,
}

impl Default for GitTransactionPolicy {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(GitState::default())),
            guard: None,
            cancel: None,
        }
    }
}

impl GitTransactionPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a policy that reports an open transaction through `guard` and
    /// observes the turn's cancellation signal while capturing and committing.
    pub fn with_guard_and_cancel(guard: Arc<AtomicBool>, cancel: HardCancellation) -> Self {
        Self {
            state: Arc::new(Mutex::new(GitState::default())),
            guard: Some(guard),
            cancel: Some(cancel),
        }
    }

    /// Publishes the current transaction state to the shared guard, if any.
    fn publish_transaction_state(&self) {
        if let Some(guard) = &self.guard {
            guard.store(true, Ordering::SeqCst);
        }
    }

    /// Clears the shared guard once the transaction is closed.
    fn clear_transaction_state(&self) {
        if let Some(guard) = &self.guard {
            guard.store(false, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl ApplyPolicy for GitTransactionPolicy {
    async fn staging(
        &mut self,
        repo_root: &Path,
        project: &ProjectConfig,
        context_manager: &ContextManager,
        edits: &[EditOperation],
        staged_state: Option<&StagedEditsState>,
        cancel_rx: Option<&HardCancellation>,
    ) -> StageEditsResult {
        prepare_staged_edits_with_config(
            repo_root,
            project,
            context_manager,
            edits,
            staged_state,
            cancel_rx,
        )
        .await
    }

    async fn apply_temporary_batches(
        &mut self,
        repo_root: &Path,
        project: &ProjectConfig,
        context_manager: &mut ContextManager,
        batches: &[&[EditOperation]],
    ) -> anyhow::Result<Vec<String>> {
        self.apply_temporary_batches_owned(repo_root, project, context_manager, batches, None)
            .await
    }

    async fn apply_temporary_batches_owned(
        &mut self,
        repo_root: &Path,
        project: &ProjectConfig,
        context_manager: &mut ContextManager,
        batches: &[&[EditOperation]],
        cancel_rx: Option<&HardCancellation>,
    ) -> anyhow::Result<Vec<String>> {
        // Capture the pre-edit checkpoint exactly once. The verification-healing
        // loop calls this method repeatedly; recreating the checkpoint on every
        // call would advance its base onto the previous temporary step commit,
        // so a later rollback or finalization could no longer return the working
        // tree to its true pre-turn state.
        let mut state = self.state.lock().await;
        anyhow::ensure!(!state.closed, "Git transaction is closed to new work");
        anyhow::ensure!(!state.applying, "Git application is already in progress");
        state.applying = true;
        if state.checkpoint.is_none() {
            let root = repo_root.to_path_buf();
            // The receiver is cloned into the offload because the closure owns
            // everything it touches; the clone shares the turn's channel, so the
            // capture observes exactly the signal the turn raises.
            let cancel = self.cancel.clone();
            self.publish_transaction_state();
            let owner = state.operations.clone();
            state
                .capture
                .start(&owner, move || create_checkpoint(&root, cancel.as_ref()))?;
            let capture = state.capture.clone();
            drop(state);
            let checkpoint = capture.result().await?;
            state = self.state.lock().await;
            anyhow::ensure!(!state.closed, "Git transaction is closed to new work");
            state.checkpoint = Some(checkpoint);
        }

        if let Some(files) = state.pending_step.clone() {
            let step = state.step.clone();
            drop(state);
            step.result().await?;
            state = self.state.lock().await;
            anyhow::ensure!(!state.closed, "Git transaction is closed to new work");
            state.pending_step = None;
            state.step = GitOperation::Idle;
            state.applying = false;
            return Ok(files);
        }

        let publication = state.publication.clone();
        drop(state);

        let changed_files = apply_edit_proposal_with_config_and_operations(
            repo_root,
            project,
            context_manager,
            batches,
            cancel_rx,
            &publication,
        )
        .await?;

        let mut state = self.state.lock().await;
        anyhow::ensure!(!state.closed, "Git transaction is closed to new work");
        if !changed_files.is_empty() {
            let root = repo_root.to_path_buf();
            let files = changed_files.clone();
            let cancel = self.cancel.clone();
            let owner = state.operations.clone();
            state.step.start(&owner, move || {
                create_step_commit(&root, &files, "initial attempt", cancel.as_ref())
            })?;
            state.pending_step = Some(changed_files.clone());
            let step = state.step.clone();
            drop(state);
            step.result().await?;
            state = self.state.lock().await;
            anyhow::ensure!(!state.closed, "Git transaction is closed to new work");
            state.pending_step = None;
            state.step = GitOperation::Idle;
        }

        state.applying = false;
        Ok(changed_files)
    }

    async fn apply_final(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        commit_message: &str,
    ) -> anyhow::Result<Option<String>> {
        self.finalize_resumable(repo_root, changed_files, commit_message, None)
            .await?
            .into_result()
    }

    async fn stop_and_wait(&mut self) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        if !state.finalization.started() {
            state.closed = true;
        }
        state.publication.close();
        let publication = state.publication.clone();
        let operations = state.operations.clone();
        drop(state);
        let published = publication.wait().await;
        let git = operations.wait().await;
        anyhow::ensure!(
            published.unconfirmed == 0 && git.unconfirmed == 0,
            "file or Git operation completion could not be confirmed"
        );
        Ok(())
    }

    async fn finalize_resumable(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        commit_message: &str,
        cancel: Option<&HardCancellation>,
    ) -> anyhow::Result<FinalizationStatus> {
        self.finalize_with_admission(repo_root, changed_files, commit_message, cancel, true)
            .await
    }

    async fn finalization_result(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        commit_message: &str,
        cancel: Option<&HardCancellation>,
    ) -> anyhow::Result<FinalizationStatus> {
        self.finalize_with_admission(repo_root, changed_files, commit_message, cancel, false)
            .await
    }

    async fn rollback(&mut self, repo_root: &Path) -> anyhow::Result<PolicyRollback> {
        // Rollback stays cancel-blind for the same reason as finalization: it has
        // to return the tree to the recorded base even when the turn was cancelled.
        let mut state = self.state.lock().await;
        state.closed = true;
        state.publication.close();
        let publication_owner = state.publication.clone();
        let capture = state.capture.clone();
        let step = state.step.clone();
        let checkpoint = state.checkpoint.clone();
        let finalization_started = state.finalization.started();
        drop(state);
        let publication = publication_owner.wait().await;
        anyhow::ensure!(
            publication.unconfirmed == 0,
            "file publication could not be confirmed"
        );
        anyhow::ensure!(
            !finalization_started,
            "Git finalization already started; retrieve its result before recovery"
        );
        let checkpoint = if checkpoint.is_none() && capture.started() {
            Some(capture.result().await?)
        } else {
            checkpoint
        };
        if step.started() {
            let _ = step.result().await;
        }
        let mut state = self.state.lock().await;
        if let Some(checkpoint) = checkpoint {
            let root = repo_root.to_path_buf();
            let owner = state.operations.clone();
            state
                .restoration
                .start(&owner, move || restore_checkpoint(&root, &checkpoint))?;
            let restoration = state.restoration.clone();
            drop(state);
            restoration.result().await?;
            state = self.state.lock().await;
            state.checkpoint = None;
            state.closed = false;
            state.capture = GitOperation::Idle;
            state.step = GitOperation::Idle;
            state.pending_step = None;
            state.applying = false;
            state.restoration = GitOperation::Idle;
            state.publication = StartedOperations::default();
            self.clear_transaction_state();
            Ok(PolicyRollback::RestoredCheckpoint)
        } else {
            state.closed = false;
            state.applying = false;
            state.publication = StartedOperations::default();
            self.clear_transaction_state();
            Ok(PolicyRollback::NoCheckpoint)
        }
    }
}

impl GitTransactionPolicy {
    async fn finalize_with_admission(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        commit_message: &str,
        cancel: Option<&HardCancellation>,
        admit: bool,
    ) -> anyhow::Result<FinalizationStatus> {
        // Finalization stays cancel-blind: it settles an outcome the turn already
        // decided, and completing it under the wall-clock budget is what leaves the
        // tree in a known state and lets the server's deferred force-abort wait for
        // the transaction to close deterministically.
        let root = repo_root.to_path_buf();
        let files = changed_files.to_vec();
        let commit_message_owned = commit_message.to_string();
        let mut state = self.state.lock().await;
        if !admit && !state.finalization.started() {
            return Ok(FinalizationStatus::NotAdmitted);
        }
        anyhow::ensure!(!state.applying, "Git application is already in progress");
        if state.pending_step.is_some() {
            let step = state.step.clone();
            drop(state);
            step.result().await?;
            state = self.state.lock().await;
            state.pending_step = None;
            state.step = GitOperation::Idle;
        }
        anyhow::ensure!(
            !state.restoration.started(),
            "Git restoration already started"
        );
        if !state.finalization.started() {
            anyhow::ensure!(!state.closed, "Git transaction is closed to new work");
            if self
                .cancel
                .as_ref()
                .is_some_and(HardCancellation::is_cancelled)
                || cancel.is_some_and(HardCancellation::is_cancelled)
            {
                return Ok(FinalizationStatus::NotAdmitted);
            }
            state.closed = true;
        }
        let checkpoint = state.checkpoint.clone();
        let owner = state.operations.clone();
        state.finalization.start(&owner, move || {
            if let Some(checkpoint) = checkpoint {
                finalize_ai_commit(&root, &checkpoint, &files, &commit_message_owned)
            } else {
                create_ai_commit(&root, &files, &commit_message_owned)
            }
        })?;
        let finalization = state.finalization.clone();
        drop(state);
        let res = finalization.result().await;

        let status = match res {
            // A verified change set can live entirely outside Git — the
            // `.tauqe/mockups/` sandbox is the documented case — so finalization
            // reports the absence of a commit rather than a hash the caller
            // would try to resolve.
            Ok(hash) => {
                self.clear_transaction_state();
                FinalizationStatus::Completed {
                    result: Ok((!hash.is_empty()).then_some(hash)),
                    cancelled: self
                        .cancel
                        .as_ref()
                        .is_some_and(HardCancellation::is_cancelled)
                        || cancel.is_some_and(HardCancellation::is_cancelled),
                }
            }
            Err(err) => {
                tracing::warn!("Failed to finalize squashed AI commit: {}", err);
                FinalizationStatus::Completed {
                    result: Err(format!("{err:#}")),
                    cancelled: self
                        .cancel
                        .as_ref()
                        .is_some_and(HardCancellation::is_cancelled)
                        || cancel.is_some_and(HardCancellation::is_cancelled),
                }
            }
        };
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::project::ProjectConfig;
    use crate::context::ContextAccess;
    use crate::stop::StopSource;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use super::{ApplyPolicy, GitTransactionPolicy, PolicyRollback};
    use crate::context::ContextManager;
    use crate::edits::EditOperation;
    use crate::edits::EditProposal;
    use crate::edits::EditReplace;

    /// Project configuration the policy tests stage under: no formatter, no hook.
    fn project() -> ProjectConfig {
        ProjectConfig::default()
    }

    fn init_repo(root: &std::path::Path) {
        crate::git::init_repository(Some(root), true).unwrap();
        std::fs::write(root.join("a.rs"), "one\n").unwrap();
        crate::git::cmd::stage_files(root, &["a.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(root, "Initial commit", false).unwrap();
    }

    fn editable_context(root: &std::path::Path) -> ContextManager {
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();
        cm
    }

    fn replace_edit() -> Vec<EditOperation> {
        vec![EditOperation::Replace(EditReplace {
            path: "a.rs".to_string(),
            old_text: "one\n".to_string(),
            new_text: "two\n".to_string(),
        })]
    }

    #[tokio::test]
    async fn git_policy_applies_and_finalizes_into_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let mut cm = editable_context(root);

        let edits = replace_edit();
        let active = Arc::new(AtomicBool::new(false));
        let stop = StopSource::new();
        let mut policy = GitTransactionPolicy::with_guard_and_cancel(
            active.clone(),
            stop.token().hard_cancellation(),
        );
        assert!(!active.load(std::sync::atomic::Ordering::SeqCst));
        let staged = policy
            .staging(root, &project(), &cm, &edits, None, None)
            .await;
        assert!(staged.is_success());

        let proposal = EditProposal {
            summary: "update a.rs".to_string(),
            edits: edits.clone(),
        };
        let changed = policy
            .apply_temporary(root, &project(), &mut cm, &proposal)
            .await
            .unwrap();
        assert!(active.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(changed, vec!["a.rs".to_string()]);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

        let commit = policy
            .apply_final(root, &changed, "update a.rs")
            .await
            .unwrap();
        assert!(commit.is_some());
        assert!(!active.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");
    }

    #[tokio::test]
    async fn git_policy_rolls_back_to_pre_edit_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let mut cm = editable_context(root);

        let edits = replace_edit();
        let active = Arc::new(AtomicBool::new(false));
        let stop = StopSource::new();
        let mut policy = GitTransactionPolicy::with_guard_and_cancel(
            active.clone(),
            stop.token().hard_cancellation(),
        );
        assert!(!active.load(std::sync::atomic::Ordering::SeqCst));
        let proposal = EditProposal {
            summary: "update a.rs".to_string(),
            edits: edits.clone(),
        };
        let changed = policy
            .apply_temporary(root, &project(), &mut cm, &proposal)
            .await
            .unwrap();
        assert!(active.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(changed, vec!["a.rs".to_string()]);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

        assert_eq!(
            policy.rollback(root).await.unwrap(),
            PolicyRollback::RestoredCheckpoint
        );
        assert!(!active.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one\n");
    }

    fn head_subject(root: &std::path::Path) -> String {
        crate::git::cmd::run_git(Some(root), &["log", "-1", "--format=%s"])
            .unwrap()
            .trim()
            .to_string()
    }

    fn commit_count(root: &std::path::Path) -> usize {
        crate::git::cmd::run_git(Some(root), &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn git_policy_rolls_back_to_pre_edit_state_after_healing_retries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let mut cm = editable_context(root);

        let mut policy = GitTransactionPolicy::new();

        let first = EditProposal {
            summary: "first attempt".to_string(),
            edits: vec![EditOperation::Replace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "one\n".to_string(),
                new_text: "two\n".to_string(),
            })],
        };
        policy
            .apply_temporary(root, &project(), &mut cm, &first)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

        let second = EditProposal {
            summary: "healing attempt".to_string(),
            edits: vec![EditOperation::Replace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "two\n".to_string(),
                new_text: "three\n".to_string(),
            })],
        };
        policy
            .apply_temporary(root, &project(), &mut cm, &second)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "three\n"
        );

        // Rolling back after several temporary applications must return the
        // tree to its pristine pre-turn state and drop the whole transaction.
        assert_eq!(
            policy.rollback(root).await.unwrap(),
            PolicyRollback::RestoredCheckpoint
        );
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one\n");
        assert_eq!(head_subject(root), "Initial commit");
        // The fixture already carries `chore: initial commit` and the
        // developer's `Initial commit`; rollback must leave no temporary step
        // commit behind, so the count returns to the pre-turn baseline of two.
        assert_eq!(commit_count(root), 2);
    }

    #[tokio::test]
    async fn git_policy_finalizes_single_commit_after_healing_retries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let mut cm = editable_context(root);

        let mut policy = GitTransactionPolicy::new();

        let first = EditProposal {
            summary: "first attempt".to_string(),
            edits: vec![EditOperation::Replace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "one\n".to_string(),
                new_text: "two\n".to_string(),
            })],
        };
        policy
            .apply_temporary(root, &project(), &mut cm, &first)
            .await
            .unwrap();

        let second = EditProposal {
            summary: "healing attempt".to_string(),
            edits: vec![EditOperation::Replace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "two\n".to_string(),
                new_text: "three\n".to_string(),
            })],
        };
        let changed = policy
            .apply_temporary(root, &project(), &mut cm, &second)
            .await
            .unwrap();

        let commit = policy
            .apply_final(root, &changed, "update a.rs")
            .await
            .unwrap();
        assert!(commit.is_some());

        // Intermediate step commits must be squashed so exactly one new commit
        // carries the final summary on top of the initial commit.
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "three\n"
        );
        assert_eq!(head_subject(root), "update a.rs");
        // Exactly one permanent commit is added on top of the fixture's two
        // baseline commits, proving the healing step commits were squashed.
        assert_eq!(commit_count(root), 3);
    }

    #[tokio::test]
    async fn git_policy_rollback_resets_checkpoint_so_next_attempt_creates_fresh_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let mut cm = editable_context(root);

        let mut policy = GitTransactionPolicy::new();

        // Attempt 1: Apply temporary and then rollback
        let first = EditProposal {
            summary: "failed attempt".to_string(),
            edits: replace_edit(),
        };
        policy
            .apply_temporary(root, &project(), &mut cm, &first)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

        assert_eq!(
            policy.rollback(root).await.unwrap(),
            PolicyRollback::RestoredCheckpoint
        );
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one\n");
        assert!(policy.state.lock().await.checkpoint.is_none());

        // Developer makes an uncommitted modification between attempts
        std::fs::write(root.join("a.rs"), "developer edit\n").unwrap();

        // Attempt 2 (e.g. coder ladder escalation): Fresh checkpoint must be formed
        let second = EditProposal {
            summary: "escalated attempt".to_string(),
            edits: vec![EditOperation::Replace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "developer edit\n".to_string(),
                new_text: "three\n".to_string(),
            })],
        };
        let changed = policy
            .apply_temporary(root, &project(), &mut cm, &second)
            .await
            .unwrap();
        assert_eq!(changed, vec!["a.rs".to_string()]);
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "three\n"
        );

        let commit = policy
            .apply_final(root, &changed, "escalated attempt")
            .await
            .unwrap();
        assert!(commit.is_some());
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "three\n"
        );
    }
    #[tokio::test]
    async fn git_policy_apply_final_reports_a_verified_empty_change_set_without_a_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);

        let mut policy = GitTransactionPolicy::new();
        // A verified change set may live entirely outside Git, where the turn
        // produced no tracked path at all: finalization reports the absence of a
        // commit instead of failing the turn.
        let hash = policy
            .apply_final(root, &[], "empty summary")
            .await
            .expect("a verified change set outside Git must not fail finalization");
        assert!(
            hash.is_none(),
            "an untracked change set produces no commit: {hash:?}"
        );
        assert_eq!(head_subject(root), "Initial commit");
    }

    /// Installs a `pre-commit` hook that announces its start and then sleeps far
    /// beyond the test's patience, so a cancellation has to release the capture.
    #[cfg(unix)]
    fn install_sleeping_hook(root: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(
            &hook,
            "#!/bin/sh\necho started > hook-started.log\nsleep 30\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).unwrap();
    }

    /// A cancellation that arrives while a repository hook holds the checkpoint
    /// commit releases the capture promptly and reports the cancellation.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancelled_capture_releases_a_hook_that_holds_the_checkpoint_commit() {
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        // The tree must be dirty for the capture to create its checkpoint commit,
        // and the project must opt into running the repository hooks.
        std::fs::write(
            root.join("tauqe.toml"),
            "[git]\nbypass_hooks = false\ntimeout_secs = 300\n",
        )
        .unwrap();
        install_sleeping_hook(root);
        let mut cm = editable_context(root);

        let stop = StopSource::new();
        let cancel = stop.token().hard_cancellation();
        let marker = root.join("hook-started.log");
        let canceller = std::thread::spawn(move || {
            for _ in 0..400 {
                if marker.exists() {
                    stop.request_hard();
                    return true;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            false
        });

        let guard = Arc::new(AtomicBool::new(false));
        let mut policy = GitTransactionPolicy::with_guard_and_cancel(guard, cancel);
        let proposal = EditProposal {
            summary: "update a.rs".to_string(),
            edits: replace_edit(),
        };
        let started = Instant::now();
        let error = policy
            .apply_temporary(root, &project(), &mut cm, &proposal)
            .await
            .expect_err("a cancelled capture must not hand back a checkpoint");

        assert!(canceller.join().unwrap(), "the hook never started");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("was cancelled"), "error: {rendered}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the cancellation did not release the capture: {:?}",
            started.elapsed()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "one\n",
            "the cancelled turn never reached the working tree"
        );
    }

    /// A turn cancelled before the capture is refused before the first probe, so no
    /// transaction marker is written and no developer work is recorded.
    #[tokio::test]
    async fn a_turn_cancelled_before_the_capture_opens_no_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        // Uncommitted developer work makes a capture observable: a capture that ran
        // would record it in a checkpoint commit and write the transaction marker.
        std::fs::write(root.join("notes.txt"), "uncommitted work\n").unwrap();
        let mut cm = editable_context(root);

        let stop = StopSource::new();
        stop.request_hard();
        let guard = Arc::new(AtomicBool::new(false));
        let mut policy =
            GitTransactionPolicy::with_guard_and_cancel(guard, stop.token().hard_cancellation());
        let proposal = EditProposal {
            summary: "update a.rs".to_string(),
            edits: replace_edit(),
        };
        let error = policy
            .apply_temporary(root, &project(), &mut cm, &proposal)
            .await
            .expect_err("a cancelled turn must not open a transaction");

        let rendered = format!("{error:#}");
        assert!(rendered.contains("was cancelled"), "error: {rendered}");
        assert_eq!(head_subject(root), "Initial commit");
        assert!(
            !root.join(".tauqe/active-transaction.json").exists(),
            "the refusal must come before the capture writes its marker"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt")).unwrap(),
            "uncommitted work\n"
        );
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one\n");
    }
}
