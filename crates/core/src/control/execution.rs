//! Shared preparation of the context used by bounded turn execution.

use super::cache::TurnInputCache;
use super::context::{CancellationToken, EventSink, LoopContext};
use super::outcome::{LoopResult, ResponseEnvelope, SystemError};
use super::prompt::PromptBuilder;
use super::turn::{run_turn_loop, TurnArtifact, TurnCompletion, TurnLoopConfig};
use crate::config::AppConfig;
use crate::context::ContextManager;
use crate::history::HistorySnapshot;
use crate::llm::CapabilityTree;
use crate::models::{ModelRef, ReasoningEffort};
use crate::prompt::{protocol::PromptProtocol, TurnBrief};
use crate::providers::LlmProvider;
use crate::workflow::WorkflowOptions;
use crate::workflow::{policy::ApplyPolicy, research::journal::TurnResearchJournal};
use std::path::Path;
use std::sync::{atomic::AtomicUsize, Arc, Mutex};

use super::transcript::TurnTranscript;

#[cfg(test)]
mod tests;

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

/// State of one bounded unit, independent of the temporary context borrowing it.
#[derive(Default)]
pub struct ExecutionUnit {
    pub(super) journal: TurnResearchJournal,
    cache: TurnInputCache,
    transcript: Arc<Mutex<TurnTranscript>>,
    rounds_spent: Arc<AtomicUsize>,
}

/// Existing entry points use the same isolated unit state.
pub type TurnInputs = ExecutionUnit;

/// Workflow-owned inputs of one bounded unit. Technical dependencies are supplied
/// separately through `TurnExecutionConfig` and the operation's apply policy.
pub struct BoundedUnitRequest<'a> {
    pub context: &'a mut ContextManager,
    pub history: Option<&'a HistorySnapshot>,
    pub capabilities: CapabilityTree,
    pub task: String,
    pub completion: TurnCompletion,
    pub brief: Option<TurnBrief>,
}

/// Workflow-owned inputs of the next request in an existing bounded unit.
pub struct ContinuationRequest<'a> {
    pub context: &'a mut ContextManager,
    pub history: Option<&'a HistorySnapshot>,
    pub capabilities: CapabilityTree,
    pub diagnostics: &'a str,
}

impl ExecutionUnit {
    /// Prepare a rebuilt structured request without borrowing dialogue history.
    /// The unit retains its cache and journal across subsequent requests.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_rebuilt<'a>(
        &'a mut self,
        execution: TurnExecutionConfig<'a>,
        context: &'a mut ContextManager,
        options: &WorkflowOptions,
        task: &str,
        grant: CapabilityTree,
        policy: &'a mut dyn ApplyPolicy,
        cancel: &'a CancellationToken,
        events: &'a EventSink,
    ) -> LoopContext<'a> {
        let builder = PromptBuilder::new(
            execution.provider,
            execution.model,
            execution.protocol,
            &mut self.cache,
        )
        .with_session_id(execution.session_id)
        .with_prompt(Some(task.to_string()))
        .with_turn_brief(Some(TurnBrief::architect()))
        .with_repo_map_config(
            execution.config.context.include_repo_map,
            options.repomap_token_budget(),
        )
        .with_patch_strategy(execution.config.repl.patch_strategy)
        .with_reasoning_effort(execution.reasoning_effort);
        LoopContext::new(
            cancel,
            events,
            execution.repo_root,
            context,
            &mut self.journal,
            policy,
            grant,
            builder,
        )
        .with_unit_state(Arc::clone(&self.transcript), Arc::clone(&self.rounds_spent))
    }
    /// Executes one bounded unit. Calling this on the same unit retains its
    /// research, original file snapshots, transcript and physical-call budget;
    /// a new `ExecutionUnit` starts an isolated unit instead.
    pub async fn execute_bounded(
        &mut self,
        execution: TurnExecutionConfig<'_>,
        request: BoundedUnitRequest<'_>,
        config: &TurnLoopConfig,
        policy: &mut dyn ApplyPolicy,
        cancel: &CancellationToken,
        events: &EventSink,
    ) -> Result<LoopResult<TurnArtifact>, SystemError> {
        let mut ctx = self.prepare(
            execution,
            TurnPrompt {
                prompt: Some(request.task),
                history: request.history,
                brief: request.brief,
            },
            request.context,
            policy,
            cancel,
            events,
        );
        ctx.capabilities = request.capabilities;
        run_turn_loop(&mut ctx, None, config, request.completion).await
    }

    /// Requests another round in this unit without opening an edit transaction.
    /// Research, original file versions, the transcript and the physical-call
    /// counter remain on the unit; the next grant and history are explicit.
    pub async fn continue_round(
        &mut self,
        execution: TurnExecutionConfig<'_>,
        request: ContinuationRequest<'_>,
        policy: &mut dyn ApplyPolicy,
        cancel: &CancellationToken,
        events: &EventSink,
    ) -> Result<ResponseEnvelope, SystemError> {
        let limit = TurnLoopConfig::from_app_config(execution.config).max_turn_rounds;
        let mut ctx = self.prepare(
            execution,
            TurnPrompt {
                prompt: None,
                history: request.history,
                brief: None,
            },
            request.context,
            policy,
            cancel,
            events,
        );
        ctx.set_turn_round_limit(Some(limit));
        Self::request_continuation(&mut ctx, request.capabilities, None, request.diagnostics).await
    }

    /// Requests another answer in the same unit after a domain payload refusal.
    /// The caller supplies the next grant and history explicitly; `Some(None)`
    /// withdraws previously supplied history. A refusal does not open a new unit.
    pub(crate) async fn request_continuation(
        ctx: &mut LoopContext<'_>,
        capabilities: CapabilityTree,
        history: Option<Option<&HistorySnapshot>>,
        diagnostics: &str,
    ) -> Result<ResponseEnvelope, SystemError> {
        let mut continued = Self::continue_with(ctx, capabilities, history, diagnostics);
        continued.request_round().await
    }

    /// Reborrows the same unit with a new request grant and domain feedback.
    /// `None` preserves the current history; `Some(None)` explicitly removes it.
    pub(crate) fn continue_with<'a>(
        ctx: &'a mut LoopContext<'_>,
        capabilities: CapabilityTree,
        history: Option<Option<&'a HistorySnapshot>>,
        diagnostics: &str,
    ) -> LoopContext<'a> {
        let mut continued = ctx.reborrow_with_capabilities(capabilities);
        if let Some(history) = history {
            continued.prompt_builder.set_history(history);
        }
        continued.transcript().set_pending_feedback(diagnostics);
        continued
    }

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
        .with_unit_state(Arc::clone(&self.transcript), Arc::clone(&self.rounds_spent))
    }
}
