pub mod naive;

pub use naive::NaiveEditWorkflow;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::ModelResult;

use crate::context::ContextManager;
use crate::edits::EditProtocol;
use crate::model::gateway::{ChatMessage, StreamEvent};
use crate::model::openrouter::OpenRouterClient;

#[derive(Debug, Clone)]
pub struct WorkflowExecutionResult {
    pub result: ModelResult,
    pub assistant_text: String,
    pub session_history_update: Option<(ChatMessage, ChatMessage)>,
}

#[async_trait]
pub trait EditWorkflow: Send + Sync {
    async fn execute(
        &self,
        prompt: &str,
        client: &OpenRouterClient,
        model_name: &str,
        context_manager: &mut ContextManager,
        session_history: &[ChatMessage],
        protocol: &dyn EditProtocol,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<WorkflowExecutionResult>;
}
