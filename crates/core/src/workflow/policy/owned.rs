//! Retained file transaction boundary shared by execution and its supervisor.

use super::{ApplyPolicy, DirectApplyPolicy};
use crate::blocking::StartedOperations;
use crate::config::project::ProjectConfig;
use crate::context::ContextManager;
use crate::edits::transaction::{PolicySettlement, PolicyTransaction, RollbackOutcome};
use crate::edits::{EditOperation, VerificationVerdict};
use crate::stop::HardCancellation;
use crate::workflow::GitTransactionPolicy;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// The terminal file facts, including a late cancellation during an admitted commit.
#[derive(Debug, Clone)]
pub enum Settlement {
    NoChanges {
        cancelled: bool,
    },
    Finalized {
        edit: crate::edits::EditOutcome,
        cancelled: bool,
    },
    RolledBack(RollbackOutcome),
}

struct State {
    transaction: PolicyTransaction,
    terminal: Option<Settlement>,
    finalizing: Option<(String, VerificationVerdict)>,
}

/// Clones retain the same transaction; no background worker or command queue is started.
#[derive(Clone)]
pub struct OwnedApplyPolicy {
    root: PathBuf,
    cancel: HardCancellation,
    state: Arc<Mutex<State>>,
}

impl OwnedApplyPolicy {
    /// Bind a policy once to the repository and its cancellation boundary.
    pub fn bind(policy: Box<dyn ApplyPolicy>, root: &Path, cancel: HardCancellation) -> Self {
        Self {
            root: root.to_path_buf(),
            cancel,
            state: Arc::new(Mutex::new(State {
                transaction: policy.begin_transaction(root),
                terminal: None,
                finalizing: None,
            })),
        }
    }

    pub fn direct(root: &Path, cancel: HardCancellation) -> Self {
        Self::bind(Box::new(DirectApplyPolicy::new()), root, cancel)
    }

    pub fn git(root: &Path, cancel: HardCancellation) -> Self {
        Self::bind(Box::new(GitTransactionPolicy::new()), root, cancel)
    }

    pub async fn apply(
        &self,
        project: &ProjectConfig,
        context: &mut ContextManager,
        touched: &[String],
        batches: &[&[EditOperation]],
    ) -> Result<Vec<String>> {
        let mut state = self.state.lock().await;
        if self.cancel.is_cancelled() {
            bail!("file application was cancelled");
        }
        if state.terminal.is_some() {
            bail!("file transaction has already been settled");
        }
        state
            .transaction
            .apply(project, context, touched, batches, Some(&self.cancel))
            .await
    }

    /// Admit a command with retained process completion and pre-command evidence.
    pub async fn begin_command(&self) -> Result<StartedOperations> {
        let state = self.state.lock().await;
        if self.cancel.is_cancelled() || state.terminal.is_some() {
            bail!("file transaction is closed to new work");
        }
        state.transaction.begin_command().await
    }

    pub async fn complete_command(&self) -> Result<Vec<String>> {
        self.state.lock().await.transaction.complete_command().await
    }

    pub async fn changed_files(&self) -> Vec<String> {
        self.state.lock().await.transaction.changed_files()
    }

    pub async fn original_bytes(&self) -> HashMap<String, Option<Vec<u8>>> {
        self.state.lock().await.transaction.original_bytes()
    }

    pub async fn finalize(
        &self,
        summary: &str,
        verification: VerificationVerdict,
    ) -> Result<Settlement> {
        let mut state = self.state.lock().await;
        if let Some(result) = &state.terminal {
            return Ok(result.clone());
        }
        if let Some((previous, verdict)) = &state.finalizing {
            if previous != summary || *verdict != verification {
                bail!("finalization arguments differ from the admitted attempt");
            }
        } else {
            state.finalizing = Some((summary.to_owned(), verification));
        }
        let result = state
            .transaction
            .finalize_resumable(summary, verification, Some(&self.cancel))
            .await?;
        let Some((result, cancelled)) = result else {
            state.finalizing = None;
            bail!("finalization was cancelled before admission");
        };
        let result = match result {
            PolicySettlement::NoChanges => Settlement::NoChanges { cancelled },
            PolicySettlement::Finalized(edit) => Settlement::Finalized { edit, cancelled },
            PolicySettlement::RolledBack(_) => bail!("unexpected rollback during finalization"),
        };
        state.terminal = Some(result.clone());
        Ok(result)
    }

    /// Close useful work and restore only after all admitted operations stop.
    pub async fn finish_interrupted(&self, context: &mut ContextManager) -> Result<Settlement> {
        let mut state = self.state.lock().await;
        if let Some(result) = &state.terminal {
            return Ok(result.clone());
        }
        if let Some((summary, verification)) = state.finalizing.clone() {
            if let Some((settlement, cancelled)) = state
                .transaction
                .finalization_result(&summary, verification, Some(&self.cancel))
                .await?
            {
                let result = match settlement {
                    PolicySettlement::NoChanges => Settlement::NoChanges { cancelled },
                    PolicySettlement::Finalized(edit) => Settlement::Finalized { edit, cancelled },
                    PolicySettlement::RolledBack(_) => {
                        bail!("unexpected rollback during finalization")
                    }
                };
                state.terminal = Some(result.clone());
                return Ok(result);
            }
            state.finalizing = None;
        }
        let result = state.transaction.rollback(context).await?;
        let PolicySettlement::RolledBack(rollback) = result else {
            bail!("unexpected finalization during recovery");
        };
        let result = Settlement::RolledBack(rollback);
        state.terminal = Some(result.clone());
        Ok(result)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
