//! Prompt assembly and model round execution for control loops.

#[cfg(test)]
use crate::context::ContextAccess;
use crate::models::{ModelRef, ReasoningEffort};
use crate::workflow::PatchStrategy;
use tokio::sync::mpsc;

use super::context::LoopContext;
use super::outcome::{ResponseEnvelope, SystemError};
use crate::config::OuterDocEntry;
use crate::control::cache::TurnInputCache;
use crate::edits::StagedEditsState;
use crate::history::HistorySnapshot;
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::PromptAssembly;
use crate::prompt::TurnBrief;
use crate::providers::types::TurnRequestReason;
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
    pub(crate) project_config: Option<crate::config::project::ProjectConfig>,
    /// Introductory brief of the turn, owned by the loop that drives it.
    turn_brief: Option<TurnBrief>,
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

    pub(crate) fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    pub(crate) fn with_prompt(mut self, prompt: Option<String>) -> Self {
        self.prompt = prompt;
        self
    }

    pub(crate) fn with_history(mut self, history: Option<&'a HistorySnapshot>) -> Self {
        self.history = history;
        self
    }

    pub(crate) fn with_repo_map_config(
        mut self,
        include_repo_map: bool,
        token_budget: usize,
    ) -> Self {
        self.include_repo_map = include_repo_map;
        self.repomap_token_budget = token_budget;
        self
    }

    pub(crate) fn with_patch_strategy(mut self, patch_strategy: PatchStrategy) -> Self {
        self.patch_strategy = patch_strategy;
        self
    }

    pub(crate) fn with_workflow(mut self, workflow: impl Into<String>) -> Self {
        self.workflow = workflow.into();
        self
    }

    pub(crate) fn with_outer_docs(mut self, outer_docs: Vec<OuterDocEntry>) -> Self {
        self.outer_docs = outer_docs;
        self
    }

    pub(crate) fn with_turn_brief(mut self, turn_brief: Option<TurnBrief>) -> Self {
        self.turn_brief = turn_brief;
        self
    }

    pub(crate) fn with_reasoning_effort(
        mut self,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
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

/// Feedback sent to the model after a round produces nothing actionable.
const EMPTY_STEP_REMINDER: &str = "\n\nYour previous reply produced no actionable result. Provide a concrete result that conforms to the active response schema.";

/// Message sequence a round sends to its provider.
///
/// The contour is declared by the typed value the loop that drives the turn passes,
/// never inferred from the shape of the capability surface: the bounded turn replays
/// the shared transcript of the turn, while an Architect synthesis assembles its own
/// sequence on every round.
#[derive(Clone, Copy)]
enum RoundAssembly<'a> {
    /// The shared round transcript of the turn.
    Transcript { reason: Option<TurnRequestReason> },
    /// The contour reassembles its own request on every round.
    Rebuild {
        /// Diagnostic the contour carries into its next request.
        diagnostics: Option<&'a str>,
    },
}

/// Runs one round of a contour that replays the shared transcript of the turn.
///
/// The first round of a turn seeds the transcript with the rendered prefix and the
/// frozen task message. Later rounds append newly discovered original files.
/// Each request has its own feedback and working file versions.
pub(crate) async fn execute_round(
    ctx: &mut LoopContext<'_>,
    staged_state: Option<&StagedEditsState>,
    reason: Option<TurnRequestReason>,
) -> Result<ResponseEnvelope, SystemError> {
    execute_round_inner(ctx, RoundAssembly::Transcript { reason }, staged_state).await
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
    let context_files = ctx
        .prompt_builder
        .input_cache
        .context_files(&root_buf, &ctx_state.items)
        .await
        .map_err(|e| SystemError::Other(e.to_string()))?;

    let files = ctx.transcript().project_files(
        context_files,
        staged_state,
        &root_buf,
        ctx.prompt_builder.patch_strategy,
    );
    let super::transcript::RoundFiles {
        originals,
        current: context_files,
        current_versions,
    } = files;
    let context_paths: Vec<String> = context_files.iter().map(|f| f.path.clone()).collect();

    let (repo_map, repo_dirs) = if ctx.prompt_builder.include_repo_map {
        let map = ctx
            .prompt_builder
            .input_cache
            .repo_map(&root_buf, ctx.prompt_builder.repomap_token_budget)
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
        originals,
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

    // The protocol was constructed with the contour's capability tree. A round
    // binds only the project declaration used by its schema and decoder.
    let scoped_protocol = ctx
        .prompt_builder
        .protocol
        .with_project(Some(project.clone()));
    let protocol: &dyn PromptProtocol = scoped_protocol.as_ref();

    let base_prompt = ctx.prompt_builder.prompt.as_deref().unwrap_or("");
    let rebuilt_user_prompt = match round_assembly {
        RoundAssembly::Transcript { .. } => None,
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
    let mut round_feedback = String::new();
    loop {
        let spent = ctx.rounds_spent();
        let max_rounds = ctx.prompt_builder.turn_round_limit;
        if let Some(limit) = max_rounds {
            if spent >= limit {
                return Err(SystemError::RoundBudgetExhausted {
                    spent,
                    last_answer: ctx.transcript().last_answer.clone(),
                });
            }
            if spent + 1 == limit && matches!(round_assembly, RoundAssembly::Transcript { .. }) {
                let mut transcript = transcript_handle
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut feedback = transcript.take_pending_feedback().unwrap_or_default();
                feedback.push_str(TURN_BUDGET_DIRECTIVE);
                transcript.set_pending_feedback(feedback);
            }
        }

        // Append original files to the stable prefix and render the current tail.
        let messages = match round_assembly {
            RoundAssembly::Transcript { .. } => {
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
                } else if !transcript.advertises(&ctx.capabilities) {
                    let assembled = assembly.assemble_chat_messages_with_context_cache(
                        base_prompt,
                        protocol,
                        ctx.prompt_builder.input_cache.context_cache(),
                    );
                    let prefix = assembled
                        .split_last()
                        .map(|(_, prefix)| prefix.to_vec())
                        .unwrap_or_default();
                    transcript.reseed_prefix(prefix, &ctx.capabilities);
                }
                transcript.append_discoveries(ctx.prompt_builder.patch_strategy);
                if let Some(feedback) = transcript.take_pending_feedback() {
                    if !round_feedback.is_empty() {
                        round_feedback.push_str("\n\n");
                    }
                    round_feedback.push_str(&feedback);
                }
                let mut messages = transcript.messages();
                let mut tail = round_feedback.clone();
                if let Some(block) = &current_versions {
                    if !tail.is_empty() {
                        tail.push_str("\n\n");
                    }
                    tail.push_str(block);
                }
                if !tail.is_empty() {
                    messages.push(crate::providers::ChatMessage::user(tail));
                }
                messages
            }
            RoundAssembly::Rebuild { .. } => {
                let mut user_prompt = rebuilt_user_prompt.clone().unwrap_or_default();
                if empty_attempts > 0 {
                    user_prompt.push_str(EMPTY_STEP_REMINDER);
                }
                if let Some(block) = &current_versions {
                    user_prompt.push_str("\n\n");
                    user_prompt.push_str(block);
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
        let mut stream_filter = protocol.create_stream_filter_with_strategy(
            context_paths.clone(),
            root_buf.clone(),
            staged_contents.clone(),
            ctx.prompt_builder.patch_strategy,
        );

        if let Some(staged) = staged_state {
            let missing = staged
                .deleted_paths
                .iter()
                .chain(
                    staged
                        .staged_files
                        .keys()
                        .filter(|path| !staged_contents.contains_key(*path)),
                )
                .cloned()
                .collect();
            stream_filter.exclude_disk_sources(missing);
        }

        let forward = tokio::spawn(async move {
            let mut text = String::new();
            let mut stream_error: Option<String> = None;
            let mut cancelled = false;
            let mut completed = false;
            let mut token_usage = None;
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
                        token_usage = None;
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
                    StreamEvent::Usage(usage) => {
                        token_usage = Some((usage.prompt_tokens, usage.cached_tokens));
                        event_sink.emit(StreamEvent::Usage(usage));
                    }
                    other => event_sink.emit(other),
                }
            }
            for filtered in stream_filter.finish() {
                event_sink.emit(filtered);
            }
            (text, stream_error, cancelled, completed, token_usage)
        });

        ctx.check_cancelled()?;
        let round = ctx
            .rounds_spent
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if let RoundAssembly::Transcript { reason } = round_assembly {
            ctx.events.emit(StreamEvent::TurnRoundStarted {
                round,
                max_rounds,
                reason: if empty_attempts == 0 {
                    reason
                } else {
                    Some(TurnRequestReason::EmptyResponseRetry)
                },
            });
        }
        let client_result = ctx
            .prompt_builder
            .provider
            .stream(
                crate::providers::LlmRequest {
                    model: &ctx.prompt_builder.model.name,
                    messages,
                    response_format,
                    reasoning_effort: ctx.prompt_builder.reasoning_effort,
                    session_id: ctx.prompt_builder.session_id.as_deref(),
                },
                llm_tx,
                ctx.cancel.receiver(),
            )
            .await;

        client_result.map_err(|error| SystemError::Provider(error.to_string()))?;

        let (assistant_text, stream_error, cancelled, completed, token_usage) = forward
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

        if let Some((prompt_tokens, cached_tokens)) = token_usage {
            tracing::info!(round, model = %ctx.prompt_builder.model.name, prompt_tokens, ?cached_tokens,
                "model request usage");
        }
        let has_answer = !assistant_text.trim().is_empty();
        let commands = if has_answer {
            protocol.decode_with_strategy(
                &assistant_text,
                &context_paths,
                ctx.prompt_builder.patch_strategy,
            )
        } else {
            Vec::new()
        };

        let protocol_errors: Vec<_> = commands
            .iter()
            .filter_map(|c| match c {
                crate::llm::LlmCommand::ProtocolError { message, .. } => Some(message.as_str()),
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

        if has_answer && matches!(round_assembly, RoundAssembly::Transcript { .. }) {
            transcript_handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .last_answer = Some(assistant_text.clone());
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

        // The reminder is the feedback of the corrective round, so it travels through
        // the single feedback slot exactly as every other correction of the turn does.
        if matches!(round_assembly, RoundAssembly::Transcript { .. }) {
            transcript_handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .set_pending_feedback(if empty_attempts == 1 {
                    EMPTY_STEP_REMINDER
                } else {
                    ""
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink, LoopContext};
    use crate::llm::CapabilityTree;
    use crate::llm::LlmCapability;
    use crate::models::ModelRef;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::policy::DirectApplyPolicy;
    use crate::workflow::research::journal::TurnResearchJournal;

    struct ReconnectingProvider;

    #[async_trait::async_trait]
    impl LlmProvider for ReconnectingProvider {
        fn kind(&self) -> crate::models::ProviderKind {
            crate::models::ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            request: crate::providers::LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            _: tokio::sync::watch::Receiver<bool>,
        ) -> crate::providers::ProviderResult {
            assert!(request
                .messages
                .iter()
                .any(|msg| msg.content.contains(TURN_BUDGET_DIRECTIVE.trim())));
            for event in [
                StreamEvent::ReasoningDelta("initial attempt".into()),
                StreamEvent::ResetStream,
                StreamEvent::ReasoningDelta("retried attempt".into()),
                StreamEvent::TextDelta("answer".into()),
                StreamEvent::Done,
            ] {
                tx.send(event).await.unwrap();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn only_admitted_calls_publish_rounds_before_provider_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let mut context = ContextManager::new(dir.path().to_path_buf());
        let mut journal = TurnResearchJournal::default();
        let provider = ReconnectingProvider;
        let model = ModelRef::openrouter("mock/model");
        let protocol =
            JsonPromptProtocol::new(CapabilityTree::from_leaves([LlmCapability::TextMessage]));
        let cancel = CancellationToken::new();
        let (events, mut rx) = EventSink::channel();
        let mut cache = TurnInputCache::default();
        let builder = PromptBuilder::new(&provider, &model, &protocol, &mut cache);
        let mut policy = DirectApplyPolicy::new();
        let mut ctx = LoopContext::new(
            &cancel,
            &events,
            dir.path(),
            &mut context,
            &mut journal,
            &mut policy,
            CapabilityTree::from_leaves([LlmCapability::TextMessage]),
            builder,
        );
        ctx.set_turn_round_limit(Some(1));
        assert!(ctx
            .request_round()
            .await
            .unwrap()
            .has(LlmCapability::TextMessage));
        assert!(matches!(
            ctx.request_round().await,
            Err(SystemError::RoundBudgetExhausted { .. })
        ));
        assert_eq!(ctx.rounds_spent(), 1);

        let observed: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| {
                matches!(
                    event,
                    StreamEvent::TurnRoundStarted { .. }
                        | StreamEvent::ReasoningDelta(_)
                        | StreamEvent::ResetStream
                )
            })
            .collect();
        assert!(matches!(
            observed.as_slice(),
            [
                StreamEvent::TurnRoundStarted {
                    round: 1,
                    max_rounds: Some(1),
                    reason: None,
                },
                StreamEvent::ReasoningDelta(_),
                StreamEvent::ResetStream,
                StreamEvent::ReasoningDelta(_),
            ]
        ));
    }

    #[tokio::test]
    async fn empty_responses_spend_calls_without_adding_assistant_messages_or_banners() {
        for (assembly, limit) in [
            (RoundAssembly::Transcript { reason: None }, Some(3)),
            (RoundAssembly::Transcript { reason: None }, Some(1)),
            (RoundAssembly::Rebuild { diagnostics: None }, None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut context = ContextManager::new(dir.path().to_path_buf());
            let mut journal = TurnResearchJournal::default();
            let provider = MockLlmProvider::scripted([
                MockResponse::text(" \n\t"),
                MockResponse::text(""),
                MockResponse::text("answer"),
            ]);
            let model = ModelRef::openrouter("mock/model");
            let protocol =
                JsonPromptProtocol::new(CapabilityTree::from_leaves([LlmCapability::TextMessage]));
            let cancel = CancellationToken::new();
            let (events, mut rx) = EventSink::channel();
            let mut cache = TurnInputCache::default();
            let builder = PromptBuilder::new(&provider, &model, &protocol, &mut cache)
                .with_prompt(Some("Return a result".into()));
            let mut policy = DirectApplyPolicy::new();
            let mut ctx = LoopContext::new(
                &cancel,
                &events,
                dir.path(),
                &mut context,
                &mut journal,
                &mut policy,
                CapabilityTree::from_leaves([LlmCapability::TextMessage]),
                builder,
            );
            ctx.set_turn_round_limit(limit);

            let result = execute_round_inner(&mut ctx, assembly, None).await;
            let requests = provider.requests();
            let mut rounds = Vec::new();
            while let Ok(event) = rx.try_recv() {
                match event {
                    StreamEvent::TurnRoundStarted {
                        round,
                        max_rounds,
                        reason,
                    } => rounds.push((round, max_rounds, reason)),
                    StreamEvent::TextDelta(text) => assert!(!text.contains("[Empty Response")),
                    _ => {}
                }
            }

            if limit == Some(1) {
                assert!(matches!(
                    result,
                    Err(SystemError::RoundBudgetExhausted { .. })
                ));
                assert_eq!(ctx.rounds_spent(), 1);
                assert_eq!(requests.len(), 1);
                assert_eq!(rounds, [(1, limit, None)]);
            } else {
                assert!(result.unwrap().has(LlmCapability::TextMessage));
                assert_eq!(ctx.rounds_spent(), 3);
                assert_eq!(requests.len(), 3);
                if limit.is_some() {
                    for pair in requests.windows(2) {
                        assert!(pair[1].messages.starts_with(&requests[0].messages));
                        assert_eq!(pair[1].response_format, pair[0].response_format);
                    }
                    let retry = Some(TurnRequestReason::EmptyResponseRetry);
                    assert_eq!(
                        rounds,
                        [(1, limit, None), (2, limit, retry), (3, limit, retry)]
                    );
                } else {
                    assert!(rounds.is_empty(), "Architect progress stays separate");
                }
                for request in &requests[1..] {
                    let feedback = request.messages.last().unwrap();
                    assert_eq!(feedback.role, "user");
                    assert_eq!(
                        feedback.content.matches(EMPTY_STEP_REMINDER.trim()).count(),
                        1
                    );
                }
            }
            for request in requests {
                assert!(!request
                    .messages
                    .iter()
                    .any(|message| message.role == "assistant"));
            }
        }
    }

    /// A round appends its own pair and never rewrites the head of the turn.
    ///
    #[tokio::test]
    async fn a_round_keeps_the_seeded_head_without_replaying_the_answer() {
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
        let capabilities = CapabilityTree::from_leaves([LlmCapability::TextMessage]);
        let protocol = JsonPromptProtocol::new(capabilities.clone());
        let cancel = CancellationToken::new();
        let events = EventSink::noop();
        let mut cache = TurnInputCache::default();
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

        let first = ctx.request_round().await.expect("first round");
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
        assert_eq!(extended, seeded);
        assert_eq!(provider.requests()[1].messages, seeded);
    }

    #[tokio::test]
    async fn changed_bytes_travel_after_the_frozen_context() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "original a\n").unwrap();
        std::fs::write(root.join("b.rs"), "beta\n").unwrap();

        let mut context = ContextManager::new(root.clone());
        context
            .add_file("a.rs", crate::context::ContextAccess::Editable)
            .unwrap();
        context
            .add_file("b.rs", crate::context::ContextAccess::ReadOnly)
            .unwrap();

        let mut journal = TurnResearchJournal::default();
        let provider = MockLlmProvider::scripted(vec![
            MockResponse::text("first answer"),
            MockResponse::text("second answer"),
            MockResponse::text("restored"),
            MockResponse::text("rebuilt"),
            MockResponse::text("next turn"),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol =
            JsonPromptProtocol::new(CapabilityTree::from_leaves([LlmCapability::TextMessage]));
        let cancel = CancellationToken::new();
        let events = EventSink::noop();
        let mut cache = TurnInputCache::default();
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

        let first = ctx.request_round().await.expect("first round");
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

        std::fs::write(root.join("a.rs"), "rewritten a\n").unwrap();
        let second = ctx.request_round().await.expect("second round");
        assert!(second.has(LlmCapability::TextMessage));

        let second_requests = provider.requests();
        let second_context = second_requests[1]
            .messages
            .iter()
            .find(|message| message.content.starts_with("<context>"))
            .expect("the reseeded round renders the context block again");
        assert_eq!(first_context, second_context);
        assert!(second_requests[1]
            .messages
            .starts_with(&first_requests[0].messages));
        let latest = &second_requests[1].messages.last().unwrap().content;
        assert!(latest.contains("<current_versions>"));
        assert!(latest.contains("rewritten a"));
        assert!(!latest.contains("b.rs"));

        assert_eq!(ctx.transcript().messages(), seeded);

        ctx.context_manager
            .add_file("b.rs", ContextAccess::Editable)
            .unwrap();
        let mut staged = StagedEditsState::default();
        staged
            .staged_files
            .insert("a.rs".into(), b"original a\n".to_vec());
        execute_round(&mut ctx, Some(&staged), None).await.unwrap();
        let restored = provider.requests().pop().unwrap();
        assert!(restored.messages.contains(first_context));
        assert!(restored.messages.starts_with(&seeded));
        assert!(!restored
            .messages
            .iter()
            .any(|message| message.content.contains("rewritten a")));

        assert!(restored
            .messages
            .last()
            .unwrap()
            .content
            .contains(r#"<access path="b.rs" value="editable" />"#));
        assert_eq!(restored.response_format, second_requests[1].response_format);

        staged.deleted_paths.insert("a.rs".into());
        staged.staged_files.clear();
        staged
            .staged_files
            .insert("moved.rs".into(), b"original a\n".to_vec());
        execute_round_inner(
            &mut ctx,
            RoundAssembly::Rebuild { diagnostics: None },
            Some(&staged),
        )
        .await
        .unwrap();
        let rebuilt = provider.requests().pop().unwrap();
        assert!(rebuilt.messages.contains(first_context));
        let tail = &rebuilt.messages.last().unwrap().content;
        assert!(tail.contains(r#"<deleted path="a.rs" />"#));
        assert!(tail.contains(r#"<file path="moved.rs" access="editable">"#));
        assert!(!tail.contains("rewritten a"));

        drop(ctx);
        let builder = PromptBuilder::new(&provider, &model, &protocol, &mut cache);
        let mut next = LoopContext::new(
            &cancel,
            &events,
            dir.path(),
            &mut context,
            &mut journal,
            &mut policy,
            CapabilityTree::from_leaves([LlmCapability::TextMessage]),
            builder,
        );
        next.request_round().await.unwrap();
        let request = provider.requests().pop().unwrap();
        let baseline = request
            .messages
            .iter()
            .find(|m| m.content.starts_with("<context>"))
            .unwrap();
        assert!(baseline.content.contains("rewritten a"));
        assert!(!baseline.content.contains("original a"));
        assert!(!request
            .messages
            .iter()
            .any(|m| m.content.contains("<current_versions>\n")));
    }
}
