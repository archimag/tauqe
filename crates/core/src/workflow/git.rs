use std::path::Path;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::ModelRef;

use super::lifecycle::{execute_workflow_lifecycle, WorkflowOptions, WorkflowTransaction};
use super::{EditWorkflow, WorkflowExecutionResult};
use crate::context::ContextManager;
use crate::edits::EditProtocol;
use crate::git::{
    create_ai_commit, create_checkpoint, create_step_commit, finalize_ai_commit,
    restore_checkpoint, CheckpointInfo,
};
use crate::history::HistoryManager;
use crate::providers::{LlmProvider, StreamEvent};

/// Git-native transaction handler managing checkpoints and atomic squash commits.
#[derive(Default)]
struct GitTransaction {
    checkpoint: Option<CheckpointInfo>,
}

impl WorkflowTransaction for GitTransaction {
    fn on_before_apply(&mut self, repo_root: &Path) -> anyhow::Result<()> {
        self.checkpoint = create_checkpoint(repo_root).ok();
        Ok(())
    }

    fn on_initial_applied(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
    ) -> anyhow::Result<()> {
        if self.checkpoint.is_some() {
            let _ = create_step_commit(repo_root, changed_files, "initial attempt");
        }
        Ok(())
    }

    fn on_step_applied(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        attempt: usize,
    ) -> anyhow::Result<()> {
        if self.checkpoint.is_some() {
            let _ = create_step_commit(
                repo_root,
                changed_files,
                &format!("retry attempt {}", attempt),
            );
        }
        Ok(())
    }

    fn on_success(
        &mut self,
        repo_root: &Path,
        changed_files: &[String],
        summary: &str,
    ) -> anyhow::Result<Option<String>> {
        let commit_hash = if let Some(cp) = &self.checkpoint {
            match finalize_ai_commit(repo_root, cp, changed_files, summary) {
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
        Ok(commit_hash)
    }

    fn on_rollback(&mut self, repo_root: &Path) -> anyhow::Result<bool> {
        if let Some(cp) = &self.checkpoint {
            let _ = restore_checkpoint(repo_root, cp);
            return Ok(true);
        }
        Ok(false)
    }
}

/// Git-native workflow providing pre-edit checkpoints and atomic AI commits.
#[derive(Debug, Clone, Default)]
pub struct GitEditWorkflow {
    pub options: WorkflowOptions,
}

impl GitEditWorkflow {
    pub fn new(max_retries: Option<usize>) -> Self {
        let mut options = WorkflowOptions::default();
        if let Some(r) = max_retries {
            options.max_retries = r;
        }
        Self { options }
    }

    pub fn with_discovery(
        mut self,
        max_discovery_rounds: Option<usize>,
        max_auto_files_per_round: Option<usize>,
    ) -> Self {
        if let Some(rounds) = max_discovery_rounds {
            self.options.max_discovery_rounds = rounds;
        }
        self.options.max_auto_files_per_round = max_auto_files_per_round;
        self
    }

    pub fn with_history_budget(
        mut self,
        budget_tokens: Option<u64>,
        tail_turns: Option<usize>,
    ) -> Self {
        if let Some(b) = budget_tokens {
            self.options.history_budget_tokens = b;
        }
        if let Some(t) = tail_turns {
            self.options.history_tail_turns = t;
        }
        self
    }

    pub fn with_repomap_budget(mut self, budget: Option<usize>) -> Self {
        if let Some(b) = budget {
            self.options.repomap_token_budget = b;
        }
        self
    }
}

#[async_trait]
impl EditWorkflow for GitEditWorkflow {
    fn name(&self) -> &'static str {
        "git"
    }

    async fn execute(
        &self,
        prompt: &str,
        provider: &dyn LlmProvider,
        model: &ModelRef,
        context_manager: &mut ContextManager,
        history_manager: &mut HistoryManager,
        protocol: &dyn EditProtocol,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<WorkflowExecutionResult> {
        execute_workflow_lifecycle(
            self.name(),
            &self.options,
            GitTransaction::default(),
            prompt,
            provider,
            model,
            context_manager,
            history_manager,
            protocol,
            stream_tx,
            cancel_rx,
        )
        .await
    }
}
