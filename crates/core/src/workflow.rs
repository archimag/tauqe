pub mod common;
pub(crate) mod discovery;
pub mod git;
pub(crate) mod lifecycle;
pub mod naive;
pub(crate) mod retry;
pub(crate) mod verification;

pub use git::GitEditWorkflow;
pub use lifecycle::WorkflowOptions;
pub use naive::NaiveEditWorkflow;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::ModelResult;

use tauqe_protocol::ModelRef;

use crate::context::ContextManager;
use crate::edits::EditProtocol;
use crate::history::HistoryManager;
use crate::providers::{LlmProvider, StreamEvent};

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
        provider: &dyn LlmProvider,
        model: &ModelRef,
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
        let app_config = crate::config::AppConfig {
            toolchain: toolchain_config.clone(),
            ..Default::default()
        };
        Self::create_workflow_from_app_config(name, &app_config)
    }

    /// Creates an EditWorkflow instance directly from the unified AppConfig.
    pub fn create_workflow_from_app_config(
        name: &str,
        config: &crate::config::AppConfig,
    ) -> Result<Box<dyn EditWorkflow>, String> {
        let options = WorkflowOptions::from_app_config(config);
        match name.trim().to_lowercase().as_str() {
            "git" => Ok(Box::new(GitEditWorkflow { options })),
            "naive" => Ok(Box::new(NaiveEditWorkflow { options })),
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
