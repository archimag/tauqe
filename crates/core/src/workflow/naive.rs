use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::ModelRef;

use super::lifecycle::{execute_workflow_lifecycle, WorkflowOptions, WorkflowTransaction};
use super::{EditWorkflow, WorkflowExecutionResult};
use crate::context::ContextManager;
use crate::edits::EditProtocol;
use crate::history::HistoryManager;
use crate::providers::{LlmProvider, StreamEvent};

/// Default no-op transaction handler for non-versioned/naive workflow executions.
#[derive(Default)]
struct NaiveTransaction;

impl WorkflowTransaction for NaiveTransaction {}

/// Non-versioned filesystem workflow applying edits directly without checkpoints or commits.
#[derive(Debug, Clone, Default)]
pub struct NaiveEditWorkflow {
    pub options: WorkflowOptions,
}

impl NaiveEditWorkflow {
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
        max_files: Option<usize>,
    ) -> Self {
        if let Some(rounds) = max_discovery_rounds {
            self.options.max_discovery_rounds = rounds;
        }
        if let Some(files) = max_files {
            self.options.max_files = files;
        }
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
impl EditWorkflow for NaiveEditWorkflow {
    fn name(&self) -> &'static str {
        "naive"
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
            NaiveTransaction,
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
