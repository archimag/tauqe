//! Bounded model turn execution.
//!
//! RPC preparation, event pacing, and transport publication live here.
//! Application owns execution assembly and lifecycle settlement.
//! [`start_model_turn`] launches an interactive turn in the background.

use crate::contracts::Project;
use std::sync::Arc;

use tauqe_core::api::UsageTotals;
use tauqe_core::control::context::EventSink;
pub(crate) use tauqe_core::control::step::{StepLoopBody, StepLoopReport};
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::edits::EditOutcome;
use tauqe_core::llm::{CapabilityTree, ModelResult};
use tauqe_core::models::ReasoningEffort;
use tauqe_core::providers::{LlmProvider, StreamEvent};
use tauqe_core::workflow::turn::TurnWorkContext;
use tauqe_protocol::{
    events, EditFileDoneEvent, EditFileRetryingEvent, EditFileStartedEvent, EditHunkEvent, Event,
    GitCommitCreatedEvent, ModelEscalatedEvent, ModelResultEvent, ModelStartedEvent, ResponseError,
    ReviewUpdatedEvent,
};

use crate::handlers::common::withdraw_streamed_attempt;
use crate::handlers::edit_cycle::{EditCycle, EditTermination};
use crate::state::{next_operation_id, AppState, OperationGuard};
use crate::transport::OutChannel;
use crate::usage::UsageGuard;

/// Result of an executed model turn, capturing termination status, errors, and outcomes.
#[derive(Debug, Clone)]
pub struct ModelTurnResult {
    pub operation_id: String,
    pub result: Option<tauqe_core::llm::ModelResult>,
    pub is_cancelled: bool,
    pub error: Option<String>,
}

struct PreparedTurn {
    provider: Arc<dyn LlmProvider>,
    model: tauqe_core::models::ModelRef,
    app_config: tauqe_core::config::AppConfig,
    reasoning_effort: Option<ReasoningEffort>,
    op_id: String,
    /// Capability surface the turn advertises and accepts.
    ///
    /// The tree belongs to the contour that started the turn, so the rendered
    /// rules, the response schema, and the decoder observe one declaration.
    capabilities: CapabilityTree,
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
}

/// Maps provider construction errors to RPC while retaining the scripted test seam.
async fn turn_provider(
    _state: &Arc<AppState>,
    resolved: &tauqe_core::api::ResolvedTurnModel,
) -> Result<Arc<dyn LlmProvider>, ResponseError> {
    #[cfg(test)]
    if let Some(provider) = _state.provider_override.lock().await.clone() {
        return Ok(provider);
    }
    resolved
        .provider()
        .map_err(|error| ResponseError::new(tauqe_protocol::errors::NO_API_KEY, error.to_string()))
}

/// Grace period granted to a workflow task to unwind cooperatively after a
/// cancellation or stream error before a supervisor force-aborts it.
const CANCEL_UNWIND_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Grace period for a workflow task to unwind cooperatively.
pub(crate) fn unwind_grace() -> std::time::Duration {
    CANCEL_UNWIND_GRACE
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
    capabilities: CapabilityTree,
    is_isolated_step: bool,
    focus: Option<DiscussionTarget>,
    custom_selection: Option<tauqe_core::models::ModelSelection>,
    plan_context: Option<tauqe_core::workflow::turn::ModelTurnPlanContext>,
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

    let mode = if is_isolated_step {
        tauqe_core::api::TurnMode::Step
    } else {
        tauqe_core::api::TurnMode::Interactive
    };
    let resolved = state
        .application
        .resolve_turn_model(custom_selection, reasoning_effort, mode)
        .await;
    let provider = turn_provider(state, &resolved).await?;
    let tauqe_core::api::ResolvedTurnModel {
        config: app_config,
        selection: _,
        model,
        role,
        effort,
        alias,
    } = resolved;
    let op_id = format!("op-{}", next_operation_id());
    let session_id = state.application.turn_session(mode, session_id).await;

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
            model: model.clone().project(),
            role: role.map(crate::contracts::Project::project),
            alias,
            effort: effort.map(crate::contracts::Project::project),
            plan_context: plan_context.map(Project::project),
            work_context: work_context.clone().map(Project::project),
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
        focus,
        is_batch,
        work_context,
        operation,
        session_id,
    })
}
async fn run_prepared_turn(
    state: &Arc<AppState>,
    prompt: String,
    prepared: PreparedTurn,
) -> Result<ModelTurnResult, ResponseError> {
    let PreparedTurn {
        provider,
        model,
        app_config,
        op_id,
        capabilities,
        focus,
        is_batch,
        work_context,
        operation,
        session_id,
        reasoning_effort,
    } = prepared;

    let mut cancel_rx = operation.cancel_receiver();
    let tauqe_core::api::StartedInteractiveExecution {
        task: wf_task,
        events: mut event_rx,
        turn_id,
    } = state
        .application
        .start_interactive_execution(
            Arc::clone(&operation),
            tauqe_core::api::InteractiveExecutionRequest {
                provider,
                model: model.clone(),
                config: app_config,
                capabilities,
                prompt,
                focus: focus.clone(),
                session_id,
                reasoning_effort,
            },
        )
        .await
        .map_err(|error| {
            ResponseError::new(
                tauqe_protocol::errors::OPERATION_IN_PROGRESS,
                error.to_string(),
            )
        })?;

    let mut stream = tauqe_core::api::InteractiveStreamState::default();
    let mut usage_guard = Some(state.begin_usage(&op_id).await);
    let edit_cycle = EditCycle::new(state.out.clone(), op_id.clone());

    while let Some(event) = event_rx.recv().await {
        state.out.pace_event(&event, &mut cancel_rx).await;
        stream.observe(&event);
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
                state.out.send_event(&Event {
                    method: events::MODEL_TEXT_DELTA.to_string(),
                    params: Some(serde_json::json!({
                        "operation_id": op_id.clone(),
                        "delta": delta,
                    })),
                });
            }
            StreamEvent::ContextChanged(ctx_state) => {
                crate::handlers::context::publish_context(&state.out, &ctx_state);
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
                        op_type: op_type.project(),
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
                        status: status.project(),
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
                        phase: phase.project(),
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
                withdraw_streamed_attempt(&state.out, &op_id, || ());
            }
            StreamEvent::Cancelled => {
                edit_cycle.close_unapplied(EditTermination::Cancelled, Vec::new(), None);
                state.out.send_event(&Event {
                    method: events::MODEL_CANCELLED.to_string(),
                    params: Some(serde_json::json!({ "operation_id": op_id.clone() })),
                });
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
                break;
            }
            _ => {}
        }
    }

    let joined = wf_task.join(&op_id).await;
    let mut publisher = FinalizationPublisher {
        state,
        operation_id: &op_id,
        edit_cycle: &edit_cycle,
    };
    let finalized = state
        .application
        .finalize_interactive_turn(
            &operation,
            tauqe_core::api::InteractiveFinalizationInput {
                joined,
                stream,
                turn_id,
                focus,
            },
            &mut usage_guard,
            &mut publisher,
        )
        .await
        .map_err(|error| {
            ResponseError::new(
                tauqe_protocol::errors::OPERATION_IN_PROGRESS,
                error.to_string(),
            )
        })?;
    let snapshot = finalized.usage;
    let operation_totals = snapshot.prev.clone();
    let completion = match finalized.outcome {
        tauqe_core::api::InteractiveFinalizationOutcome::Completed(completion) => completion,
        tauqe_core::api::InteractiveFinalizationOutcome::Interrupted { cancelled, error } => {
            let result = ModelTurnResult {
                operation_id: op_id,
                result: None,
                is_cancelled: cancelled,
                error,
            };
            log_turn_finished(
                &result.operation_id,
                &model,
                is_batch,
                turn_status(&result),
                &operation_totals,
                result.error.as_deref(),
                work_context.as_ref(),
            );
            return Ok(result);
        }
    };
    let plan_rejections = completion.plan_rejections;
    let plan_warnings = completion.plan_warnings;
    let model_result = completion.result;
    let error_msg = completion.error;
    let full_text = match &model_result {
        ModelResult::Answer { text } => text.clone(),
        ModelResult::Edit { summary, .. } => summary.clone(),
    };

    state.out.send_event(&Event {
        method: events::MODEL_RESULT.to_string(),
        params: Some(serde_json::json!(ModelResultEvent {
            operation_id: op_id.clone(),
            result: super::result::result_dto(model_result.clone()),
            usage: Some(crate::usage::wire_usage(&operation_totals.to_usage_info())),
            session_total_cost: Some(snapshot.session.cost),
            current_cost: Some(operation_totals.cost),
            plan_rejections: plan_rejections
                .into_iter()
                .map(crate::contracts::Project::project)
                .collect(),
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

/// Initiates an asynchronous model turn in the background and immediately returns its operation ID.
///
/// Designed as a lightweight non-blocking wrapper for interactive single-prompt RPC requests.
pub async fn start_model_turn(
    state: &Arc<AppState>,
    prompt: String,
    focus: Option<tauqe_protocol::DiscussionTarget>,
    custom_selection: Option<tauqe_core::models::ModelSelection>,
    plan_context: Option<tauqe_core::workflow::turn::ModelTurnPlanContext>,
) -> Result<String, ResponseError> {
    let focus = focus.map(Project::project);
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
    let state_clone = Arc::clone(state);
    tokio::spawn(async move {
        let _ = run_prepared_turn(&state_clone, prompt, prepared).await;
    });
    Ok(op_id)
}

/// Options of one step-endpoint execution through the control loops.
pub(crate) struct StepLoopOptions {
    /// Explicit model selection, or the active selection of the session.
    pub(crate) selection: Option<tauqe_core::models::ModelSelection>,
    /// Reasoning depth of the run.
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// Turn marker bound to the tagged control channel.
    pub(crate) marker: Option<String>,
    /// Provider session identifier shared by every turn of the run.
    pub(crate) session_id: Option<String>,
    /// Registration of the batch that owns the run.
    pub(crate) operation: Arc<OperationGuard>,
    /// Plan coordinates published with `model/started`.
    pub(crate) plan_context: Option<tauqe_core::workflow::turn::ModelTurnPlanContext>,
    /// Work the run executes, published with `model/started`.
    pub(crate) work_context: Option<TurnWorkContext>,
    /// Body the run executes.
    pub(crate) body: StepLoopBody,
}

/// Executes one step endpoint through `control::plan` and `control::turn`.
///
/// The daemon supplies provider test injection, the usage window, and the
/// telemetry relay. Application assembles execution dependencies and drives
/// the bounded turn: research,
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

    let resolved = state
        .application
        .resolve_turn_model(selection, reasoning_effort, tauqe_core::api::TurnMode::Step)
        .await;
    let provider = match turn_provider(state, &resolved).await {
        Ok(provider) => provider,
        Err(error) => return StepLoopReport::Failed(error.message),
    };
    let tauqe_core::api::ResolvedTurnModel {
        config,
        selection,
        model,
        role,
        effort,
        alias,
    } = resolved;
    let op_id = format!("op-{}", next_operation_id());
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
            model: model.clone().project(),
            role: role.map(crate::contracts::Project::project),
            alias,
            effort: effort.map(crate::contracts::Project::project),
            plan_context: plan_context.map(Project::project),
            work_context: work_context.map(Project::project),
            is_batch: true,
        })
        .ok(),
    });

    let execution = match state
        .application
        .prepare_step_execution(
            &operation,
            tauqe_core::api::StepExecutionRequest {
                provider,
                model,
                config,
                selection,
                reasoning_effort: effort,
                marker,
                session_id,
                body,
            },
        )
        .await
    {
        Ok(execution) => execution,
        Err(error) => return StepLoopReport::Failed(error.to_string()),
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

    // Core owns task supervision, recovery, and merging the returned context.
    let report = state
        .application
        .execute_prepared_step(&operation, &op_id, execution, sink.clone())
        .await;
    let completion = tauqe_core::api::classify_step_edit_completion(&report);
    let termination = match completion.disposition {
        tauqe_core::api::StepEditDisposition::Completed => EditTermination::Answered,
        tauqe_core::api::StepEditDisposition::Refused => EditTermination::StagingRefused,
        tauqe_core::api::StepEditDisposition::Cancelled => EditTermination::Cancelled,
        tauqe_core::api::StepEditDisposition::Failed => EditTermination::ProtocolFailure,
    };
    edit_cycle.close_outcome(termination, completion.outcome);
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
            crate::handlers::context::publish_context(out, &ctx_state);
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
                    op_type: op_type.project(),
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
                    status: status.project(),
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
                phase: phase.project(),
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
                model: model.project(),
                role: Some(role.project()),
                alias: None,
                effort: effort.map(crate::contracts::Project::project),
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
            Some(tauqe_core::llm::ModelResult::Edit { outcome, .. }) if outcome.applied => {
                "applied"
            }
            Some(tauqe_core::llm::ModelResult::Edit { .. }) => "edit_failed",
            Some(tauqe_core::llm::ModelResult::Answer { .. }) => "answer",
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
    model: &tauqe_core::models::ModelRef,
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
            usage: crate::usage::wire_usage(&totals.to_usage_info()),
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

struct FinalizationPublisher<'a> {
    state: &'a Arc<AppState>,
    operation_id: &'a str,
    edit_cycle: &'a EditCycle,
}
#[async_trait::async_trait]
impl tauqe_core::api::InteractiveFinalizationObserver for FinalizationPublisher<'_> {
    async fn publish(&mut self, update: tauqe_core::api::InteractiveFinalizationUpdate<'_>) {
        use tauqe_core::api::InteractiveFinalizationUpdate as Update;
        match update {
            Update::Cancelled => {
                self.edit_cycle
                    .close_unapplied(EditTermination::Cancelled, Vec::new(), None);
                self.state.out.send_event(&Event {
                    method: events::MODEL_CANCELLED.to_string(),
                    params: Some(serde_json::json!({"operation_id":self.operation_id})),
                });
            }
            Update::UsageFlushed(_) => emit_legacy_usage_event(self.state, self.operation_id).await,
            Update::EditCompletion(completion) => {
                if let Some(reason) = &completion.error {
                    close_failed(
                        self.edit_cycle,
                        EditTermination::StagingRefused,
                        reason.clone(),
                    );
                } else {
                    match &completion.result {
                        ModelResult::Answer { .. } => {
                            self.edit_cycle.close_answered();
                        }
                        ModelResult::Edit {
                            outcome, summary, ..
                        } => {
                            if let Some(hash) = &outcome.commit_hash {
                                self.state.out.send_event(&Event {
                                    method: events::GIT_COMMIT_CREATED.to_string(),
                                    params: serde_json::to_value(GitCommitCreatedEvent {
                                        commit_hash: hash.clone(),
                                        summary: summary.clone(),
                                        changed_files: outcome.changed_files.clone(),
                                    })
                                    .ok(),
                                });
                                if let Some(repository) =
                                    self.state.application.repository_after_turn_commit().await
                                {
                                    self.state.out.send_event(&Event {method:events::GIT_STATE_CHANGED.to_string(),params:Some(serde_json::json!({"repository":crate::handlers::git::repository_dto(repository)}))});
                                }
                            }
                            self.edit_cycle.open();
                            self.edit_cycle
                                .close_outcome(EditTermination::Answered, outcome.clone());
                        }
                    }
                }
            }
            Update::Context(context) => {
                crate::handlers::context::publish_context(&self.state.out, &context)
            }
            Update::Plans(updates) => {
                if let Some(plan) = updates.active_plan {
                    self.state.out.send_event(&Event {
                        method: events::PLAN_UPDATED.to_string(),
                        params: serde_json::to_value(tauqe_protocol::PlanUpdatedEvent {
                            plan: plan.project(),
                            is_save: true,
                        })
                        .ok(),
                    });
                }
                self.state.out.send_event(&Event {
                    method: events::PLAN_LIST_CHANGED.to_string(),
                    params: serde_json::to_value(tauqe_protocol::PlanListChangedEvent {
                        plans: updates.plans.into_iter().map(Project::project).collect(),
                        active_id: updates.active_id,
                    })
                    .ok(),
                });
            }
            Update::Review(updates) => {
                for _ in 0..updates.decisions_applied {
                    self.state.out.send_event(&Event {
                        method: events::REVIEW_UPDATED.to_string(),
                        params: serde_json::to_value(ReviewUpdatedEvent {
                            session: updates.session.clone().project(),
                        })
                        .ok(),
                    });
                }
            }
        }
    }
}
