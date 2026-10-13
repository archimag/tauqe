pub mod batch;
pub mod common;
pub(crate) mod discovery;
pub mod execution;
pub mod fsm;
pub mod git_policy;
pub mod loops;
pub mod policy;
pub mod task;
pub(crate) mod retry;
pub(crate) mod verification;

#[cfg(test)]
mod policy_tests;

pub use batch::{
    BatchExecution, BatchReport, BatchStep, BatchStepExecutor, BatchStepOutcome, BatchStepReport,
    BatchStepStatus, BatchStop, BatchUsage,
};
pub use execution::{ExecutionMode, SingleExecution, WorkflowOptions};
pub use fsm::{run_fsm_turn, FsmTurnOutcome};
pub use git_policy::GitTransactionPolicy;
pub use loops::{AnswerEditLoop, EditLoop};
pub use policy::{ApplyPolicy, DirectApplyPolicy};
pub use task::{
    DiscoveryStep, HealingStep, StagingStep, Task, TaskEnv, TaskStep, TurnCycle, VerificationStep,
};

use tauqe_protocol::ModelResult;

/// Result of a completed workflow turn.
#[derive(Debug, Clone)]
pub struct WorkflowExecutionResult {
    pub result: ModelResult,
    pub assistant_text: String,
    /// Coder's block report when the turn ended with a semantic `<blocked>`
    /// signal. Preserved so the plan supervisor can move the step to `Blocked`.
    pub blocked: Option<String>,
}

/// Factory responsible for reporting and validating the supported workflow names.
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
    fn test_workflow_factory_validation() {
        assert!(WorkflowFactory::is_valid("git"));
        assert!(WorkflowFactory::is_valid("naive"));
        assert!(!WorkflowFactory::is_valid("toolchain"));
        assert!(!WorkflowFactory::is_valid("unknown_wf"));
    }
}
