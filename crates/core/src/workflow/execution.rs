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
use tauqe_protocol::{
    ModelEntry, ModelRef, ModelResult, ModelRole, PatchStrategy, PromptLayoutKind, ReasoningEffort,
    TurnIntent,
};
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::context::ContextManager;
use crate::history::HistoryManager;
use crate::prompt::protocol::PromptProtocol;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::policy::ApplyPolicy;

use super::WorkflowExecutionResult;
use crate::workflow::discovery::DiscoveryLimits;
use crate::workflow::fsm::guard::rollback_or_log;
use crate::workflow::fsm::{begin_session_turn, record_session_response, run_fsm_turn_body};

/// Execution options and token budgets for edit workflows.
#[derive(Debug, Clone)]
pub struct WorkflowOptions {
    pub max_retries: usize,
    pub max_discovery_rounds: usize,
    pub files_token_budget: usize,
    pub history_budget_tokens: u64,
    pub history_tail_turns: usize,
    pub repomap_token_budget: usize,
    /// Whether the eager global symbol map is injected into every proposal.
    ///
    /// When disabled, the prompt carries the compact directory outline instead,
    /// so the model still knows where subsystems live without a full map.
    pub include_repo_map: bool,
    /// Operational caps the Discovery phase applies to one round.
    pub discovery_limits: DiscoveryLimits,
    /// Maximum characters of one project command stream reported back to the model.
    pub max_feedback_chars: usize,
    /// Maximum characters of a turn scratchpad block before safe truncation.
    pub max_scratchpad_chars: usize,
    pub history_model: ModelRef,
    pub auto_level_up: bool,
    pub models_config: crate::config::ModelsConfig,
    pub initial_selection: Option<tauqe_protocol::ModelSelection>,
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
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        Self {
            max_retries: 3,
            max_discovery_rounds: crate::config::DEFAULT_MAX_DISCOVERY_ROUNDS,
            files_token_budget: crate::config::DEFAULT_FILES_TOKEN_BUDGET,
            history_budget_tokens: crate::history::DEFAULT_HISTORY_BUDGET_TOKENS,
            history_tail_turns: crate::history::DEFAULT_TAIL_TURNS_COUNT,
            repomap_token_budget: crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
            include_repo_map: true,
            discovery_limits: DiscoveryLimits::default(),
            max_feedback_chars: crate::config::DEFAULT_MAX_FEEDBACK_CHARS,
            max_scratchpad_chars: crate::config::DEFAULT_MAX_SCRATCHPAD_CHARS,
            history_model: ModelRef::openrouter(crate::config::BUILTIN_DEFAULT_SCRIBE_MODEL),
            auto_level_up: false,
            models_config: crate::config::ModelsConfig::default(),
            initial_selection: None,
            app_config: None,
            reasoning_effort: None,
            session_id: None,
        }
    }
}

impl WorkflowOptions {
    /// Constructs workflow options from the unified application configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        let max_retries = config.develop.max_retries;
        Self {
            max_retries,
            max_discovery_rounds: config.context.max_discovery_rounds,
            files_token_budget: config.context.files_token_budget,
            history_budget_tokens: config.history.budget_tokens,
            history_tail_turns: config.history.tail_turns,
            repomap_token_budget: config.context.repomap_token_budget,
            include_repo_map: config.context.include_repo_map,
            discovery_limits: DiscoveryLimits::from_context_config(&config.context),
            max_feedback_chars: config.context.max_feedback_chars,
            max_scratchpad_chars: config.context.max_scratchpad_chars,
            history_model: config.history_model(),
            auto_level_up: config.models.auto_level_up,
            models_config: config.models.clone(),
            initial_selection: None,
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
    pub fn patch_strategy(&self) -> PatchStrategy {
        self.app_config
            .as_ref()
            .map(|config| config.develop.patch_strategy)
            .unwrap_or_default()
    }

    /// Returns the prompt layout policy that composes this turn's messages.
    /// The declared `[develop]` key of the governing project configuration
    /// wins for every turn; without one the turn intent decides: an
    /// interactive REPL turn and a plan discussion keep a growing session
    /// history and share one byte-stable head, so both build the chronological
    /// transcript, while an isolated step keeps the semantic block order.
    /// Returns the prompt layout policy that composes this turn's messages.
    ///
    /// The declared `[develop]` key of the governing project configuration
    /// wins for every turn; without one the turn intent decides: an
    /// interactive REPL turn and a plan discussion keep a growing session
    /// history and share one byte-stable head, so both build the chronological
    /// transcript, while an isolated step keeps the semantic block order.
    /// Options that carry no governing configuration keep the semantic policy,
    /// which every intent renders correctly.
    pub fn prompt_layout(&self, intent: TurnIntent) -> PromptLayoutKind {
        self.app_config
            .as_ref()
            .map(|config| config.resolve_prompt_layout(intent))
            .unwrap_or(PromptLayoutKind::Semantic)
    }
}

/// Advances the coder escalation ladder by one rung.
///
/// Returns the next ladder index with its tier, or `None` when the ladder is
/// already at its strongest rung so escalation terminates deterministically.
/// A rung counts as an advance when either the model or the reasoning effort
/// changes, which lets the ladder deepen reasoning on the same model before
/// switching to a stronger model. The rule itself lives in
/// `ModelsConfig::next_coder_tier`, so the harness holds one authoritative
/// definition of an advancing rung.
fn next_coder_tier(
    models_config: &crate::config::ModelsConfig,
    current_idx: usize,
) -> Option<(usize, ModelEntry)> {
    models_config.next_coder_tier(current_idx)
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
        intent: TurnIntent,
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
        intent: TurnIntent,
        provider: &dyn LlmProvider,
        model: &ModelRef,
        context_manager: &mut ContextManager,
        history_manager: &mut HistoryManager,
        protocol: &dyn PromptProtocol,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<WorkflowExecutionResult> {
        // Session bookkeeping runs once for the whole bounded turn. Coder-tier
        // escalation replays the turn body on a stronger tier, so recording the
        // prompt and response here, rather than inside every attempt, keeps a
        // retried turn from leaving duplicate history entries behind.
        let turn_id = begin_session_turn(
            prompt,
            intent,
            context_manager,
            history_manager,
            provider,
            &cancel_rx,
            &self.options,
        )
        .await;

        let current_role = match self.options.initial_selection {
            Some(tauqe_protocol::ModelSelection::Role(role)) => Some(role),
            _ => None,
        };
        let mut current_model = model.clone();
        let mut ladder_idx: usize = 0;
        let mut dynamic_provider: Option<Box<dyn LlmProvider>> = None;
        let mut turn_options = self.options.clone();

        loop {
            let active_provider: &dyn LlmProvider = if let Some(ref p) = dynamic_provider {
                p.as_ref()
            } else {
                provider
            };

            let turn_res = run_fsm_turn_body(
                prompt,
                intent,
                active_provider,
                &current_model,
                context_manager,
                history_manager,
                protocol,
                policy,
                self.name(),
                stream_tx.clone(),
                cancel_rx.clone(),
                &turn_options,
            )
            .await;

            let turn = match turn_res {
                Ok(turn) => turn,
                Err(err) => {
                    rollback_or_log(policy, context_manager.repo_root()).await;
                    // The sweep only drops context paths that no longer exist on
                    // disk and runs off the async executor thread. The auto layer
                    // survives a failed attempt so escalation inherits it.
                    crate::blocking::run_blocking(|| {
                        context_manager.prune_missing_files();
                    });
                    if let Some(id) = turn_id {
                        let _ = history_manager.rollback_entry(id, format!("Turn failed: {}", err));
                    }
                    return Err(err);
                }
            };

            let is_blocked = turn.blocked.is_some();
            let is_error = matches!(
                &turn.result,
                ModelResult::Edit { outcome, .. } if outcome.error.is_some() && !outcome.applied
            );
            let is_empty_edit = matches!(
                &turn.result,
                ModelResult::Edit { edits, outcome, .. }
                    if edits.is_empty() && outcome.error.is_none()
            );
            // A coder that answers without edits and without an explicit
            // `<success>` signal has not validated the step: escalate it like a
            // failed or empty edit instead of accepting an idle reply (c-3).
            let is_unverified_answer = turn.is_unverified_answer();

            if (is_error || is_empty_edit || is_unverified_answer)
                && !turn.is_cancelled
                && !is_blocked
                && turn_options.auto_level_up
                && current_role == Some(ModelRole::Coder)
            {
                if let Some((next_idx, next_tier)) =
                    next_coder_tier(&turn_options.models_config, ladder_idx)
                {
                    let next_m = next_tier.model.clone();

                    // Build the destination provider before committing to the
                    // escalation. A provider-construction failure must never
                    // leave the harness retrying a foreign model on the current
                    // provider, so the escalation is skipped and the failure is
                    // surfaced as a diagnostic instead of silently reusing the
                    // previous provider.
                    let new_provider = if next_m.provider != current_model.provider {
                        match turn_options.app_config.as_ref().map(|config| {
                            crate::providers::create_provider(next_m.provider, config)
                        }) {
                            Some(Ok(built)) => Some(built),
                            Some(Err(err)) => {
                                tracing::error!(
                                    "Failed to create a provider for escalation tier {} (`{}`): {}. Skipping escalation and returning the current turn outcome.",
                                    next_idx, next_m, err
                                );
                                None
                            }
                            None => {
                                tracing::error!(
                                    "Cannot escalate to tier {} (`{}`): no application configuration is available to build its provider. Skipping escalation and returning the current turn outcome.",
                                    next_idx, next_m
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };

                    let provider_ready =
                        next_m.provider == current_model.provider || new_provider.is_some();
                    if provider_ready {
                        // A failed rollback must stay observable: the next
                        // attempt starts from a tree that may still hold the
                        // failed writes, so the developer is warned before the
                        // stronger tier begins.
                        if !rollback_or_log(policy, context_manager.repo_root()).await {
                            let notice = concat!(
                                "\n\n> ⚠️ **Rollback Failed:** ",
                                "The failed attempt could not be reverted; ",
                                "the working tree may still hold its writes. ",
                                "Escalating anyway...\n\n"
                            )
                            .to_string();
                            let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;
                        }
                        // Pruning missing files performs filesystem metadata
                        // checks, so it runs off the async executor thread.
                        crate::blocking::run_blocking(|| context_manager.prune_missing_files());

                        let reason = if is_error {
                            match &turn.result {
                                ModelResult::Edit { outcome, .. } if outcome.error.is_some() => {
                                    outcome
                                        .error
                                        .clone()
                                        .unwrap_or_else(|| "Unknown error".to_string())
                                }
                                _ => "Unknown error".to_string(),
                            }
                        } else if is_unverified_answer {
                            "Unverified answer without edits".to_string()
                        } else {
                            "No valid edits".to_string()
                        };

                        tracing::warn!(
                            from_tier = ladder_idx,
                            to_tier = next_idx,
                            from_model = %current_model,
                            to_model = %next_m,
                            effort = ?next_tier.effort,
                            reason = %reason,
                            "coder ladder escalation"
                        );
                        // The escalation is announced as a typed event before the
                        // textual notice, so clients carry the active tier from
                        // structured data instead of parsing prose.
                        let _ = stream_tx
                            .send(StreamEvent::ModelEscalated {
                                model: next_m.clone(),
                                role: current_role,
                                alias: None,
                                effort: next_tier.effort,
                                tier: next_idx,
                            })
                            .await;
                        let notice = format!(
                            "\n\n> ⚠️ **Level Up Escalation:** Turn failed on coder tier {} (`{}`, {}). Escalating to tier {} (`{}`)...\n\n",
                            ladder_idx, current_model, reason, next_idx, next_m
                        );
                        let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;

                        ladder_idx = next_idx;
                        turn_options.reasoning_effort = next_tier.effort;

                        if let Some(built) = new_provider {
                            dynamic_provider = Some(built);
                        }
                        current_model = next_m;
                        continue;
                    }
                }
            }

            if turn.is_cancelled {
                if let Some(id) = turn_id {
                    let _ = history_manager.rollback_entry(id, "Turn cancelled by the developer");
                }
            } else {
                record_session_response(history_manager, &turn);
            }
            // A plan mutation the storage engine refused never fails the turn,
            // but it is a protocol-level rejection the developer must see: the
            // reason is streamed as a warning banner, and the history record
            // written above hands it to the model's next explicit round.
            if let Some(notice) = turn.plan_rejection_notice() {
                tracing::warn!("{}", notice);
                let _ = stream_tx
                    .send(StreamEvent::TextDelta(format!(
                        "\n\n---\n\u{26a0}\u{fe0f} {}\n\n",
                        notice
                    )))
                    .await;
            }
            return Ok(WorkflowExecutionResult {
                result: turn.result,
                assistant_text: turn.assistant_text,
                blocked: turn.blocked,
                skip_reason: turn.skip_reason,
                abort: turn.abort,
                plan_saved: turn.plan_saved,
                plan_errors: turn.plan_errors,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use tauqe_protocol::{
        ContextAccess, EditOperation, EditProposal, ModelRef, ModelResult, ProviderKind,
        ReasoningEffort,
    };
    use tokio::sync::{mpsc, watch};

    use crate::context::ContextManager;
    use crate::edits::{StageEditsResult, StagedEditsState};
    use crate::history::HistoryManager;
    use crate::prompt::protocol::PromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
    use crate::workflow::policy::{ApplyPolicy, DirectApplyPolicy};
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
        // Role lists are decoded through the application configuration, so the
        // test exercises the same catalog resolution a project config uses.
        let config: crate::config::AppConfig = toml::from_str(
            r#"
[models]
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:base/model",
    { model = "openrouter:base/model", effort = "high" },
    { model = "openrouter:strong/model", effort = "xhigh" },
]
"#,
        )
        .expect("parse coder ladder");
        let config = config.models;

        let (first_idx, first) = next_coder_tier(&config, 0).expect("deepen reasoning first");
        assert_eq!(first_idx, 1);
        assert_eq!(first.model, ModelRef::openrouter("base/model"));
        assert_eq!(first.effort, Some(ReasoningEffort::High));

        let (second_idx, second) = next_coder_tier(&config, 1).expect("switch to stronger model");
        assert_eq!(second_idx, 2);
        assert_eq!(second.model, ModelRef::openrouter("strong/model"));
        assert_eq!(second.effort, Some(ReasoningEffort::XHigh));

        assert!(next_coder_tier(&config, 2).is_none(), "ladder saturates");
        assert!(
            next_coder_tier(&config, 9).is_none(),
            "beyond the last rung saturates"
        );
    }

    #[tokio::test]
    async fn single_execution_runs_one_bounded_turn() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&repo_root), true).unwrap();
        std::fs::write(
            repo_root.join("target.rs"),
            "pub fn compute() -> i32 { 0 }\n",
        )
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
                tauqe_protocol::TurnIntent::InteractiveRepl,
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
            ModelResult::Edit { outcome, .. } => {
                assert!(
                    outcome.error.is_none(),
                    "unexpected error: {:?}",
                    outcome.error
                );
                assert!(outcome.applied, "verified turn should be applied");
            }
            other => panic!("expected applied edits, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(repo_root.join("target.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
    }

    fn repo_with_target() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&repo_root), true).unwrap();
        std::fs::write(
            repo_root.join("target.rs"),
            "pub fn compute() -> i32 { 0 }\n",
        )
        .expect("write target fixture");
        crate::git::cmd::stage_files(&repo_root, &["target.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(&repo_root, "Initial commit", false).unwrap();
        (dir, repo_root)
    }

    fn coder_ladder_options() -> WorkflowOptions {
        // A role list resolves through the application configuration, exactly
        // like a project `tauqe.toml`, so the fixture keeps the real decoding
        // path instead of hand-building the resolved struct.
        let config: crate::config::AppConfig = toml::from_str(
            r#"
[models]
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:base/model",
    { model = "openrouter:base/model", effort = "high" },
]
"#,
        )
        .expect("parse coder ladder");
        let models_config = config.models;
        WorkflowOptions {
            auto_level_up: true,
            initial_selection: Some(tauqe_protocol::ModelSelection::Role(
                tauqe_protocol::ModelRole::Coder,
            )),
            models_config,
            ..WorkflowOptions::default()
        }
    }

    /// Policy delegating every phase to the direct policy while its rollback
    /// always fails, to exercise the durability seam of the escalation path.
    #[derive(Default)]
    struct FailingRollbackPolicy {
        inner: DirectApplyPolicy,
    }

    #[async_trait::async_trait]
    impl ApplyPolicy for FailingRollbackPolicy {
        async fn staging(
            &mut self,
            repo_root: &Path,
            context_manager: &ContextManager,
            edits: &[EditOperation],
            staged_state: Option<&StagedEditsState>,
        ) -> StageEditsResult {
            self.inner
                .staging(repo_root, context_manager, edits, staged_state)
                .await
        }

        async fn apply_temporary(
            &mut self,
            repo_root: &Path,
            context_manager: &mut ContextManager,
            proposal: &EditProposal,
        ) -> anyhow::Result<Vec<String>> {
            self.inner
                .apply_temporary(repo_root, context_manager, proposal)
                .await
        }

        async fn apply_final(
            &mut self,
            repo_root: &Path,
            changed_files: &[String],
            summary: &str,
        ) -> anyhow::Result<Option<String>> {
            self.inner
                .apply_final(repo_root, changed_files, summary)
                .await
        }

        async fn rollback(&mut self, _repo_root: &Path) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("simulated rollback failure"))
        }
    }

    #[tokio::test]
    async fn single_execution_escalates_coder_on_unverified_answer() {
        let (_dir, repo_root) = repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker(MARKER).unwrap();

        let edit = format!(
            "<tauqe_edits_{MARKER} summary=\"Set compute to 42\">\n<edit_{MARKER} path=\"target.rs\">\n<search_{MARKER}>\npub fn compute() -> i32 {{ 0 }}\n</search_{MARKER}>\n<replace_{MARKER}>\npub fn compute() -> i32 {{ 42 }}\n</replace_{MARKER}>\n</edit_{MARKER}>\n</tauqe_edits_{MARKER}>"
        );
        // The first reply is a bare answer: no edits and no <success> signal.
        let provider = MockLlmProvider::scripted([
            MockResponse::text("I need more information before changing the code."),
            MockResponse::text(edit),
        ]);
        let model = ModelRef::openrouter("base/model");
        let (tx, _rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut policy = DirectApplyPolicy::new();

        let exec = SingleExecution::new(coder_ladder_options());
        let execution = exec
            .execute(
                &mut policy,
                "Set compute to return 42",
                tauqe_protocol::TurnIntent::InteractiveRepl,
                &provider,
                &model,
                &mut context,
                &mut history,
                protocol.as_ref(),
                tx,
                cancel_rx,
            )
            .await
            .expect("escalating execution runs to completion");

        match execution.result {
            ModelResult::Edit { outcome, .. } => {
                assert!(outcome.applied, "the escalated tier must apply its edits");
            }
            other => panic!("expected applied edits after escalation, got {other:?}"),
        }
        assert_eq!(
            provider.requests().len(),
            2,
            "an unverified answer must trigger a second coder-tier attempt"
        );
        assert_eq!(
            std::fs::read_to_string(repo_root.join("target.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
    }

    /// A plan mutation the storage engine refuses is a diagnostic, not a silent
    /// journal trace: the reason reaches the recorded history (so the next
    /// round's prompt carries it), the developer-facing turn stream, and the turn
    /// result. The accepted mutation above proves the diagnostic stays absent
    /// when the payload is valid.
    #[tokio::test]
    async fn rejected_plan_mutation_reaches_the_history_and_the_turn_stream() {
        let (_dir, repo_root) = repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        // An established plan is protected from Develop-mode mutation: only the
        // Discussion mode may modify it, so the storage engine refuses this
        // payload and its refusal must stay visible to the model and to the
        // developer.
        let existing = tauqe_protocol::Plan {
            id: "existing-plan".to_string(),
            title: "Existing Plan".to_string(),
            ..Default::default()
        };
        crate::plan::storage::PlanStorage::save_plan(&repo_root, &existing)
            .expect("seed the established plan");

        let rejected = concat!(
            "Drafting the plan.\n",
            "<plan action=\"save\" id=\"existing-plan\" title=\"Rewritten\">\n",
            "  <concept>\n",
            "    <concept_item id=\"c-1\" title=\"Invariant\" />\n",
            "  </concept>\n",
            "  <item id=\"1\" title=\"First step\" />\n",
            "</plan>",
        );
        let provider = MockLlmProvider::scripted([MockResponse::text(rejected)]);
        let model = ModelRef::openrouter("mock/model");
        let (tx, mut rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut policy = DirectApplyPolicy::new();

        // Drain concurrently: the turn drops the sender as it settles.
        let collector = tokio::spawn(async move {
            let mut streamed = String::new();
            while let Some(event) = rx.recv().await {
                if let StreamEvent::TextDelta(delta) = event {
                    streamed.push_str(&delta);
                }
            }
            streamed
        });

        let exec = SingleExecution::new(WorkflowOptions::default());
        let execution = exec
            .execute(
                &mut policy,
                "Plan the work",
                tauqe_protocol::TurnIntent::InteractiveRepl,
                &provider,
                &model,
                &mut context,
                &mut history,
                &protocol,
                tx,
                cancel_rx,
            )
            .await
            .expect("the turn executes");

        assert_eq!(
            execution.plan_errors.len(),
            1,
            "{:?}",
            execution.plan_errors
        );
        assert!(
            execution.plan_errors[0].contains("Develop mode"),
            "the reason must be carried on the turn result: {:?}",
            execution.plan_errors
        );

        let streamed = collector.await.expect("the collector joins");
        assert!(
            streamed.contains("Plan mutation rejected"),
            "the developer must see the rejection in the turn stream: {streamed:?}"
        );

        let messages: Vec<String> = history
            .get_entries()
            .expect("the history reads")
            .iter()
            .filter_map(|entry| match entry {
                crate::history::HistoryEntry::Response { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("Plan mutation rejected")
                    && message.contains("Develop mode")),
            "the rejection must reach the recorded history: {messages:?}"
        );
        assert!(
            history
                .format_history_tag()
                .expect("the history tag renders")
                .contains("Plan mutation rejected"),
            "the next round's prompt must carry the rejection"
        );
    }

    #[tokio::test]
    async fn single_execution_emits_typed_escalation_event() {
        let (_dir, repo_root) = repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker(MARKER).unwrap();

        let edit = format!(
            "<tauqe_edits_{MARKER} summary=\"Set compute to 42\">\n<edit_{MARKER} path=\"target.rs\">\n<search_{MARKER}>\npub fn compute() -> i32 {{ 0 }}\n</search_{MARKER}>\n<replace_{MARKER}>\npub fn compute() -> i32 {{ 42 }}\n</replace_{MARKER}>\n</edit_{MARKER}>\n</tauqe_edits_{MARKER}>"
        );
        // The first reply is a bare answer, so the ladder advances to its next
        // rung before the edit finally lands.
        let provider = MockLlmProvider::scripted([
            MockResponse::text("I need more information before changing the code."),
            MockResponse::text(edit),
        ]);
        let model = ModelRef::openrouter("base/model");
        let (tx, mut rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut policy = DirectApplyPolicy::new();

        // Drain the stream concurrently: the sender is dropped with the turn, so
        // collecting only afterwards would race the channel shutdown.
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });

        let exec = SingleExecution::new(coder_ladder_options());
        exec.execute(
            &mut policy,
            "Set compute to return 42",
            tauqe_protocol::TurnIntent::InteractiveRepl,
            &provider,
            &model,
            &mut context,
            &mut history,
            protocol.as_ref(),
            tx,
            cancel_rx,
        )
        .await
        .expect("escalating execution runs to completion");

        let events = collector.await.expect("event collector joins");
        let escalations: Vec<&StreamEvent> = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ModelEscalated { .. }))
            .collect();
        assert_eq!(
            escalations.len(),
            1,
            "one ladder advance must emit exactly one typed event"
        );
        let escalated = escalations.first().expect("escalation event is present");
        match escalated {
            StreamEvent::ModelEscalated {
                model,
                role,
                alias,
                effort,
                tier,
            } => {
                assert_eq!(model, &ModelRef::openrouter("base/model"));
                assert!(matches!(role, Some(tauqe_protocol::ModelRole::Coder)));
                assert!(alias.is_none(), "the rung configures no alias");
                assert_eq!(effort, &Some(ReasoningEffort::High));
                assert_eq!(*tier, 1, "the event names the rung that was entered");
            }
            other => panic!("expected a model escalation event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn single_execution_accepts_coder_answer_marked_success() {
        let (_dir, repo_root) = repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker(MARKER).unwrap();

        // A terminal <success> marks the answer as a validated conclusion, so
        // the coder ladder must not escalate. The tag carries the active turn
        // marker, matching how the harness binds structural tags to the turn:
        // an unmarked tag would be treated as literal text and the reply as an
        // unverified answer.
        let success =
            format!("The step is already satisfied.\n<success_{MARKER} summary=\"Verified\" />");
        let provider = MockLlmProvider::scripted([MockResponse::text(success)]);
        let model = ModelRef::openrouter("base/model");
        let (tx, _rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut policy = DirectApplyPolicy::new();

        let exec = SingleExecution::new(coder_ladder_options());
        let execution = exec
            .execute(
                &mut policy,
                "Confirm the step is satisfied",
                tauqe_protocol::TurnIntent::InteractiveRepl,
                &provider,
                &model,
                &mut context,
                &mut history,
                protocol.as_ref(),
                tx,
                cancel_rx,
            )
            .await
            .expect("validated answer runs to completion");

        assert!(matches!(execution.result, ModelResult::Answer { .. }));
        assert_eq!(
            provider.requests().len(),
            1,
            "a terminal success must not trigger escalation"
        );
    }

    #[tokio::test]
    async fn escalation_surfaces_a_failed_rollback_in_the_stream() {
        let (_dir, repo_root) = repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker(MARKER).unwrap();

        let edit = format!(
            "<tauqe_edits_{MARKER} summary=\"Set compute to 42\">\n<edit_{MARKER} path=\"target.rs\">\n<search_{MARKER}>\npub fn compute() -> i32 {{ 0 }}\n</search_{MARKER}>\n<replace_{MARKER}>\npub fn compute() -> i32 {{ 42 }}\n</replace_{MARKER}>\n</edit_{MARKER}>\n</tauqe_edits_{MARKER}>"
        );
        // The first reply is a bare answer, so the ladder escalates — and the
        // rollback that precedes the retry fails.
        let provider = MockLlmProvider::scripted([
            MockResponse::text("I need more information before changing the code."),
            MockResponse::text(edit),
        ]);
        let model = ModelRef::openrouter("base/model");
        let (tx, mut rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut policy = FailingRollbackPolicy::default();

        // Drain the stream concurrently: the sender is dropped with the turn, so
        // collecting only afterwards would race the channel shutdown.
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });

        let exec = SingleExecution::new(coder_ladder_options());
        let execution = exec
            .execute(
                &mut policy,
                "Set compute to return 42",
                tauqe_protocol::TurnIntent::InteractiveRepl,
                &provider,
                &model,
                &mut context,
                &mut history,
                protocol.as_ref(),
                tx,
                cancel_rx,
            )
            .await
            .expect("escalation survives a failed rollback");

        match execution.result {
            ModelResult::Edit { outcome, .. } => {
                assert!(outcome.applied, "the escalated attempt must still apply");
            }
            other => panic!("expected applied edits after a failed rollback, got {other:?}"),
        }
        let events = collector.await.expect("event collector joins");
        let streamed: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            streamed.contains("Rollback Failed"),
            "a failed rollback must surface in the streamed diagnostics: {streamed}"
        );
        assert_eq!(
            provider.requests().len(),
            2,
            "the escalation must still run the stronger tier after the rollback failure"
        );
    }
}
