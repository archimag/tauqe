mod kind;
pub mod turn;
pub use kind::{EditProtocolKind, PatchStrategy, WorkflowKind};
pub mod batch;
pub mod git_policy;
pub mod implementation;
pub mod policy;
pub mod research;
pub(crate) mod retry;
pub(crate) mod verification;

#[cfg(test)]
mod policy_tests;

pub use batch::{
    BatchExecution, BatchReport, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStepReport,
    BatchStepStatus, BatchStop, BatchUsage,
};
pub use git_policy::GitTransactionPolicy;
pub use policy::{ApplyPolicy, DirectApplyPolicy};

/// Round budget for bounded workflows.
#[derive(Debug, Clone)]
pub struct WorkflowOptions {
    max_turn_rounds: usize,
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        Self {
            max_turn_rounds: crate::config::DEFAULT_MAX_TURN_ROUNDS,
        }
    }
}

impl WorkflowOptions {
    pub fn max_turn_rounds(&self) -> usize {
        self.max_turn_rounds
    }

    /// Constructs workflow options from the unified application configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        Self {
            max_turn_rounds: config.repl.max_turn_rounds,
        }
    }
}

/// Factory responsible for reporting and validating the supported workflow names.
pub struct WorkflowFactory;

impl WorkflowFactory {
    /// Returns the list of all supported workflow kinds.
    pub fn available_workflows() -> Vec<WorkflowKind> {
        vec![WorkflowKind::Git, WorkflowKind::Naive]
    }

    /// Validates whether a workflow name is supported.
    #[cfg(test)]
    fn is_valid(name: &str) -> bool {
        name.parse::<WorkflowKind>().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_factory_available() {
        let workflows = WorkflowFactory::available_workflows();
        assert!(workflows.contains(&WorkflowKind::Git));
        assert!(workflows.contains(&WorkflowKind::Naive));
    }

    #[test]
    fn test_workflow_factory_validation() {
        assert!(WorkflowFactory::is_valid("git"));
        assert!(WorkflowFactory::is_valid("naive"));
        assert!(!WorkflowFactory::is_valid("toolchain"));
        assert!(!WorkflowFactory::is_valid("unknown_wf"));
    }
}
