//! Git-backed transactional apply policy for edit workflows.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauqe_protocol::{EditOperation, EditProposal};

use super::policy::ApplyPolicy;
use crate::context::ContextManager;
use crate::edits::{
    apply_edit_proposal, stage_and_validate_edits, StageEditsResult, StagedEditsState,
};
use crate::git::{
    create_ai_commit, create_checkpoint, create_step_commit, finalize_ai_commit, restore_checkpoint,
    CheckpointInfo,
};

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
#[derive(Debug, Default, Clone)]
pub struct GitTransactionPolicy {
    checkpoint: Option<CheckpointInfo>,
    guard: Option<Arc<AtomicBool>>,
}

impl GitTransactionPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a policy that reports an open transaction through `guard`.
    pub fn with_guard(guard: Arc<AtomicBool>) -> Self {
        Self {
            checkpoint: None,
            guard: Some(guard),
        }
    }

    /// Publishes the current transaction state to the shared guard, if any.
    fn publish_transaction_state(&self) {
        if let Some(guard) = &self.guard {
            guard.store(self.checkpoint.is_some(), Ordering::SeqCst);
        }
    }

    /// Clears the shared guard once the transaction is closed.
    fn clear_transaction_state(&self) {
        if let Some(guard) = &self.guard {
            guard.store(false, Ordering::SeqCst);
        }
    }
}

impl ApplyPolicy for GitTransactionPolicy {
    fn staging(
        &mut self,
        repo_root: &Path,
        context_manager: &ContextManager,
        edits: &[EditOperation],
        staged_state: Option<&StagedEditsState>,
    ) -> StageEditsResult {
        stage_and_validate_edits(repo_root, context_manager, edits, staged_state)
    }

    fn apply_temporary(
        &mut self,
        repo_root: &Path,
        context_manager: &mut ContextManager,
        proposal: &EditProposal,
    ) -> anyhow::Result<Vec<String>> {
        // Capture the pre-edit checkpoint exactly once. The verification-healing
        // loop calls this method repeatedly; recreating the checkpoint on every
        // call would advance its base onto the previous temporary step commit,
        // so a later rollback or finalization could no longer return the working
        // tree to its true pre-turn state.
        if self.checkpoint.is_none() {
            self.checkpoint = create_checkpoint(repo_root).ok();
        }
        self.publish_transaction_state();

        let changed_files = apply_edit_proposal(repo_root, context_manager, proposal)?;

        if self.checkpoint.is_some() {
            let _ = create_step_commit(repo_root, &changed_files, "initial attempt");
        }

        Ok(changed_files)
    }

    fn apply_final(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        summary: &str,
    ) -> anyhow::Result<Option<String>> {
        let commit_hash = if let Some(checkpoint) = &self.checkpoint {
            match finalize_ai_commit(repo_root, checkpoint, changed_files, summary) {
                Ok(hash) => Some(hash),
                Err(err) => {
                    tracing::warn!("Failed to finalize squashed AI commit: {}", err);
                    None
                }
            }
        } else {
            match create_ai_commit(repo_root, changed_files, summary) {
                Ok(hash) => Some(hash),
                Err(err) => {
                    tracing::warn!("Failed to create AI commit: {}", err);
                    None
                }
            }
        };
        self.clear_transaction_state();
        Ok(commit_hash)
    }

    fn rollback(&mut self, repo_root: &Path) -> anyhow::Result<bool> {
        if let Some(checkpoint) = &self.checkpoint {
            restore_checkpoint(repo_root, checkpoint)?;
            self.clear_transaction_state();
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplyPolicy, GitTransactionPolicy};
    use crate::context::ContextManager;
    use tauqe_protocol::{ContextAccess, EditOperation, EditProposal};

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
        vec![EditOperation::Replace {
            path: "a.rs".to_string(),
            old_text: "one\n".to_string(),
            new_text: "two\n".to_string(),
        }]
    }

    #[test]
    fn git_policy_applies_and_finalizes_into_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let mut cm = editable_context(root);

        let edits = replace_edit();
        let mut policy = GitTransactionPolicy::new();
        let staged = policy.staging(root, &cm, &edits, None);
        assert!(staged.is_success());

        let proposal = EditProposal {
            summary: "update a.rs".to_string(),
            edits: edits.clone(),
        };
        let changed = policy.apply_temporary(root, &mut cm, &proposal).unwrap();
        assert_eq!(changed, vec!["a.rs".to_string()]);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

        let commit = policy.apply_final(root, &changed, "update a.rs").unwrap();
        assert!(commit.is_some());
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");
    }

    #[test]
    fn git_policy_rolls_back_to_pre_edit_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let mut cm = editable_context(root);

        let edits = replace_edit();
        let mut policy = GitTransactionPolicy::new();
        let proposal = EditProposal {
            summary: "update a.rs".to_string(),
            edits: edits.clone(),
        };
        let changed = policy.apply_temporary(root, &mut cm, &proposal).unwrap();
        assert_eq!(changed, vec!["a.rs".to_string()]);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

        assert!(policy.rollback(root).unwrap());
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

    #[test]
    fn git_policy_rolls_back_to_pre_edit_state_after_healing_retries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let mut cm = editable_context(root);

        let mut policy = GitTransactionPolicy::new();

        let first = EditProposal {
            summary: "first attempt".to_string(),
            edits: vec![EditOperation::Replace {
                path: "a.rs".to_string(),
                old_text: "one\n".to_string(),
                new_text: "two\n".to_string(),
            }],
        };
        policy.apply_temporary(root, &mut cm, &first).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

        let second = EditProposal {
            summary: "healing attempt".to_string(),
            edits: vec![EditOperation::Replace {
                path: "a.rs".to_string(),
                old_text: "two\n".to_string(),
                new_text: "three\n".to_string(),
            }],
        };
        policy.apply_temporary(root, &mut cm, &second).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "three\n");

        // Rolling back after several temporary applications must return the
        // tree to its pristine pre-turn state and drop the whole transaction.
        assert!(policy.rollback(root).unwrap());
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "one\n");
        assert_eq!(head_subject(root), "Initial commit");
        // The fixture already carries `chore: initial commit` and the
        // developer's `Initial commit`; rollback must leave no temporary step
        // commit behind, so the count returns to the pre-turn baseline of two.
        assert_eq!(commit_count(root), 2);
    }

    #[test]
    fn git_policy_finalizes_single_commit_after_healing_retries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let mut cm = editable_context(root);

        let mut policy = GitTransactionPolicy::new();

        let first = EditProposal {
            summary: "first attempt".to_string(),
            edits: vec![EditOperation::Replace {
                path: "a.rs".to_string(),
                old_text: "one\n".to_string(),
                new_text: "two\n".to_string(),
            }],
        };
        policy.apply_temporary(root, &mut cm, &first).unwrap();

        let second = EditProposal {
            summary: "healing attempt".to_string(),
            edits: vec![EditOperation::Replace {
                path: "a.rs".to_string(),
                old_text: "two\n".to_string(),
                new_text: "three\n".to_string(),
            }],
        };
        let changed = policy.apply_temporary(root, &mut cm, &second).unwrap();

        let commit = policy.apply_final(root, &changed, "update a.rs").unwrap();
        assert!(commit.is_some());

        // Intermediate step commits must be squashed so exactly one new commit
        // carries the final summary on top of the initial commit.
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "three\n");
        assert_eq!(head_subject(root), "update a.rs");
        // Exactly one permanent commit is added on top of the fixture's two
        // baseline commits, proving the healing step commits were squashed.
        assert_eq!(commit_count(root), 3);
    }
}
