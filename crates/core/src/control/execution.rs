//! Shared preparation of the context used by bounded turn execution.

use super::cache::TurnInputCache;
use super::context::{CancellationToken, EventSink, LoopContext};
use super::prompt::PromptBuilder;
use crate::config::AppConfig;
use crate::context::ContextManager;
use crate::history::HistorySnapshot;
use crate::models::{ModelRef, ReasoningEffort};
use crate::prompt::{protocol::PromptProtocol, TurnBrief};
use crate::providers::LlmProvider;
use crate::workflow::{policy::ApplyPolicy, research::journal::TurnResearchJournal};
use std::path::Path;

/// Provider and project settings resolved before execution begins.
pub struct TurnExecutionConfig<'a> {
    pub repo_root: &'a Path,
    pub provider: &'a dyn LlmProvider,
    pub model: &'a ModelRef,
    pub protocol: &'a dyn PromptProtocol,
    pub config: &'a AppConfig,
    pub session_id: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Presentation inputs supplied by the calling scenario.
pub(super) struct TurnPrompt<'a> {
    pub prompt: Option<String>,
    pub history: Option<&'a HistorySnapshot>,
    pub brief: Option<TurnBrief>,
}

/// Owned inputs borrowed by the loop and all of its child contexts.
#[derive(Default)]
pub struct TurnInputs {
    journal: TurnResearchJournal,
    cache: TurnInputCache,
}

impl TurnInputs {
    pub fn prepare_step<'a>(
        &'a mut self,
        execution: TurnExecutionConfig<'a>,
        prompt: Option<String>,
        context: &'a mut ContextManager,
        policy: &'a mut dyn ApplyPolicy,
        cancel: &'a CancellationToken,
        events: &'a EventSink,
    ) -> LoopContext<'a> {
        assert_eq!(
            execution.protocol.capability_expr(),
            super::turn::turn_capability_expr()
        );
        self.prepare(
            execution,
            TurnPrompt {
                prompt,
                history: None,
                brief: Some(TurnBrief::step()),
            },
            context,
            policy,
            cancel,
            events,
        )
    }

    /// Starts a sequence member with fresh turn state and the parent's live operation.
    pub(super) fn prepare_member<'a>(
        &'a mut self,
        parent: &'a mut LoopContext<'_>,
    ) -> LoopContext<'a> {
        let mut builder = parent.prompt_builder.reborrow();
        builder.input_cache = &mut self.cache;
        LoopContext::new(
            parent.cancel,
            parent.events,
            parent.repo_root,
            parent.context_manager,
            &mut self.journal,
            parent.policy,
            parent.capabilities.clone(),
            builder,
        )
    }

    pub(super) fn prepare<'a>(
        &'a mut self,
        execution: TurnExecutionConfig<'a>,
        prompt: TurnPrompt<'a>,
        context: &'a mut ContextManager,
        policy: &'a mut dyn ApplyPolicy,
        cancel: &'a CancellationToken,
        events: &'a EventSink,
    ) -> LoopContext<'a> {
        let config = execution.config;
        let builder = PromptBuilder::new(
            execution.provider,
            execution.model,
            execution.protocol,
            &mut self.cache,
        )
        .with_session_id(execution.session_id)
        .with_prompt(prompt.prompt)
        .with_history(prompt.history)
        .with_turn_brief(prompt.brief)
        .with_repo_map_config(
            config.context.include_repo_map,
            config.context.repomap_token_budget,
        )
        .with_patch_strategy(config.repl.patch_strategy)
        .with_workflow(config.repl.workflow.to_string())
        .with_outer_docs(config.outer_docs.clone())
        .with_reasoning_effort(execution.reasoning_effort);
        LoopContext::new(
            cancel,
            events,
            execution.repo_root,
            context,
            &mut self.journal,
            policy,
            execution.protocol.capability_expr(),
            builder,
        )
    }
}
