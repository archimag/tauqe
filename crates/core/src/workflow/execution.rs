//! Execution modes governing how bounded turns are orchestrated.
//!
//! An [`ExecutionMode`] is the top orchestration layer of the turn pipeline: it
//! decides how model turns are composed, supervised, and accounted for, while
//! the lower FSM layers execute the individual turns. Keeping this decision out
//! of the FSM cycle and the apply policy lets single-turn and batch scenarios
//! evolve independently.
//!
//! [`SingleExecution`] runs exactly one bounded turn by composing the
//! single-turn workflow lifecycle, which performs role escalation
//! (`auto_level_up`) and cost accounting once per turn.

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::{CoderTier, ModelRef, ModelResult, ModelRole, ReasoningEffort};

use crate::context::ContextManager;
use crate::history::HistoryManager;
use crate::providers::{LlmProvider, StreamEvent};
use crate::prompt::protocol::PromptProtocol;
use crate::workflow::policy::ApplyPolicy;

use super::WorkflowExecutionResult;
use crate::workflow::task::{Task, TaskEnv};
use crate::workflow::loops::AnswerEditLoop;

/// Execution options and token budgets for edit workflows.
#[derive(Debug, Clone)]
pub struct WorkflowOptions {
    pub max_retries: usize,
    pub max_discovery_rounds: usize,
    pub max_files: usize,
    pub history_budget_tokens: u64,
    pub history_tail_turns: usize,
    pub repomap_token_budget: usize,
    pub history_model: ModelRef,
    pub auto_level_up: bool,
    pub models_config: crate::config::ModelsConfig,
    pub initial_selection: Option<tauqe_protocol::ModelSelection>,
    pub app_config: Option<crate::config::AppConfig>,
    pub discovery_mode: crate::config::DiscoveryMode,
    /// Reasoning effort applied to the current turn, resolved from the active
    /// semantic role. Updated on `auto_level_up` escalation so a
    /// Coder→Architect switch inherits the deeper Architect effort.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        Self {
            max_retries: 3,
            max_discovery_rounds: crate::config::DEFAULT_MAX_DISCOVERY_ROUNDS,
            max_files: crate::config::DEFAULT_MAX_CONTEXT_FILES,
            history_budget_tokens: crate::history::DEFAULT_HISTORY_BUDGET_TOKENS,
            history_tail_turns: crate::history::DEFAULT_TAIL_TURNS_COUNT,
            repomap_token_budget: crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
            history_model: ModelRef::openrouter(crate::config::BUILTIN_DEFAULT_SCRIBE_MODEL),
            auto_level_up: false,
            models_config: crate::config::ModelsConfig::default(),
            initial_selection: None,
            app_config: None,
            discovery_mode: crate::config::DiscoveryMode::default(),
            reasoning_effort: None,
        }
    }
}

impl WorkflowOptions {
    /// Constructs workflow options from the unified application configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        let max_retries = config
            .toolchain
            .max_retries
            .unwrap_or(config.develop.max_retries);
        Self {
            max_retries,
            max_discovery_rounds: config.context.max_discovery_rounds,
            max_files: config.context.max_files,
            history_budget_tokens: config.history.budget_tokens,
            history_tail_turns: config.history.tail_turns,
            repomap_token_budget: config.context.repomap_token_budget,
            history_model: config.history_model(),
            auto_level_up: config.models.auto_level_up,
            models_config: config.models.clone(),
            initial_selection: None,
            app_config: Some(config.clone()),
            discovery_mode: config.context.discovery_mode,
            reasoning_effort: None,
        }
    }
}

/// Advances the coder escalation ladder by one rung.
///
/// Returns the next ladder index with its tier, or `None` when the ladder is
/// already at its strongest rung so escalation terminates deterministically.
/// A rung counts as an advance when either the model or the reasoning effort
/// changes, which lets the ladder deepen reasoning on the same model before
/// switching to a stronger model.
fn next_coder_tier(
    models_config: &crate::config::ModelsConfig,
    current_idx: usize,
) -> Option<(usize, CoderTier)> {
    let current = models_config.coder_tier(current_idx);
    let next_idx = current_idx + 1;
    let next = models_config.coder_tier(next_idx);
    if next.model == current.model && next.effort == current.effort {
        None
    } else {
        Some((next_idx, next))
    }
}

/// Strategy governing how bounded turns are executed and supervised.
///
/// The mode owns the high-level execution decision: how many turns run, how
/// their cost is accounted for, and how failures propagate. The transactional
/// boundary of a single turn is injected per call as an [`ApplyPolicy`],
/// so the same mode drives Git-backed and direct-write workflows alike.
#[async_trait]
pub trait ExecutionMode: Send + Sync {
    /// Stable identifier used in diagnostics.
    fn name(&self) -> &'static str;

    /// Executes a single bounded turn against the provided collaborators.
    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        policy: &mut dyn ApplyPolicy,
        prompt: &str,
        provider: &dyn LlmProvider,
        model: &ModelRef,
        context_manager: &mut ContextManager,
        history_manager: &mut HistoryManager,
        protocol: &dyn PromptProtocol,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<WorkflowExecutionResult>;
}

/// Execution mode that runs exactly one bounded turn.
#[derive(Debug, Clone, Default)]
pub struct SingleExecution {
    options: WorkflowOptions,
}

impl SingleExecution {
    /// Creates a single-turn execution mode bound to the given workflow options.
    pub fn new(options: WorkflowOptions) -> Self {
        Self { options }
    }
}

#[async_trait]
impl ExecutionMode for SingleExecution {
    fn name(&self) -> &'static str {
        "single"
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        policy: &mut dyn ApplyPolicy,
        prompt: &str,
        provider: &dyn LlmProvider,
        model: &ModelRef,
        context_manager: &mut ContextManager,
        history_manager: &mut HistoryManager,
        protocol: &dyn PromptProtocol,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<WorkflowExecutionResult> {
        let mut current_model = model.clone();
        let current_role = match self.options.initial_selection {
            Some(tauqe_protocol::ModelSelection::Role(role)) => Some(role),
            _ => None,
        };
        let mut ladder_idx: usize = 0;
        let mut dynamic_provider: Option<Box<dyn LlmProvider>> = None;
        let mut turn_options = self.options.clone();

        loop {
            let active_provider: &dyn LlmProvider = if let Some(ref p) = dynamic_provider {
                p.as_ref()
            } else {
                provider
            };

            let mut env = TaskEnv {
                provider: active_provider,
                model: &current_model,
                context: context_manager,
                history: history_manager,
                protocol,
                policy,
                workflow_name: self.name(),
                stream_tx: stream_tx.clone(),
                cancel_rx: cancel_rx.clone(),
                options: &turn_options,
            };

            let loop_task = AnswerEditLoop::new(prompt);
            let out = loop_task.execute(&mut env).await?;

            let is_blocked = out.blocked.is_some();
            let is_error = matches!(&out.result, ModelResult::Edit { error: Some(_), applied: false, .. });
            let is_empty_edit = matches!(&out.result, ModelResult::Edit { edits, error: None, .. } if edits.is_empty());

            if (is_error || is_empty_edit)
                && !out.is_cancelled
                && !is_blocked
                && turn_options.auto_level_up
                && current_role == Some(ModelRole::Coder)
            {
                if let Some((next_idx, next_tier)) =
                    next_coder_tier(&turn_options.models_config, ladder_idx)
                {
                    let next_m = next_tier.model.clone();
                    let _ = policy.rollback(context_manager.repo_root());
                    context_manager.prune_missing_files();

                    let reason = if is_error {
                        if let ModelResult::Edit { error: Some(ref err_msg), .. } = out.result {
                            err_msg.clone()
                        } else {
                            "Unknown error".to_string()
                        }
                    } else {
                        "No valid edits".to_string()
                    };

                    let notice = format!(
                        "\n\n> ⚠️ **Level Up Escalation:** Turn failed on coder tier {} (`{}`, {}). Escalating to tier {} (`{}`)...\n\n",
                        ladder_idx, current_model, reason, next_idx, next_m
                    );
                    let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;

                    ladder_idx = next_idx;
                    turn_options.reasoning_effort = next_tier.effort;

                    if next_m.provider != current_model.provider {
                        if let Some(ref app_cfg) = turn_options.app_config {
                            if let Ok(np) = crate::providers::create_provider(next_m.provider, app_cfg) {
                                dynamic_provider = Some(np);
                            }
                        }
                    }
                    current_model = next_m;
                    continue;
                }
            }

            return Ok(WorkflowExecutionResult {
                result: out.result,
                assistant_text: out.assistant_text,
                blocked: out.blocked,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::{mpsc, watch};
    use tauqe_protocol::{ContextAccess, ModelRef, ModelResult, ProviderKind, ReasoningEffort};

    use crate::context::ContextManager;
    use crate::history::HistoryManager;
    use crate::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
    use crate::prompt::protocol::PromptProtocol;
    use crate::workflow::policy::DirectApplyPolicy;
    use crate::workflow::WorkflowOptions;

    use super::{next_coder_tier, ExecutionMode, SingleExecution};

    const MARKER: &str = "EXEC";

    struct MockProvider {
        chunk: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenRouter
        }

        async fn stream_chat(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            _response_format: Option<ResponseFormat>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let _ = tx.send(StreamEvent::TextDelta(self.chunk.clone())).await;
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }

        async fn stream_text(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }
    }

    #[test]
    fn single_execution_reports_stable_name() {
        let exec = SingleExecution::new(WorkflowOptions::default());
        assert_eq!(exec.name(), "single");
    }

    #[test]
    fn next_coder_tier_deepens_effort_then_model_and_saturates() {
        let config: crate::config::ModelsConfig = toml::from_str(
            r#"
coder = [
    "openrouter:base/model",
    { model = "openrouter:base/model", effort = "high" },
    { model = "openrouter:strong/model", effort = "xhigh" },
]
"#,
        )
        .expect("parse coder ladder");

        let (first_idx, first) = next_coder_tier(&config, 0).expect("deepen reasoning first");
        assert_eq!(first_idx, 1);
        assert_eq!(first.model, ModelRef::openrouter("base/model"));
        assert_eq!(first.effort, Some(ReasoningEffort::High));

        let (second_idx, second) = next_coder_tier(&config, 1).expect("switch to stronger model");
        assert_eq!(second_idx, 2);
        assert_eq!(second.model, ModelRef::openrouter("strong/model"));
        assert_eq!(second.effort, Some(ReasoningEffort::XHigh));

        assert!(next_coder_tier(&config, 2).is_none(), "ladder saturates");
        assert!(next_coder_tier(&config, 9).is_none(), "beyond the last rung saturates");
    }

    #[tokio::test]
    async fn single_execution_runs_one_bounded_turn() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&repo_root), true).unwrap();
        std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n")
            .expect("write target fixture");
        crate::git::cmd::stage_files(&repo_root, &["target.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(&repo_root, "Initial commit", false).unwrap();

        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker(MARKER).unwrap();
        
        let chunk = format!(
            "<tauqe_edits_{MARKER} summary=\"Set compute to 42\">\n<edit_{MARKER} path=\"target.rs\">\n<search_{MARKER}>\npub fn compute() -> i32 {{ 0 }}\n</search_{MARKER}>\n<replace_{MARKER}>\npub fn compute() -> i32 {{ 42 }}\n</replace_{MARKER}>\n</edit_{MARKER}>\n</tauqe_edits_{MARKER}>"
        );
        let provider = MockProvider { chunk };
        let model = ModelRef::openrouter("mock/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let mut policy = DirectApplyPolicy::new();

        let exec = SingleExecution::new(WorkflowOptions::default());
        let execution = exec
            .execute(
                &mut policy,
                "Set compute to return 42",
                &provider,
                &model,
                &mut context,
                &mut history,
                protocol.as_ref(),
                tx,
                cancel_rx,
            )
            .await
            .expect("single execution runs to completion");

        match execution.result {
            ModelResult::Edit { applied, error, .. } => {
                assert!(error.is_none(), "unexpected error: {error:?}");
                assert!(applied, "verified turn should be applied");
            }
            other => panic!("expected applied edits, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(repo_root.join("target.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
    }
}
