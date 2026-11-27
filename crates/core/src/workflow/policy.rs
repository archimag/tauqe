use crate::config::project::ProjectConfig;

pub mod owned;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::blocking::StartedOperations;
use crate::edits::transaction::PolicyTransaction;
use crate::edits::EditOperation;
use crate::edits::EditProposal;
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::context::ContextManager;
use crate::edits::{
    apply_edit_proposal_with_config_and_operations, prepare_staged_edits_with_config,
    StageEditsResult, StagedEditsState,
};
use crate::stop::HardCancellation;

/// The observed result of an admission attempt, including cancellation that
/// arrived after irreversible work had already been admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizationStatus {
    NotAdmitted,
    Completed {
        result: Result<Option<String>, String>,
        cancelled: bool,
    },
}

impl FinalizationStatus {
    pub(crate) fn into_result(self) -> anyhow::Result<Option<String>> {
        match self {
            Self::NotAdmitted => anyhow::bail!("the turn was cancelled before finalization"),
            Self::Completed { result, .. } => result.map_err(anyhow::Error::msg),
        }
    }
}

/// Phased strategy that governs how validated edits reach the working tree.
///
/// Keeping the transactional model of a turn out of the FSM loop lets the same
/// pipeline run under different safety nets: direct writes for lightweight
/// sessions and checkpointed Git transactions for verified ones.
#[async_trait]
pub trait ApplyPolicy: Send + Sync {
    /// Validates `edits` in memory under the turn's frozen project configuration,
    /// retaining any earlier staged state supplied by the caller. No bytes are
    /// written to disk at this stage.
    async fn staging(
        &mut self,
        repo_root: &Path,
        project: &ProjectConfig,
        context_manager: &ContextManager,
        edits: &[EditOperation],
        staged_state: Option<&StagedEditsState>,
        cancel_rx: Option<&HardCancellation>,
    ) -> StageEditsResult;

    /// Writes the validated proposal to the working tree under the turn's frozen
    /// project configuration, so it can be compiled and tested before being
    /// finalized.
    async fn apply_temporary(
        &mut self,
        repo_root: &Path,
        project: &ProjectConfig,
        context_manager: &mut ContextManager,
        proposal: &EditProposal,
    ) -> anyhow::Result<Vec<String>> {
        self.apply_temporary_batches(repo_root, project, context_manager, &[&proposal.edits])
            .await
    }

    /// Writes the net result of accepted rounds in a single transaction.
    async fn apply_temporary_batches(
        &mut self,
        repo_root: &Path,
        project: &ProjectConfig,
        context_manager: &mut ContextManager,
        batches: &[&[EditOperation]],
    ) -> anyhow::Result<Vec<String>>;

    /// Retains publication work under the policy while a turn can be cancelled.
    /// The phased entry point remains available to policies being migrated.
    async fn apply_temporary_batches_owned(
        &mut self,
        repo_root: &Path,
        project: &ProjectConfig,
        context_manager: &mut ContextManager,
        batches: &[&[EditOperation]],
        _cancel: Option<&HardCancellation>,
    ) -> anyhow::Result<Vec<String>> {
        self.apply_temporary_batches(repo_root, project, context_manager, batches)
            .await
    }

    /// Makes a verified change set permanent, returning an optional commit hash.
    async fn apply_final(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        summary: &str,
    ) -> anyhow::Result<Option<String>>;

    /// Stop accepting publication and confirm every operation already admitted.
    /// This does not discard the policy's recovery state.
    async fn stop_and_wait(&mut self) -> anyhow::Result<()>;

    /// Admit finalization at the cancellation boundary, or retrieve the result
    /// of an already admitted attempt without starting it again.
    async fn finalize_resumable(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        summary: &str,
        cancel: Option<&HardCancellation>,
    ) -> anyhow::Result<FinalizationStatus>;

    /// Observe an admitted finalization without authorizing a new one.
    async fn finalization_result(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        summary: &str,
        cancel: Option<&HardCancellation>,
    ) -> anyhow::Result<FinalizationStatus>;

    /// Reverts the working tree to the state before `apply_temporary`.
    /// Returns `true` when changes were rolled back.
    async fn rollback(&mut self, repo_root: &Path) -> anyhow::Result<PolicyRollback>;
}

/// Whether the policy restored its checkpoint. The transaction also restores
/// backed-up paths outside Git coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyRollback {
    RestoredCheckpoint,
    NoCheckpoint,
}

impl dyn ApplyPolicy {
    /// Opens the owned file transaction of one turn. Standalone temporary
    /// writes remain policy primitives rather than implicit transactions.
    pub(crate) fn begin_transaction(self: Box<Self>, root: &Path) -> PolicyTransaction {
        PolicyTransaction::new(root, self)
    }
}

/// Writes edits straight to disk, without checkpoints or commits.
///
/// Intended for the naive workflow and local tests, where no transactional
/// safety net is required and changes cannot be recovered after the fact.
#[derive(Default, Clone)]
pub struct DirectApplyPolicy {
    operations: Arc<Mutex<StartedOperations>>,
    finalization: Arc<Mutex<Option<FinalizationStatus>>>,
    admitted: Arc<AtomicBool>,
}

impl DirectApplyPolicy {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ApplyPolicy for DirectApplyPolicy {
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
        cancel: Option<&HardCancellation>,
    ) -> anyhow::Result<Vec<String>> {
        let operations = self.operations.lock().await.clone();
        Ok(apply_edit_proposal_with_config_and_operations(
            repo_root,
            project,
            context_manager,
            batches,
            cancel,
            &operations,
        )
        .await?)
    }

    async fn apply_final(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        summary: &str,
    ) -> anyhow::Result<Option<String>> {
        self.finalize_resumable(repo_root, changed_files, summary, None)
            .await?
            .into_result()
    }

    async fn stop_and_wait(&mut self) -> anyhow::Result<()> {
        let operations = self.operations.lock().await.clone();
        operations.close();
        let summary = operations.wait().await;
        anyhow::ensure!(
            summary.unconfirmed == 0,
            "file publication could not be confirmed"
        );
        Ok(())
    }

    async fn finalize_resumable(
        &mut self,
        _repo_root: &Path,
        _changed_files: &[String],
        _summary: &str,
        cancel: Option<&HardCancellation>,
    ) -> anyhow::Result<FinalizationStatus> {
        let mut finalization = self.finalization.lock().await;
        if let Some(status) = finalization.as_ref() {
            return Ok(status.clone());
        }
        if cancel.is_some_and(HardCancellation::is_cancelled) {
            return Ok(FinalizationStatus::NotAdmitted);
        }
        self.admitted.store(true, Ordering::SeqCst);
        let operations = self.operations.lock().await.clone();
        operations.close();
        // Do not hold the finalization slot while awaiting publication.
        drop(finalization);
        let summary = operations.wait().await;
        anyhow::ensure!(
            summary.unconfirmed == 0,
            "file publication could not be confirmed"
        );
        finalization = self.finalization.lock().await;
        let status = FinalizationStatus::Completed {
            result: Ok(None),
            cancelled: cancel.is_some_and(HardCancellation::is_cancelled),
        };
        *finalization = Some(status.clone());
        Ok(status)
    }

    async fn finalization_result(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        summary: &str,
        cancel: Option<&HardCancellation>,
    ) -> anyhow::Result<FinalizationStatus> {
        if !self.admitted.load(Ordering::SeqCst) {
            return Ok(FinalizationStatus::NotAdmitted);
        }
        self.finalize_resumable(repo_root, changed_files, summary, cancel)
            .await
    }

    async fn rollback(&mut self, _repo_root: &Path) -> anyhow::Result<PolicyRollback> {
        self.stop_and_wait().await?;
        *self.operations.lock().await = StartedOperations::default();
        *self.finalization.lock().await = None;
        self.admitted.store(false, Ordering::SeqCst);
        Ok(PolicyRollback::NoCheckpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplyPolicy, DirectApplyPolicy, PolicyRollback};
    use crate::config::project::ProjectConfig;
    use crate::context::ContextAccess;
    use crate::context::ContextManager;
    use crate::edits::EditOperation;
    use crate::edits::EditProposal;
    use crate::edits::EditReplace;

    #[tokio::test]
    async fn direct_policy_stages_applies_and_finalizes_without_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "one\n").unwrap();
        let mut cm = ContextManager::new(root.to_path_buf());
        cm.add_file("a.rs", ContextAccess::Editable).unwrap();

        let edits = vec![EditOperation::Replace(EditReplace {
            path: "a.rs".to_string(),
            old_text: "one\n".to_string(),
            new_text: "two\n".to_string(),
        })];

        let mut policy = DirectApplyPolicy::new();
        let project = ProjectConfig::default();
        let staged = policy
            .staging(root, &project, &cm, &edits, None, None)
            .await;
        assert!(staged.is_success());

        let proposal = EditProposal {
            summary: "update a.rs".to_string(),
            edits: edits.clone(),
        };
        let changed = policy
            .apply_temporary(root, &project, &mut cm, &proposal)
            .await
            .unwrap();
        assert_eq!(changed, vec!["a.rs".to_string()]);
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");

        let commit = policy
            .apply_final(root, &changed, "update a.rs")
            .await
            .unwrap();
        assert!(commit.is_none());
        assert_eq!(
            policy.rollback(root).await.unwrap(),
            PolicyRollback::NoCheckpoint
        );
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "two\n");
    }
}
