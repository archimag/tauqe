//! Prompt assembly and model round execution for control loops.

use tauqe_protocol::{
    ContextAccess, ModelRef, PatchStrategy, PromptLayoutKind, ReasoningEffort, TurnIntent,
};
use tokio::sync::mpsc;

use super::context::LoopContext;
use super::outcome::{ResponseEnvelope, SystemError};
use crate::config::OuterDocEntry;
use crate::context::ContextFileContent;
use crate::edits::StagedEditsState;
use crate::history::HistorySnapshot;
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::PromptAssembly;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::fsm::cache::TurnInputCache;

/// Dedicated builder and executor for model prompt rounds.
///
/// Encapsulates model and protocol parameters, session affinity, prompt caching,
/// and repository outline options away from the ambient execution context.
pub struct PromptBuilder<'a> {
    pub(crate) provider: &'a dyn LlmProvider,
    pub(crate) model: &'a ModelRef,
    pub(crate) protocol: &'a dyn PromptProtocol,
    pub(crate) input_cache: &'a mut TurnInputCache,
    pub(crate) session_id: Option<String>,
    pub(crate) prompt: Option<String>,
    pub(crate) history: Option<&'a HistorySnapshot>,
    pub(crate) include_repo_map: bool,
    pub(crate) repomap_token_budget: usize,
    pub(crate) patch_strategy: PatchStrategy,
    pub(crate) prompt_layout: Option<PromptLayoutKind>,
    pub(crate) workflow: String,
    pub(crate) outer_docs: Vec<OuterDocEntry>,
    pub(crate) project_config: Option<tauqe_protocol::ProjectConfig>,
    pub(crate) intent: TurnIntent,
    pub(crate) scratchpad: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
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
            prompt_layout: None,
            workflow: "git".to_string(),
            outer_docs: Vec::new(),
            project_config: None,
            intent: TurnIntent::InteractiveRepl,
            scratchpad: None,
            reasoning_effort: None,
        }
    }

    /// Derives a reborrowed child builder sharing the underlying cache.
    pub fn reborrow<'b>(&'b mut self) -> PromptBuilder<'b> {
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
            prompt_layout: self.prompt_layout,
            workflow: self.workflow.clone(),
            outer_docs: self.outer_docs.clone(),
            project_config: self.project_config.clone(),
            intent: self.intent,
            scratchpad: self.scratchpad.clone(),
            reasoning_effort: self.reasoning_effort,
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

    pub fn with_prompt_layout(mut self, prompt_layout: Option<PromptLayoutKind>) -> Self {
        self.prompt_layout = prompt_layout;
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

    pub fn with_project_config(
        mut self,
        project_config: Option<tauqe_protocol::ProjectConfig>,
    ) -> Self {
        self.project_config = project_config;
        self
    }

    pub fn with_intent(mut self, intent: TurnIntent) -> Self {
        self.intent = intent;
        self
    }

    pub fn with_scratchpad(mut self, scratchpad: Option<String>) -> Self {
        self.scratchpad = scratchpad;
        self
    }

    pub fn with_reasoning_effort(mut self, reasoning_effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = reasoning_effort;
        self
    }

    pub fn set_scratchpad(&mut self, scratchpad: Option<String>) -> &mut Self {
        self.scratchpad = scratchpad;
        self
    }

    pub fn set_history(&mut self, history: Option<&'a HistorySnapshot>) -> &mut Self {
        self.history = history;
        self
    }

    pub fn set_repo_map_config(
        &mut self,
        include_repo_map: bool,
        token_budget: usize,
    ) -> &mut Self {
        self.include_repo_map = include_repo_map;
        self.repomap_token_budget = token_budget;
        self
    }
}

pub(crate) async fn execute_round(
    ctx: &mut LoopContext<'_>,
    repair_prompt: Option<&str>,
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

    assembly.target_language = ctx.target_language.clone();
    assembly.intent = ctx.prompt_builder.intent;
    assembly.turn_scratchpad = ctx.prompt_builder.scratchpad.clone();
    if let Some(layout) = ctx.prompt_builder.prompt_layout {
        assembly.prompt_layout = layout;
    }

    let base_protocol = ctx
        .prompt_builder
        .protocol
        .with_capability_set(ctx.capabilities.clone());
    let effective_caps = assembly.effective_capabilities(base_protocol.as_ref());
    let scoped_protocol = base_protocol.with_capability_set(effective_caps);
    let protocol: &dyn PromptProtocol = scoped_protocol.as_ref();

    let base_prompt = ctx.prompt_builder.prompt.as_deref().unwrap_or("");
    let user_prompt = ctx.journal.compose_user_prompt(base_prompt, repair_prompt);

    let messages = assembly.assemble_chat_messages_with_context_cache(
        &user_prompt,
        protocol,
        ctx.prompt_builder.input_cache.context_cache(),
    );

    let response_format = protocol.response_format(
        &context_paths,
        ctx.prompt_builder.patch_strategy,
        Some(&project),
    );

    let (llm_tx, mut llm_rx) = mpsc::channel::<StreamEvent>(64);
    let event_sink = ctx.events.clone();
    let staged_contents: std::collections::HashMap<String, String> = context_files
        .iter()
        .map(|f| (f.path.clone(), f.content.clone()))
        .collect();
    let mut stream_filter =
        protocol.create_stream_filter(context_paths.clone(), root_buf, staged_contents);

    let forward = tokio::spawn(async move {
        let mut text = String::new();
        let mut stream_error: Option<String> = None;
        let mut cancelled = false;
        while let Some(event) = llm_rx.recv().await {
            match event {
                StreamEvent::TextDelta(delta) => {
                    text.push_str(&delta);
                    for filtered in stream_filter.push_chunk(&delta) {
                        event_sink.emit(filtered);
                    }
                }
                StreamEvent::Error(message) => {
                    event_sink.emit(StreamEvent::Error(message.clone()));
                    stream_error = Some(message);
                }
                StreamEvent::Cancelled => {
                    event_sink.emit(StreamEvent::Cancelled);
                    cancelled = true;
                }
                other => event_sink.emit(other),
            }
        }
        for filtered in stream_filter.finish() {
            event_sink.emit(filtered);
        }
        (text, stream_error, cancelled)
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

    let (assistant_text, stream_error, cancelled) = forward
        .await
        .map_err(|error| SystemError::Other(format!("control stream task failed: {error}")))?;

    if cancelled || ctx.cancel.is_cancelled() {
        return Err(SystemError::Cancelled);
    }
    if let Some(message) = stream_error {
        return Err(SystemError::Provider(message));
    }
    if assistant_text.trim().is_empty() {
        return Ok(ResponseEnvelope::default());
    }

    let commands = protocol.decode(&assistant_text, &context_paths);
    Ok(ResponseEnvelope::from_commands(&commands).with_raw_text(assistant_text))
}
