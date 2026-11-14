//! Bounded model turn execution.
//!
//! Provider resolution, protocol selection, streaming relay, and the turn's
//! usage accounting live here. [`start_model_turn`] launches an interactive
//! turn in the background, while [`execute_model_turn`] awaits one as a batch
//! member step.

use std::sync::Arc;

use tauqe_core::control::context::{CancellationToken, EventSink, LoopContext};
use tauqe_core::control::outcome::{LoopResult, SystemError};
use tauqe_core::control::plan::sequence::{
    run_plan_sequence_loop_with, PlanSequenceArtifact, PlanSequenceLoopConfig,
    ReviewMemberPersistence,
};
use tauqe_core::control::plan::step::PlanStepLoopConfig;
use tauqe_core::control::prompt::PromptBuilder;
use tauqe_core::control::turn::{run_turn_loop, TurnArtifact, TurnLoopConfig};
use tauqe_core::prompt::json::JsonPromptProtocol;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::prompt::TurnBrief;
use tauqe_core::providers::{
    create_provider, format_session_id, LlmProvider, StreamEvent, SESSION_PREFIX_REPL,
};
use tauqe_core::review::ReviewStorage;
use tauqe_core::workflow::research::journal::TurnResearchJournal;
use tauqe_protocol::model::{TurnWorkContext, UsageTotals, UsageTotalsSnapshot};
use tauqe_protocol::{
    events, CapabilityExpr, DiscussionTarget, EditFileDoneEvent, EditFileRetryingEvent,
    EditFileStartedEvent, EditHunkEvent, EditOutcome, Event, GitCommitCreatedEvent,
    ModelEscalatedEvent, ModelResultEvent, ModelRole, ModelStartedEvent, ReasoningEffort,
    ResponseError, ReviewItem, ReviewSession, ReviewUpdatedEvent,
};
use tokio::sync::watch;

use crate::handlers::common::withdraw_streamed_attempt;
use crate::handlers::edit_cycle::{EditCycle, EditTermination};
use crate::handlers::offload;
use crate::state::{next_operation_id, AppState, OperationGuard};
use crate::transport::OutChannel;
use crate::usage::UsageGuard;

/// Result of an executed model turn, capturing termination status, errors, and outcomes.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ModelTurnResult {
    pub operation_id: String,
    pub result: Option<tauqe_protocol::ModelResult>,
    pub is_cancelled: bool,
    pub error: Option<String>,
    pub has_terminal_success: bool,
    /// Coder's block report when the turn ended with a semantic `<blocked>`
    /// signal. Preserved so plan and review supervisors can react accordingly.
    pub blocked: Option<String>,
    /// Coder's abort report when the turn ended with a semantic `<abort>`
    /// signal. Preserved so a supervisor can stop its queue without architect
    /// escalation while the developer receives the exact action to perform.
    pub abort: Option<tauqe_core::control::outcome::AbortReport>,
}

impl ModelTurnResult {
    /// Evaluates if the step executed successfully according to atomic completion invariants.
    ///
    /// A step is considered successful strictly when:
    /// 1. The turn was not cancelled and encountered no unhandled errors.
    /// 2. Code edits were verified and applied (`Edit` with `applied: true`).
    /// 3. Or an explicit terminal success tag was emitted (`<success ... />`).
    ///
    /// A turn that reported an unconditional `<abort>` never succeeded: the
    /// requested work needs an action only the developer can perform.
    #[allow(dead_code)]
    pub fn is_success(&self) -> bool {
        if self.is_cancelled || self.error.is_some() || self.abort.is_some() {
            return false;
        }
        match &self.result {
            Some(tauqe_protocol::ModelResult::Edit { outcome, .. })
                if outcome.applied && outcome.error.is_none() =>
            {
                true
            }
            Some(tauqe_protocol::ModelResult::Answer { .. }) => self.has_terminal_success,
            _ => false,
        }
    }
}

/// A programmatic bounded turn that could not be started.
///
/// A supervisor reads the diagnostic through `error.message`; the identity of
/// the operation belongs to the batch that owns it, so the failure carries no
/// identifier of its own.
#[derive(Debug)]
#[allow(dead_code)]
pub struct TurnError {
    /// The failure reported to the client.
    ///
    /// The payload is boxed because a bare [`ResponseError`] already fills the
    /// error channel of every `Result` in this subsystem; carrying a second
    /// copy of it inline would make each failed turn hand a large `Err`
    /// variant to every caller. Callers keep reading `error.message` through
    /// the box.
    pub error: Box<ResponseError>,
}

impl From<ResponseError> for TurnError {
    fn from(error: ResponseError) -> Self {
        Self {
            error: Box::new(error),
        }
    }
}

struct PreparedTurn {
    provider: Arc<dyn LlmProvider>,
    model: tauqe_protocol::ModelRef,
    app_config: tauqe_core::config::AppConfig,
    reasoning_effort: Option<ReasoningEffort>,
    op_id: String,
    /// Capability surface the turn advertises and accepts.
    ///
    /// The tree belongs to the contour that started the turn, so the rendered
    /// rules, the response schema, and the decoder observe one declaration.
    capabilities: CapabilityExpr,
    /// Whether the turn executes one isolated plan step.
    is_isolated_step: bool,
    /// Dialogue branch the turn is anchored on, if any.
    focus: Option<DiscussionTarget>,
    /// Whether the turn runs as a member of a batch supervisor. Recorded so the
    /// turn's terminal log can distinguish batch steps from interactive turns.
    is_batch: bool,
    /// Work the turn executes, declared by the caller that starts it.
    ///
    /// The value travels unchanged into `model/started` and the turn's journal
    /// records, so a client renders the running work from typed fields instead
    /// of reconstructing it from the stored plans and reviews.
    work_context: Option<TurnWorkContext>,
    /// Operation backing this turn. A turn that starts on its own owns the
    /// registration; a batch step shares the batch-level registration so the
    /// slot never lapses between steps.
    operation: Arc<OperationGuard>,
    /// Provider session identifier binding the turn to a warm prompt prefix.
    ///
    /// `None` for a turn that carries no dialogue, so an isolated step never
    /// inherits the cache of the interactive session.
    session_id: Option<String>,
    /// Sink the turn publishes its live phase and toolchain telemetry into.
    ///
    /// Both contours of the bounded turn publish it: an interactive turn and a
    /// batch step alike, so the client's phase pipeline advances during a
    /// step's staging, verification, and healing rounds instead of waiting for
    /// the batch result. The phases themselves stay owned by the FSM, which
    /// remains their single source of truth.
    phase_out: OutChannel,
}

/// Builds a real network provider from configuration, mapping failures to a
/// `NO_API_KEY` response error.
pub(crate) fn build_configured_provider(
    config: &tauqe_core::config::AppConfig,
    kind: tauqe_protocol::ProviderKind,
) -> Result<Arc<dyn LlmProvider>, ResponseError> {
    create_provider(kind, config)
        .map(Arc::from)
        .map_err(|err| ResponseError::new(tauqe_protocol::errors::NO_API_KEY, err.to_string()))
}

/// Grace period granted to a workflow task to unwind cooperatively after a
/// cancellation or stream error before a supervisor force-aborts it.
const CANCEL_UNWIND_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
// Turn-scoped override of [`CANCEL_UNWIND_GRACE`].
//
// The last-resort path of a turn is bounded by two consecutive expiries of the
// grace period, so exercising it would otherwise cost the production grace
// twice. The override is scoped to the task tree that installed it, so a
// concurrently running test keeps observing the production grace.
#[cfg(test)]
tokio::task_local! {
    static UNWIND_GRACE_OVERRIDE: std::time::Duration;
}

/// Resolves the unwind grace currently in force for this turn.
#[cfg(test)]
pub(crate) fn unwind_grace() -> std::time::Duration {
    UNWIND_GRACE_OVERRIDE
        .try_with(|value| *value)
        .unwrap_or(CANCEL_UNWIND_GRACE)
}

/// Resolves the unwind grace currently in force for this turn.
///
/// Production always waits out [`CANCEL_UNWIND_GRACE`]; only a test may scope a
/// shorter value to the task tree that drives the turn.
#[cfg(not(test))]
pub(crate) fn unwind_grace() -> std::time::Duration {
    CANCEL_UNWIND_GRACE
}

/// Runs `work` under a turn-scoped override of the unwind grace.
///
/// The seam mirrors `AppState::marker_override`: production always waits out
/// [`CANCEL_UNWIND_GRACE`], while a deterministic test drives the last-resort
/// path without waiting it out.
#[cfg(test)]
pub(crate) async fn with_unwind_grace<F>(grace: std::time::Duration, work: F) -> F::Output
where
    F: std::future::Future,
{
    UNWIND_GRACE_OVERRIDE.scope(grace, work).await
}

/// Builds the apply policy for a turn, wiring the shared git-transaction guard and
/// the turn's cancellation signal into the Git-backed policy.
///
/// The guard keeps a supervisor from force-aborting a workflow task while a
/// transaction is open, and the signal lets the checkpoint capture and the step
/// commit release a repository hook that holds the commit. Finalization and
/// rollback stay cancel-blind, so the transaction settles under its wall-clock
/// budget and the supervisor's deferred abort has a deterministic end.
pub(crate) fn build_apply_policy(
    state: &Arc<AppState>,
    workflow: tauqe_protocol::WorkflowKind,
    cancel: watch::Receiver<bool>,
) -> Box<dyn tauqe_core::workflow::policy::ApplyPolicy> {
    match workflow {
        tauqe_protocol::WorkflowKind::Naive => {
            Box::new(tauqe_core::workflow::policy::DirectApplyPolicy::new())
        }
        tauqe_protocol::WorkflowKind::Git => Box::new(
            tauqe_core::workflow::git_policy::GitTransactionPolicy::with_guard_and_cancel(
                Arc::clone(&state.git_transaction_active),
                cancel,
            ),
        ),
    }
}

/// Resolves the provider and model for a turn, emits `model/started`, and
/// registers the cancellable active operation. `is_batch` marks turns that run
/// as members of a batch supervisor so clients can suppress per-step signals.
/// The declared `work_context` travels unchanged into `model/started` and the
/// turn's journal record, so the running work is reported from typed fields.
///
/// The collaborators are intentionally threaded as separate parameters: every
/// turn entry point supplies exactly the subset it owns, so a wrapper struct
/// would only restate the same fields without removing a single argument.
#[allow(clippy::too_many_arguments)]
async fn prepare_turn(
    state: &Arc<AppState>,
    capabilities: CapabilityExpr,
    is_isolated_step: bool,
    focus: Option<DiscussionTarget>,
    custom_selection: Option<tauqe_protocol::ModelSelection>,
    plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    work_context: Option<TurnWorkContext>,
    reasoning_effort: Option<ReasoningEffort>,
    is_batch: bool,
    session_id: Option<String>,
    shared_operation: Option<Arc<OperationGuard>>,
) -> Result<PreparedTurn, ResponseError> {
    // Acquire (or borrow) the active operation first: registration is atomic,
    // so a concurrent turn fails with BUSY instead of silently clobbering the
    // running operation's cancel handle.
    let operation = match shared_operation {
        Some(operation) => operation,
        None => Arc::new(state.try_begin_operation().map_err(|()| {
            ResponseError::new(
                tauqe_protocol::errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            )
        })?),
    };

    let (provider, model, app_config, selection) = {
        let config_guard = state.config.lock().await;
        let selection = match custom_selection {
            Some(s) => s,
            None => config_guard.selection.clone(),
        };
        let model = config_guard.config.resolve_model(&selection);
        let config = &config_guard.config;
        // Deterministic unit tests inject a scripted provider through the
        // state's test seam; production builds always construct a real
        // network provider from configuration.
        #[cfg(test)]
        let provider: Arc<dyn LlmProvider> = match state.provider_override.lock().await.clone() {
            Some(provider) => provider,
            None => build_configured_provider(config, model.provider)?,
        };

        #[cfg(not(test))]
        let provider: Arc<dyn LlmProvider> = build_configured_provider(config, model.provider)?;

        (provider, model, config.clone(), selection)
    };

    let op_id = format!("op-{}", next_operation_id());
    // An explicit session wins: a plan execution or a review audit names the
    // long-lived context whose warm prefix this turn extends. Without one a
    // dialogue turn binds the interactive REPL session and an isolated step
    // stays stateless.
    let session_id = match session_id {
        Some(session) => Some(session),
        None => resolve_session_id(is_isolated_step, state.session_uuid().await),
    };

    let role = match &selection {
        tauqe_protocol::ModelSelection::Role(r) => Some(*r),
        tauqe_protocol::ModelSelection::Specific(_) => None,
        tauqe_protocol::ModelSelection::Tier { .. } => None,
    };
    // Report the running model as structured data: the effective reasoning
    // effort, the role coordinates, and the catalog display alias, so the client
    // renders the active model without re-deriving any of them from
    // configuration. The reasoning depth is a runtime parameter of a role, so it
    // is resolved by the configuration resolver against the role that owns the
    // selection instead of by scanning the configured role lists; an explicit
    // model carries no role of its own and resolves against the mandatory base
    // role.
    let effort = reasoning_effort.or_else(|| {
        app_config.resolve_selection_effort(role.unwrap_or(ModelRole::REPL), &selection)
    });
    // A display alias is catalog metadata of the model itself, so it is resolved
    // from the centralized provider catalog of the configuration rather than
    // from any role slot.
    let alias = app_config.model_alias(&model).map(str::to_string);

    tracing::info!(
        operation_id = %op_id,
        model = %model,
        role = ?role,
        effort = ?effort,
        alias = alias.as_deref().unwrap_or("none"),
        isolated_step = is_isolated_step,
        work = work_kind(work_context.as_ref()),
        is_batch,
        session = session_id.as_deref().unwrap_or("none"),
        "Model turn started",
    );

    state.out.send_event(&Event {
        method: events::MODEL_STARTED.to_string(),
        params: serde_json::to_value(ModelStartedEvent {
            operation_id: op_id.clone(),
            model: model.clone(),
            role,
            alias,
            effort,
            plan_context,
            work_context: work_context.clone(),
            is_batch,
        })
        .ok(),
    });

    Ok(PreparedTurn {
        provider,
        model,
        app_config,
        reasoning_effort: effort,
        op_id,
        capabilities,
        is_isolated_step,
        focus,
        is_batch,
        work_context,
        operation,
        session_id,
        // The daemon channel is the default telemetry sink; a step endpoint
        // replaces it with the sink its supervisor supplies.
        phase_out: state.out.clone(),
    })
}

/// Resolves the prompt protocol that carries a declared capability surface.
///
/// The tree belongs to the contour that starts the turn: a dialogue composes
/// the surface its anchor authorizes, and an isolated step composes the surface
/// of one implementation turn. The transport never narrows or widens that
/// surface, so it only decides how the commands travel.
///
/// The grant runs through the idempotent `with_capability_expr`, so the rendered
/// instructions, the response schema, and the decoder observe one declaration:
/// a plan anchor adds plan mutation, a review anchor adds the recorded decision,
/// and the harness keeps validating either mutation itself after decoding.
///
/// Under Structured Output the operating mode follows the surface: a surface
/// that permits file edits composes the registry edit schema, where
/// `plan_update` is already declared, so a plan discussion shares the composite
/// schema instead of a separate read-only discussion schema.
pub(super) fn resolve_turn_protocol(capabilities: CapabilityExpr) -> Box<dyn PromptProtocol> {
    let base_protocol: Box<dyn PromptProtocol> = Box::new(JsonPromptProtocol::edits());
    base_protocol.with_capability_expr(capabilities)
}

/// Resolves the provider session identifier that binds a turn to a warm prompt
/// prefix.
///
/// Only a dialogue turn carries a session: interactive REPL turns and focused
/// discussions share one identifier, so successive rounds reuse the backend's
/// cached prefix. A turn without a dialogue receives `None`, so an isolated step
/// never inherits — or poisons — the interactive cache.
pub(super) fn resolve_session_id(
    is_isolated_step: bool,
    dialogue_session: String,
) -> Option<String> {
    if is_isolated_step {
        None
    } else {
        Some(format_session_id(SESSION_PREFIX_REPL, &dialogue_session))
    }
}

async fn run_prepared_turn(
    state: &Arc<AppState>,
    prompt: String,
    prepared: PreparedTurn,
    policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
) -> Result<ModelTurnResult, ResponseError> {
    run_prepared_turn_control_loops(state, prompt, prepared, policy).await
}

async fn run_prepared_turn_control_loops(
    state: &Arc<AppState>,
    prompt: String,
    prepared: PreparedTurn,
    policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
) -> Result<ModelTurnResult, ResponseError> {
    use tauqe_core::control::context::{CancellationToken, EventSink, LoopContext};
    use tauqe_core::control::outcome::LoopResult;
    use tauqe_core::control::prompt::PromptBuilder;
    use tauqe_core::control::repl::{run_repl_loop, ReplArtifact, ReplLoopConfig};
    use tauqe_core::workflow::research::journal::TurnResearchJournal;
    use tauqe_protocol::{HistoryFileChange, ModelResult};

    let PreparedTurn {
        provider,
        model,
        app_config,
        op_id,
        capabilities,
        is_isolated_step,
        focus,
        is_batch,
        work_context,
        operation,
        session_id,
        phase_out: _,
        reasoning_effort,
    } = prepared;

    let mut cancel_rx = operation.cancel_receiver();
    let protocol = resolve_turn_protocol(capabilities);
    let (event_sink, mut event_rx) = EventSink::channel();

    let state_clone = Arc::clone(state);
    let model_clone = model.clone();
    let prompt_clone = prompt.clone();
    let cancel_token = CancellationToken::from_receiver(cancel_rx.clone());
    let repo_root = state.repo_root().await;
    // The dialogue branch travels with the turn: the caller that started it
    // resolved the anchor from the typed discussion target, so the loop opens
    // exactly the branch the declared surface authorizes.
    let focus_target = focus.clone();

    // The starting context is captured before the history lock is taken: the
    // highest insertion watermark it carries is what a chronological transcript
    // uses to place a file discovered later, so the turn records it together
    // with the prompt instead of leaving the arrival signal permanently absent.
    let (context_files, context_watermark) = {
        let context = state.context.read().await;
        let watermark = context
            .get_state()
            .items
            .iter()
            .map(|item| item.insertion_seq)
            .max()
            .filter(|watermark| *watermark > 0);
        let files = context
            .read_context_files()
            .into_iter()
            .map(|file| file.path)
            .collect();
        (files, watermark)
    };
    let (turn_id, history_snapshot) = {
        let mut history = state.history.lock().await;
        let snapshot = history.snapshot().ok();
        let turn_id = history
            .record_turn_with_watermark(&prompt, context_files, context_watermark)
            .ok()
            .and_then(|entry| entry.id());
        (turn_id, snapshot)
    };

    let loop_config = {
        let mut cfg = ReplLoopConfig::from_app_config(&app_config);
        cfg.turn.edit.staging.reasoning_effort = reasoning_effort;
        cfg
    };
    let history_snapshot_clone = history_snapshot.clone();
    let app_config_clone = app_config.clone();
    let loop_task = tokio::spawn(async move {
        let _session_guard = state_clone.session_lock.lock().await;
        let mut context_manager = state_clone.context.read().await.clone();
        let mut journal = TurnResearchJournal::default();
        let mut input_cache = tauqe_core::control::cache::TurnInputCache::default();
        let mut policy = policy;

        let capabilities = tauqe_core::control::repl_capabilities(focus_target.as_ref());
        let prompt_builder = PromptBuilder::new(
            provider.as_ref(),
            &model_clone,
            protocol.as_ref(),
            &mut input_cache,
        )
        .with_session_id(session_id)
        .with_prompt(Some(prompt_clone))
        .with_history(history_snapshot_clone.as_ref())
        .with_repo_map_config(
            app_config_clone.context.include_repo_map,
            app_config_clone.context.repomap_token_budget,
        )
        .with_patch_strategy(app_config_clone.repl.patch_strategy)
        .with_prompt_layout(Some(
            app_config_clone.resolve_prompt_layout(!is_isolated_step),
        ))
        .with_workflow(app_config_clone.repl.workflow.to_string())
        .with_outer_docs(app_config_clone.outer_docs.clone())
        .with_reasoning_effort(reasoning_effort);

        let mut ctx = LoopContext::new(
            &cancel_token,
            &event_sink,
            &repo_root,
            &mut context_manager,
            &mut journal,
            policy.as_mut(),
            capabilities,
            prompt_builder,
        );
        let result = run_repl_loop(&mut ctx, &loop_config, focus_target.as_ref()).await;

        (result, context_manager)
    });

    let wf_task = loop_task;
    operation.set_abort_handle(wf_task.abort_handle());

    let mut raw_streamed_text = String::new();
    let mut usage_guard = Some(state.begin_usage(&op_id).await);
    let edit_cycle = EditCycle::new(state.out.clone(), op_id.clone());
    let mut is_cancelled = false;
    let mut is_error = false;
    let mut stream_error_msg = None;

    while let Some(event) = event_rx.recv().await {
        state.out.pace_event(&event, &mut cancel_rx).await;
        match event {
            StreamEvent::ReasoningDelta(delta) => {
                state.out.send_event(&Event {
                    method: events::MODEL_REASONING_DELTA.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id.clone(),
                        "delta": delta,
                    })),
                });
            }
            StreamEvent::TextDelta(delta) => {
                raw_streamed_text.push_str(&delta);
                state.out.send_event(&Event {
                    method: events::MODEL_TEXT_DELTA.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id.clone(),
                        "delta": delta,
                    })),
                });
            }
            StreamEvent::ContextChanged(ctx_state) => {
                state.out.send_event(&Event {
                    method: events::CONTEXT_CHANGED.to_string(),
                    params: Some(serde_json::json!({ "state": ctx_state })),
                });
            }
            StreamEvent::EditStarted => {
                edit_cycle.open();
            }
            StreamEvent::EditFileStarted { path, op_type } => {
                edit_cycle.open();
                state.out.send_event(&Event {
                    method: events::EDIT_FILE_STARTED.to_string(),
                    params: serde_json::to_value(EditFileStartedEvent {
                        operation_id: op_id.clone(),
                        path,
                        op_type,
                    })
                    .ok(),
                });
            }
            StreamEvent::EditHunk {
                path,
                hunk_index,
                old_start,
                old_lines,
                new_start,
                new_lines,
                context_hint,
                old_text,
                new_text,
            } => {
                state.out.send_event(&Event {
                    method: events::EDIT_HUNK.to_string(),
                    params: serde_json::to_value(EditHunkEvent {
                        operation_id: op_id.clone(),
                        path,
                        hunk_index,
                        old_start,
                        old_lines,
                        new_start,
                        new_lines,
                        context_hint,
                        old_text,
                        new_text,
                    })
                    .ok(),
                });
            }
            StreamEvent::EditFileDone {
                path,
                status,
                error,
                hunks_count,
            } => {
                state.out.send_event(&Event {
                    method: events::EDIT_FILE_DONE.to_string(),
                    params: serde_json::to_value(EditFileDoneEvent {
                        operation_id: op_id.clone(),
                        path,
                        status,
                        error,
                        hunks_count,
                    })
                    .ok(),
                });
            }
            StreamEvent::EditFileRetrying {
                path,
                attempt,
                max_retries,
                reason,
            } => {
                state.out.send_event(&Event {
                    method: events::EDIT_FILE_RETRYING.to_string(),
                    params: serde_json::to_value(EditFileRetryingEvent {
                        operation_id: op_id.clone(),
                        path,
                        attempt,
                        max_retries,
                        reason,
                    })
                    .ok(),
                });
            }
            StreamEvent::TurnPhase {
                phase,
                round,
                max_rounds,
                detail,
            } => {
                state.out.send_event(&Event {
                    method: events::TURN_PHASE.to_string(),
                    params: serde_json::to_value(tauqe_protocol::TurnPhaseEvent {
                        operation_id: op_id.clone(),
                        phase,
                        round,
                        max_rounds,
                        detail,
                    })
                    .ok(),
                });
            }
            StreamEvent::ToolchainStarted { command } => {
                state.out.send_event(&Event {
                    method: events::TOOLCHAIN_STARTED.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ToolchainStartedEvent {
                        operation_id: op_id.clone(),
                        command,
                    })
                    .ok(),
                });
            }
            StreamEvent::ToolchainFinished {
                command,
                success,
                message,
            } => {
                state.out.send_event(&Event {
                    method: events::TOOLCHAIN_FINISHED.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ToolchainFinishedEvent {
                        operation_id: op_id.clone(),
                        command,
                        success,
                        message,
                    })
                    .ok(),
                });
            }
            StreamEvent::Usage(usage) => {
                if let Some(guard) = usage_guard.as_ref() {
                    guard.observe(&usage).await;
                }
                emit_legacy_usage_event(state, &op_id).await;
            }
            StreamEvent::RepoMapTokens(tokens) => {
                state.out.send_event(&Event {
                    method: events::TURN_REPOMAP.to_string(),
                    params: serde_json::to_value(tauqe_protocol::RepoMapInfoEvent {
                        operation_id: op_id.clone(),
                        tokens,
                    })
                    .ok(),
                });
            }
            StreamEvent::RetryingConnection {
                attempt,
                max_attempts,
                delay_secs,
                reason,
            } => {
                state.out.send_event(&Event {
                    method: events::MODEL_CONNECTION_RETRYING.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ModelConnectionRetryingEvent {
                        operation_id: op_id.clone(),
                        attempt,
                        max_attempts,
                        delay_secs,
                        reason,
                    })
                    .ok(),
                });
            }
            StreamEvent::ResetStream => {
                edit_cycle.close_unapplied(EditTermination::Withdrawn, Vec::new(), None);
                withdraw_streamed_attempt(&state.out, &op_id, || raw_streamed_text.clear());
            }
            StreamEvent::Cancelled => {
                edit_cycle.close_unapplied(EditTermination::Cancelled, Vec::new(), None);
                state.out.send_event(&Event {
                    method: events::MODEL_CANCELLED.to_string(),
                    params: Some(serde_json::json!({ "operation_id": op_id.clone() })),
                });
                is_cancelled = true;
                break;
            }
            StreamEvent::Error(err) => {
                close_failed(
                    &edit_cycle,
                    EditTermination::ProviderError,
                    format!("Operation failed before edits were applied: {}", err),
                );
                state.out.send_event(&Event {
                    method: events::MODEL_ERROR.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id.clone(),
                        "message": err.clone(),
                    })),
                });
                stream_error_msg = Some(err);
                is_error = true;
                break;
            }
            _ => {}
        }
    }

    let joined = wf_task.await;
    let (loop_res, local_context) = match joined {
        Ok(res) => (res.0, res.1),
        Err(join_err) => {
            let cancelled = is_cancelled || *cancel_rx.borrow();
            let text = if cancelled {
                "Operation cancelled".to_string()
            } else {
                format!("Workflow task failed: {}", join_err)
            };
            {
                let mut history = state.history.lock().await;
                if cancelled {
                    if let Some(id) = turn_id {
                        let _ = history.rollback_entry(id, "Operation cancelled");
                    }
                } else {
                    let _ = history.record_response(&text, None, None, Vec::new());
                }
            }
            let snapshot = flush_usage(state, &mut usage_guard).await;
            emit_legacy_usage_event(state, &op_id).await;
            let error = if cancelled {
                Some("Operation cancelled".to_string())
            } else {
                Some(join_err.to_string())
            };
            let status = if cancelled { "cancelled" } else { "error" };
            log_turn_finished(
                &op_id,
                &model,
                is_batch,
                status,
                &snapshot.prev,
                error.as_deref(),
                work_context.as_ref(),
            );
            if cancelled && !is_cancelled {
                edit_cycle.close_unapplied(EditTermination::Cancelled, Vec::new(), None);
                state.out.send_event(&Event {
                    method: events::MODEL_CANCELLED.to_string(),
                    params: Some(serde_json::json!({ "operation_id": op_id.clone() })),
                });
            }
            return Ok(ModelTurnResult {
                operation_id: op_id,
                result: None,
                is_cancelled: cancelled,
                error,
                has_terminal_success: false,
                blocked: None,
                abort: None,
            });
        }
    };

    if is_cancelled || *cancel_rx.borrow() {
        if !is_cancelled {
            edit_cycle.close_unapplied(EditTermination::Cancelled, Vec::new(), None);
            state.out.send_event(&Event {
                method: events::MODEL_CANCELLED.to_string(),
                params: Some(serde_json::json!({ "operation_id": op_id.clone() })),
            });
        }
        if let Some(id) = turn_id {
            let mut history = state.history.lock().await;
            let _ = history.rollback_entry(id, "Operation cancelled");
        }
        let snapshot = flush_usage(state, &mut usage_guard).await;
        emit_legacy_usage_event(state, &op_id).await;
        let error = Some("Operation cancelled".to_string());
        log_turn_finished(
            &op_id,
            &model,
            is_batch,
            "cancelled",
            &snapshot.prev,
            error.as_deref(),
            work_context.as_ref(),
        );
        return Ok(ModelTurnResult {
            operation_id: op_id,
            result: None,
            is_cancelled: true,
            error,
            has_terminal_success: false,
            blocked: None,
            abort: None,
        });
    }

    if is_error {
        let text = if !raw_streamed_text.trim().is_empty() {
            raw_streamed_text.clone()
        } else {
            stream_error_msg
                .clone()
                .unwrap_or_else(|| "Operation error".to_string())
        };
        {
            let mut history = state.history.lock().await;
            let _ = history.record_response(&text, None, None, Vec::new());
        }
        let snapshot = flush_usage(state, &mut usage_guard).await;
        emit_legacy_usage_event(state, &op_id).await;
        log_turn_finished(
            &op_id,
            &model,
            is_batch,
            "error",
            &snapshot.prev,
            stream_error_msg.as_deref(),
            work_context.as_ref(),
        );
        return Ok(ModelTurnResult {
            operation_id: op_id,
            result: None,
            is_cancelled: false,
            error: stream_error_msg,
            has_terminal_success: false,
            blocked: None,
            abort: None,
        });
    }

    let snapshot = flush_usage(state, &mut usage_guard).await;
    emit_legacy_usage_event(state, &op_id).await;
    let operation_totals = snapshot.prev.clone();
    let (artifact, error_msg) = match loop_res {
        Ok(LoopResult::Success(success)) => (Some(success.value), None),
        Ok(LoopResult::Diverged(_)) => (None, Some("Execution diverged".to_string())),
        Ok(LoopResult::Exhausted(report)) => (None, Some(report.reason)),
        Err(err) => (None, Some(err.to_string())),
    };

    let (plans_applied, plan_rejections, plan_warnings, review_decisions_applied) = match &artifact
    {
        Some(a) => (
            a.plans_applied,
            a.plan_rejections.clone(),
            a.plan_warnings.clone(),
            a.review_decisions_applied.clone(),
        ),
        None => (false, Vec::new(), Vec::new(), Vec::new()),
    };
    if let Err(err) = state
        .mutate_context(move |ctx| ctx.merge_turn_context(&local_context))
        .await
    {
        tracing::warn!(error = %err, "Failed to merge the turn context");
    }

    let model_result = match artifact {
        Some(ReplArtifact {
            message,
            modified_paths,
            plans_applied: _,
            plan_rejections: _,
            plan_warnings: _,
            review_decisions_applied: _,
            envelope: _,
            commit_hash,
        }) => {
            let text = message.unwrap_or_else(|| {
                if !raw_streamed_text.trim().is_empty() {
                    raw_streamed_text
                } else {
                    "Research completed.".to_string()
                }
            });
            if !modified_paths.is_empty() {
                if let Some(hash) = &commit_hash {
                    state.out.send_event(&Event {
                        method: events::GIT_COMMIT_CREATED.to_string(),
                        params: serde_json::to_value(GitCommitCreatedEvent {
                            commit_hash: hash.clone(),
                            summary: "REPL changes".to_string(),
                            changed_files: modified_paths.clone(),
                        })
                        .ok(),
                    });

                    let repo_root = state.repo_root().await;
                    match offload(move || {
                        let root = repo_root.as_path();
                        tauqe_core::git::get_repository_state(Some(root))
                    })
                    .await
                    {
                        Ok(repo_state) => {
                            state.out.send_event(&Event {
                                method: events::GIT_STATE_CHANGED.to_string(),
                                params: Some(serde_json::json!({ "repository": repo_state })),
                            });
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "Failed to read repository state after commit");
                        }
                    }
                }

                let outcome = EditOutcome {
                    applied: true,
                    error: None,
                    changed_files: modified_paths.clone(),
                    commit_hash: commit_hash.clone(),
                    verification: Some(tauqe_protocol::VerificationVerdict::Passed),
                };

                edit_cycle.open();
                edit_cycle.close_outcome(EditTermination::Answered, outcome.clone());

                {
                    let mut history = state.history.lock().await;
                    let files = modified_paths
                        .iter()
                        .map(|p| HistoryFileChange::plain(p.clone()))
                        .collect();
                    let _ = history.record_response(
                        &text,
                        commit_hash,
                        Some("REPL changes".to_string()),
                        files,
                    );
                }

                // The transient context of the turn is released on the commit
                // path: the material that served this decision never inflates the
                // next one. The commit is the boundary the release belongs to, so
                // the dialogue session identifier rotates with the prompt prefix
                // the finished task rebuilt, and the next round is never routed to
                // a provider cache warmed by material the dialogue no longer sends.
                let _ = state.clear_transient_context().await;
                state.reset_session_uuid().await;

                ModelResult::Edit {
                    summary: "REPL changes".to_string(),
                    edits: Vec::new(),
                    proposal: None,
                    outcome,
                }
            } else {
                edit_cycle.close_unapplied(EditTermination::Answered, Vec::new(), None);
                {
                    let mut history = state.history.lock().await;
                    let _ = history.record_response(&text, None, None, Vec::new());
                }
                ModelResult::Answer { text }
            }
        }
        None => {
            let text = if !raw_streamed_text.trim().is_empty() {
                raw_streamed_text
            } else {
                error_msg
                    .clone()
                    .unwrap_or_else(|| "Operation failed".to_string())
            };
            edit_cycle.close_unapplied(EditTermination::Answered, Vec::new(), None);
            {
                let mut history = state.history.lock().await;
                let _ = history.record_response(&text, None, None, Vec::new());
            }
            ModelResult::Answer { text }
        }
    };

    let ctx_state = state.context.read().await.get_state();
    state.out.send_event(&Event {
        method: events::CONTEXT_CHANGED.to_string(),
        params: Some(serde_json::json!({ "state": ctx_state })),
    });

    if plans_applied {
        let repo_root = state.repo_root().await;
        let (active_id, plans) = offload(move || {
            let active = tauqe_core::plan::storage::PlanStorage::load_active_id(&repo_root)
                .ok()
                .flatten();
            let plans = tauqe_core::plan::storage::PlanStorage::load_all(&repo_root).ok();
            (active, plans)
        })
        .await
        .unwrap_or((None, None));

        if let Some(plans) = plans {
            if let Some(ref aid) = active_id {
                if let Some(active_plan) = plans.iter().find(|p| &p.id == aid) {
                    state.out.send_event(&Event {
                        method: events::PLAN_UPDATED.to_string(),
                        params: serde_json::to_value(tauqe_protocol::PlanUpdatedEvent {
                            plan: active_plan.clone(),
                            is_save: true,
                        })
                        .ok(),
                    });
                }
            }
            state.out.send_event(&Event {
                method: events::PLAN_LIST_CHANGED.to_string(),
                params: serde_json::to_value(tauqe_protocol::PlanListChangedEvent {
                    plans,
                    active_id,
                })
                .ok(),
            });
        }
    }

    if !review_decisions_applied.is_empty() {
        let review_id_opt = match focus.as_ref() {
            Some(DiscussionTarget::Review { id }) => Some(id.clone()),
            Some(DiscussionTarget::ReviewGroup { session_id, .. }) => Some(session_id.clone()),
            _ => None,
        };
        if let Some(review_id) = review_id_opt {
            if let Some(session) = load_review_session(state, &review_id).await {
                for _ in &review_decisions_applied {
                    state.out.send_event(&Event {
                        method: events::REVIEW_UPDATED.to_string(),
                        params: serde_json::to_value(ReviewUpdatedEvent {
                            session: session.clone(),
                        })
                        .ok(),
                    });
                }
            }
        }
    }

    let full_text = match &model_result {
        ModelResult::Answer { text } => text.clone(),
        ModelResult::Edit { summary, .. } => summary.clone(),
    };

    state.out.send_event(&Event {
        method: events::MODEL_RESULT.to_string(),
        params: Some(serde_json::json!(ModelResultEvent {
            operation_id: op_id.clone(),
            result: model_result.clone(),
            usage: Some(operation_totals.to_usage_info()),
            session_total_cost: Some(snapshot.session.cost),
            current_cost: Some(operation_totals.cost),
            plan_rejections,
            plan_warnings,
        })),
    });
    state.out.send_event(&Event {
        method: events::MODEL_FINISHED.to_string(),
        params: Some(serde_json::json!({
            "operation_id": op_id.clone(),
            "full_text": full_text,
        })),
    });

    let turn_result = ModelTurnResult {
        operation_id: op_id,
        result: Some(model_result),
        is_cancelled: false,
        error: error_msg,
        has_terminal_success: true,
        blocked: None,
        abort: None,
    };

    log_turn_finished(
        &turn_result.operation_id,
        &model,
        is_batch,
        turn_status(&turn_result),
        &operation_totals,
        turn_result.error.as_deref(),
        work_context.as_ref(),
    );

    Ok(turn_result)
}
/// Rebuilds the shared history manager from the authoritative journal.
///
/// A turn that panicked or was force-aborted never reached its adoption step,
/// so its private history clone — and the identifier watermark it consumed —
/// was dropped with it while the entries it already appended stay in the
/// journal. Rebuilding the manager from `<repo_root>/.tauqe/history.jsonl`
/// restores the watermark the daemon itself derives at startup, so the next
/// record can never reuse an identifier that is already written.
#[allow(dead_code)]
async fn resync_history(state: &Arc<AppState>) {
    if let Err(err) = state.resync_history_from_journal().await {
        tracing::warn!(
            error = %err,
            "Failed to rebuild the shared history manager from the journal"
        );
    }
}

/// Reads the review session a dialogue is anchored on.
///
/// The read is advisory in both directions it is used: it captures what the
/// session looked like before the turn and it fetches the refreshed session the
/// terminal publication carries. A storage failure therefore degrades to an
/// absent snapshot with a record, never to a failed turn whose decisions were
/// already persisted.
async fn load_review_session(state: &Arc<AppState>, review_id: &str) -> Option<ReviewSession> {
    let root = state.repo_root().await;
    let id = review_id.to_string();
    match offload(move || ReviewStorage::load_review(&root, &id)).await {
        Ok(Ok(session)) => session,
        Ok(Err(err)) => {
            tracing::warn!(
                review_id = %review_id,
                error = %err,
                "review session could not be read",
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                review_id = %review_id,
                error = %err,
                "review session read failed",
            );
            None
        }
    }
}

/// Reports whether a decision moved the stored state of one finding.
///
/// A decision rewrites the lifecycle status, the resolution note, and the agreed
/// remediation of the finding, so exactly those fields decide whether the turn
/// affected it. Presentation-only data and the finding's prose are never part of
/// a decision and never make a finding affected.
#[allow(dead_code)]
fn finding_moved(before: &ReviewItem, after: &ReviewItem) -> bool {
    before.status != after.status
        || before.resolution_notes != after.resolution_notes
        || before.recommendations != after.recommendations
}

/// Publishes the review findings a dialogue turn moved.
///
/// A decision the engineer reaches in a discussion is durable state, so the
/// client learns about it as soon as the turn completes instead of waiting for
/// the next explicit review fetch. The publication is scoped to the findings the
/// turn actually changed: the pre-turn snapshot is the reference, so a turn that
/// recorded no decision publishes nothing and the client never rewrites state the
/// developer is already looking at.
///
/// Every affected finding is published on its own `review/updated` event. The
/// event carries the refreshed session, which is the shape the wire contract
/// declares, so a client that tracks a finding by identity observes each change
/// the turn made.
#[allow(dead_code)]
async fn publish_review_decisions(
    state: &Arc<AppState>,
    review_id: &str,
    baseline: Option<&ReviewSession>,
) {
    let Some(baseline) = baseline else {
        // Without the pre-turn state the publication cannot tell which findings
        // moved, and republishing the whole session would rewrite state the
        // client already holds.
        tracing::warn!(
            review_id = %review_id,
            "review decisions were not published: the pre-turn session state is unavailable",
        );
        return;
    };
    let Some(session) = load_review_session(state, review_id).await else {
        return;
    };
    let affected = session
        .items
        .iter()
        .filter(|item| match baseline.find_item(item.id) {
            Some(before) => finding_moved(before, item),
            // A finding the pre-turn session did not hold cannot have been moved
            // by a decision: only an existing finding can be decided.
            None => false,
        })
        .map(|item| item.id)
        .collect::<Vec<u32>>();
    if affected.is_empty() {
        return;
    }
    tracing::info!(
        review_id = %review_id,
        findings = affected.len(),
        item_ids = ?affected,
        "review decisions recorded in the dialogue were published",
    );
    // The event payload is the refreshed session, so one publication per affected
    // finding lets a client that tracks a finding identity observe every change
    // of the turn; a repeated snapshot is idempotent.
    for _ in &affected {
        state.out.send_event(&Event {
            method: events::REVIEW_UPDATED.to_string(),
            params: serde_json::to_value(ReviewUpdatedEvent {
                session: session.clone(),
            })
            .ok(),
        });
    }
}

/// Initiates an asynchronous model turn in the background and immediately returns its operation ID.
///
/// Designed as a lightweight non-blocking wrapper for interactive single-prompt RPC requests.
pub async fn start_model_turn(
    state: &Arc<AppState>,
    prompt: String,
    focus: Option<DiscussionTarget>,
    custom_selection: Option<tauqe_protocol::ModelSelection>,
    plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
) -> Result<String, ResponseError> {
    let capabilities = tauqe_core::control::repl_capabilities(focus.as_ref());
    let prepared = prepare_turn(
        state,
        capabilities,
        false,
        focus,
        custom_selection,
        plan_context,
        // An interactive turn answers the developer rather than executing
        // stored work, so it declares itself instead of leaving the running
        // work anonymous.
        Some(TurnWorkContext::Interactive),
        None,
        false,
        None,
        None,
    )
    .await?;
    let op_id = prepared.op_id.clone();
    // The policy observes the very signal that interrupts the turn, so a
    // cancellation reaches the checkpoint capture instead of waiting for a
    // repository hook to finish.
    let cancel = prepared.operation.cancel_receiver();
    let state_clone = Arc::clone(state);
    let workflow = prepared.app_config.repl.workflow;
    tokio::spawn(async move {
        let policy = build_apply_policy(&state_clone, workflow, cancel);
        let _ = run_prepared_turn(&state_clone, prompt, prepared, policy).await;
    });
    Ok(op_id)
}

/// Optional controls for a programmatic batch step turn.
///
/// Bundles the per-step targeting, escalation, and cancellation wiring so the
/// turn entry point keeps a compact signature. An explicit `reasoning_effort`
/// overrides the effort derived from the selection, letting callers target a
/// specific coder-ladder rung without changing the semantic role. An explicit
/// `marker` binds the turn's structural protocol tags to a per-turn suffix so
/// literal source tags are never mistaken for protocol commands. A
/// `shared_operation` borrows the batch-level registration instead of opening a
/// new one, so the batch slot never lapses between steps. The declared `intent`
/// names the turn mode explicitly, so an isolated step is never inferred from
/// the prompt text. An explicit `session_id` names the long-lived context — a
/// plan execution or a review audit — whose warm provider prefix the turn
/// extends; without one an isolated step stays stateless. The declared
/// `work_context` names the work the step executes, so the client renders the
/// running work from typed fields instead of reconstructing it from the stored
/// plans and reviews.
// The legacy turn contour keeps this request alive until it is removed together
// with the bounded-turn FSM; the step chain of the daemon already runs on the
// control loops.
#[allow(dead_code)]
pub struct StepTurnOptions {
    /// Capability surface the turn advertises and accepts.
    pub capabilities: CapabilityExpr,
    /// Whether the turn executes one isolated plan step.
    pub is_isolated_step: bool,
    pub custom_selection: Option<tauqe_protocol::ModelSelection>,
    pub plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    /// Work the step executes, declared by the caller that starts it.
    ///
    /// The value reaches `model/started` unchanged, so the client renders the
    /// running work: its kind, the entity that owns it, and the active member
    /// with its position. Nothing is reconstructed from the stored plans and
    /// reviews.
    pub work_context: Option<TurnWorkContext>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub marker: Option<String>,
    pub session_id: Option<String>,
    pub shared_operation: Option<Arc<OperationGuard>>,
    /// Sink the turn publishes its live phase and toolchain telemetry into.
    ///
    /// `None` publishes through the daemon's client channel. A step endpoint
    /// supplies its own sink when it owns the telemetry surface, so a step's
    /// staging, verification, and healing rounds advance the client's phase
    /// pipeline exactly as an interactive turn does.
    pub phase_out: Option<OutChannel>,
}

/// Executes an isolated model turn to completion and awaits its outcome.
///
/// Returns the full `ModelTurnResult` representing the atomic turn lifecycle,
/// enabling server-side step orchestration to inspect step success
/// deterministically. Turns started here always execute inside a batch
/// supervisor, so they are marked as batch members to let clients suppress
#[allow(dead_code)]
pub async fn execute_model_turn(
    state: &Arc<AppState>,
    prompt: String,
    policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
    options: StepTurnOptions,
) -> Result<ModelTurnResult, TurnError> {
    let StepTurnOptions {
        capabilities,
        is_isolated_step,
        custom_selection,
        plan_context,
        work_context,
        reasoning_effort,
        marker: _marker,
        session_id,
        shared_operation,
        phase_out,
    } = options;
    let focus = state.discussion_focus().await.target().cloned();
    let mut prepared = prepare_turn(
        state,
        capabilities,
        is_isolated_step,
        focus,
        custom_selection,
        plan_context,
        work_context,
        reasoning_effort,
        true,
        session_id,
        shared_operation,
    )
    .await
    .map_err(TurnError::from)?;
    // A step endpoint that owns the telemetry surface replaces the default
    // daemon sink, so the step's live phases reach the client through the very
    // translation an interactive turn uses.
    if let Some(phase_out) = phase_out {
        prepared.phase_out = phase_out;
    }
    run_prepared_turn(state, prompt, prepared, policy)
        .await
        .map_err(TurnError::from)
}

/// Which run body a step endpoint executes through the control contour.
#[allow(dead_code)]
pub(crate) enum StepLoopBody {
    /// One bounded turn assembled from the prompt the caller prepared.
    Single(String),
    /// An ordered sequence of the steps a review horizon made ready.
    Sequence {
        /// Document that records the executed steps.
        persistence: ReviewMemberPersistence,
        /// Identifiers of the steps to execute, in dependency order.
        steps: Vec<String>,
    },
}

/// Options of one step-endpoint execution through the control loops.
pub(crate) struct StepLoopOptions {
    /// Explicit model selection, or the active selection of the session.
    pub(crate) selection: Option<tauqe_protocol::ModelSelection>,
    /// Reasoning depth of the run.
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// Turn marker bound to the tagged control channel.
    pub(crate) marker: Option<String>,
    /// Provider session identifier shared by every turn of the run.
    pub(crate) session_id: Option<String>,
    /// Registration of the batch that owns the run.
    pub(crate) operation: Arc<OperationGuard>,
    /// Plan coordinates published with `model/started`.
    pub(crate) plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    /// Work the run executes, published with `model/started`.
    pub(crate) work_context: Option<TurnWorkContext>,
    /// Body the run executes.
    pub(crate) body: StepLoopBody,
}

/// Terminal report of one step-endpoint execution.
#[allow(dead_code)]
pub(crate) enum StepLoopReport {
    /// A single bounded turn reached its terminal result.
    Turn(LoopResult<TurnArtifact>),
    /// A sequence of plan steps reached its terminal result.
    Sequence(LoopResult<PlanSequenceArtifact>),
    /// The run observed a cancellation request.
    Cancelled,
    /// The run could not be started or failed outside the domain outcome.
    Failed(String),
}

/// Executes one step endpoint through `control::plan` and `control::turn`.
///
/// The daemon contributes what only it owns — the provider, the protocol of the
/// turn, the context snapshot, the apply policy, the usage window, and the
/// telemetry relay — while the engine drives the bounded turn itself: research,
/// staging, verification, healing, the coder ladder, and the durable step
/// transitions all belong to the control loops. Every event the loops emit is
/// translated into the wire telemetry the interface already reads, so a step
/// run observes the same stream an interactive turn does.
pub(crate) async fn run_step_loop(
    state: &Arc<AppState>,
    options: StepLoopOptions,
) -> StepLoopReport {
    let StepLoopOptions {
        selection,
        reasoning_effort,
        marker,
        session_id,
        operation,
        plan_context,
        work_context,
        body,
    } = options;

    let (provider, model, selection, config, effort) = {
        let guard = state.config.lock().await;
        let config = guard.config.clone();
        let selection = selection.unwrap_or_else(|| guard.selection.clone());
        let model = config.resolve_model(&selection);
        let effort = reasoning_effort
            .or_else(|| config.resolve_selection_effort(ModelRole::Coder, &selection));
        // Deterministic unit tests inject a scripted provider through the state's
        // test seam; production builds always construct a real network provider.
        #[cfg(test)]
        let provider: Arc<dyn LlmProvider> = match state.provider_override.lock().await.clone() {
            Some(provider) => provider,
            None => match build_configured_provider(&config, model.provider) {
                Ok(provider) => provider,
                Err(err) => return StepLoopReport::Failed(err.message),
            },
        };
        #[cfg(not(test))]
        let provider: Arc<dyn LlmProvider> =
            match build_configured_provider(&config, model.provider) {
                Ok(provider) => provider,
                Err(err) => return StepLoopReport::Failed(err.message),
            };
        (provider, model, selection, config, effort)
    };

    let op_id = format!("op-{}", next_operation_id());
    let role = match &selection {
        tauqe_protocol::ModelSelection::Role(role) => Some(*role),
        tauqe_protocol::ModelSelection::Specific(_) => None,
        tauqe_protocol::ModelSelection::Tier { .. } => None,
    };
    tracing::info!(
        operation_id = %op_id,
        model = %model,
        role = ?role,
        effort = ?effort,
        work = work_kind(work_context.as_ref()),
        session = session_id.as_deref().unwrap_or("none"),
        "step run started"
    );
    state.out.send_event(&Event {
        method: events::MODEL_STARTED.to_string(),
        params: serde_json::to_value(ModelStartedEvent {
            operation_id: op_id.clone(),
            model: model.clone(),
            role,
            alias: config.model_alias(&model).map(str::to_string),
            effort,
            plan_context,
            work_context,
            is_batch: true,
        })
        .ok(),
    });

    let protocol = resolve_turn_protocol(tauqe_core::control::turn::turn_capability_expr());
    let cancel_rx = operation.cancel_receiver();
    let cancel = CancellationToken::from_receiver(cancel_rx.clone());
    let policy = build_apply_policy(state, config.repl.workflow, cancel_rx.clone());
    let repo_root = state.repo_root().await;
    let mut context_manager = state.context.read().await.clone();
    let mut journal = TurnResearchJournal::default();
    let mut input_cache = tauqe_core::control::cache::TurnInputCache::default();
    let capabilities = tauqe_core::control::turn::turn_capability_expr();

    let prompt = match &body {
        StepLoopBody::Single(prompt) => Some(prompt.clone()),
        StepLoopBody::Sequence { .. } => None,
    };
    let turn_config = TurnLoopConfig::from_app_config(&config);
    let sequence_config = PlanSequenceLoopConfig {
        step: PlanStepLoopConfig {
            turn: turn_config.clone(),
            models: config.models.clone(),
            protocol: tauqe_protocol::EditProtocolKind::Structured,
            marker: marker.clone(),
            selection: Some(selection.clone()),
        },
    };

    let (sink, mut event_rx) = EventSink::channel();
    let edit_cycle = Arc::new(EditCycle::new(state.out.clone(), op_id.clone()));
    let relay = {
        let out = state.out.clone();
        let relay_op = op_id.clone();
        let usage_guard = state.begin_usage(&op_id).await;
        let relay_edit_cycle = Arc::clone(&edit_cycle);
        tokio::spawn(async move {
            let mut usage_guard = usage_guard;
            let mut assistant_text = String::new();
            while let Some(event) = event_rx.recv().await {
                match &event {
                    StreamEvent::TextDelta(delta) => {
                        assistant_text.push_str(delta);
                    }
                    StreamEvent::ResetStream => {
                        assistant_text.clear();
                    }
                    _ => {}
                }
                forward_step_event(&out, &relay_op, event, &mut usage_guard, &relay_edit_cycle)
                    .await;
            }
            // The run publishes the same terminal event an interactive turn does,
            // so a client observes the answer of the step before the verdict of
            // the entity the step addressed is settled and published.
            out.send_event(&Event {
                method: events::MODEL_FINISHED.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": relay_op,
                    "full_text": assistant_text,
                })),
            });
            usage_guard.flush().await;
        })
    };

    // The run is spawned so a hard cancel reaches the task that really executes
    // it: the operation slot registers the abort handle of this very task, exactly
    // as an interactive turn does, and the task hands its context back so the
    // material the step discovered survives the run.
    let task_state = Arc::clone(state);
    let task_sink = sink.clone();
    let loop_task = tokio::spawn(async move {
        let mut policy = policy;
        let prompt_builder = PromptBuilder::new(
            provider.as_ref(),
            &model,
            protocol.as_ref(),
            &mut input_cache,
        )
        .with_session_id(session_id)
        .with_prompt(prompt)
        .with_history(None)
        .with_repo_map_config(
            config.context.include_repo_map,
            config.context.repomap_token_budget,
        )
        .with_patch_strategy(config.repl.patch_strategy)
        .with_prompt_layout(Some(config.resolve_prompt_layout(false)))
        .with_workflow(config.repl.workflow.to_string())
        .with_outer_docs(config.outer_docs.clone())
        .with_turn_brief(Some(TurnBrief::step()))
        .with_reasoning_effort(effort);
        let mut ctx = LoopContext::new(
            &cancel,
            &task_sink,
            &repo_root,
            &mut context_manager,
            &mut journal,
            policy.as_mut(),
            capabilities,
            prompt_builder,
        );
        let report = match body {
            StepLoopBody::Single(_) => match run_turn_loop(&mut ctx, None, &turn_config).await {
                Ok(result) => StepLoopReport::Turn(result),
                Err(SystemError::Cancelled) => StepLoopReport::Cancelled,
                Err(err) => StepLoopReport::Failed(err.to_string()),
            },
            StepLoopBody::Sequence { persistence, steps } => {
                match run_plan_sequence_loop_with(
                    &mut ctx,
                    &persistence,
                    &steps,
                    &sequence_config,
                    None,
                )
                .await
                {
                    Ok(result) => StepLoopReport::Sequence(result),
                    Err(SystemError::Cancelled) => StepLoopReport::Cancelled,
                    Err(err) => StepLoopReport::Failed(err.to_string()),
                }
            }
        };
        (report, context_manager)
    });
    operation.set_abort_handle(loop_task.abort_handle());
    let joined = loop_task.await;
    operation.clear_abort_handle();

    let (report, context_manager) = match joined {
        Ok(joined) => joined,
        Err(join_err) => {
            // A task force-aborted through the operation slot never reports its own
            // outcome, so the authoritative cancel signal classifies the ending: a
            // raised cancel stays a cancellation, while a task that died without it
            // keeps its failure classification.
            let cancelled = *cancel_rx.borrow();
            if !cancelled {
                tracing::warn!(
                    operation_id = op_id.as_str(),
                    error = %join_err,
                    "step run task terminated unexpectedly"
                );
            }
            let report = if cancelled {
                StepLoopReport::Cancelled
            } else {
                StepLoopReport::Failed(format!(
                    "The step run task terminated unexpectedly: {join_err}"
                ))
            };
            (report, task_state.context.read().await.clone())
        }
    };

    // The run widened the context of the step, so the material it discovered is
    // merged back before the relay closes: the next step starts from it.
    if let Err(err) = state
        .mutate_context(move |ctx| ctx.merge_turn_context(&context_manager))
        .await
    {
        tracing::warn!(error = %err, "Failed to merge the step context");
    }
    match &report {
        StepLoopReport::Turn(LoopResult::Success(success)) => {
            let artifact = &success.value;
            if let Some(err) = &artifact.commit_error {
                let outcome = EditOutcome {
                    applied: false,
                    error: Some(err.clone()),
                    changed_files: artifact.modified_paths.clone(),
                    commit_hash: None,
                    verification: artifact.verification,
                };
                edit_cycle.close_outcome(EditTermination::StagingRefused, outcome);
            } else if artifact.commit_hash.is_some() {
                let outcome = EditOutcome {
                    applied: true,
                    error: None,
                    changed_files: artifact.modified_paths.clone(),
                    commit_hash: artifact.commit_hash.clone(),
                    verification: artifact.verification,
                };
                edit_cycle.close_outcome(EditTermination::Answered, outcome);
            } else {
                edit_cycle.close_unapplied(
                    EditTermination::Answered,
                    artifact.modified_paths.clone(),
                    artifact.verification,
                );
            }
        }
        StepLoopReport::Turn(LoopResult::Exhausted(_)) => {
            edit_cycle.close_unapplied(EditTermination::StagingRefused, Vec::new(), None);
        }
        StepLoopReport::Turn(LoopResult::Diverged(_)) => {
            edit_cycle.close_unapplied(EditTermination::Answered, Vec::new(), None);
        }
        StepLoopReport::Cancelled => {
            edit_cycle.close_unapplied(EditTermination::Cancelled, Vec::new(), None);
        }
        StepLoopReport::Failed(reason) => {
            let outcome = EditOutcome {
                applied: false,
                error: Some(reason.clone()),
                changed_files: Vec::new(),
                commit_hash: None,
                verification: None,
            };
            edit_cycle.close_outcome(EditTermination::ProtocolFailure, outcome);
        }
        StepLoopReport::Sequence(LoopResult::Success(seq)) => {
            if seq.value.is_success() {
                let modified: Vec<String> = seq
                    .value
                    .executed_steps
                    .iter()
                    .flat_map(|s| s.modified_paths.clone())
                    .collect();
                let last_commit = seq
                    .value
                    .executed_steps
                    .iter()
                    .rev()
                    .find_map(|s| s.commit.clone());
                edit_cycle.close_outcome(
                    EditTermination::Answered,
                    EditOutcome {
                        applied: true,
                        error: None,
                        changed_files: modified,
                        commit_hash: last_commit,
                        verification: Some(tauqe_protocol::VerificationVerdict::Passed),
                    },
                );
            } else {
                edit_cycle.close_unapplied(EditTermination::Answered, Vec::new(), None);
            }
        }
        StepLoopReport::Sequence(_) => {
            edit_cycle.close_unapplied(EditTermination::Answered, Vec::new(), None);
        }
    }
    drop(sink);
    if let Err(err) = relay.await {
        tracing::warn!(
            operation_id = op_id.as_str(),
            error = %err,
            "step telemetry relay terminated unexpectedly"
        );
    }
    emit_legacy_usage_event(state, &op_id).await;

    report
}

/// Translates one control-loop event into the wire telemetry of the client.
pub(crate) async fn forward_step_event(
    out: &OutChannel,
    operation_id: &str,
    event: StreamEvent,
    usage: &mut UsageGuard,
    edit_cycle: &EditCycle,
) {
    match event {
        StreamEvent::TextDelta(delta) => out.send_event(&Event {
            method: events::MODEL_TEXT_DELTA.to_string(),
            params: Some(serde_json::json!({
                "operation_id": operation_id,
                "delta": delta,
            })),
        }),
        StreamEvent::ReasoningDelta(delta) => out.send_event(&Event {
            method: events::MODEL_REASONING_DELTA.to_string(),
            params: Some(serde_json::json!({
                "operation_id": operation_id,
                "delta": delta,
            })),
        }),
        StreamEvent::ContextChanged(ctx_state) => {
            out.send_event(&Event {
                method: events::CONTEXT_CHANGED.to_string(),
                params: Some(serde_json::json!({ "state": ctx_state })),
            });
        }
        StreamEvent::EditStarted => {
            edit_cycle.open();
        }
        StreamEvent::EditFileStarted { path, op_type } => {
            edit_cycle.open();
            out.send_event(&Event {
                method: events::EDIT_FILE_STARTED.to_string(),
                params: serde_json::to_value(EditFileStartedEvent {
                    operation_id: operation_id.to_string(),
                    path,
                    op_type,
                })
                .ok(),
            });
        }
        StreamEvent::EditHunk {
            path,
            hunk_index,
            old_start,
            old_lines,
            new_start,
            new_lines,
            context_hint,
            old_text,
            new_text,
        } => {
            out.send_event(&Event {
                method: events::EDIT_HUNK.to_string(),
                params: serde_json::to_value(EditHunkEvent {
                    operation_id: operation_id.to_string(),
                    path,
                    hunk_index,
                    old_start,
                    old_lines,
                    new_start,
                    new_lines,
                    context_hint,
                    old_text,
                    new_text,
                })
                .ok(),
            });
        }
        StreamEvent::EditFileDone {
            path,
            status,
            error,
            hunks_count,
        } => {
            out.send_event(&Event {
                method: events::EDIT_FILE_DONE.to_string(),
                params: serde_json::to_value(EditFileDoneEvent {
                    operation_id: operation_id.to_string(),
                    path,
                    status,
                    error,
                    hunks_count,
                })
                .ok(),
            });
        }
        StreamEvent::EditFileRetrying {
            path,
            attempt,
            max_retries,
            reason,
        } => {
            out.send_event(&Event {
                method: events::EDIT_FILE_RETRYING.to_string(),
                params: serde_json::to_value(EditFileRetryingEvent {
                    operation_id: operation_id.to_string(),
                    path,
                    attempt,
                    max_retries,
                    reason,
                })
                .ok(),
            });
        }
        StreamEvent::TurnPhase {
            phase,
            round,
            max_rounds,
            detail,
        } => out.send_event(&Event {
            method: events::TURN_PHASE.to_string(),
            params: serde_json::to_value(tauqe_protocol::TurnPhaseEvent {
                operation_id: operation_id.to_string(),
                phase,
                round,
                max_rounds,
                detail,
            })
            .ok(),
        }),
        StreamEvent::ToolchainStarted { command } => out.send_event(&Event {
            method: events::TOOLCHAIN_STARTED.to_string(),
            params: serde_json::to_value(tauqe_protocol::ToolchainStartedEvent {
                operation_id: operation_id.to_string(),
                command,
            })
            .ok(),
        }),
        StreamEvent::ToolchainFinished {
            command,
            success,
            message,
        } => out.send_event(&Event {
            method: events::TOOLCHAIN_FINISHED.to_string(),
            params: serde_json::to_value(tauqe_protocol::ToolchainFinishedEvent {
                operation_id: operation_id.to_string(),
                command,
                success,
                message,
            })
            .ok(),
        }),
        StreamEvent::RepoMapTokens(tokens) => out.send_event(&Event {
            method: events::TURN_REPOMAP.to_string(),
            params: serde_json::to_value(tauqe_protocol::RepoMapInfoEvent {
                operation_id: operation_id.to_string(),
                tokens,
            })
            .ok(),
        }),
        StreamEvent::TierEscalated {
            tier,
            model,
            role,
            effort,
        } => out.send_event(&Event {
            method: events::MODEL_ESCALATED.to_string(),
            params: serde_json::to_value(ModelEscalatedEvent {
                operation_id: operation_id.to_string(),
                model,
                role: Some(role),
                alias: None,
                effort,
                tier,
            })
            .ok(),
        }),
        StreamEvent::ResetStream => {
            edit_cycle.close_unapplied(EditTermination::Withdrawn, Vec::new(), None);
            withdraw_streamed_attempt(out, operation_id, || ());
        }
        StreamEvent::Usage(observed) => usage.observe(&observed).await,
        StreamEvent::Cancelled => {
            edit_cycle.close_unapplied(EditTermination::Cancelled, Vec::new(), None);
            out.send_event(&Event {
                method: events::MODEL_CANCELLED.to_string(),
                params: Some(serde_json::json!({ "operation_id": operation_id })),
            });
        }
        StreamEvent::Error(message) => {
            close_failed(
                edit_cycle,
                EditTermination::ProviderError,
                format!("Operation failed before edits were applied: {message}"),
            );
            out.send_event(&Event {
                method: events::MODEL_ERROR.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": operation_id,
                    "message": message,
                })),
            });
        }
        _ => {}
    }
}

/// Classifies a completed turn into a stable status label for the journal.
fn turn_status(result: &ModelTurnResult) -> &'static str {
    if result.is_cancelled {
        "cancelled"
    } else if result.error.is_some() {
        "error"
    } else {
        match &result.result {
            Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) if outcome.applied => "applied",
            Some(tauqe_protocol::ModelResult::Edit { .. }) => "edit_failed",
            Some(tauqe_protocol::ModelResult::Answer { .. }) => "answer",
            None => "empty",
        }
    }
}

/// Compact journal label of the work a turn executes.
///
/// The journal records the declared kind of work instead of a rendered
/// description, so the value stays stable, bounded, and free of prompt bodies
/// and entity titles.
fn work_kind(work_context: Option<&TurnWorkContext>) -> &'static str {
    match work_context {
        None => "none",
        Some(TurnWorkContext::Interactive) => "interactive",
        Some(TurnWorkContext::PlanStep { .. }) => "plan_step",
        Some(TurnWorkContext::ReviewGroupStep { .. }) => "review_group_step",
        Some(TurnWorkContext::ReviewStep { .. }) => "review_step",
        Some(TurnWorkContext::ReviewPhase { .. }) => "review_phase",
    }
}

/// Records the terminal outcome of a model turn with its aggregated usage.
///
/// Only metadata is logged — identifiers, the final status, and the aggregated
/// token, cost, and duration counters — never streamed deltas, hunks, or prompt
/// bodies, upholding the Anti-Bloat logging invariant. The duration is the one
/// the harness measured for the operation, so the journal never mixes provider
/// timing with wall-clock time.
fn log_turn_finished(
    op_id: &str,
    model: &tauqe_protocol::ModelRef,
    is_batch: bool,
    status: &'static str,
    usage: &UsageTotals,
    error: Option<&str>,
    work_context: Option<&TurnWorkContext>,
) {
    if matches!(status, "cancelled" | "error" | "edit_failed") {
        tracing::warn!(
            operation_id = %op_id,
            model = %model,
            is_batch,
            work = work_kind(work_context),
            status,
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            total_tokens = usage.total_tokens,
            cost = usage.cost,
            duration_ms = usage.duration_ms,
            error = error.unwrap_or(""),
            "Model turn finished",
        );
    } else {
        tracing::info!(
            operation_id = %op_id,
            model = %model,
            is_batch,
            work = work_kind(work_context),
            status,
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            total_tokens = usage.total_tokens,
            cost = usage.cost,
            duration_ms = usage.duration_ms,
            "Model turn finished",
        );
    }
}

/// Closes the turn's usage window exactly once and returns the snapshot.
///
/// Every terminal path of a turn — success, cancellation, and failure — calls
/// this once, so the tokens and time the turn already consumed are folded into
/// the session totals instead of being dropped. Ownership moves out of the
/// `Option`, which makes a second flush of the same window impossible.
async fn flush_usage(state: &Arc<AppState>, guard: &mut Option<UsageGuard>) -> UsageTotalsSnapshot {
    if let Some(guard) = guard.take() {
        guard.flush().await;
    }
    state.usage_snapshot().await
}

/// Mirrors the authoritative totals onto the legacy `model/usage` channel.
///
/// The aggregate snapshot travels through `model/usageTotals`; this event keeps
/// clients that predate it reading the same numbers, including the in-flight
/// operation while it is still running.
pub(crate) async fn emit_legacy_usage_event(state: &Arc<AppState>, op_id: &str) {
    let snapshot = state.usage_snapshot().await;
    let totals = snapshot.operation_totals().clone();
    state.out.send_event(&Event {
        method: events::MODEL_USAGE.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
            operation_id: op_id.to_string(),
            usage: totals.to_usage_info(),
            session_total_cost: snapshot.session.cost,
            current_cost: Some(totals.cost),
        })
        .ok(),
    });
}

/// Closes the edit cycle as unapplied, carrying the failure that ended it.
///
/// The typed `termination` classifies the ending; `reason` is the detail the
/// closing reports to the client. A cycle that is not open stays untouched.
fn close_failed(cycle: &EditCycle, termination: EditTermination, reason: String) {
    cycle.close_outcome(
        termination,
        EditOutcome {
            applied: false,
            error: Some(reason),
            ..EditOutcome::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dialogue turn binds the interactive session, while an isolated step
    /// stays stateless so it can never inherit the interactive prefix cache.
    #[test]
    fn resolve_session_id_keeps_isolated_steps_stateless() {
        assert_eq!(resolve_session_id(true, "session-1".to_string()), None);
        assert!(resolve_session_id(false, "session-1".to_string()).is_some());
    }
}
