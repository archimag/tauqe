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
pub use task::{Task, TaskEnv};

use tauqe_protocol::{ModelResult, WorkflowKind};

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
    /// Returns the list of all supported workflow kinds.
    pub fn available_workflows() -> Vec<WorkflowKind> {
        vec![WorkflowKind::Git, WorkflowKind::Naive]
    }

    /// Validates whether a workflow name is supported.
    pub fn is_valid(name: &str) -> bool {
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
        assert!(WorkflowFactory::is_valid("direct"));
        assert!(WorkflowFactory::is_valid("single"));
        assert!(!WorkflowFactory::is_valid("toolchain"));
        assert!(!WorkflowFactory::is_valid("unknown_wf"));
    }
}
