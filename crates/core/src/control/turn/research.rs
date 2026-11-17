//! Discovery step of the bounded turn.
//!
//! This module provides [`handle_research_commands`] for draining and executing
//! discovery commands against [`crate::context::ContextManager`] and declared project
//! tools, and [`execute_research_step`] for executing exactly one discovery round and
//! classifying what it delivered. The rounds themselves belong to
//! [`crate::control::turn::run_turn_loop`]: the phase owns no loop of its own, so a
//! discovery round can never terminate the turn.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use tauqe_protocol::{ContextAccess, ContextRequestCommand, LlmCapability, LlmCommand, TurnPhase};

use crate::control::context::LoopContext;
use crate::control::outcome::{ResponseEnvelope, SystemError};
use crate::providers::StreamEvent;
use crate::workflow::research::{execute_research_commands, ResearchLimits};

/// Capabilities processed by the research and investigation contour.
pub(crate) const RESEARCH_CAPABILITIES: &[LlmCapability] = &[
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
pub(super) const MAX_SATISFIED_RESEARCH_ROUNDS: usize = 1;

/// Maximum consecutive rounds a discovery round may widen nothing before the turn stops asking.
pub(super) const MAX_EMPTY_RESEARCH_ROUNDS: usize = 1;

/// Directive appended to the journal when a discovery round widened nothing.
///
/// The trigger is that the round widened nothing: the round added no file,
/// reported no missing path, ran no search, rendered no outline, listing, or
/// targeted map, loaded no document, and explained nothing already loaded.
/// The working tree could not be inspected, the context budget refused the
/// material, or the requests carried nothing the harness could resolve, so the
/// directive names exactly those causes and forbids repeating them.
const EMPTY_RESEARCH_DIRECTIVE: &str = "<response_directive>\nYour discovery round did not widen the context: either the working tree could not be inspected, the declared context budget refused the requested material, or the requests carried nothing the harness could resolve into a file, a search, a listing, an outline, or a document. Repeating the same requests would produce the same result, so do not repeat them. Close the round with one of the branches the schema declares: request material that exists in the working tree (take exact paths from <repo_map> and <repo_dirs>), answer the task through the 'answer' branch, or deliver the code changes through the 'edit' branch.\n</response_directive>";

/// Directive appended to the journal when a round only repeated requests the context already satisfied.
const ALREADY_SATISFIED_DIRECTIVE: &str = "<response_directive>\nEvery file you requested is already loaded in <context>. Do not request those paths again: answer the task from the material you already hold. Write the analysis or the conclusion in plain conversational text, and emit file edits only when the code itself must change.\n</response_directive>";

/// Feedback and outcomes aggregated from executing research commands.
#[derive(Debug, Clone, Default)]
pub struct ResearchFeedback {
    /// Repository files added to the auto context layer.
    /// Repository files added to the auto context layer.
    added_files: Vec<String>,
    /// Repository files requested that were missing from disk.
    missing_files: Vec<String>,
    /// Repository files requested that were already present in context.
    already_satisfied_files: Vec<String>,
    /// Rendered feedback blocks from regex searches, directory outlines, or system documentation.
    feedback_blocks: Vec<String>,
    /// Whether turn scratchpad notes were recorded into the journal.
    scratchpad_recorded: bool,
    /// Whether the round widened repository knowledge (added files, missing files, searches, docs, tools).
    widened: bool,
}

/// Flat configuration of the research loop.
#[derive(Debug, Clone)]
pub struct ResearchLoopConfig {
    /// Aggregate token budget of the context a round may widen.
    /// Aggregate token budget of the context a round may widen.
    token_budget: usize,
    /// Token budget of a targeted repository map rendered by the round.
    repomap_token_budget: usize,
    /// Operational caps applied to one research round.
    research_limits: ResearchLimits,
}

impl ResearchLoopConfig {
    /// Creates research loop configuration from project context configuration.
    pub(crate) fn from_context(config: &crate::config::ContextConfig) -> Self {
        Self {
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

/// Drains research commands from the envelope and executes them.
///
/// Returns `Ok(None)` if no research commands were present in the envelope.
fn drain_scratchpad(ctx: &mut LoopContext<'_>, envelope: &mut ResponseEnvelope) -> bool {
    let mut recorded = false;
    for cmd in envelope.take(LlmCapability::Scratchpad) {
        if let LlmCommand::Scratchpad(command) = cmd {
            let notes = command.notes;
            if !notes.trim().is_empty() {
                ctx.journal
                    .push_block(format!("<scratchpad>\n{}\n</scratchpad>", notes.trim()));
                recorded = true;
            }
        }
    }
    recorded
}

pub(crate) async fn handle_research_commands(
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

    // The round marks its own boundary: the feedback published below covers the
    // records that follow it, so the model is told what this round delivered
    // instead of receiving the whole turn's investigation again.
    ctx.begin_research_round();

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
            ctx.mark_context_churn();
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

    // The round's feedback enters the transcript slot the next round appends as
    // its user message, so the model reads what its own round delivered.
    ctx.publish_round_feedback();

    Ok(Some(feedback))
}

/// Classification of one executed discovery round.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResearchStepKind {
    /// The round carried no discovery command, so the phase has nothing left to spend.
    #[default]
    Idle,
    /// The round widened what the context holds, so the turn continues against it.
    Widened,
    /// The round asked only for material the context already holds, so the phase
    /// explains that and asks once more.
    AlreadySatisfied,
    /// The round widened nothing, so the phase explains the dead end and asks once more.
    DeadEnd,
}

/// What one executed discovery round delivered.
#[derive(Debug, Default)]
pub struct ResearchStep {
    /// Classification of the round.
    pub(crate) kind: ResearchStepKind,
    /// Feedback the round produced, empty when it carried no discovery command.
    #[cfg(test)]
    feedback: ResearchFeedback,
}

/// Executes exactly one discovery round and classifies what it delivered.
///
/// The step drains the discovery commands of the envelope, records the findings
/// into the journal of the turn, and announces the round it spent. It never
/// requests a round of its own: the budget of the turn and the request that spends
/// it belong to [`crate::control::turn::run_turn_loop`].
pub(crate) async fn execute_research_step(
    ctx: &mut LoopContext<'_>,
    envelope: &mut ResponseEnvelope,
    config: &ResearchLoopConfig,
) -> Result<ResearchStep, SystemError> {
    let Some(feedback) = handle_research_commands(ctx, envelope, config).await? else {
        return Ok(ResearchStep::default());
    };

    let kind = classify_research_round(&feedback);
    match kind {
        ResearchStepKind::DeadEnd => {
            // The round delivered nothing, so the harness names the causes and
            // forbids repeating them. The directive is recorded here and reaches
            // the model through the corrective round the turn requests next.
            ctx.journal.push_block(EMPTY_RESEARCH_DIRECTIVE);
        }
        ResearchStepKind::AlreadySatisfied => {
            ctx.journal.push_block(ALREADY_SATISFIED_DIRECTIVE);
            announce_research_round(ctx, &feedback);
        }
        ResearchStepKind::Widened => announce_research_round(ctx, &feedback),
        ResearchStepKind::Idle => {}
    }

    Ok(ResearchStep {
        kind,
        #[cfg(test)]
        feedback,
    })
}

/// Classifies the round the step just executed.
///
/// The classification reads what the round delivered and nothing about the prose it
/// carried: the branch grammar already states what the round did, so a round that
/// asked only for material the context holds is a satisfied request whether or not
/// it narrated the request.
fn classify_research_round(feedback: &ResearchFeedback) -> ResearchStepKind {
    if feedback.widened {
        return ResearchStepKind::Widened;
    }
    if feedback.already_satisfied_files.is_empty() {
        return ResearchStepKind::DeadEnd;
    }
    ResearchStepKind::AlreadySatisfied
}

/// Announces the round a step spent on the interface and in the journal.
///
/// The ordinal is the one the round boundary of the journal carries, so the label
/// names the investigation round the step executed rather than a counter a contour
/// keeps of its own.
fn announce_research_round(ctx: &LoopContext<'_>, feedback: &ResearchFeedback) {
    let round = ctx.journal.round_index();
    ctx.events.emit(StreamEvent::TextDelta(format!(
        "\n\n---\n**[Round {}]** {}\n\n",
        round,
        format_round_summary(feedback)
    )));
    super::announce_turn_phase(
        ctx,
        TurnPhase::Research,
        round,
        None,
        format!("Research round {round}"),
    );
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
pub(super) fn holds_a_higher_contour_command(envelope: &ResponseEnvelope) -> bool {
    envelope
        .remaining_capabilities()
        .into_iter()
        .any(|capability| capability != LlmCapability::TextMessage)
}

/// Reports whether the round carried a written answer.
pub(super) fn envelope_has_text(envelope: &ResponseEnvelope) -> bool {
    envelope.commands().iter().any(|cmd| match cmd {
        LlmCommand::Text(command) => !command.content.trim().is_empty(),
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
        CapabilityTree, ContextAccess, ContextRequestCommand, DocRequestCommand, DocTopic,
        ModelRef, ScratchpadCommand, TextCommand,
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
            let caps = CapabilityTree::set(
                RESEARCH_CAPABILITIES
                    .iter()
                    .copied()
                    .chain([LlmCapability::TextMessage])
                    .map(CapabilityTree::leaf),
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

        /// Commits `path` with `content` so the working tree holds it.
        fn commit(&self, path: &str, content: &str) {
            let root = self.dir.path();
            std::fs::write(root.join(path), content).expect("fixture file");
            crate::git::cmd::stage_files(root, &[path.to_string()]).expect("stage");
            crate::git::cmd::commit_with_developer_author(root, "initial", false).expect("commit");
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

    #[tokio::test]
    async fn execute_research_step_reports_a_widened_round() {
        let mut fixture = Fixture::new(Vec::new());
        crate::git::init_repository(Some(fixture.dir.path()), true).expect("repository");
        fixture.commit("helper.rs", "pub const ANSWER: i32 = 42;\n");
        let mut envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::ContextRequest(ContextRequestCommand {
                path: "helper.rs".to_string(),
                access: ContextAccess::ReadOnly,
            })]);
        let mut ctx = fixture.context();

        let step = execute_research_step(&mut ctx, &mut envelope, &ResearchLoopConfig::default())
            .await
            .expect("executes");
        assert_eq!(step.kind, ResearchStepKind::Widened);
        assert!(
            ctx.context_manager
                .contains("helper.rs")
                .expect("the path resolves"),
            "the delivered file joins the context"
        );
        assert!(
            ctx.transcript().prefix_is_stale(),
            "the widened context marks the transcript prefix stale"
        );
    }

    #[tokio::test]
    async fn execute_research_step_explains_a_request_the_context_already_satisfies() {
        let mut fixture = Fixture::new(Vec::new());
        crate::git::init_repository(Some(fixture.dir.path()), true).expect("repository");
        fixture.commit("helper.rs", "pub const ANSWER: i32 = 42;\n");
        fixture
            .context
            .add_file("helper.rs", ContextAccess::ReadOnly)
            .expect("the helper is already loaded");
        let mut envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::ContextRequest(ContextRequestCommand {
                path: "helper.rs".to_string(),
                access: ContextAccess::ReadOnly,
            })]);
        let mut ctx = fixture.context();

        let step = execute_research_step(&mut ctx, &mut envelope, &ResearchLoopConfig::default())
            .await
            .expect("executes");
        assert_eq!(step.kind, ResearchStepKind::AlreadySatisfied);
        assert!(
            fixture
                .journal
                .entries()
                .iter()
                .any(|block| block.contains(ALREADY_SATISFIED_DIRECTIVE)),
            "the directive explains that the material is already loaded"
        );
    }

    #[tokio::test]
    async fn execute_research_step_explains_a_round_that_widened_nothing() {
        let mut fixture = Fixture::new(Vec::new());
        crate::git::init_repository(Some(fixture.dir.path()), true).expect("repository");
        fixture.commit("helper.rs", "pub const ANSWER: i32 = 42;\n");
        let mut envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::ContextRequest(ContextRequestCommand {
                path: "helper.rs".to_string(),
                access: ContextAccess::ReadOnly,
            })]);
        let mut ctx = fixture.context();
        let config = ResearchLoopConfig {
            token_budget: 1,
            ..ResearchLoopConfig::default()
        };

        let step = execute_research_step(&mut ctx, &mut envelope, &config)
            .await
            .expect("executes");
        assert_eq!(step.kind, ResearchStepKind::DeadEnd);
        assert!(
            fixture
                .journal
                .entries()
                .iter()
                .any(|block| block.contains(EMPTY_RESEARCH_DIRECTIVE)),
            "the dead end is explained in the journal"
        );
    }

    #[tokio::test]
    async fn execute_research_step_reports_a_missing_file_as_progress() {
        let mut fixture = Fixture::new(Vec::new());
        crate::git::init_repository(Some(fixture.dir.path()), true).expect("repository");
        fixture.commit("helper.rs", "pub const ANSWER: i32 = 42;\n");
        let mut envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::ContextRequest(ContextRequestCommand {
                path: "missing.rs".to_string(),
                access: ContextAccess::ReadOnly,
            })]);
        let mut ctx = fixture.context();

        let step = execute_research_step(&mut ctx, &mut envelope, &ResearchLoopConfig::default())
            .await
            .expect("executes");
        assert_eq!(step.kind, ResearchStepKind::Widened);
        assert_eq!(step.feedback.missing_files, vec!["missing.rs".to_string()]);
        assert!(
            !fixture
                .journal
                .entries()
                .iter()
                .any(|block| block.contains(EMPTY_RESEARCH_DIRECTIVE)),
            "a round that reported a missing path is never an empty round"
        );
    }

    #[test]
    fn research_loop_config_from_context_preserves_token_budget() {
        let context_config = crate::config::ContextConfig {
            token_budget: 123456,
            ..Default::default()
        };
        let config = ResearchLoopConfig::from_context(&context_config);
        assert_eq!(config.token_budget, 123456);
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
}
