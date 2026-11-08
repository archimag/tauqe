pub mod batch;
pub mod common;
pub mod execution;
pub mod fsm;
pub mod git_policy;
pub mod implementation;
pub mod loops;
pub mod policy;
pub mod research;
pub(crate) mod retry;
pub mod task;
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

use tauqe_protocol::{ModelResult, PlanRejection, WorkflowKind};

/// Result of a completed workflow turn.
#[derive(Debug, Clone)]
pub struct WorkflowExecutionResult {
    pub result: ModelResult,
    pub assistant_text: String,
    /// Whether the developer cancelled the turn before it completed.
    ///
    /// Cancellation is a valid outcome rather than a failure, so the turn still
    /// returns `Ok`: the flag is what tells an interrupted turn apart from an
    /// answer that carried no text, and a supervisor maps it onto its cancelled
    /// terminal status instead of reporting a failed step.
    pub is_cancelled: bool,
    /// Whether the model marked its answer as a validated conclusion.
    ///
    /// An answer without edits is a completed step only when the model declared
    /// it with an explicit terminal success signal; otherwise it is an
    /// unverified reply. The FSM classifies a step by exactly that distinction,
    /// so the flag travels beside the result instead of being re-derived from
    /// the assistant text by whoever reads it.
    pub has_terminal_success: bool,
    /// Coder's block report when the turn ended with a semantic `<blocked>`
    /// signal. Preserved so the plan supervisor can move the step to `Blocked`.
    pub blocked: Option<String>,
    /// Diagnostic of the edit-protocol violation that ended the turn without an
    /// applied change set.
    ///
    /// The fact travels beside its diagnostic, exactly like `blocked` and
    /// `abort`: it exists only when the model violated the edit protocol, so a
    /// consumer classifies the ending by presence and never by the wording of
    /// the error the turn reports.
    pub protocol_failure: Option<String>,
    /// Coder's skip report when the turn ended with a semantic `<skip>`
    /// signal. Preserved so a review supervisor can mark the finding as
    /// skipped instead of treating the empty turn as an unverified reply.
    pub skip_reason: Option<String>,
    /// Coder's abort report when the turn ended with a semantic `<abort>`
    /// signal. Preserved so a supervisor can stop its queue without architect
    /// escalation while the developer receives the exact action to perform.
    pub abort: Option<fsm::AbortReport>,
    /// Whether the turn applied a plan `save` action that created or fully
    /// replaced a plan. Propagated to the `plan/updated` event so clients can
    /// focus a freshly created plan instead of an incremental update.
    pub plan_saved: bool,
    /// Plan mutations the storage engine refused during the turn.
    /// A rejection never fails the turn, but it is carried to the caller so the
    /// daemon records it instead of leaving it in the engine journal only. Each
    /// entry is classified where the refusal was produced, so a surface renders
    /// the wording and reads the category from the typed field.
    pub plan_errors: Vec<PlanRejection>,
    /// Status repairs the plan barrier performed on an applied payload.
    ///
    /// A normalization is a real event of the plan lifecycle, so it travels to
    /// the caller instead of staying a journal trace: the daemon forwards it to
    /// the client beside the typed rejections, and the turn stream and the
    /// session history carry the same notice.
    pub plan_warnings: Vec<String>,
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
        assert!(!WorkflowFactory::is_valid("toolchain"));
        assert!(!WorkflowFactory::is_valid("unknown_wf"));
    }
}
