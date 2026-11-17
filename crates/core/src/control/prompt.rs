//! Prompt assembly and model round execution for control loops.

use tauqe_protocol::{ContextAccess, ModelRef, PatchStrategy, ReasoningEffort};
use tokio::sync::mpsc;

use super::context::LoopContext;
use super::outcome::{ResponseEnvelope, SystemError};
use crate::config::OuterDocEntry;
use crate::context::ContextFileContent;
use crate::control::cache::TurnInputCache;
use crate::edits::StagedEditsState;
use crate::history::HistorySnapshot;
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::PromptAssembly;
use crate::prompt::TurnBrief;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::research::journal::TurnResearchJournal;

/// Dedicated builder and executor for model prompt rounds.
///
/// Encapsulates model and protocol parameters, session affinity, prompt caching,
/// and repository outline options away from the ambient execution context.
pub struct PromptBuilder<'a> {
    provider: &'a dyn LlmProvider,
    model: &'a ModelRef,
    protocol: &'a dyn PromptProtocol,
    pub(crate) input_cache: &'a mut TurnInputCache,
    session_id: Option<String>,
    pub(crate) prompt: Option<String>,
    history: Option<&'a HistorySnapshot>,
    include_repo_map: bool,
    repomap_token_budget: usize,
    pub(crate) patch_strategy: PatchStrategy,
    workflow: String,
    pub(crate) outer_docs: Vec<OuterDocEntry>,
    pub(crate) project_config: Option<tauqe_protocol::ProjectConfig>,
    /// Introductory brief of the turn, owned by the loop that drives it.
    pub(crate) turn_brief: Option<TurnBrief>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    turn_round_limit: Option<usize>,
}

impl<'a> PromptBuilder<'a> {
    /// Creates a new prompt builder with default prompt configuration.
    pub fn new(
        provider: &'a dyn LlmProvider,
        model: &'a ModelRef,
        protocol: &'a dyn PromptProtocol,
        input_cache: &'a mut TurnInputCache,
    ) -> Self {
        Self {
            provider,
            model,
            protocol,
            input_cache,
            session_id: None,
            prompt: None,
            history: None,
            include_repo_map: crate::config::DEFAULT_INCLUDE_REPO_MAP,
            repomap_token_budget: crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
            patch_strategy: PatchStrategy::default(),
            workflow: "git".to_string(),
            outer_docs: Vec::new(),
            project_config: None,
            turn_brief: None,
            reasoning_effort: None,
            turn_round_limit: None,
        }
    }

    /// Derives a reborrowed child builder sharing the underlying cache.
    pub(crate) fn reborrow<'b>(&'b mut self) -> PromptBuilder<'b> {
        PromptBuilder {
            provider: self.provider,
            model: self.model,
            protocol: self.protocol,
            input_cache: self.input_cache,
            session_id: self.session_id.clone(),
            prompt: self.prompt.clone(),
            history: self.history,
            include_repo_map: self.include_repo_map,
            repomap_token_budget: self.repomap_token_budget,
            patch_strategy: self.patch_strategy,
            workflow: self.workflow.clone(),
            outer_docs: self.outer_docs.clone(),
            project_config: self.project_config.clone(),
            turn_brief: self.turn_brief.clone(),
            reasoning_effort: self.reasoning_effort,
            turn_round_limit: self.turn_round_limit,
        }
    }

    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    pub fn with_prompt(mut self, prompt: Option<String>) -> Self {
        self.prompt = prompt;
        self
    }

    pub fn with_history(mut self, history: Option<&'a HistorySnapshot>) -> Self {
        self.history = history;
        self
    }

    pub fn with_repo_map_config(mut self, include_repo_map: bool, token_budget: usize) -> Self {
        self.include_repo_map = include_repo_map;
        self.repomap_token_budget = token_budget;
        self
    }

    pub fn with_patch_strategy(mut self, patch_strategy: PatchStrategy) -> Self {
        self.patch_strategy = patch_strategy;
        self
    }

    pub fn with_workflow(mut self, workflow: impl Into<String>) -> Self {
        self.workflow = workflow.into();
        self
    }

    pub fn with_outer_docs(mut self, outer_docs: Vec<OuterDocEntry>) -> Self {
        self.outer_docs = outer_docs;
        self
    }

    pub fn with_turn_brief(mut self, turn_brief: Option<TurnBrief>) -> Self {
        self.turn_brief = turn_brief;
        self
    }

    pub fn with_reasoning_effort(mut self, reasoning_effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = reasoning_effort;
        self
    }

    pub(crate) fn set_turn_round_limit(&mut self, limit: Option<usize>) -> &mut Self {
        self.turn_round_limit = limit;
        self
    }

    pub(crate) fn set_turn_brief(&mut self, turn_brief: Option<TurnBrief>) -> &mut Self {
        self.turn_brief = turn_brief;
        self
    }

    #[cfg(test)]
    pub(crate) fn set_history(&mut self, history: Option<&'a HistorySnapshot>) -> &mut Self {
        self.history = history;
        self
    }
}

/// Corrective rounds granted to a contour that answered with nothing actionable.
const MAX_EMPTY_STEP_RETRIES: usize = 2;
/// Directive appended to the feedback when the turn reaches its final allowed round.
const TURN_BUDGET_DIRECTIVE: &str = "\n\n<response_directive>\nThis is your final allowed round for this turn: the turn round budget is reaching its limit. Conclude the task now by stating your final conclusion in the 'answer' branch, delivering complete code changes in the 'edit' branch (with commit_message), or declaring a blocker in the 'blocked' branch. Do not issue further discovery requests.\n</response_directive>";

/// Corrective suffix appended to the request after an empty round.
///
/// Silence is a transport-level non-answer rather than a conclusion, so a round
/// that carries nothing actionable is asked again with an explicit result
/// contract. The suffix extends the previous request, so the provider keeps
/// matching the prompt prefix it has already cached and the task stays in view.
const EMPTY_STEP_REMINDER: &str = "\n\nYour previous reply was empty: it carried no text, no file edit, no research command, and no terminal signal, so nothing could be acted upon. Respond again with a concrete result: state the analysis or conclusion in the 'answer' branch, emit code changes in the 'edit' branch (with commit_message), or declare a blocker in the 'blocked' branch.";

/// Message sequence a round sends to its provider.
///
/// The contour is declared by the typed value the loop that drives the turn passes,
/// never inferred from the shape of the capability surface: the bounded turn replays
/// the shared transcript of the turn, while an Architect synthesis assembles its own
/// sequence on every round.
#[derive(Clone, Copy)]
enum RoundAssembly<'a> {
    /// The shared round transcript of the turn.
    Transcript,
    /// The contour reassembles its own request on every round.
    Rebuild {
        /// Diagnostic the contour carries into its next request.
        diagnostics: Option<&'a str>,
    },
}

/// Runs one round of a contour that replays the shared transcript of the turn.
///
/// The first round of a turn seeds the transcript with the rendered prefix and the
/// frozen task message. Every later round appends the answer it produced and the
/// feedback of the round that follows, so the request of round `N + 1` is exactly the
/// request of round `N` plus that pair: no token the provider already cached is
/// rendered or moved again inside a phase. A phase boundary — recognized by a
/// different capability tree or a context churn the change set marked —
/// renders the prefix once more and keeps the task and the rounds already
/// exchanged byte for byte.
pub(crate) async fn execute_round(
    ctx: &mut LoopContext<'_>,
    staged_state: Option<&StagedEditsState>,
) -> Result<ResponseEnvelope, SystemError> {
    execute_round_inner(ctx, RoundAssembly::Transcript, staged_state).await
}

/// Runs one round of a contour that assembles its own message sequence.
///
/// An Architect synthesis reasons about a plan instead of patching a working tree, so
/// it keeps its own reassembly and its own scratchpad injection
/// (`crates/core/src/control/refine.rs`) and never replays a transcript it does not own.
pub(crate) async fn execute_rebuilt_round(
    ctx: &mut LoopContext<'_>,
    diagnostics: Option<&str>,
    staged_state: Option<&StagedEditsState>,
) -> Result<ResponseEnvelope, SystemError> {
    execute_round_inner(ctx, RoundAssembly::Rebuild { diagnostics }, staged_state).await
}

/// Composes the user message of a contour that reassembles its own request.
///
/// The order is fixed: the task first, the accumulated epistemic tail next, the
/// diagnostic of the current phase last. Nothing is inserted before the task, so the
/// stable part of the message keeps its bytes across the rounds of a phase.
fn compose_rebuilt_user_prompt(
    journal: &TurnResearchJournal,
    base_prompt: &str,
    diagnostics: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(base_prompt.trim_end());

    if let Some(tail) = journal.rendered() {
        out.push_str("\n\n");
        out.push_str(&tail);
    }

    if let Some(diagnostics) = diagnostics {
        let diagnostics = diagnostics.trim();
        if !diagnostics.is_empty() {
            out.push_str("\n\n");
            out.push_str(diagnostics);
        }
    }

    out
}

async fn execute_round_inner(
    ctx: &mut LoopContext<'_>,
    round_assembly: RoundAssembly<'_>,
    staged_state: Option<&StagedEditsState>,
) -> Result<ResponseEnvelope, SystemError> {
    if ctx.cancel.is_cancelled() {
        return Err(SystemError::Cancelled);
    }

    let root_buf = ctx.repo_root.to_path_buf();
    let root_for_git = root_buf.clone();
    let repo_state = crate::blocking::run_blocking_async(move || {
        crate::git::state::get_repository_state(Some(&root_for_git))
    })
    .await
    .ok();

    let ctx_state = ctx.context_manager.get_state();
    let mut context_files = ctx
        .prompt_builder
        .input_cache
        .context_files(&root_buf, &ctx_state.items)
        .await
        .map_err(|e| SystemError::Other(e.to_string()))?;

    if let Some(staged) = staged_state {
        let staged_text = staged.staged_text_files();
        let mut appended = Vec::new();
        for (path, content) in &staged_text {
            if let Some(file) = context_files.iter_mut().find(|file| &file.path == path) {
                file.content = content.clone();
            } else {
                appended.push(ContextFileContent {
                    path: path.clone(),
                    access: ContextAccess::Editable,
                    content: content.clone(),
                    insertion_seq: 0,
                });
            }
        }
        appended.sort_by(|a, b| a.path.cmp(&b.path));
        context_files.extend(appended);
        context_files.retain(|file| !staged.deleted_paths.contains(&file.path));
    }

    let context_paths: Vec<String> = context_files.iter().map(|f| f.path.clone()).collect();

    let (repo_map, repo_dirs) = if ctx.prompt_builder.include_repo_map {
        let map = ctx
            .prompt_builder
            .input_cache
            .repo_map(
                &root_buf,
                &context_paths,
                ctx.prompt_builder.repomap_token_budget,
            )
            .await;
        (map, None)
    } else {
        let dirs = ctx
            .prompt_builder
            .input_cache
            .repo_dirs(&root_buf, ctx.context_manager.pinned_patterns())
            .await;
        (None, dirs)
    };

    let repomap_tokens = repo_map
        .as_deref()
        .or(repo_dirs.as_deref())
        .map(crate::prompt::estimate_tokens)
        .unwrap_or(0);
    ctx.events.emit(StreamEvent::RepoMapTokens(repomap_tokens));

    let history = ctx
        .prompt_builder
        .history
        .map(|h| std::sync::Arc::new((*h).clone()));

    let project = if let Some(ref p) = ctx.prompt_builder.project_config {
        p.clone()
    } else {
        ctx.prompt_builder
            .input_cache
            .project(&root_buf, Some(&ctx.cancel.receiver()))
            .await
    };

    let mut assembly = PromptAssembly::new(
        repo_state,
        ctx_state.revision,
        context_files.clone(),
        &ctx.prompt_builder.workflow,
        ctx.prompt_builder.protocol.kind(),
        history,
        repo_map,
    )
    .with_repo_dirs(repo_dirs)
    .with_project(Some(project.clone()))
    .with_patch_strategy(ctx.prompt_builder.patch_strategy)
    .with_outer_docs(ctx.prompt_builder.outer_docs.clone());

    assembly.turn_brief = ctx.prompt_builder.turn_brief.clone();
    // The scratchpad is never re-injected into the request a round sends: the
    // model reads its own notes back from the assistant answers the transcript
    // replays, so the head of the turn stays byte-stable inside a phase.

    // The contour owns the capability tree of the round, so the protocol the
    // round renders and decodes with is exactly the surface the loop declared.
    let scoped_protocol = ctx
        .prompt_builder
        .protocol
        .with_capability_expr(ctx.capabilities.clone())
        .with_project(Some(project.clone()));
    let protocol: &dyn PromptProtocol = scoped_protocol.as_ref();

    let base_prompt = ctx.prompt_builder.prompt.as_deref().unwrap_or("");
    let rebuilt_user_prompt = match round_assembly {
        RoundAssembly::Transcript => None,
        RoundAssembly::Rebuild { diagnostics } => Some(compose_rebuilt_user_prompt(
            &*ctx.journal,
            base_prompt,
            diagnostics,
        )),
    };

    let staged_contents: std::collections::HashMap<String, String> = context_files
        .iter()
        .map(|file| (file.path.clone(), file.content.clone()))
        .collect();
    let transcript_handle = std::sync::Arc::clone(&ctx.transcript);

    let mut empty_attempts = 0usize;
    loop {
        let spent = ctx
            .rounds_spent
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(limit) = ctx.prompt_builder.turn_round_limit {
            if spent >= limit {
                return Err(SystemError::RoundBudgetExhausted);
            }
            if spent + 1 == limit && matches!(round_assembly, RoundAssembly::Transcript) {
                let mut transcript = transcript_handle
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut feedback = transcript.take_pending_feedback().unwrap_or_default();
                feedback.push_str(TURN_BUDGET_DIRECTIVE);
                transcript.set_pending_feedback(feedback);
            }
        }

        // The sequence of the round is the sequence of its predecessor plus the pair
        // the turn exchanged, so a transcript-driven round renders nothing of its own.
        let messages = match round_assembly {
            RoundAssembly::Transcript => {
                let mut transcript = transcript_handle
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !transcript.is_seeded() {
                    let assembled = assembly.assemble_chat_messages_with_context_cache(
                        base_prompt,
                        protocol,
                        ctx.prompt_builder.input_cache.context_cache(),
                    );
                    let Some((task, prefix)) = assembled.split_last() else {
                        return Err(SystemError::Other(
                            "the prompt assembly rendered no message".to_string(),
                        ));
                    };
                    transcript.seed(prefix.to_vec(), task.clone(), &ctx.capabilities);
                } else if !transcript.advertises(&ctx.capabilities) || transcript.prefix_is_stale()
                {
                    // A different capability tree or a marked context churn is
                    // the phase boundary of the turn: the head is rendered once
                    // more from the current assembly — relocated files carry
                    // the anchors of their current bytes — while the task and
                    // the rounds already exchanged keep their exact bytes.
                    let assembled = assembly.assemble_chat_messages_with_context_cache(
                        base_prompt,
                        protocol,
                        ctx.prompt_builder.input_cache.context_cache(),
                    );
                    let prefix = assembled
                        .split_last()
                        .map(|(_, prefix)| prefix.to_vec())
                        .unwrap_or_default();
                    transcript.reseed_prefix(prefix);
                }
                if let Some(feedback) = transcript.take_pending_feedback() {
                    transcript.push_feedback(feedback);
                }
                transcript.messages()
            }
            RoundAssembly::Rebuild { .. } => {
                let mut user_prompt = rebuilt_user_prompt.clone().unwrap_or_default();
                if empty_attempts > 0 {
                    user_prompt.push_str(&EMPTY_STEP_REMINDER.repeat(empty_attempts));
                }
                assembly.assemble_chat_messages_with_context_cache(
                    &user_prompt,
                    protocol,
                    ctx.prompt_builder.input_cache.context_cache(),
                )
            }
        };

        let response_format = protocol.response_format(
            &context_paths,
            ctx.prompt_builder.patch_strategy,
            Some(&project),
        );
        let schema_str = match &response_format {
            Some(crate::providers::ResponseFormat::JsonSchema { json_schema, .. }) => {
                serde_json::to_string_pretty(&json_schema.schema)
                    .unwrap_or_else(|_| json_schema.schema.to_string())
            }
            _ => String::new(),
        };

        let (llm_tx, mut llm_rx) = mpsc::channel::<StreamEvent>(64);
        let event_sink = ctx.events.clone();
        let mut stream_filter = protocol.create_stream_filter(
            context_paths.clone(),
            root_buf.clone(),
            staged_contents.clone(),
        );

        let forward = tokio::spawn(async move {
            let mut text = String::new();
            let mut stream_error: Option<String> = None;
            let mut cancelled = false;
            let mut completed = false;
            while let Some(event) = llm_rx.recv().await {
                match event {
                    StreamEvent::TextDelta(delta) => {
                        text.push_str(&delta);
                        for filtered in stream_filter.push_chunk(&delta) {
                            event_sink.emit(filtered);
                        }
                    }
                    // The transport voided the interrupted attempt and re-issued
                    // the request, so the payload accumulated for decoding and the
                    // streamed preview restart together with the new attempt.
                    StreamEvent::ResetStream => {
                        text.clear();
                        stream_filter.reset();
                        event_sink.emit(StreamEvent::ResetStream);
                    }
                    StreamEvent::Error(message) => {
                        event_sink.emit(StreamEvent::Error(message.clone()));
                        stream_error = Some(message);
                    }
                    StreamEvent::Cancelled => {
                        event_sink.emit(StreamEvent::Cancelled);
                        cancelled = true;
                    }
                    StreamEvent::Done => {
                        completed = true;
                        event_sink.emit(StreamEvent::Done);
                    }
                    other => event_sink.emit(other),
                }
            }
            for filtered in stream_filter.finish() {
                event_sink.emit(filtered);
            }
            (text, stream_error, cancelled, completed)
        });

        let client_result = ctx
            .prompt_builder
            .provider
            .stream_chat_with_session(
                &ctx.prompt_builder.model.name,
                messages,
                response_format,
                ctx.prompt_builder.reasoning_effort,
                ctx.prompt_builder.session_id.as_deref(),
                llm_tx,
                ctx.cancel.receiver(),
            )
            .await;

        client_result.map_err(|error| SystemError::Provider(error.to_string()))?;

        let (assistant_text, stream_error, cancelled, completed) = forward
            .await
            .map_err(|error| SystemError::Other(format!("control stream task failed: {error}")))?;

        if cancelled || ctx.cancel.is_cancelled() {
            return Err(SystemError::Cancelled);
        }
        if let Some(message) = stream_error {
            return Err(SystemError::Provider(message));
        }
        // A body that ended without its terminal event was interrupted, so the
        // bytes it accumulated are a fragment of an answer rather than an answer.
        if !completed {
            return Err(SystemError::Provider(
                "the provider stream closed without completing".to_string(),
            ));
        }

        let commands = if assistant_text.trim().is_empty() {
            Vec::new()
        } else {
            protocol.decode(&assistant_text, &context_paths)
        };

        let protocol_errors: Vec<_> = commands
            .iter()
            .filter_map(|c| match c {
                tauqe_protocol::LlmCommand::ProtocolError { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        if !protocol_errors.is_empty() {
            tracing::warn!(
                repo_root = %ctx.repo_root.display(),
                errors = ?protocol_errors,
                schema = %schema_str,
                raw_response = %assistant_text,
                "protocol violation detected in model response"
            );
        }

        // The answer belongs to the transcript the moment it arrives: the round a
        // contour requests next replays it even before that contour produced the
        // feedback the round after it carries.
        if matches!(round_assembly, RoundAssembly::Transcript) {
            transcript_handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push_assistant(assistant_text.as_str());
        }

        if !commands.is_empty() {
            return Ok(ResponseEnvelope::from_commands(&commands).with_raw_text(assistant_text));
        }

        tracing::warn!(
            repo_root = %ctx.repo_root.display(),
            attempt = empty_attempts + 1,
            schema = %schema_str,
            raw_response = %assistant_text,
            "model round produced zero actionable commands"
        );

        // Silence is a transport-level non-answer: asking again with an explicit
        // result contract keeps a structurally empty reply from being recorded as
        // a conclusion, and the corrective budget bounds the asking.
        if empty_attempts >= MAX_EMPTY_STEP_RETRIES {
            tracing::warn!(
                repo_root = %ctx.repo_root.display(),
                attempts = empty_attempts,
                schema = %schema_str,
                raw_response = %assistant_text,
                "the model answered with nothing actionable after every corrective round"
            );
            return Err(SystemError::Other(format!(
                "The model produced an empty response after {empty_attempts} additional attempt(s)."
            )));
        }
        empty_attempts += 1;
        let banner = format!(
            "\n\n---\n**[Empty Response {}/{}]** The previous reply carried nothing actionable; asking for a concrete result...\n\n",
            empty_attempts, MAX_EMPTY_STEP_RETRIES
        );
        ctx.events.emit(StreamEvent::TextDelta(banner));

        // The reminder is the feedback of the corrective round, so it travels through
        // the single feedback slot exactly as every other correction of the turn does.
        if matches!(round_assembly, RoundAssembly::Transcript) {
            transcript_handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .set_pending_feedback(EMPTY_STEP_REMINDER);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink, LoopContext};
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::policy::DirectApplyPolicy;
    use crate::workflow::research::journal::TurnResearchJournal;
    use tauqe_protocol::{CapabilityTree, LlmCapability, ModelRef};

    /// A round appends its own pair and never rewrites the head of the turn.
    ///
    /// The transcript is the only channel the working memory travels through: the
    /// head a turn seeds is reproduced byte for byte by every later round, and the
    /// note the model wrote reaches the next round inside the assistant answer
    /// that produced it.
    #[tokio::test]
    async fn a_round_keeps_the_seeded_head_and_replays_its_own_scratchpad_answer() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let mut context = ContextManager::new(dir.path().to_path_buf());
        let mut journal = TurnResearchJournal::default();
        let provider = MockLlmProvider::scripted(vec![
            MockResponse::text(
                "The helper module answers the question.\n\n<scratchpad>\n- Hypothesis: helper defines ANSWER\n</scratchpad>",
            ),
            MockResponse::text("The helper already defines the answer."),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = JsonPromptProtocol::edits();
        let capabilities = CapabilityTree::from_leaves([LlmCapability::TextMessage]);
        let cancel = CancellationToken::new();
        let events = EventSink::noop();
        let mut cache = crate::control::cache::TurnInputCache::default();
        let prompt_builder = PromptBuilder::new(&provider, &model, &protocol, &mut cache);
        let mut policy = DirectApplyPolicy::new();
        let mut ctx = LoopContext::new(
            &cancel,
            &events,
            dir.path(),
            &mut context,
            &mut journal,
            &mut policy,
            capabilities,
            prompt_builder,
        );

        let first = execute_round(&mut ctx, None).await.expect("first round");
        assert!(first.has(LlmCapability::TextMessage));

        let seeded = ctx.transcript().messages();
        assert!(seeded.len() > 1);
        assert_eq!(seeded[0].role, "system");
        // The head carries the contract and the outline alone: a section rewritten
        // between rounds would move a byte the provider has already cached.
        assert!(!seeded[0].content.contains("## Turn Scratchpad"));
        assert!(!seeded[0].content.contains("<scratchpad>"));

        ctx.request_round().await.expect("second round");
        let extended = ctx.transcript().messages();
        assert_eq!(
            &extended[..seeded.len()],
            &seeded[..],
            "a round appends its pair and never rewrites a message before it"
        );
        // The note the model wrote travels back through its own answer, which the
        // transcript replays unchanged.
        assert!(extended.iter().any(|message| {
            message.role == "assistant"
                && message
                    .content
                    .contains("- Hypothesis: helper defines ANSWER")
        }));
    }

    /// A context churn the change set marked reseeds the prefix: the rewritten
    /// file renders again from the bytes now on disk, and the relocation
    /// moves it to the tail of its layer, so the repair or healing round
    /// addresses fresh material instead of the rendering the turn opened
    /// with.
    #[tokio::test]
    async fn a_context_churn_reseeds_the_prefix_with_fresh_bytes_at_the_tail() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "original a\n").unwrap();
        std::fs::write(root.join("b.rs"), "beta\n").unwrap();

        let mut context = ContextManager::new(root.clone());
        context
            .add_file("a.rs", tauqe_protocol::ContextAccess::Editable)
            .unwrap();
        context
            .add_file("b.rs", tauqe_protocol::ContextAccess::ReadOnly)
            .unwrap();

        let mut journal = TurnResearchJournal::default();
        let provider = MockLlmProvider::scripted(vec![
            MockResponse::text("first answer"),
            MockResponse::text("second answer"),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = JsonPromptProtocol::edits();
        let cancel = CancellationToken::new();
        let events = EventSink::noop();
        let mut cache = crate::control::cache::TurnInputCache::default();
        let prompt_builder = PromptBuilder::new(&provider, &model, &protocol, &mut cache);
        let mut policy = DirectApplyPolicy::new();
        let mut ctx = LoopContext::new(
            &cancel,
            &events,
            dir.path(),
            &mut context,
            &mut journal,
            &mut policy,
            CapabilityTree::from_leaves([LlmCapability::TextMessage]),
            prompt_builder,
        );

        let first = execute_round(&mut ctx, None).await.expect("first round");
        assert!(first.has(LlmCapability::TextMessage));
        let seeded = ctx.transcript().messages();

        let first_requests = provider.requests();
        let first_context = first_requests[0]
            .messages
            .iter()
            .find(|message| message.content.starts_with("<context>"))
            .expect("the opening round renders the context block");
        assert!(first_context.content.contains("original a"));
        let a_open = first_context
            .content
            .find(r#"path="a.rs""#)
            .expect("the element is placed");
        let b_open = first_context
            .content
            .find(r#"path="b.rs""#)
            .expect("the neighbor is placed");
        assert!(a_open < b_open, "the opening round renders a.rs first");

        // The turn rewrote the file and marked the churn the staging loop
        // marks: the relocation moves the rewritten path to the tail of its
        // layer and the epoch stales the rendered prefix.
        std::fs::write(root.join("a.rs"), "rewritten a\n").unwrap();
        ctx.note_context_churn(&["a.rs".to_string()], &[]);
        assert!(ctx.transcript().prefix_is_stale());

        let second = execute_round(&mut ctx, None).await.expect("second round");
        assert!(second.has(LlmCapability::TextMessage));

        let second_requests = provider.requests();
        let second_context = second_requests[1]
            .messages
            .iter()
            .find(|message| message.content.starts_with("<context>"))
            .expect("the reseeded round renders the context block again");
        assert!(
            second_context.content.contains("rewritten a"),
            "the reseeded prefix renders the bytes now on disk"
        );
        assert!(
            !second_context.content.contains("original a"),
            "the stale rendering is gone"
        );
        let a_reseed = second_context
            .content
            .find(r#"path="a.rs""#)
            .expect("the element is placed");
        let b_reseed = second_context
            .content
            .find(r#"path="b.rs""#)
            .expect("the neighbor is placed");
        assert!(
            b_reseed < a_reseed,
            "the relocation renders the rewritten file after its untouched neighbor"
        );

        // The task and the round already exchanged keep their exact bytes; the
        // answer of the second round appends behind them as the new tail.
        let extended = ctx.transcript().messages();
        assert_eq!(extended.len(), seeded.len() + 1);
        assert_eq!(
            extended[seeded.len() - 1].content,
            "first answer",
            "the exchanged round keeps its bytes across the reseed"
        );
    }
}
