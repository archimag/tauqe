//! Execution facts of one research operation. Continuation and corrective
//! directives belong to the caller, not to this mechanism.

use crate::context::ContextAccess;
use std::collections::BTreeMap;
use std::path::Path;

use crate::llm::LlmCapability;
use crate::llm::{ContextRequestCommand, LlmCommand};
use serde_json::Value;

use super::journal::TurnResearchJournal;
use crate::context::ContextManager;
use crate::execution::{CancellationToken, EventSink, ExecutionError, ResponseEnvelope};
use crate::workflow::research::{execute_research_commands, ResearchLimits, ResearchRoundOutcome};

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

/// Inputs to one operation, with no provider or request builder.
pub struct ResearchEnvironment<'a> {
    pub repo_root: &'a Path,
    pub context_manager: &'a mut ContextManager,
    pub journal: &'a mut TurnResearchJournal,
    pub outer_docs: &'a [crate::config::OuterDocEntry],
    pub project: &'a crate::config::project::ProjectConfig,
    pub cancel: &'a CancellationToken,
    pub events: &'a EventSink,
    pub round: usize,
}

/// Feedback and outcomes aggregated from executing research commands.
#[derive(Debug, Clone, Default)]
pub struct ResearchFeedback {
    /// Repository files added to the auto context layer.
    added_files: Vec<String>,
    /// Repository files requested that were missing from disk.
    missing_files: Vec<String>,
    /// Repository files requested that were already present in context.
    already_satisfied_files: Vec<String>,
    /// Rendered feedback blocks from regex searches, directory outlines, or system documentation.
    feedback_blocks: Vec<String>,
    /// Whether the decoded round supplied scratchpad notes.
    has_scratchpad: bool,
    /// Whether the round widened repository knowledge (added files, missing files, searches, docs, tools).
    widened: bool,
}

/// Flat configuration of the research loop.
#[derive(Debug, Clone)]
pub struct ResearchLoopConfig {
    /// Operational caps applied to one research round.
    research_limits: ResearchLimits,
}

impl ResearchLoopConfig {
    /// Creates research loop configuration from project context configuration.
    pub fn from_context(config: &crate::config::ContextConfig) -> Self {
        Self {
            research_limits: ResearchLimits::from_context_config(config),
        }
    }

    pub fn limits(&self) -> ResearchLimits {
        self.research_limits
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
async fn handle_research_commands(
    ctx: &mut ResearchEnvironment<'_>,
    envelope: &mut ResponseEnvelope,
    config: &ResearchLoopConfig,
    repairing: bool,
) -> Result<Option<(ResearchFeedback, ResearchRoundOutcome)>, ExecutionError> {
    if ctx.cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }

    let mut commands = envelope.take_any(RESEARCH_CAPABILITIES);
    if commands.is_empty() {
        return Ok(None);
    }

    // The round marks its own boundary: the feedback published below covers the
    // records that follow it, so the model is told what this round delivered
    // instead of receiving the whole turn's investigation again.
    ctx.journal.begin_round(ctx.journal.round_index() + 1);

    let mut feedback = ResearchFeedback::default();

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
    let registry = crate::outer_docs::OuterDocsRegistry::new(ctx.outer_docs);
    let hard = ctx.cancel.clone();

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
                            Some(&hard),
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
                            Some(&hard),
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
    let mut research_outcome = ResearchRoundOutcome::default();
    // 2. Execute codebase research commands (context requests, searches, maps, docs)
    if !regular_commands.is_empty() {
        let mut outcome = execute_research_commands(
            ctx.context_manager,
            ctx.repo_root,
            &regular_commands,
            config.research_limits,
            !repairing,
            Some(&hard),
        )
        .await;

        for (path, err) in outer_docs_failures {
            outcome.record_outer_docs_failure(&path, &err);
        }

        if !outcome.added_files.is_empty() {
            let paths = outcome
                .added_files
                .iter()
                .map(|path| format!("<path>{}</path>", crate::escape::escape_xml_text(path)))
                .collect::<Vec<_>>()
                .join("\n");
            ctx.journal.push_block(format!(
                "<context_feedback round=\"{}\">\nLoaded {} file(s) into context in this round:\n{paths}\n</context_feedback>",
                ctx.round, outcome.added_files.len()
            ));
        }

        ctx.journal.record_outcome(&outcome);
        feedback.widened = outcome.has_progress(false);
        feedback.added_files = outcome.added_files.clone();
        feedback.missing_files = outcome.missing_files.clone();
        feedback.already_satisfied_files = outcome.already_satisfied_files.clone();
        feedback
            .feedback_blocks
            .extend(outcome.feedback_blocks.clone());
        research_outcome = outcome;
    }

    // 3. Execute project command custom tools against declared allowlist
    if !tool_invocations.is_empty() {
        feedback.widened = true;
        for (name, args) in tool_invocations {
            if ctx.cancel.is_cancelled() {
                return Err(ExecutionError::Cancelled);
            }
            let block =
                execute_custom_tool(ctx.repo_root, ctx.project, &name, &args, ctx.cancel).await;
            ctx.journal.push_evidence_block(block.as_str());
            feedback.feedback_blocks.push(block);
        }
    }

    Ok(Some((feedback, research_outcome)))
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
#[derive(Debug, Clone, Default)]
pub struct ResearchStep {
    /// Classification of the round.
    pub kind: ResearchStepKind,
    /// Feedback the round produced, empty when it carried no discovery command.
    feedback: ResearchFeedback,
    /// Detailed facts recorded by the research handlers in the journal once.
    pub outcome: ResearchRoundOutcome,
}

impl ResearchStep {
    pub fn summary(&self) -> String {
        format_round_summary(&self.feedback)
    }
}

/// Executes exactly one discovery round and classifies what it delivered.
///
/// The step drains the discovery commands of the envelope, records the findings
/// into the supplied journal. It never requests another round or issues a
/// directive telling the model how to continue.
pub async fn execute_research_step(
    ctx: &mut ResearchEnvironment<'_>,
    envelope: &mut ResponseEnvelope,
    config: &ResearchLoopConfig,
    repairing: bool,
    has_scratchpad: bool,
) -> Result<(), ExecutionError> {
    ctx.journal.reset_step();
    let Some((feedback, outcome)) =
        handle_research_commands(ctx, envelope, config, repairing).await?
    else {
        return Ok(());
    };

    let feedback = ResearchFeedback {
        has_scratchpad,
        ..feedback
    };
    let kind = classify_research_round(&feedback);
    ctx.journal.record_step(ResearchStep {
        kind,
        feedback,
        outcome,
    });
    Ok(())
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
    if feedback.has_scratchpad {
        parts.push("scratchpad noted".to_string());
    }
    if parts.is_empty() {
        "no new files added.".to_string()
    } else {
        format!("{}.", parts.join(", "))
    }
}

/// Executes one custom tool invocation against the declared project allowlist.
async fn execute_custom_tool(
    repo_root: &Path,
    project: &crate::config::project::ProjectConfig,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
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

    let hard = cancel.clone();
    match crate::toolchain::run_project_command_with_config(
        repo_root,
        project,
        name,
        &arguments,
        Some(&hard),
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
    use crate::llm::DocTopic;
    use crate::llm::{ContextRequestCommand, DocRequestCommand, ScratchpadCommand, TextCommand};
    use crate::workflow::research::journal::TurnResearchJournal;

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: TurnResearchJournal,
        project: crate::config::project::ProjectConfig,
        cancel: CancellationToken,
        events: EventSink,
    }

    impl Fixture {
        fn new(_responses: Vec<()>) -> Self {
            let dir = tempfile::tempdir().expect("scratch directory");
            let root = dir.path().to_path_buf();
            let context = ContextManager::new(root);
            Self {
                dir,
                context,
                journal: TurnResearchJournal::default(),
                project: Default::default(),
                cancel: CancellationToken::never(),
                events: EventSink::noop(),
            }
        }

        fn context(&mut self) -> ResearchEnvironment<'_> {
            ResearchEnvironment {
                repo_root: self.dir.path(),
                context_manager: &mut self.context,
                journal: &mut self.journal,
                outer_docs: &[],
                project: &self.project,
                cancel: &self.cancel,
                events: &self.events,
                round: 1,
            }
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

        let result = handle_research_commands(
            &mut ctx,
            &mut envelope,
            &ResearchLoopConfig::default(),
            false,
        )
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

        let result = handle_research_commands(
            &mut ctx,
            &mut envelope,
            &ResearchLoopConfig::default(),
            false,
        )
        .await
        .expect("executes");
        assert!(result.is_some());
        let (fb, outcome) = result.unwrap();
        assert!(!fb.feedback_blocks.is_empty());
        assert!(outcome.has_progress(false));
        assert!(envelope.is_empty());
        assert!(!fixture.journal.is_empty());
    }

    #[tokio::test]
    async fn handle_research_commands_leaves_scratchpad_untouched() {
        let mut fixture = Fixture::new(Vec::new());
        let mut envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::Scratchpad(ScratchpadCommand {
                notes: "initial thoughts".to_string(),
            })]);
        let mut ctx = fixture.context();

        let result = handle_research_commands(
            &mut ctx,
            &mut envelope,
            &ResearchLoopConfig::default(),
            false,
        )
        .await
        .expect("executes");
        assert!(result.is_none());
        assert_eq!(envelope.len(), 1);
        assert!(envelope.has(LlmCapability::Scratchpad));
        assert!(
            fixture.journal.is_empty(),
            "private notes must stay out of research feedback"
        );
    }

    #[tokio::test]
    async fn idle_research_resets_result_without_draining_other_commands() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.journal.record_step(ResearchStep {
            kind: ResearchStepKind::Widened,
            ..Default::default()
        });
        let commands = [
            LlmCommand::Text(TextCommand {
                content: "answer".into(),
            }),
            LlmCommand::Scratchpad(ScratchpadCommand {
                notes: "private".into(),
            }),
        ];
        let mut envelope = ResponseEnvelope::from_commands(&commands);
        execute_research_step(
            &mut fixture.context(),
            &mut envelope,
            &ResearchLoopConfig::default(),
            false,
            true,
        )
        .await
        .expect("idle round");
        assert_eq!(fixture.journal.last_step().kind, ResearchStepKind::Idle);
        assert_eq!(envelope.commands(), &commands);
        assert!(fixture.journal.rendered().is_none());
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

        execute_research_step(
            &mut ctx,
            &mut envelope,
            &ResearchLoopConfig::default(),
            false,
            false,
        )
        .await
        .expect("executes");
        let step = ctx.journal.last_step();
        assert_eq!(step.kind, ResearchStepKind::Widened);
        assert!(step.outcome.has_progress(true));
        assert_eq!(
            step.outcome.architect_banner_summary(),
            "Added 1 file(s) to context: helper.rs"
        );
        assert!(
            ctx.context_manager
                .contains("helper.rs")
                .expect("the path resolves"),
            "the delivered file joins the context"
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

        execute_research_step(
            &mut ctx,
            &mut envelope,
            &ResearchLoopConfig::default(),
            false,
            false,
        )
        .await
        .expect("executes");
        let step = ctx.journal.last_step();
        assert_eq!(step.kind, ResearchStepKind::AlreadySatisfied);
        assert_eq!(step.feedback.already_satisfied_files, ["helper.rs"]);
        assert!(!step.outcome.has_progress(false));
        assert!(step.outcome.has_progress(true));
        assert_eq!(
            step.outcome.architect_banner_summary(),
            "Files already in context: helper.rs"
        );
    }

    #[tokio::test]
    async fn execute_research_step_does_not_apply_a_separate_context_budget() {
        let mut fixture = Fixture::new(Vec::new());
        crate::git::init_repository(Some(fixture.dir.path()), true).expect("repository");
        fixture.commit("helper.rs", "pub const ANSWER: i32 = 42;\n");
        let mut envelope =
            ResponseEnvelope::from_commands(&[LlmCommand::ContextRequest(ContextRequestCommand {
                path: "helper.rs".to_string(),
                access: ContextAccess::ReadOnly,
            })]);
        let mut ctx = fixture.context();
        execute_research_step(
            &mut ctx,
            &mut envelope,
            &ResearchLoopConfig::default(),
            false,
            false,
        )
        .await
        .expect("executes");
        let step = ctx.journal.last_step();
        assert_eq!(step.kind, ResearchStepKind::Widened);
        assert!(step.feedback.widened);
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

        execute_research_step(
            &mut ctx,
            &mut envelope,
            &ResearchLoopConfig::default(),
            false,
            false,
        )
        .await
        .expect("executes");
        let step = ctx.journal.last_step();
        assert_eq!(step.kind, ResearchStepKind::Widened);
        assert_eq!(step.feedback.missing_files, vec!["missing.rs".to_string()]);
        assert!(step.feedback.widened);
    }

    #[tokio::test]
    async fn detailed_facts_do_not_record_the_round_twice() {
        let mut fixture = Fixture::new(Vec::new());
        crate::git::init_repository(Some(fixture.dir.path()), true).expect("repository");
        fixture.commit("helper.rs", "pub const ANSWER: i32 = 42;\n");
        let mut envelope = ResponseEnvelope::from_commands(&[
            LlmCommand::ContextRequest(ContextRequestCommand {
                path: "helper.rs".into(),
                access: ContextAccess::ReadOnly,
            }),
            LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::Workflow,
            }),
        ]);
        execute_research_step(
            &mut fixture.context(),
            &mut envelope,
            &ResearchLoopConfig::default(),
            false,
            false,
        )
        .await
        .expect("executes");
        let step = fixture.journal.last_step();
        assert_eq!(step.kind, ResearchStepKind::Widened);
        assert!(step.outcome.has_progress(true));
        assert!(step.summary().contains("1 file(s) added"));
        let feedback = fixture.journal.render_round_feedback();
        assert_eq!(feedback.matches("Loaded 1 file(s)").count(), 1);
        assert_eq!(feedback.matches("<system_documentation>").count(), 1);
        assert_eq!(
            fixture
                .journal
                .rendered()
                .unwrap()
                .matches("<system_documentation>")
                .count(),
            1
        );
    }

    #[test]
    fn research_loop_config_from_context_preserves_operational_limits() {
        let context_config = crate::config::ContextConfig {
            max_search_requests_per_round: 17,
            ..Default::default()
        };
        let config = ResearchLoopConfig::from_context(&context_config);
        assert_eq!(
            config.limits(),
            ResearchLimits::from_context_config(&context_config)
        );
    }
}
