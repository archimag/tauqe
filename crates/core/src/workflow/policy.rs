use std::path::Path;

use async_trait::async_trait;
use tauqe_protocol::{EditOperation, EditProposal};

use crate::context::ContextManager;
use crate::edits::{apply_edit_proposal, prepare_staged_edits, StageEditsResult, StagedEditsState};

/// Phased strategy that governs how validated edits reach the working tree.
///
/// Keeping the transactional model of a turn out of the FSM loop lets the same
/// pipeline run under different safety nets: direct writes for lightweight
/// sessions and checkpointed Git transactions for verified ones.
#[async_trait]
pub trait ApplyPolicy: Send + Sync {
    /// Validates `edits` in memory, retaining any earlier staged state supplied
    /// by the caller. No bytes are written to disk at this stage.
    async fn staging(
        &mut self,
        repo_root: &Path,
        context_manager: &ContextManager,
        edits: &[EditOperation],
        staged_state: Option<&StagedEditsState>,
    ) -> StageEditsResult;

    /// Writes the validated proposal to the working tree so it can be compiled
    /// and tested before being finalized.
    async fn apply_temporary(
        &mut self,
        repo_root: &Path,
        context_manager: &mut ContextManager,
        proposal: &EditProposal,
    ) -> anyhow::Result<Vec<String>>;

    /// Makes a verified change set permanent, returning an optional commit hash.
    async fn apply_final(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        summary: &str,
    ) -> anyhow::Result<Option<String>>;

    /// Reverts the working tree to the state before `apply_temporary`.
    /// Returns `true` when changes were rolled back.
    async fn rollback(&mut self, repo_root: &Path) -> anyhow::Result<bool>;
}

/// Writes edits straight to disk, without checkpoints or commits.
///
/// Intended for the naive workflow and local tests, where no transactional
/// safety net is required and changes cannot be recovered after the fact.
#[derive(Debug, Default, Clone)]
pub struct DirectApplyPolicy;

impl DirectApplyPolicy {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ApplyPolicy for DirectApplyPolicy {
    async fn staging(
        &mut self,
        repo_root: &Path,
        context_manager: &ContextManager,
        edits: &[EditOperation],
        staged_state: Option<&StagedEditsState>,
    ) -> StageEditsResult {
        prepare_staged_edits(repo_root, context_manager, edits, staged_state).await
    }

    async fn apply_temporary(
        &mut self,
        repo_root: &Path,
        context_manager: &mut ContextManager,
        proposal: &EditProposal,
    ) -> anyhow::Result<Vec<String>> {
        let changed_files = apply_edit_proposal(repo_root, context_manager, proposal).await?;
        Ok(changed_files)
    }

    async fn apply_final(
        &mut self,
        _repo_root: &Path,
        _changed_files: &[String],
        _summary: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    async fn rollback(&mut self, _repo_root: &Path) -> anyhow::Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplyPolicy, DirectApplyPolicy};
    use crate::context::ContextManager;
    use tauqe_protocol::{ContextAccess, EditOperation, EditProposal};

    #[tokio::test]
    async fn direct_policy_stages_applies_and_finalizes_without_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "one\n").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();

        let edits = vec![EditOperation::Replace {
            path: "a.rs".to_string(),
            old_text: "one\n".to_string(),
            new_text: "two\n".to_string(),
        }];

        let mut policy = DirectApplyPolicy::new();
        let staged = policy.staging(root, &cm, &edits, None).await;
        assert!(staged.is_success());

        let proposal = EditProposal {
            summary: "update a.rs".to_string(),
            edits: edits.clone(),
        };
        let changed = policy
            .apply_temporary(root, &mut cm, &proposal)
            .await
            .unwrap();
        assert_eq!(changed, vec!["a.rs".to_string()]);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

        let commit = policy
            .apply_final(root, &changed, "update a.rs")
            .await
            .unwrap();
        assert!(commit.is_none());
        assert!(!policy.rollback(root).await.unwrap());
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");
    }
}
