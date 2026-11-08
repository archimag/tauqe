//! Execution modes governing how bounded turns are orchestrated.
//!
//! An [`ExecutionMode`] is the top orchestration layer of the turn pipeline: it
//! decides how model turns are composed, supervised, and accounted for, while
//! the lower FSM layers execute the individual turns. Keeping this decision out
//! of the FSM cycle and the apply policy lets single-turn and batch scenarios
//! evolve independently.
//!
//! [`SingleExecution`] runs exactly one bounded turn by composing the
//! single-turn workflow lifecycle: session bookkeeping, the FSM cycle, and the
//! recording of the turn response.

use async_trait::async_trait;
use tauqe_protocol::{ModelRef, PatchStrategy, PromptLayoutKind, ReasoningEffort, TurnIntent};
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::context::ContextManager;
use crate::history::HistoryManager;
use crate::prompt::protocol::PromptProtocol;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::policy::ApplyPolicy;

use super::{FsmTurnOutcome, WorkflowExecutionResult};
use crate::workflow::fsm::guard::rollback_or_log;
use crate::workflow::fsm::{
    announce_plan_diagnostics, begin_session_turn, record_session_response, run_fsm_turn_body,
};
use crate::workflow::research::ResearchLimits;

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
    /// The declared `[repl]` key of the governing project configuration
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
        // Session bookkeeping runs once for the whole bounded turn: recording the
        // prompt and the response here, rather than inside the turn body, keeps a
        // failed attempt from leaving duplicate history entries behind.
        let turn_id = begin_session_turn(
            prompt,
            context_manager,
            history_manager,
            provider,
            &cancel_rx,
            &self.options,
        )
        .await;

        let turn = match run_fsm_turn_body(
            prompt,
            intent,
            provider,
            model,
            context_manager,
            history_manager,
            protocol,
            policy,
            self.name(),
            stream_tx.clone(),
            cancel_rx,
            &self.options,
        )
        .await
        {
            Ok(turn) => turn,
            Err(err) => {
                rollback_or_log(policy, context_manager.repo_root()).await;
                // The sweep only drops context paths that no longer exist on
                // disk and runs off the async executor thread. The auto layer
                // survives a failed attempt so a retry inherits it.
                crate::blocking::run_blocking(|| {
                    context_manager.prune_missing_files();
                });
                if let Some(id) = turn_id {
                    let _ = history_manager.rollback_entry(id, format!("Turn failed: {}", err));
                }
                return Err(err);
            }
        };

        if turn.is_cancelled {
            if let Some(id) = turn_id {
                let _ = history_manager.rollback_entry(id, "Turn cancelled by the developer");
            }
        } else {
            record_session_response(history_manager, &turn, intent);
        }
        // A refused or repaired plan mutation never fails the turn, but it is a
        // protocol-level diagnostic the developer must see, so the turn stream
        // announces it exactly once.
        announce_plan_diagnostics(&turn, &stream_tx).await;
        // The turn outcome is destructured exhaustively, without a rest pattern,
        // so the packed result cannot drop a field the FSM decides: an omission
        // fails to compile here instead of disappearing from every caller.
        let FsmTurnOutcome {
            assistant_text,
            result,
            is_cancelled,
            blocked,
            protocol_failure,
            has_terminal_success,
            plan_saved,
            plan_errors,
            plan_warnings,
            skip_reason,
            abort,
        } = turn;
        Ok(WorkflowExecutionResult {
            result,
            assistant_text,
            is_cancelled,
            has_terminal_success,
            blocked,
            protocol_failure,
            skip_reason,
            abort,
            plan_saved,
            plan_errors,
            plan_warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use tauqe_protocol::{ContextAccess, ModelRef, ModelResult, PlanRejectionKind, ProviderKind};
    use tokio::sync::{mpsc, watch};

    use crate::context::ContextManager;
    use crate::history::HistoryManager;
    use crate::prompt::protocol::PromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
    use crate::workflow::policy::DirectApplyPolicy;
    use crate::workflow::WorkflowOptions;

    use super::{ExecutionMode, SingleExecution};

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

    /// A conversational reply without edits and without an explicit `<success>`
    /// signal is a bare answer. The single bounded turn returns it unchanged:
    /// the harness no longer climbs the coder ladder inside a turn, so exactly
    /// one generation is spent and the retry policy of a step executor decides
    /// what happens next.
    #[tokio::test]
    async fn single_execution_returns_an_unverified_answer_without_a_second_attempt() {
        let (_dir, repo_root) = repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker(MARKER).unwrap();

        // The spare attempt stays in the script: a turn that answers without edits
        // must never be replayed on a stronger tier.
        let spare_edit = format!(
            "<tauqe_edits_{MARKER} summary=\"Set compute to 42\">\n<edit_{MARKER} path=\"target.rs\">\n<search_{MARKER}>\npub fn compute() -> i32 {{ 0 }}\n</search_{MARKER}>\n<replace_{MARKER}>\npub fn compute() -> i32 {{ 42 }}\n</replace_{MARKER}>\n</edit_{MARKER}>\n</tauqe_edits_{MARKER}>"
        );
        let provider = MockLlmProvider::scripted([
            MockResponse::text("I need more information before changing the code."),
            MockResponse::text(spare_edit),
        ]);
        let model = ModelRef::openrouter("base/model");
        let (tx, _rx) = mpsc::channel(256);
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
            .expect("the turn executes");

        assert!(
            matches!(execution.result, ModelResult::Answer { .. }),
            "an unverified answer is the outcome of the turn: {:?}",
            execution.result
        );
        assert!(
            !execution.has_terminal_success,
            "an unverified answer is not a validated conclusion"
        );
        assert!(!execution.is_cancelled, "the turn ran to completion");
        assert!(execution.abort.is_none(), "no abort report is produced");
        assert_eq!(
            provider.requests().len(),
            1,
            "one bounded turn spends exactly one generation"
        );
        assert_eq!(
            provider.remaining(),
            1,
            "the spare attempt must never be consumed"
        );
        assert_eq!(
            std::fs::read_to_string(repo_root.join("target.rs")).unwrap(),
            "pub fn compute() -> i32 { 0 }\n"
        );
    }

    /// A cancelled turn is a valid outcome rather than a failure, so it still
    /// returns `Ok`. The packed result carries the flag that tells an
    /// interrupted turn apart from an answer that carried no text, so a
    /// supervisor reports it as cancelled instead of failed.
    #[tokio::test]
    async fn single_execution_reports_a_cancelled_turn_as_cancelled() {
        let (_dir, repo_root) = repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let provider = MockLlmProvider::scripted([MockResponse::text("unused")]);
        let model = ModelRef::openrouter("mock/model");
        let (tx, _rx) = mpsc::channel(256);
        // The signal is already raised, so the provider observes the
        // cancellation before it produces a single token.
        let (_cancel_tx, cancel_rx) = watch::channel(true);
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
                &protocol,
                tx,
                cancel_rx,
            )
            .await
            .expect("a cancelled turn is not a failure");

        assert!(execution.is_cancelled, "the turn reports its interruption");
        assert!(
            !execution.has_terminal_success,
            "an interrupted turn concluded nothing"
        );
        assert_eq!(
            std::fs::read_to_string(repo_root.join("target.rs")).unwrap(),
            "pub fn compute() -> i32 { 0 }\n"
        );
        assert!(
            history.get_entries().expect("the history reads").is_empty(),
            "a cancelled turn leaves no recorded entry behind"
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
        // An established plan is protected from REPL-mode mutation: only the
        // Discussion mode may modify it, so the storage engine refuses this
        // payload and its refusal must stay visible to the model and to the
        // developer.
        let existing = tauqe_protocol::Plan::from_parts(
            "existing-plan".to_string(),
            "Existing Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![],
        );
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
        assert_eq!(
            execution.plan_errors[0].kind,
            PlanRejectionKind::ModeForbidden,
            "the regime branch declares the category: {:?}",
            execution.plan_errors
        );
        assert!(
            execution.plan_errors[0].message.contains("REPL mode"),
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
                    && message.contains("REPL mode")),
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

    /// The turn's telemetry carries no escalation notice: a failed attempt is
    /// reported to the caller, and the decision to retry or to escalate belongs
    /// to the step executor, never to the turn itself.
    #[tokio::test]
    async fn single_execution_streams_no_escalation_notice() {
        let (_dir, repo_root) = repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker(MARKER).unwrap();

        let provider = MockLlmProvider::scripted([MockResponse::text(
            "I need more information before changing the code.",
        )]);
        let model = ModelRef::openrouter("base/model");
        let (tx, mut rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut policy = DirectApplyPolicy::new();

        // Drain the stream concurrently: the sender is dropped with the turn, so
        // collecting only afterwards would race the channel shutdown.
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
        .expect("the turn executes");

        let streamed = collector.await.expect("event collector joins");
        assert!(
            !streamed.contains("Level Up Escalation"),
            "the turn must not announce an escalation it no longer performs: {streamed:?}"
        );
        assert_eq!(provider.requests().len(), 1);
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

        // A terminal <success> marks the answer as a validated conclusion. The
        // tag carries the active turn marker, matching how the harness binds
        // structural tags to the turn: an unmarked tag would be treated as
        // literal text and the reply would stay an unverified answer.
        let success =
            format!("The step is already satisfied.\n<success_{MARKER} summary=\"Verified\" />");
        let provider = MockLlmProvider::scripted([MockResponse::text(success)]);
        let model = ModelRef::openrouter("base/model");
        let (tx, _rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut policy = DirectApplyPolicy::new();

        let exec = SingleExecution::new(WorkflowOptions::default());
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
        assert!(
            execution.has_terminal_success,
            "an explicit terminal success validates the answer"
        );
        assert_eq!(
            provider.requests().len(),
            1,
            "a terminal success must not trigger escalation"
        );
    }

    /// An isolated step journals a compact result card instead of the reply the
    /// model wrote, so a batch of steps never inflates the journal with the
    /// conversational text of every step.
    #[tokio::test]
    async fn isolated_step_records_a_compact_result_card() {
        let (_dir, repo_root) = repo_with_target();
        let mut context = ContextManager::new(repo_root.clone());
        context
            .add_file("target.rs", ContextAccess::Editable)
            .expect("add target to context");
        let mut history = HistoryManager::new(repo_root.clone());

        let protocol = crate::prompt::xml::XmlPromptProtocol;
        let protocol = protocol.with_turn_marker(MARKER).unwrap();

        let narrative = "Applying the approved design to the target module.";
        let chunk = format!(
            "{narrative}\n<tauqe_edits_{MARKER} summary=\"Set compute to 42\">\n<edit_{MARKER} path=\"target.rs\">\n<search_{MARKER}>\npub fn compute() -> i32 {{ 0 }}\n</search_{MARKER}>\n<replace_{MARKER}>\npub fn compute() -> i32 {{ 42 }}\n</replace_{MARKER}>\n</edit_{MARKER}>\n</tauqe_edits_{MARKER}>\n<success_{MARKER} commit_message=\"Set compute to 42\" />"
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
                "Execute the approved step",
                tauqe_protocol::TurnIntent::IsolatedStep,
                &provider,
                &model,
                &mut context,
                &mut history,
                protocol.as_ref(),
                tx,
                cancel_rx,
            )
            .await
            .expect("the isolated step executes");

        assert!(
            matches!(&execution.result, ModelResult::Edit { outcome, .. } if outcome.applied),
            "the step must apply its change set: {:?}",
            execution.result
        );

        let entries = history.get_entries().expect("the journal reads");
        assert_eq!(
            entries.len(),
            2,
            "the step journals the card that opened it and the card that closed it"
        );
        match &entries[1] {
            crate::history::HistoryEntry::Response {
                message, commit, ..
            } => {
                assert_eq!(message, "Step completed: Set compute to 42");
                assert!(
                    commit.is_none(),
                    "a direct policy records no commit on the card"
                );
                assert!(
                    !message.contains(narrative),
                    "the card never carries the reply of the model: {message}"
                );
            }
            other => panic!("expected the step result card, got {other:?}"),
        }
    }
}
