//! Context research and investigation control loops.
//!
//! This module provides [`handle_research_commands`] for draining and executing
//! discovery commands against [`crate::context::ContextManager`] and declared project
//! tools, and [`run_research_loop`] for autonomous multi-round context enrichment.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use tauqe_protocol::{ContextAccess, ContextRequestCommand, LlmCapability, LlmCommand, TurnPhase};

use crate::control::context::LoopContext;
use crate::control::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use crate::providers::StreamEvent;
use crate::workflow::research::{execute_research_commands, ResearchLimits};
/// Capabilities processed by the research and investigation contour.
pub const RESEARCH_CAPABILITIES: &[LlmCapability] = &[
    LlmCapability::ContextRequest,
    LlmCapability::RepoSearch,
    LlmCapability::RepoDirs,
    LlmCapability::RepoMap,
    LlmCapability::DirList,
    LlmCapability::DocRequest,
    LlmCapability::OuterDocRequest,
    LlmCapability::CustomTools,
];

/// Maximum consecutive rounds granted to deliver an already-satisfied files explanation.
const MAX_SATISFIED_RESEARCH_ROUNDS: usize = 1;

/// Maximum consecutive empty research rounds allowed before terminating the phase.
const MAX_EMPTY_RESEARCH_ROUNDS: usize = 1;

/// Directive appended to the journal when a discovery round widened nothing.
///
/// The trigger is [`ResearchFeedback::is_empty`]: the round added no file,
/// reported no missing path, ran no search, rendered no outline, listing, or
/// targeted map, loaded no document, and explained nothing already loaded.
/// The working tree could not be inspected, the context budget refused the
/// material, or the requests carried nothing the harness could resolve, so
/// the directive names exactly those causes and forbids repeating them.
const EMPTY_RESEARCH_DIRECTIVE: &str = "<response_directive>\nYour discovery round did not widen the context: either the working tree could not be inspected, the declared context budget refused the requested material, or the requests carried nothing the harness could resolve into a file, a search, a listing, an outline, or a document. Repeating the same requests would produce the same result, so do not repeat them. Close the round with one of the branches the schema declares: request material that exists in the working tree (take exact paths from <repo_map> and <repo_dirs>), answer the task through the 'answer' branch, or deliver the code changes through the 'edit' branch.\n</response_directive>";

/// Directive appended to the journal when a round only repeated requests the context already satisfied.
const ALREADY_SATISFIED_DIRECTIVE: &str = "<response_directive>\nEvery file you requested is already loaded in <context>. Do not request those paths again: answer the task from the material you already hold. Write the analysis or the conclusion in plain conversational text, and emit file edits only when the code itself must change.\n</response_directive>";

/// Directive appended to the journal when the research round limit is exhausted.
const RESEARCH_LIMIT_DIRECTIVE: &str = "<response_directive>\nThe research round limit is exhausted: no further files, repository searches, directory listings, or documentation will be loaded in this turn. Your investigation for this turn is complete. Do not request additional paths. Answer the task from the material you already hold, stating the conclusions or emitting the file edits the gathered context supports.\n</response_directive>";

/// Feedback and outcomes aggregated from executing research commands.
#[derive(Debug, Clone, Default)]
pub struct ResearchFeedback {
    /// Repository files added to the auto context layer.
    pub added_files: Vec<String>,
    /// Repository files requested that were missing from disk.
    pub missing_files: Vec<String>,
    /// Repository files requested that were already present in context.
    pub already_satisfied_files: Vec<String>,
    /// Rendered feedback blocks from regex searches, directory outlines, or system documentation.
    pub feedback_blocks: Vec<String>,
    /// Whether turn scratchpad notes were recorded into the journal.
    pub scratchpad_recorded: bool,
    /// Whether the round widened repository knowledge (added files, missing files, searches, docs, tools).
    pub widened: bool,
}

impl ResearchFeedback {
    /// Returns true if the round produced no research progress or hint.
    pub fn is_empty(&self) -> bool {
        !self.widened && self.already_satisfied_files.is_empty()
    }
}

/// Flat configuration of the research loop.
#[derive(Debug, Clone)]
pub struct ResearchLoopConfig {
    /// Maximum consecutive research rounds granted.
    pub max_rounds: usize,
    /// Aggregate token budget of the context a round may widen.
    pub token_budget: usize,
    /// Token budget of a targeted repository map rendered by the round.
    pub repomap_token_budget: usize,
    /// Operational caps applied to one research round.
    pub research_limits: ResearchLimits,
}

impl ResearchLoopConfig {
    /// Creates research loop configuration from project context configuration.
    pub fn from_context(config: &crate::config::ContextConfig) -> Self {
        Self {
            max_rounds: config.max_research_rounds,
            token_budget: config.token_budget,
            repomap_token_budget: config.repomap_token_budget,
            research_limits: ResearchLimits::from_context_config(config),
        }
    }
}

impl Default for ResearchLoopConfig {
    fn default() -> Self {
        Self::from_context(&crate::config::ContextConfig::default())
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
pub fn drain_scratchpad(ctx: &mut LoopContext<'_>, envelope: &mut ResponseEnvelope) -> bool {
    let mut recorded = false;
    for cmd in envelope.take(LlmCapability::Scratchpad) {
        if let LlmCommand::Scratchpad(command) = cmd {
            let notes = command.notes;
            if !notes.trim().is_empty() {
                ctx.set_scratchpad(Some(notes.trim().to_string()));
                ctx.journal
                    .push_block(format!("<scratchpad>\n{}\n</scratchpad>", notes.trim()));
                recorded = true;
            }
        }
    }
    recorded
}

pub async fn handle_research_commands(
    ctx: &mut LoopContext<'_>,
    envelope: &mut ResponseEnvelope,
    config: &ResearchLoopConfig,
) -> Result<Option<ResearchFeedback>, SystemError> {
    ctx.check_cancelled()?;

    let scratchpad_recorded = drain_scratchpad(ctx, envelope);

    let mut commands = envelope.take_any(RESEARCH_CAPABILITIES);
    if commands.is_empty() {
        return Ok(None);
    }

    let mut feedback = ResearchFeedback {
        scratchpad_recorded,
        ..Default::default()
    };

    // Partition custom tools and codebase research commands
    let mut tool_invocations = Vec::new();
    let mut regular_commands = Vec::new();

    for cmd in commands.drain(..) {
        match cmd {
            LlmCommand::CustomTool(command) => {
                let (name, args) = (command.name, command.args);
                tool_invocations.push((name, args));
            }
            other => regular_commands.push(other),
        }
    }

    // 1. Execute outer docs materialization if requested
    let outer_docs_entries = if !ctx.prompt_builder.outer_docs.is_empty() {
        ctx.prompt_builder.outer_docs.clone()
    } else {
        crate::blocking::run_blocking({
            let root = ctx.repo_root.to_path_buf();
            move || crate::config::load_section(&root, "outer_docs")
        })
    };
    let registry = crate::outer_docs::OuterDocsRegistry::new(&outer_docs_entries);

    let mut outer_docs_failures = Vec::new();
    let mut outer_docs_cache_requests = Vec::new();
    for cmd in &regular_commands {
        match cmd {
            LlmCommand::ContextRequest(command) => {
                let path = &command.path;
                if crate::outer_docs::is_outer_docs_cache_path(path) {
                    match registry
                        .materialize_request(
                            ctx.repo_root,
                            path,
                            crate::outer_docs::MAX_DOCUMENT_BYTES,
                            Some(&ctx.cancel.receiver()),
                        )
                        .await
                    {
                        Ok(Some(cache_path)) => {
                            tracing::info!(
                                path = cache_path.as_str(),
                                "outer documentation materialized for the research round"
                            );
                        }
                        Ok(None) => {}
                        Err(err) => {
                            outer_docs_failures.push((path.clone(), err));
                        }
                    }
                }
            }
            LlmCommand::OuterDocRequest(command) => {
                let id = command.id.trim();
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
                            outer_docs_cache_requests.push(LlmCommand::ContextRequest(
                                ContextRequestCommand {
                                    path: cache_path,
                                    access: ContextAccess::ReadOnly,
                                },
                            ));
                        }
                        Err(err) => {
                            outer_docs_failures.push((id.to_string(), err));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    regular_commands.extend(outer_docs_cache_requests);
    // 2. Execute codebase research commands (context requests, searches, maps, docs)
    if !regular_commands.is_empty() {
        let mut outcome = execute_research_commands(
            ctx.context_manager,
            ctx.repo_root,
            &regular_commands,
            config.token_budget,
            config.repomap_token_budget,
            config.research_limits,
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

        feedback.widened = outcome.has_progress(false);
        feedback.added_files = outcome.added_files;
        feedback.missing_files = outcome.missing_files;
        feedback.already_satisfied_files = outcome.already_satisfied_files;
        feedback.feedback_blocks.extend(outcome.feedback_blocks);
    }

    // 3. Execute project command custom tools against declared allowlist
    if !tool_invocations.is_empty() {
        feedback.widened = true;
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
    let mut satisfied_rounds = 0usize;
    let mut empty_rounds = 0usize;
    let mut has_model_prose = envelope_has_text(&current_envelope);

    loop {
        ctx.check_cancelled()?;

        if rounds_spent >= config.max_rounds && envelope_has_research_commands(&current_envelope) {
            let warn = format!(
                "\n\n---\n⚠️ **[Research Limit Reached]** Maximum research rounds ({}) reached. No further repository material will be loaded in this turn.\n\n",
                config.max_rounds
            );
            ctx.events.emit(StreamEvent::TextDelta(warn));
            ctx.journal.push_block(RESEARCH_LIMIT_DIRECTIVE);
            let _ = current_envelope.take_any(RESEARCH_CAPABILITIES);
            current_envelope = ctx.request_round().await?;
            return Ok(LoopResult::Success(LoopSuccess::new(
                ResearchArtifact {
                    rounds_spent,
                    files_added,
                },
                current_envelope,
            )));
        }

        let research = handle_research_commands(ctx, &mut current_envelope, config).await?;
        let Some(feedback) = research else {
            return Ok(LoopResult::Success(LoopSuccess::new(
                ResearchArtifact {
                    rounds_spent,
                    files_added,
                },
                current_envelope,
            )));
        };
        if feedback.is_empty() {
            if empty_rounds >= MAX_EMPTY_RESEARCH_ROUNDS {
                return Ok(LoopResult::Success(LoopSuccess::new(
                    ResearchArtifact {
                        rounds_spent,
                        files_added,
                    },
                    current_envelope,
                )));
            }
            empty_rounds += 1;
            ctx.journal.push_block(EMPTY_RESEARCH_DIRECTIVE);
            current_envelope = ctx.request_round().await?;
            has_model_prose |= envelope_has_text(&current_envelope);
            continue;
        }

        let hint_only = !feedback.widened
            && !feedback.already_satisfied_files.is_empty()
            && !envelope_has_text(&current_envelope)
            && !has_model_prose;

        if !feedback.widened && !hint_only {
            return Ok(LoopResult::Success(LoopSuccess::new(
                ResearchArtifact {
                    rounds_spent,
                    files_added,
                },
                current_envelope,
            )));
        }

        if hint_only {
            if satisfied_rounds >= MAX_SATISFIED_RESEARCH_ROUNDS {
                return Ok(LoopResult::Success(LoopSuccess::new(
                    ResearchArtifact {
                        rounds_spent,
                        files_added,
                    },
                    current_envelope,
                )));
            }
            satisfied_rounds += 1;
            ctx.journal.push_block(ALREADY_SATISFIED_DIRECTIVE);
        } else {
            rounds_spent += 1;
        }

        files_added.extend(feedback.added_files.clone());

        let banner_summary = format_round_summary(&feedback);
        let banner = format!(
            "\n\n---\n**[Round {}]** {}\n\n",
            if hint_only {
                rounds_spent + 1
            } else {
                rounds_spent
            },
            banner_summary
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

        current_envelope = ctx.request_round().await?;
        has_model_prose |= envelope_has_text(&current_envelope);
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
    if !feedback.already_satisfied_files.is_empty() && feedback.added_files.is_empty() {
        parts.push(format!(
            "{} file(s) already in context",
            feedback.already_satisfied_files.len()
        ));
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
fn holds_a_higher_contour_command(envelope: &ResponseEnvelope) -> bool {
    envelope
        .remaining_capabilities()
        .into_iter()
        .any(|capability| {
            capability != LlmCapability::TextMessage
                && capability != LlmCapability::ShortDescription
        })
}

fn envelope_has_research_commands(envelope: &ResponseEnvelope) -> bool {
    RESEARCH_CAPABILITIES.iter().any(|cap| envelope.has(*cap))
}

fn envelope_has_text(envelope: &ResponseEnvelope) -> bool {
    envelope.commands().iter().any(|cmd| match cmd {
        LlmCommand::Text(command) => !command.content.trim().is_empty(),
        LlmCommand::ShortDescription(command) => !command.content.trim().is_empty(),
        _ => false,
    })
}

/// Executes one custom tool invocation against the declared project allowlist.
async fn execute_custom_tool(
    repo_root: &Path,
    project: &tauqe_protocol::ProjectConfig,
    name: &str,
    args: &Value,
    cancel: &crate::control::context::CancellationToken,
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
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;
    use tauqe_protocol::{
        CapabilityExpr, DocRequestCommand, DocTopic, EditReplace, ModelRef, ScratchpadCommand,
        TextCommand,
    };

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: TurnResearchJournal,
        provider: MockLlmProvider,
        model: ModelRef,
        protocol: JsonPromptProtocol,
        cache: crate::control::cache::TurnInputCache,
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
                protocol: JsonPromptProtocol::edits(),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::policy::DirectApplyPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = CapabilityExpr::set(
                RESEARCH_CAPABILITIES
                    .iter()
                    .copied()
                    .chain([LlmCapability::TextMessage])
                    .map(CapabilityExpr::leaf),
            );
            let prompt_builder = crate::control::prompt::PromptBuilder::new(
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
        let mut envelope = ResponseEnvelope::from_commands(&[LlmCommand::Text(TextCommand {
            content: "hello".to_string(),
        })]);
        let mut ctx = fixture.context();

        let result =
            handle_research_commands(&mut ctx, &mut envelope, &ResearchLoopConfig::default())
                .await
                .expect("executes");
        assert!(result.is_none());
        assert_eq!(envelope.len(), 1);
    }

    #[tokio::test]
    async fn handle_research_commands_executes_doc_request() {
        let mut fixture = Fixture::new(Vec::new());
        let mut envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::Workflow,
            })]);
        let mut ctx = fixture.context();

        let result =
            handle_research_commands(&mut ctx, &mut envelope, &ResearchLoopConfig::default())
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
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::Text(TextCommand {
            content: "direct answer".to_string(),
        })]);
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
        let envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::Workflow,
            })]);
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
            LlmCommand::Text(TextCommand {
                content: "Let me read the workflow chapter first.".to_string(),
            }),
            LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::Workflow,
            }),
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
                LlmCommand::Text(command) => Some(command.content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["The workflow chapter is loaded."]);
    }

    #[tokio::test]
    async fn run_research_loop_hands_a_change_set_up_without_another_round() {
        let mut fixture = Fixture::new(Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[
            LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::Workflow,
            }),
            LlmCommand::EditReplace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "pub fn compute() -> i32 { 0 }".to_string(),
                new_text: "pub fn compute() -> i32 { 42 }".to_string(),
            }),
        ]);
        let mut ctx = fixture.context();

        let outcome = run_research_loop(&mut ctx, envelope, &ResearchLoopConfig::default())
            .await
            .expect("runs loop");

        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.rounds_spent, 1);
        assert!(success.response.has(LlmCapability::EditReplace));
    }

    #[tokio::test]
    async fn handle_research_commands_absorbs_scratchpad() {
        let mut fixture = Fixture::new(Vec::new());
        let mut envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::Scratchpad(ScratchpadCommand {
                notes: "initial thoughts".to_string(),
            })]);
        let mut ctx = fixture.context();

        let result =
            handle_research_commands(&mut ctx, &mut envelope, &ResearchLoopConfig::default())
                .await
                .expect("executes");
        assert!(result.is_none());
        assert!(envelope.is_empty());
        assert!(!fixture.journal.is_empty());
    }

    /// A round that carries only conversational text and a scratchpad note
    /// carries no discovery commands, so the research phase exits immediately
    /// without sending an extra round.
    #[tokio::test]
    async fn run_research_loop_terminates_after_a_scratchpad_only_round() {
        let mut fixture = Fixture::new(Vec::new());
        let envelope = ResponseEnvelope::from_commands(&[
            LlmCommand::Text(TextCommand {
                content: "The function already returns 42.".to_string(),
            }),
            LlmCommand::Scratchpad(ScratchpadCommand {
                notes: "checked the call site".to_string(),
            }),
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
            !fixture.journal.is_empty(),
            "the note is recorded into the turn journal"
        );
        assert_eq!(
            fixture.provider.requests().len(),
            0,
            "no extra round is sent when the envelope already has an answer"
        );
    }

    #[tokio::test]
    async fn run_research_loop_completes_when_max_rounds_reached() {
        let responses = vec![MockResponse::text("Continued without loop")];
        let mut fixture = Fixture::new(responses);
        let envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::Workflow,
            })]);
        let mut ctx = fixture.context();
        let config = ResearchLoopConfig {
            max_rounds: 1,
            ..ResearchLoopConfig::default()
        };

        let outcome = run_research_loop(&mut ctx, envelope, &config)
            .await
            .expect("runs loop");

        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.rounds_spent, 1);
    }

    #[tokio::test]
    async fn run_research_loop_explains_a_request_the_context_already_satisfies() {
        let responses = vec![MockResponse::text(
            "The helper is already loaded; here is the answer.",
        )];
        let mut fixture = Fixture::new(responses);
        let root = fixture.dir.path();
        crate::git::init_repository(Some(root), true).expect("repository");
        std::fs::write(root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n").unwrap();
        crate::git::cmd::stage_files(root, &["helper.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(root, "initial", false).unwrap();
        fixture
            .context
            .add_file("helper.rs", tauqe_protocol::ContextAccess::ReadOnly)
            .unwrap();
        let envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::ContextRequest(ContextRequestCommand {
                path: "helper.rs".to_string(),
                access: tauqe_protocol::ContextAccess::ReadOnly,
            })]);
        let mut ctx = fixture.context();

        let outcome = run_research_loop(&mut ctx, envelope, &ResearchLoopConfig::default())
            .await
            .expect("runs loop");

        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(
            success.value.rounds_spent, 0,
            "hint round does not increment rounds_spent"
        );
        assert!(
            fixture
                .journal
                .entries()
                .iter()
                .any(|block| block
                    .contains("Every file you requested is already loaded in <context>")),
            "directive must enter the journal"
        );
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

    /// The directive names the condition that actually triggers it, so the model
    /// is never told that a round which sent requests was an empty one.
    #[test]
    fn empty_research_directive_names_the_factual_trigger() {
        assert!(EMPTY_RESEARCH_DIRECTIVE.contains("did not widen the context"));
        assert!(EMPTY_RESEARCH_DIRECTIVE.contains("'answer' branch"));
        assert!(EMPTY_RESEARCH_DIRECTIVE.contains("'edit' branch"));
        assert!(!EMPTY_RESEARCH_DIRECTIVE.contains("all request arrays"));
        assert!(!EMPTY_RESEARCH_DIRECTIVE.contains("were empty"));
    }

    /// A round whose request the context budget refuses widens nothing, so the
    /// loop explains the dead end instead of resending the same request.
    #[tokio::test]
    async fn run_research_loop_explains_a_round_that_widened_nothing() {
        let responses = vec![MockResponse::text(
            "Nothing further to load; here is the answer.",
        )];
        let mut fixture = Fixture::new(responses);
        let root = fixture.dir.path();
        crate::git::init_repository(Some(root), true).expect("repository");
        std::fs::write(root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n").unwrap();
        crate::git::cmd::stage_files(root, &["helper.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(root, "initial", false).unwrap();
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::ContextRequest(
            tauqe_protocol::ContextRequestCommand {
                path: "helper.rs".to_string(),
                access: tauqe_protocol::ContextAccess::ReadOnly,
            },
        )]);
        let mut ctx = fixture.context();
        let config = ResearchLoopConfig {
            token_budget: 1,
            ..ResearchLoopConfig::default()
        };

        let outcome = run_research_loop(&mut ctx, envelope, &config)
            .await
            .expect("runs loop");

        let success = outcome.into_success().expect("success");
        assert_eq!(
            success.value.rounds_spent, 0,
            "a round that widened nothing spends no research round"
        );
        assert!(
            fixture
                .journal
                .entries()
                .iter()
                .any(|block| block.contains(EMPTY_RESEARCH_DIRECTIVE)),
            "the dead end is explained in the journal"
        );
        assert_eq!(
            fixture.provider.requests().len(),
            1,
            "the model is asked again against the explained dead end"
        );
    }

    /// A request for a path the working tree does not hold is progress, not a
    /// dead end: the missing path carries its own feedback block, so the
    /// empty-research directive never fires for it.
    #[tokio::test]
    async fn run_research_loop_reports_a_missing_file_instead_of_the_empty_directive() {
        let responses = vec![MockResponse::text("Here is the answer.")];
        let mut fixture = Fixture::new(responses);
        let root = fixture.dir.path();
        crate::git::init_repository(Some(root), true).expect("repository");
        std::fs::write(root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n").unwrap();
        crate::git::cmd::stage_files(root, &["helper.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(root, "initial", false).unwrap();
        let envelope = ResponseEnvelope::from_commands(&[LlmCommand::ContextRequest(
            tauqe_protocol::ContextRequestCommand {
                path: "missing.rs".to_string(),
                access: tauqe_protocol::ContextAccess::ReadOnly,
            },
        )]);
        let mut ctx = fixture.context();

        let outcome = run_research_loop(&mut ctx, envelope, &ResearchLoopConfig::default())
            .await
            .expect("runs loop");

        let success = outcome.into_success().expect("success");
        assert_eq!(
            success.value.rounds_spent, 1,
            "a missing path widens the round, so the phase spends it and asks again"
        );
        assert!(
            !fixture
                .journal
                .entries()
                .iter()
                .any(|block| block.contains(EMPTY_RESEARCH_DIRECTIVE)),
            "a round that reported a missing path is never an empty round"
        );
        assert_eq!(fixture.provider.requests().len(), 1);
    }
}
