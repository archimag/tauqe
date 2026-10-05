pub mod common;
pub mod git;
pub mod naive;
pub mod toolchain;

pub use git::GitEditWorkflow;
pub use naive::NaiveEditWorkflow;
pub use toolchain::ToolchainEditWorkflow;

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
    fn name(&self) -> &'static str;

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

/// Factory responsible for discovering, validating, and creating EditWorkflow instances.
pub struct WorkflowFactory;

impl WorkflowFactory {
    /// Returns the list of all supported workflow names.
    pub fn available_workflows() -> Vec<String> {
        vec![
            "toolchain".to_string(),
            "git".to_string(),
            "naive".to_string(),
        ]
    }

    /// Validates whether a workflow name is supported.
    pub fn is_valid(name: &str) -> bool {
        Self::available_workflows()
            .iter()
            .any(|w| w.eq_ignore_ascii_case(name.trim()))
    }

    /// Creates an EditWorkflow instance from its name and toolchain configuration.
    pub fn create_workflow(
        name: &str,
        toolchain_config: &crate::config::ToolchainConfig,
    ) -> Result<Box<dyn EditWorkflow>, String> {
        match name.trim().to_lowercase().as_str() {
            "toolchain" => Ok(Box::new(ToolchainEditWorkflow::new(
                toolchain_config.check_command.clone(),
                toolchain_config.max_retries,
                toolchain_config.auto_heal,
            ))),
            "git" => Ok(Box::new(GitEditWorkflow)),
            "naive" => Ok(Box::new(NaiveEditWorkflow)),
            other => Err(format!(
                "Unknown workflow '{}'. Available workflows: {}",
                other,
                Self::available_workflows().join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_factory_available() {
        let workflows = WorkflowFactory::available_workflows();
        assert!(workflows.contains(&"toolchain".to_string()));
        assert!(workflows.contains(&"git".to_string()));
        assert!(workflows.contains(&"naive".to_string()));
    }

    #[test]
    fn test_workflow_factory_validation_and_creation() {
        assert!(WorkflowFactory::is_valid("toolchain"));
        assert!(WorkflowFactory::is_valid("git"));
        assert!(WorkflowFactory::is_valid("naive"));
        assert!(!WorkflowFactory::is_valid("unknown_wf"));

        let cfg = crate::config::ToolchainConfig::default();
        let wf = WorkflowFactory::create_workflow("toolchain", &cfg).unwrap();
        assert_eq!(wf.name(), "toolchain");

        let wf_git = WorkflowFactory::create_workflow("git", &cfg).unwrap();
        assert_eq!(wf_git.name(), "git");

        let err = WorkflowFactory::create_workflow("invalid", &cfg);
        assert!(err.is_err());
    }
}
