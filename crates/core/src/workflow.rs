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

use crate::models::ReasoningEffort;
use crate::workflow::research::ResearchLimits;

/// Execution options and token budgets for edit workflows.
#[derive(Debug, Clone)]
pub struct WorkflowOptions {
    pub(crate) max_turn_rounds: usize,
    pub(crate) token_budget: usize,
    pub(crate) repomap_token_budget: usize,
    /// Whether the eager global symbol map is injected into every proposal.
    ///
    /// When disabled, the prompt carries the compact directory outline instead,
    /// so the model still knows where subsystems live without a full map.
    pub(crate) include_repo_map: bool,
    /// Operational caps the Research phase applies to one round.
    pub(crate) research_limits: ResearchLimits,
    app_config: Option<crate::config::AppConfig>,
    /// Reasoning effort applied to the current turn, resolved from the active
    /// semantic role. Updated on `auto_level_up` escalation so a
    /// Coder→Architect switch inherits the deeper Architect effort.
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// Provider session identifier that binds this turn to a warm prompt-prefix
    /// cache on the provider side.
    ///
    /// The caller owns the affinity scope: an interactive dialogue, a plan
    /// execution, or a review audit each pass their own identifier. `None`
    /// disables affinity and the provider serves every round statelessly.
    pub session_id: Option<String>,
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        Self {
            max_turn_rounds: crate::config::DEFAULT_MAX_TURN_ROUNDS,
            token_budget: crate::config::DEFAULT_TOKEN_BUDGET,
            repomap_token_budget: crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
            include_repo_map: true,
            research_limits: ResearchLimits::default(),
            app_config: None,
            reasoning_effort: None,
            session_id: None,
        }
    }
}

impl WorkflowOptions {
    pub fn max_turn_rounds(&self) -> usize {
        self.max_turn_rounds
    }
    pub fn token_budget(&self) -> usize {
        self.token_budget
    }

    /// Constructs workflow options from the unified application configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        Self {
            max_turn_rounds: config.repl.max_turn_rounds,
            token_budget: config.context.token_budget,
            repomap_token_budget: config.context.repomap_token_budget,
            include_repo_map: config.context.include_repo_map,
            research_limits: ResearchLimits::from_context_config(&config.context),
            app_config: Some(config.clone()),
            reasoning_effort: None,
            session_id: None,
        }
    }

    /// Returns the patch strategy declared by the governing project configuration.
    ///
    /// The strategy decides how a change set addresses the lines it replaces, so
    /// every phase that describes a change set — prompt rendering and
    /// patch-retry diagnostics alike — reads it from this single accessor
    /// instead of deriving its own copy from the configuration.
    pub(crate) fn patch_strategy(&self) -> PatchStrategy {
        self.app_config
            .as_ref()
            .map(|config| config.repl.patch_strategy)
            .unwrap_or_default()
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
