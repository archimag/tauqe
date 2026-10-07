pub mod common;
pub mod git;
pub mod naive;

pub use git::GitEditWorkflow;
pub use naive::NaiveEditWorkflow;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::ModelResult;

use crate::context::ContextManager;
use crate::edits::EditProtocol;
use crate::history::HistoryManager;
use crate::model::gateway::StreamEvent;
use crate::model::openrouter::OpenRouterClient;

#[derive(Debug, Clone)]
pub struct WorkflowExecutionResult {
    pub result: ModelResult,
    pub assistant_text: String,
}

#[async_trait]
pub trait EditWorkflow: Send + Sync {
    fn name(&self) -> &'static str;

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        prompt: &str,
        client: &OpenRouterClient,
        model_name: &str,
        context_manager: &mut ContextManager,
        history_manager: &mut HistoryManager,
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
        vec!["git".to_string(), "naive".to_string()]
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
        Self::create_workflow_with_edit_config(name, toolchain_config, None)
    }

    /// Creates an EditWorkflow instance with optional EditConfig overrides.
    pub fn create_workflow_with_edit_config(
        name: &str,
        toolchain_config: &crate::config::ToolchainConfig,
        edit_config: Option<&crate::config::EditConfig>,
    ) -> Result<Box<dyn EditWorkflow>, String> {
        Self::create_workflow_full(name, toolchain_config, edit_config, None)
    }

    /// Creates an EditWorkflow instance with full configuration support.
    pub fn create_workflow_full(
        name: &str,
        toolchain_config: &crate::config::ToolchainConfig,
        edit_config: Option<&crate::config::EditConfig>,
        context_config: Option<&crate::config::ContextConfig>,
    ) -> Result<Box<dyn EditWorkflow>, String> {
        let max_retries = toolchain_config
            .max_retries
            .or_else(|| edit_config.map(|e| e.max_retries));
        let max_discovery_rounds = context_config.map(|c| c.max_discovery_rounds);
        let max_auto_files_per_round = context_config.and_then(|c| c.max_auto_files_per_round);

        match name.trim().to_lowercase().as_str() {
            "git" => Ok(Box::new(
                GitEditWorkflow::new(max_retries)
                    .with_discovery(max_discovery_rounds, max_auto_files_per_round),
            )),
            "naive" => Ok(Box::new(
                NaiveEditWorkflow::new(max_retries)
                    .with_discovery(max_discovery_rounds, max_auto_files_per_round),
            )),
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
        assert!(workflows.contains(&"git".to_string()));
        assert!(workflows.contains(&"naive".to_string()));
        assert!(!workflows.contains(&"toolchain".to_string()));
    }

    #[test]
    fn test_workflow_factory_validation_and_creation() {
        assert!(WorkflowFactory::is_valid("git"));
        assert!(WorkflowFactory::is_valid("naive"));
        assert!(!WorkflowFactory::is_valid("toolchain"));
        assert!(!WorkflowFactory::is_valid("unknown_wf"));

        let cfg = crate::config::ToolchainConfig::default();
        let wf_git = WorkflowFactory::create_workflow("git", &cfg).unwrap();
        assert_eq!(wf_git.name(), "git");

        let wf_naive = WorkflowFactory::create_workflow("naive", &cfg).unwrap();
        assert_eq!(wf_naive.name(), "naive");

        let err = WorkflowFactory::create_workflow("invalid", &cfg);
        assert!(err.is_err());
    }
}
