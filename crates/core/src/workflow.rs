pub mod batch;
pub mod common;
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

use crate::workflow::research::ResearchLimits;
use tauqe_protocol::{
    ModelRef, ModelResult, PatchStrategy, PlanRejection, PromptLayoutKind, ReasoningEffort,
    WorkflowKind,
};

/// Execution options and token budgets for edit workflows.
#[derive(Debug, Clone)]
pub struct WorkflowOptions {
    pub max_retries: usize,
    pub max_research_rounds: usize,
    pub token_budget: usize,
    pub history_budget_tokens: u64,
    pub history_tail_turns: usize,
    pub repomap_token_budget: usize,
    /// Whether the eager global symbol map is injected into every proposal.
    ///
    /// When disabled, the prompt carries the compact directory outline instead,
    /// so the model still knows where subsystems live without a full map.
    pub include_repo_map: bool,
    /// Operational caps the Research phase applies to one round.
    pub research_limits: ResearchLimits,
    /// Maximum characters of one project command stream reported back to the model.
    pub max_feedback_chars: usize,
    /// Maximum characters of a turn scratchpad block before safe truncation.
    pub max_scratchpad_chars: usize,
    pub history_model: ModelRef,
    pub app_config: Option<crate::config::AppConfig>,
    /// Reasoning effort applied to the current turn, resolved from the active
    /// semantic role. Updated on `auto_level_up` escalation so a
    /// Coder→Architect switch inherits the deeper Architect effort.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Provider session identifier that binds this turn to a warm prompt-prefix
    /// cache on the provider side.
    ///
    /// The caller owns the affinity scope: an interactive dialogue, a plan
    /// execution, or a review audit each pass their own identifier. `None`
    /// disables affinity and the provider serves every round statelessly.
    pub session_id: Option<String>,
    /// Review session the dialogue is anchored on, if any.
    ///
    /// A dialogue anchored on a review session may record a decision for one of
    /// its findings. The identity travels with the turn so a decoded decision is
    /// persisted against the session the dialogue discusses, never against an
    /// ambient one.
    pub review_session_id: Option<String>,
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        Self {
            max_retries: 3,
            max_research_rounds: crate::config::DEFAULT_MAX_RESEARCH_ROUNDS,
            token_budget: crate::config::DEFAULT_TOKEN_BUDGET,
            history_budget_tokens: crate::history::DEFAULT_HISTORY_BUDGET_TOKENS,
            history_tail_turns: crate::history::DEFAULT_TAIL_TURNS_COUNT,
            repomap_token_budget: crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
            include_repo_map: true,
            research_limits: ResearchLimits::default(),
            max_feedback_chars: crate::config::DEFAULT_MAX_FEEDBACK_CHARS,
            max_scratchpad_chars: crate::config::DEFAULT_MAX_SCRATCHPAD_CHARS,
            history_model: ModelRef::openrouter(crate::config::BUILTIN_DEFAULT_SCRIBE_MODEL),
            app_config: None,
            reasoning_effort: None,
            session_id: None,
            review_session_id: None,
        }
    }
}

impl WorkflowOptions {
    /// Constructs workflow options from the unified application configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        let max_retries = config.repl.max_retries;
        Self {
            max_retries,
            max_research_rounds: config.context.max_research_rounds,
            token_budget: config.context.token_budget,
            history_budget_tokens: config.history.budget_tokens,
            history_tail_turns: config.history.tail_turns,
            repomap_token_budget: config.context.repomap_token_budget,
            include_repo_map: config.context.include_repo_map,
            research_limits: ResearchLimits::from_context_config(&config.context),
            max_feedback_chars: config.context.max_feedback_chars,
            max_scratchpad_chars: config.context.max_scratchpad_chars,
            history_model: config.history_model(),
            app_config: Some(config.clone()),
            reasoning_effort: None,
            session_id: None,
            review_session_id: None,
        }
    }

    /// Returns the patch strategy declared by the governing project configuration.
    ///
    /// The strategy decides how a change set addresses the lines it replaces, so
    /// every phase that describes a change set — prompt rendering and
    /// patch-retry diagnostics alike — reads it from this single accessor
    /// instead of deriving its own copy from the configuration.
    pub fn patch_strategy(&self) -> PatchStrategy {
        self.app_config
            .as_ref()
            .map(|config| config.repl.patch_strategy)
            .unwrap_or_default()
    }

    /// Returns the prompt layout policy that composes this turn's messages.
    ///
    /// The declared `[repl]` key of the governing project configuration wins
    /// for every turn; without one the caller's typed argument decides: a turn
    /// that carries a growing session history builds the chronological
    /// transcript, while a turn that carries none keeps the semantic block
    /// order. Options that carry no governing configuration keep the semantic
    /// policy, which every contour renders correctly.
    pub fn prompt_layout(&self, carries_session_history: bool) -> PromptLayoutKind {
        self.app_config
            .as_ref()
            .map(|config| config.resolve_prompt_layout(carries_session_history))
            .unwrap_or(PromptLayoutKind::Semantic)
    }
}

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
