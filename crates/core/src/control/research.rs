//! Context research and investigation control loops.
//!
//! This module provides [`handle_research_commands`] for draining and executing
//! discovery commands against [`crate::context::ContextManager`] and declared project
//! tools, and [`run_research_loop`] for autonomous multi-round context enrichment.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use tauqe_protocol::{ContextAccess, LlmCapability, LlmCommand, TurnPhase};

use super::context::LoopContext;
use super::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use crate::providers::StreamEvent;
use crate::workflow::research::{execute_research_commands, ResearchLimits};
/// Capabilities processed by the research and investigation contour.
pub const RESEARCH_CAPABILITIES: &[LlmCapability] = &[
    LlmCapability::Research,
    LlmCapability::DocRequest,
    LlmCapability::OuterDocRequest,
    LlmCapability::CustomTools,
    LlmCapability::Scratchpad,
];

/// Feedback and outcomes aggregated from executing research commands.
#[derive(Debug, Clone, Default)]
pub struct ResearchFeedback {
    /// Repository files added to the auto context layer.
    pub added_files: Vec<String>,
    /// Repository files requested that were missing from disk.
    pub missing_files: Vec<String>,
    /// Rendered feedback blocks from regex searches, directory outlines, or system documentation.
    pub feedback_blocks: Vec<String>,
    /// Whether turn scratchpad notes were recorded into the journal.
    pub scratchpad_recorded: bool,
}

impl ResearchFeedback {
    /// Returns true if the round produced no research progress.
    ///
    /// A scratchpad note is ephemeral working memory: it widens no context and
    /// discovers no repository knowledge, so on its own it never warrants
    /// another discovery round. The note still reaches the turn journal through
    /// [`handle_research_commands`], but the research phase must not spend a
    /// round on it.
    pub fn is_empty(&self) -> bool {
        self.added_files.is_empty()
            && self.missing_files.is_empty()
            && self.feedback_blocks.is_empty()
    }
}

/// Flat configuration of the research loop.
#[derive(Debug, Clone)]
pub struct ResearchLoopConfig {
    /// Maximum consecutive research rounds granted.
    pub max_rounds: usize,
}

impl ResearchLoopConfig {
    /// Creates research loop configuration from project context configuration.
    pub fn from_context(config: &crate::config::ContextConfig) -> Self {
        Self {
            max_rounds: config.max_research_rounds,
        }
    }
}

impl Default for ResearchLoopConfig {
    fn default() -> Self {
        Self {
            max_rounds: crate::config::ContextConfig::default().max_research_rounds,
        }
    }
}

/// Artifact produced by a successful research phase.
#[derive(Debug, Clone, Default)]
pub struct ResearchArtifact {
    /// Number of research rounds executed.
    pub rounds_spent: usize,
    /// Files added to the context manager during research.
    pub files_added: Vec<String>,
}

/// Drains research commands from the envelope and executes them.
///
/// Returns `Ok(None)` if no research commands were present in the envelope.
pub async fn handle_research_commands(
    ctx: &mut LoopContext<'_>,
    envelope: &mut ResponseEnvelope,
) -> Result<Option<ResearchFeedback>, SystemError> {
    ctx.check_cancelled()?;

    // Drop any ambient language directive without defecting
    let _ = envelope.take(LlmCapability::UserLanguage);

    let mut commands = envelope.take_any(RESEARCH_CAPABILITIES);
    if commands.is_empty() {
        return Ok(None);
    }

    let mut feedback = ResearchFeedback::default();

    // Partition custom tools, scratchpad, and codebase research commands
    let mut tool_invocations = Vec::new();
    let mut regular_commands = Vec::new();

    for cmd in commands.drain(..) {
        match cmd {
            LlmCommand::Scratchpad { notes } => {
                if !notes.trim().is_empty() {
                    ctx.set_scratchpad(Some(notes.trim().to_string()));
                    ctx.journal
                        .push_block(format!("<scratchpad>\n{}\n</scratchpad>", notes.trim()));
                    feedback.scratchpad_recorded = true;
                }
            }
            LlmCommand::CustomTool { name, args } => {
                tool_invocations.push((name, args));
            }
            other => regular_commands.push(other),
        }
    }

    // 1. Execute outer docs materialization if requested
    let outer_docs_entries: Vec<crate::config::OuterDocEntry> = crate::blocking::run_blocking({
        let root = ctx.repo_root.to_path_buf();
        move || crate::config::load_section(&root, "outer_docs")
    });
    let registry = crate::outer_docs::OuterDocsRegistry::new(&outer_docs_entries);

    let mut outer_docs_failures = Vec::new();
    let mut outer_docs_cache_requests = Vec::new();
    for cmd in &regular_commands {
        if let LlmCommand::OuterDocRequest { id } = cmd {
            let id = id.trim();
            if !id.is_empty() {
                match registry
                    .materialize_declared(
                        ctx.repo_root,
                        id,
                        crate::outer_docs::MAX_DOCUMENT_BYTES,
                        Some(&ctx.cancel.receiver()),
                    )
                    .await
                {
                    Ok(cache_path) => {
                        outer_docs_cache_requests.push(LlmCommand::ContextRequest {
                            path: cache_path,
                            access: ContextAccess::ReadOnly,
                        });
                    }
                    Err(err) => {
                        outer_docs_failures.push((id.to_string(), err));
                    }
                }
            }
        }
    }
    regular_commands.extend(outer_docs_cache_requests);
    // 2. Execute codebase research commands (context requests, searches, maps, docs)
    if !regular_commands.is_empty() {
        let mut outcome = execute_research_commands(
            ctx.context_manager,
            ctx.repo_root,
            &regular_commands,
            300_000,
            crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
            ResearchLimits::default(),
            true,
            Some(&ctx.cancel.receiver()),
        )
        .await;

        for (path, err) in outer_docs_failures {
            outcome.record_outer_docs_failure(&path, &err);
        }

        ctx.journal.record_outcome(&outcome);

        if !outcome.added_files.is_empty() {
            ctx.events
                .emit(StreamEvent::ContextChanged(ctx.context_manager.get_state()));
        }

        feedback.added_files = outcome.added_files;
        feedback.missing_files = outcome.missing_files;
        feedback.feedback_blocks.extend(outcome.feedback_blocks);
    }

    // 3. Execute project command custom tools against declared allowlist
    if !tool_invocations.is_empty() {
        let project =
            crate::toolchain::effective_project_config(ctx.repo_root, Some(&ctx.cancel.receiver()))
                .await;

        for (name, args) in tool_invocations {
            ctx.check_cancelled()?;
            let block =
                execute_custom_tool(ctx.repo_root, &project, &name, &args, ctx.cancel).await;
            ctx.journal.push_block(block.as_str());
            feedback.feedback_blocks.push(block);
        }
    }

    Ok(Some(feedback))
}

/// Autonomous multi-round context research loop.
///
/// The phase ends on exactly three boundaries:
///
/// 1. A round carried no discovery command, so the phase has nothing left to
///    spend.
/// 2. A round carried a command a higher contour owns — a change set, a plan
///    operation, a review decision, an outcome — so the round was more than one
///    discovery step and its envelope travels upward untouched.
/// 3. A round carried the discovery commands and nothing else, which is the
///    ordinary opening of an investigation: the round was spent exactly once
///    more and the model is asked again against the context it widened.
///
/// Narration is not a boundary. A round that asks for material and describes
/// what it is looking for is a preamble, not the answer, so the loop keeps
/// going while the envelope holds nothing but the narration; the answer arrives
/// in the round that has the material in front of it.
pub async fn run_research_loop(
    ctx: &mut LoopContext<'_>,
    mut current_envelope: ResponseEnvelope,
    config: &ResearchLoopConfig,
) -> Result<LoopResult<ResearchArtifact>, SystemError> {
    ctx.check_cancelled()?;

    let mut rounds_spent = 0;
    let mut files_added = Vec::new();

    loop {
        ctx.check_cancelled()?;

        let Some(feedback) = handle_research_commands(ctx, &mut current_envelope).await? else {
            return Ok(LoopResult::Success(LoopSuccess::new(
                ResearchArtifact {
                    rounds_spent,
                    files_added,
                },
                current_envelope,
            )));
        };

        // If the research commands produced no new knowledge or files (e.g. empty research branch
        // sent by model to provide final conversational answer), research is finished.
        if feedback.is_empty() {
            return Ok(LoopResult::Success(LoopSuccess::new(
                ResearchArtifact {
                    rounds_spent,
                    files_added,
                },
                current_envelope,
            )));
        }

        files_added.extend(feedback.added_files.clone());
        rounds_spent += 1;

        // Emit banner text delta so the user sees round summary in the UI chat log
        let banner_summary = format_round_summary(&feedback);
        let banner = format!(
            "\n\n---\n**[Round {}]** {}\n\n",
            rounds_spent, banner_summary
        );
        ctx.events.emit(StreamEvent::TextDelta(banner));

        ctx.events.emit(StreamEvent::TurnPhase {
            phase: TurnPhase::Research,
            round: Some(rounds_spent),
            max_rounds: Some(config.max_rounds),
            detail: Some(format!(
                "Research round {}/{}",
                rounds_spent, config.max_rounds
            )),
        });

        if holds_a_higher_contour_command(&current_envelope) {
            return Ok(LoopResult::Success(LoopSuccess::new(
                ResearchArtifact {
                    rounds_spent,
                    files_added,
                },
                current_envelope,
            )));
        }

        if rounds_spent >= config.max_rounds {
            let warn = format!(
                "\n\n---\n⚠️ **[Research Limit Reached]** Maximum research rounds ({}) reached. No further repository material will be loaded in this turn.\n\n",
                config.max_rounds
            );
            ctx.events.emit(StreamEvent::TextDelta(warn));
            ctx.journal.push_block(
                "<response_directive>\nThe research round limit is exhausted: no further files, repository searches, directory listings, or documentation will be loaded in this turn. Your investigation for this turn is complete. Do not request additional paths. Answer the task from the material you already hold, stating the conclusions or emitting the file edits the gathered context supports.\n</response_directive>",
            );
            current_envelope = ctx.request_round().await?;
            return Ok(LoopResult::Success(LoopSuccess::new(
                ResearchArtifact {
                    rounds_spent,
                    files_added,
                },
                current_envelope,
            )));
        }
        current_envelope = ctx.request_round().await?;
    }
}

fn format_round_summary(feedback: &ResearchFeedback) -> String {
    let mut parts = Vec::new();
    if !feedback.added_files.is_empty() {
        parts.push(format!("{} file(s) added", feedback.added_files.len()));
    }
    if !feedback.missing_files.is_empty() {
        parts.push(format!("{} file(s) missing", feedback.missing_files.len()));
    }
    if !feedback.feedback_blocks.is_empty() {
        parts.push(format!(
            "{} feedback block(s)",
            feedback.feedback_blocks.len()
        ));
    }
    if feedback.scratchpad_recorded {
        parts.push("scratchpad noted".to_string());
    }
    if parts.is_empty() {
        "no new files added.".to_string()
    } else {
        format!("{}.", parts.join(", "))
    }
}

/// Returns whether the residual of a discovery round belongs to a contour above.
///
/// The narration of a discovery round is the model saying what it is looking
/// for; the research stage is the wrong place to read it, because the round
/// already proved the investigation is not finished. Every other capability is
/// a branch a higher contour owns, so the envelope stops here and travels up.
fn holds_a_higher_contour_command(envelope: &ResponseEnvelope) -> bool {
    envelope
        .remaining_capabilities()
        .into_iter()
        .any(|capability| capability != LlmCapability::TextMessage)
}

/// Executes one custom tool invocation against the declared project allowlist.
async fn execute_custom_tool(
    repo_root: &Path,
    project: &tauqe_protocol::ProjectConfig,
    name: &str,
    args: &Value,
    cancel: &super::context::CancellationToken,
) -> String {
    let arguments = match parse_named_arguments(args) {
        Ok(args) => args,
        Err(err) => {
            return format!(
                "<project_command_feedback name=\"{}\" success=\"false\">\nrejected: {}\n</project_command_feedback>",
                crate::escape::escape_xml_attr(name),
                crate::escape::escape_xml_text(&err)
            );
        }
    };

    match crate::toolchain::run_project_command_with_config(
        repo_root,
        project,
        name,
        &arguments,
        Some(&cancel.receiver()),
    )
    .await
    {
        Ok(result) => {
            let mut block = format!(
                "<project_command_feedback name=\"{}\" success=\"{}\">\nexit status: {}",
                crate::escape::escape_xml_attr(name),
                result.success,
                if result.success { "success" } else { "failure" }
            );
            if !result.stdout.trim().is_empty() {
                block.push_str(&format!("\nstdout:\n{}", result.stdout.trim()));
            }
            if !result.stderr.trim().is_empty() {
                block.push_str(&format!("\nstderr:\n{}", result.stderr.trim()));
            }
            block.push_str("\n</project_command_feedback>");
            block
        }
        Err(err) => format!(
            "<project_command_feedback name=\"{}\" success=\"false\">\nrejected: {}\n</project_command_feedback>",
            crate::escape::escape_xml_attr(name),
            crate::escape::escape_xml_text(&err)
        ),
    }
}

fn parse_named_arguments(args: &Value) -> Result<BTreeMap<String, String>, String> {
    match args {
        Value::Null => Ok(BTreeMap::new()),
        Value::Object(map) => {
            let mut arguments = BTreeMap::new();
            for (name, value) in map {
                let val_str = match value {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => {
                        return Err(format!(
                            "the parameter `{name}` carries a structured value; supply a literal value"
                        ));
                    }
                };
                arguments.insert(name.clone(), val_str);
            }
            Ok(arguments)
        }
        _ => Err("arguments must be provided as a key-value object".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink};
    use crate::prompt::xml::XmlPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;
    use tauqe_protocol::{CapabilitySet, DocTopic, EditOperation, ModelRef};

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: TurnResearchJournal,
        provider: MockLlmProvider,
        model: ModelRef,
        protocol: XmlPromptProtocol,
        cache: crate::workflow::fsm::cache::TurnInputCache,
        cancel: CancellationToken,
        events: EventSink,
        policy: crate::workflow::policy::DirectApplyPolicy,
    }

    impl Fixture {
        fn new(responses: Vec<MockResponse>) -> Self {
            let dir = tempfile::tempdir().expect("scratch directory");
            let root = dir.path().to_path_buf();
            let context = ContextManager::new(root);
            Self {
                dir,
                context,
                journal: TurnResearchJournal::default(),
                provider: MockLlmProvider::scripted(responses),
                model: ModelRef::openrouter("mock/model"),
                protocol: XmlPromptProtocol,
                cache: crate::workflow::fsm::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::policy::DirectApplyPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let mut caps = CapabilitySet::from_slice(RESEARCH_CAPABILITIES);
            caps.insert(LlmCapability::TextMessage);
            let prompt_builder = super::super::prompt::PromptBuilder::new(
                &self.provider,
                &self.model,
                &self.protocol,
                &mut self.cache,
            );
            LoopContext::new(
                &self.cancel,
                &self.events,
                self.dir.path(),
                &mut self.context,
                &mut self.journal,
                &mut self.policy,
                caps,
                prompt_builder,
            )
        }
    }

    #[tokio::test]
    async fn handle_research_commands_noop_when_no_research() {
        let mut fixture = Fixture::new(Vec::new());
        let mut envelope = ResponseEnvelope::from_commands(&[LlmCommand::Text {
            content: "hello".to_string(),
        }]);
        let mut ctx = fixture.context();

        let result = handle_research_commands(&mut ctx, &mut envelope)
            .await
            .expect("executes");
        assert!(result.is_none());
        assert_eq!(envelope.len(), 1);
    }

    #[tokio::test]
    async fn handle_research_commands_executes_doc_request() {
        let mut fixture = Fixture::new(Vec::new());
        let mut envelope = ResponseEnvelope::from_commands(&[LlmCommand::DocRequest {
            topic: DocTopic::Workflow,
        }]);
        let mut ctx = fixture.context();

        let result = handle_research_commands(&mut ctx, &mut envelope)
            .await
            .expect("executes");
        assert!(result.is_some());
        let fb = result.unwrap();
        assert!(!fb.feedback_blocks.is_empty());
        assert!(envelope.is_empty());
        assert!(!fixture.journal.is_empty());
    }

    #[tokio::test]
    async fn run_research_loop_completes_immediately_when_no_research_commands() {
        let mut fixture = Fixture::new(Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::Text {
            content: "direct answer".to_string(),
        }]);
        let mut ctx = fixture.context();

        let outcome = run_research_loop(&mut ctx, envelope, &ResearchLoopConfig::default())
            .await
            .expect("runs loop");

        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.rounds_spent, 0);
        assert_eq!(success.response.len(), 1);
    }

    #[tokio::test]
    async fn run_research_loop_executes_round_and_returns_success() {
        let responses = vec![MockResponse::text("Here is the plan analysis.")];
        let mut fixture = Fixture::new(responses);
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::DocRequest {
            topic: DocTopic::Workflow,
        }]);
        let mut ctx = fixture.context();

        let outcome = run_research_loop(&mut ctx, envelope, &ResearchLoopConfig::default())
            .await
            .expect("runs loop");

        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.rounds_spent, 1);
        assert!(!success.response.is_empty());
    }

    #[tokio::test]
    async fn run_research_loop_continues_past_a_round_that_only_narrates() {
        let responses = vec![MockResponse::text("The workflow chapter is loaded.")];
        let mut fixture = Fixture::new(responses);
        let envelope = ResponseEnvelope::from_commands(&[
            LlmCommand::Text {
                content: "Let me read the workflow chapter first.".to_string(),
            },
            LlmCommand::DocRequest {
                topic: DocTopic::Workflow,
            },
        ]);
        let mut ctx = fixture.context();

        let outcome = run_research_loop(&mut ctx, envelope, &ResearchLoopConfig::default())
            .await
            .expect("runs loop");

        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.rounds_spent, 1);
        assert_eq!(
            fixture.provider.requests().len(),
            1,
            "the narration of a discovery round does not end the phase"
        );
        let texts: Vec<&str> = success
            .response
            .commands()
            .iter()
            .filter_map(|command| match command {
                LlmCommand::Text { content } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["The workflow chapter is loaded."]);
    }

    #[tokio::test]
    async fn run_research_loop_hands_a_change_set_up_without_another_round() {
        let mut fixture = Fixture::new(Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[
            LlmCommand::DocRequest {
                topic: DocTopic::Workflow,
            },
            LlmCommand::Edit {
                operation: EditOperation::Replace {
                    path: "a.rs".to_string(),
                    old_text: "pub fn compute() -> i32 { 0 }".to_string(),
                    new_text: "pub fn compute() -> i32 { 42 }".to_string(),
                },
            },
        ]);
        let mut ctx = fixture.context();

        let outcome = run_research_loop(&mut ctx, envelope, &ResearchLoopConfig::default())
            .await
            .expect("runs loop");

        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.rounds_spent, 1);
        assert!(success.response.has(LlmCapability::FileEdits));
        assert!(fixture.provider.requests().is_empty());
    }

    #[tokio::test]
    async fn handle_research_commands_absorbs_scratchpad() {
        let mut fixture = Fixture::new(Vec::new());
        let mut envelope = ResponseEnvelope::from_commands(&[LlmCommand::Scratchpad {
            notes: "initial thoughts".to_string(),
        }]);
        let mut ctx = fixture.context();

        let result = handle_research_commands(&mut ctx, &mut envelope)
            .await
            .expect("executes");
        assert!(result.is_some());
        let fb = result.unwrap();
        assert!(fb.scratchpad_recorded);
        assert!(envelope.is_empty());
        assert!(!fixture.journal.is_empty());
    }
    /// A round that carries only conversational text and a scratchpad note
    /// makes no research progress, so the phase terminates with the answer
    /// intact and never asks the model again.
    #[tokio::test]
    async fn run_research_loop_terminates_after_a_scratchpad_only_round() {
        let mut fixture = Fixture::new(Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[
            LlmCommand::Text {
                content: "The function already returns 42.".to_string(),
            },
            LlmCommand::Scratchpad {
                notes: "checked the call site".to_string(),
            },
        ]);
        let mut ctx = fixture.context();

        let outcome = run_research_loop(&mut ctx, envelope, &ResearchLoopConfig::default())
            .await
            .expect("runs loop");

        let success = outcome.into_success().expect("success");
        assert_eq!(
            success.value.rounds_spent, 0,
            "a scratchpad note is not research progress"
        );
        assert!(
            success.response.has(LlmCapability::TextMessage),
            "the conversational answer travels upward untouched"
        );
        assert!(
            fixture.provider.requests().is_empty(),
            "no further research round is requested"
        );
        assert!(
            !fixture.journal.is_empty(),
            "the note is recorded into the turn journal"
        );
    }

    #[tokio::test]
    async fn run_research_loop_completes_when_max_rounds_reached() {
        let responses = vec![MockResponse::text("Continued without loop")];
        let mut fixture = Fixture::new(responses);
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::DocRequest {
            topic: DocTopic::Workflow,
        }]);
        let mut ctx = fixture.context();
        let config = ResearchLoopConfig { max_rounds: 1 };

        let outcome = run_research_loop(&mut ctx, envelope, &config)
            .await
            .expect("runs loop");

        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.rounds_spent, 1);
    }

    #[test]
    fn research_loop_config_from_context_inherits_max_rounds() {
        let context_config = crate::config::ContextConfig {
            max_research_rounds: 10,
            ..Default::default()
        };
        let config = ResearchLoopConfig::from_context(&context_config);
        assert_eq!(config.max_rounds, 10);
    }
}
