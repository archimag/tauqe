//! Bounded model turn execution.
//!
//! Provider resolution, protocol selection, streaming relay, and the turn's
//! usage accounting live here. [`start_model_turn`] launches an interactive
//! turn in the background, while [`execute_model_turn`] awaits one as a batch
//! member step.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauqe_core::prompt::json::JsonPromptProtocol;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::prompt::xml::XmlPromptProtocol;
use tauqe_core::providers::{
    create_provider, format_session_id, LlmProvider, StreamEvent, SESSION_PREFIX_REPL,
};
use tauqe_core::workflow::{ExecutionMode, SingleExecution};
use tauqe_protocol::model::{UsageTotals, UsageTotalsSnapshot};
use tauqe_protocol::{
    events, DiscussionTarget, EditFileDoneEvent, EditFileRetryingEvent, EditFileStartedEvent,
    EditFinishedEvent, EditHunkEvent, EditOutcome, EditStartedEvent, Event, GitCommitCreatedEvent,
    LlmCapability, ModelResultEvent, ModelRole, ModelStartedEvent, ReasoningEffort, ResponseError,
    TurnIntent,
};

use crate::handlers::common::withdraw_streamed_attempt;
use crate::handlers::offload;
use crate::state::{next_operation_id, AppState, OperationGuard};
use crate::transport::OutChannel;
use crate::usage::UsageGuard;

/// Result of an executed model turn, capturing termination status, errors, and outcomes.
#[derive(Debug, Clone)]
pub struct ModelTurnResult {
    pub operation_id: String,
    pub result: Option<tauqe_protocol::ModelResult>,
    pub assistant_text: String,
    pub is_cancelled: bool,
    pub error: Option<String>,
    pub has_terminal_success: bool,
    /// Coder's block report when the turn ended with a semantic `<blocked>`
    /// signal. Preserved so plan and review supervisors can react accordingly.
    pub blocked: Option<String>,
    /// Coder's abort report when the turn ended with a semantic `<abort>`
    /// signal. Preserved so a supervisor can stop its queue without architect
    /// escalation while the developer receives the exact action to perform.
    pub abort: Option<tauqe_core::workflow::fsm::AbortReport>,
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
/// The operation identity travels with the failure, so a supervisor publishes
/// its failure event under the operation that actually produced it instead of
/// minting an identifier that matches no registered operation. The identity is
/// absent only when the turn was refused before any operation existed — for
/// example because another operation already held the slot.
#[derive(Debug)]
pub struct TurnError {
    /// Identifier of the operation the failed turn belonged to.
    pub operation_id: Option<String>,
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
            operation_id: None,
            error: Box::new(error),
        }
    }
}

struct PreparedTurn {
    provider: Arc<dyn LlmProvider>,
    model: tauqe_protocol::ModelRef,
    app_config: tauqe_core::config::AppConfig,
    selection: tauqe_protocol::ModelSelection,
    reasoning_effort: Option<ReasoningEffort>,
    op_id: String,
    /// Semantic mode of the turn, declared by the caller that starts it.
    intent: TurnIntent,
    /// Whether the turn runs as a member of a batch supervisor. Recorded so the
    /// turn's terminal log can distinguish batch steps from interactive turns.
    is_batch: bool,
    /// Operation backing this turn. A turn that starts on its own owns the
    /// registration; a batch step shares the batch-level registration so the
    /// slot never lapses between steps.
    operation: Arc<OperationGuard>,
    /// Active turn marker binding structural protocol tags to this turn.
    marker: Option<String>,
    /// Provider session identifier binding the turn to a warm prompt prefix.
    ///
    /// `None` for a turn that carries no dialogue, so an isolated step never
    /// inherits the cache of the interactive session.
    session_id: Option<String>,
}

/// Builds a real network provider from configuration, mapping failures to a
/// `NO_API_KEY` response error.
fn build_configured_provider(
    config: &tauqe_core::config::AppConfig,
    kind: tauqe_protocol::ProviderKind,
) -> Result<Arc<dyn LlmProvider>, ResponseError> {
    create_provider(kind, config)
        .map(Arc::from)
        .map_err(|err| ResponseError::new(tauqe_protocol::errors::NO_API_KEY, err.to_string()))
}

/// Grace period granted to a workflow task to unwind cooperatively after a
/// cancellation or stream error before a supervisor force-aborts it.
pub(crate) const CANCEL_UNWIND_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Builds the apply policy for a turn, wiring the shared git-transaction guard
/// into the Git-backed policy so a supervisor never force-aborts a workflow
/// task while a transaction is open.
pub(crate) fn build_apply_policy(
    state: &Arc<AppState>,
    workflow: tauqe_protocol::WorkflowKind,
) -> Box<dyn tauqe_core::workflow::policy::ApplyPolicy> {
    match workflow {
        tauqe_protocol::WorkflowKind::Naive => {
            Box::new(tauqe_core::workflow::policy::DirectApplyPolicy::new())
        }
        tauqe_protocol::WorkflowKind::Git => Box::new(
            tauqe_core::workflow::git_policy::GitTransactionPolicy::with_guard(Arc::clone(
                &state.git_transaction_active,
            )),
        ),
    }
}

/// Resolves the provider and model for a turn, emits `model/started`, and
/// registers the cancellable active operation. `is_batch` marks turns that run
/// as members of a batch supervisor so clients can suppress per-step signals.
///
/// The collaborators are intentionally threaded as separate parameters: every
/// turn entry point supplies exactly the subset it owns, so a wrapper struct
/// would only restate the same fields without removing a single argument.
#[allow(clippy::too_many_arguments)]
async fn prepare_turn(
    state: &Arc<AppState>,
    intent: TurnIntent,
    custom_selection: Option<tauqe_protocol::ModelSelection>,
    plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    reasoning_effort: Option<ReasoningEffort>,
    is_batch: bool,
    marker: Option<String>,
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
        None => resolve_session_id(intent, state.session_uuid().await),
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
        app_config.resolve_selection_effort(role.unwrap_or(ModelRole::Primary), &selection)
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
        intent = ?intent,
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
            is_batch,
        })
        .ok(),
    });

    Ok(PreparedTurn {
        provider,
        model,
        app_config,
        selection,
        reasoning_effort: effort,
        op_id,
        intent,
        is_batch,
        operation,
        marker,
        session_id,
    })
}

/// Resolves the prompt protocol for a turn from its declared intent.
///
/// A read-only plan discussion always uses the structured discussion protocol,
/// regardless of the configured `develop.protocol`: the discussion system prompt
/// instructs the model to answer with the discussion JSON schema, so pairing it
/// with an XML stream filter would leak the raw JSON scaffolding into the REPL.
///
/// An interactive review focus dialogue and a plain develop turn keep the
/// configured base protocol. This preserves the dual capability of a focused
/// review (addressed concept c-3): the developer may reason about a finding and
/// directly request a code fix, so the turn keeps the file-edit and
/// toolchain-verification capabilities of the configured protocol instead of
/// collapsing into the text-only discussion schema.
///
/// The configured transport chooses the encoding only, never the capability
/// surface: a `structured` project and an `xml` project advertise the same
/// operational surface for the same intent, so the format changes how commands
/// are serialized and nothing else. The two-contour `mixed` mode keeps the XML
/// transport for code editing, while a plan discussion still answers with the
/// discussion JSON Schema.
///
/// The plan permissions matrix is a property of the dialogue, never of the
/// transport: a user may ask for a plan at any moment of an interactive turn,
/// so plan authorship already belongs to the interactive profile, while
/// modifying an existing plan requires the dialogue to be anchored on one.
/// `plan_focused` carries that anchor, and an interactive turn whose focus is a
/// plan mixes plan mutation into the chosen protocol's capability set. The mix
/// runs through `with_capability_set`, so both encodings advertise and decode
/// the very same mutation, and the harness keeps validating the mutation itself
/// after decoding.
///
/// The active turn marker is bound onto the chosen protocol when its wire format
/// supports it; the structured discussion protocol encodes its structure in JSON
/// Schema and therefore declines the marker.
pub(super) fn resolve_turn_protocol(
    intent: TurnIntent,
    develop_protocol: tauqe_protocol::EditProtocolKind,
    plan_focused: bool,
    marker: Option<&str>,
) -> Box<dyn PromptProtocol> {
    // The declared intent, not the transport, decides what the turn may do:
    // `PromptAssembly::effective_capabilities` narrows the rendered prompt and
    // the FSM narrows the decoder of an isolated step, both through the
    // intent-oriented profiles. Selecting a profile here by wire format would
    // make XML and Structured Output disagree about the very same turn.
    let base_protocol: Box<dyn PromptProtocol> = if intent.is_readonly_discussion() {
        Box::new(JsonPromptProtocol::discussion())
    } else {
        match develop_protocol {
            tauqe_protocol::EditProtocolKind::Structured => Box::new(JsonPromptProtocol::edits()),
            tauqe_protocol::EditProtocolKind::Xml | tauqe_protocol::EditProtocolKind::Mixed => {
                Box::new(XmlPromptProtocol)
            }
        }
    };
    // Plan mutation follows the dialogue focus rather than the wire format: an
    // isolated step owns no plan permission at all, and a focused interactive
    // turn gains exactly the mutation capability the anchor grants.
    let base_protocol = if plan_focused && intent.is_interactive_repl() {
        let capabilities = base_protocol
            .capability_set()
            .with(LlmCapability::PlanUpdate);
        base_protocol.with_capability_set(capabilities)
    } else {
        base_protocol
    };
    match marker {
        Some(marker) => base_protocol
            .with_turn_marker(marker)
            .unwrap_or(base_protocol),
        None => base_protocol,
    }
}

/// Resolves the provider session identifier that binds a turn to a warm prompt
/// prefix.
///
/// Only a dialogue turn carries a session: interactive REPL turns and focused
/// discussions share one identifier, so successive rounds reuse the backend's
/// cached prefix. A turn without a dialogue receives `None`, so an isolated step
/// never inherits — or poisons — the interactive cache.
pub(super) fn resolve_session_id(intent: TurnIntent, dialogue_session: String) -> Option<String> {
    if intent.is_isolated_step() {
        None
    } else {
        Some(format_session_id(SESSION_PREFIX_REPL, &dialogue_session))
    }
}

async fn run_prepared_turn(
    state: &Arc<AppState>,
    prompt: String,
    prepared: PreparedTurn,
    mut policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
) -> Result<ModelTurnResult, ResponseError> {
    let PreparedTurn {
        provider,
        model,
        app_config,
        selection,
        reasoning_effort,
        op_id,
        intent,
        is_batch,
        operation,
        marker,
        session_id,
    } = prepared;

    let cancel_rx = operation.cancel_receiver();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

    // The dialogue focus is snapshotted before the turn starts, so a concurrent
    // focus change can never alter the protocol of a turn that is already
    // running.
    let plan_focused = matches!(
        state.discussion_focus().await.target(),
        Some(DiscussionTarget::Plan { .. })
    );
    let protocol = resolve_turn_protocol(
        intent,
        app_config.develop.protocol,
        plan_focused,
        marker.as_deref(),
    );

    let mut workflow_opts = tauqe_core::workflow::WorkflowOptions::from_app_config(&app_config);
    // The prepared effort already carries the role-resolved reasoning depth, so
    // the workflow receives exactly the depth the turn reports instead of
    // resolving it a second time.
    workflow_opts.reasoning_effort = reasoning_effort;
    workflow_opts.initial_selection = Some(selection);
    workflow_opts.session_id = session_id;
    let execution = SingleExecution::new(workflow_opts);
    let (wf_tx, wf_rx) = (tx.clone(), cancel_rx.clone());
    drop(tx);

    let state_clone = Arc::clone(state);
    let model_clone = model.clone();
    let prompt_clone = prompt.clone();

    let workflow_future = async move {
        let _session_guard = state_clone.session_lock.lock().await;
        let mut local_context = state_clone.context.read().await.clone();
        let mut history = state_clone.history.lock().await;
        let res = execution
            .execute(
                policy.as_mut(),
                &prompt_clone,
                intent,
                provider.as_ref(),
                &model_clone,
                &mut local_context,
                &mut history,
                protocol.as_ref(),
                wf_tx,
                wf_rx,
            )
            .await;
        if let Err(err) = state_clone
            .mutate_context(move |ctx| ctx.merge_turn_context(&local_context))
            .await
        {
            tracing::warn!(error = %err, "Failed to merge the turn context");
        }
        res
    };

    let mut wf_task = tokio::spawn(workflow_future);
    operation.set_abort_handle(wf_task.abort_handle());

    let mut raw_streamed_text = String::new();
    // Open the shared usage window for this turn. Every provider call reports
    // into it, and only the terminal flush folds it into the session totals, so
    // intermediate usage events can never be counted twice.
    let mut usage_guard = Some(state.begin_usage().await);
    let mut edit_events_sent = false;
    let mut edit_finished_sent = false;
    let mut is_cancelled = false;
    let mut is_error = false;
    let mut stream_error_msg = None;

    while let Some(event) = rx.recv().await {
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
                edit_events_sent = true;
                state.out.send_event(&Event {
                    method: events::EDIT_STARTED.to_string(),
                    params: serde_json::to_value(EditStartedEvent {
                        operation_id: op_id.clone(),
                    })
                    .ok(),
                });
            }
            StreamEvent::EditFileStarted { path, op_type } => {
                edit_events_sent = true;
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
                old_text,
                new_text,
            } => {
                state.out.send_event(&Event {
                    method: events::EDIT_HUNK.to_string(),
                    params: serde_json::to_value(EditHunkEvent {
                        operation_id: op_id.clone(),
                        path,
                        hunk_index,
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
            StreamEvent::ModelEscalated {
                model,
                role,
                alias,
                effort,
                tier,
            } => {
                // The harness promotes the turn to a stronger coder tier of the
                // same role instead of switching roles. Report the new model
                // coordinates so the client can relabel the running turn without
                // waiting for it to finish.
                tracing::info!(
                    operation_id = %op_id,
                    model = %model,
                    tier,
                    "Model turn escalated to a stronger tier",
                );
                state.out.send_event(&Event {
                    method: events::MODEL_ESCALATED.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ModelEscalatedEvent {
                        operation_id: op_id.clone(),
                        model,
                        role,
                        alias,
                        effort,
                        tier,
                    })
                    .ok(),
                });
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
                // The provider broke after partial output: everything streamed
                // so far belongs to the aborted attempt, so the client discards
                // it and this turn stops carrying it as assistant text. The
                // withdrawal itself is shared with every other stream contour.
                withdraw_streamed_attempt(&state.out, &op_id, || raw_streamed_text.clear());
            }
            StreamEvent::Done => {}
            StreamEvent::Cancelled => {
                if edit_events_sent && !edit_finished_sent {
                    send_edit_aborted(
                        &state.out,
                        &op_id,
                        "Operation cancelled before edits were applied".to_string(),
                    );
                    edit_finished_sent = true;
                }
                state.out.send_event(&Event {
                    method: events::MODEL_CANCELLED.to_string(),
                    params: Some(serde_json::json!({ "operation_id": op_id.clone() })),
                });
                is_cancelled = true;
                break;
            }
            StreamEvent::Error(err) => {
                if edit_events_sent && !edit_finished_sent {
                    send_edit_aborted(
                        &state.out,
                        &op_id,
                        format!("Operation failed before edits were applied: {}", err),
                    );
                    edit_finished_sent = true;
                }
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
        }
    }

    if is_cancelled || is_error {
        // Cooperative cancellation is the primary path: the workflow observes
        // the cancel signal and unwinds its own transaction. The handle is
        // polled by reference, so the grace wait never consumes the join result
        // and the task stays owned while this turn decides what happens next.
        let unwind = tokio::time::timeout(CANCEL_UNWIND_GRACE, &mut wf_task).await;
        let mut unwound_error = None;
        match unwind {
            Ok(Ok(Err(err))) => {
                // The workflow unwound on its own but reported a failure: the
                // detail belongs to the turn instead of a discarded join.
                unwound_error = Some(err.to_string());
            }
            Ok(Ok(Ok(_))) => {
                tracing::info!(
                    operation_id = %op_id,
                    "Cancelled turn finished its workflow before the grace period elapsed",
                );
            }
            Ok(Err(join_err)) => {
                tracing::warn!(
                    operation_id = %op_id,
                    error = %join_err,
                    "Cancelled turn's workflow task did not join cleanly",
                );
            }
            Err(_elapsed) => {
                // Force-aborting is a last resort with a generous grace period,
                // and it is deferred while a git transaction is still open,
                // because an abort lands at the next `.await` and could
                // otherwise strand a half-finished checkpoint or step commit
                // until startup crash recovery runs.
                if state.git_transaction_active.load(Ordering::SeqCst) {
                    tracing::warn!(
                        operation_id = %op_id,
                        grace_secs = CANCEL_UNWIND_GRACE.as_secs(),
                        "Turn did not unwind within the grace period; deferring abort while a git transaction is open",
                    );
                    // The task and this turn's operation registration move into
                    // the watchdog together, so the slot is released only once
                    // the task has truly exited. Returning here would hand a
                    // half-unwound workflow back to the client.
                    state.defer_abort_owning_operation(Arc::clone(&operation), wf_task);
                } else {
                    tracing::warn!(
                        operation_id = %op_id,
                        grace_secs = CANCEL_UNWIND_GRACE.as_secs(),
                        "Turn did not unwind within the grace period; aborting workflow task",
                    );
                    wf_task.abort();
                    // Wait, bounded, for the abort to land: returning while the
                    // task may still sit between abort landing points would
                    // release the slot under a live task.
                    let _ = tokio::time::timeout(CANCEL_UNWIND_GRACE, wf_task).await;
                }
            }
        }

        // Close the usage window exactly once on this terminal path: tokens and
        // time already consumed by a cancelled or failed turn are still charged,
        // and the partial report never leaks into the next operation.
        let snapshot = flush_usage(state, &mut usage_guard).await;
        emit_legacy_usage_event(state, &op_id).await;

        let reason = if is_cancelled {
            Some("Operation cancelled".to_string())
        } else {
            stream_error_msg
        };
        // A workflow that unwound with its own failure keeps that detail: the
        // cancellation reason alone would hide why the transaction failed.
        let error = match (reason, unwound_error) {
            (Some(reason), Some(detail)) => Some(format!("{}: {}", reason, detail)),
            (None, detail) => detail,
            (reason, None) => reason,
        };

        let status = if is_cancelled { "cancelled" } else { "error" };
        log_turn_finished(
            &op_id,
            &model,
            is_batch,
            status,
            &snapshot.prev,
            error.as_deref(),
        );

        return Ok(ModelTurnResult {
            operation_id: op_id,
            result: None,
            assistant_text: raw_streamed_text,
            is_cancelled,
            error,
            has_terminal_success: false,
            blocked: None,
            abort: None,
        });
    }

    // Each arm yields the turn result together with the usage totals of the
    // operation it just closed, so the terminal log never re-derives them.
    let (turn_result, final_usage) = match wf_task.await {
        Ok(Ok(wf_result)) => {
            let snapshot = flush_usage(state, &mut usage_guard).await;
            emit_legacy_usage_event(state, &op_id).await;
            let operation_totals = snapshot.prev.clone();

            // A plan mutation the storage engine refused is a diagnostic, not a
            // failed turn: the reasons travel to the developer through the turn
            // stream and the session history, and the daemon records them once
            // per turn so the refusal stays reconstructible from the journal.
            if !wf_result.plan_errors.is_empty() {
                tracing::warn!(
                    operation_id = %op_id,
                    rejected = wf_result.plan_errors.len(),
                    reasons = %wf_result.plan_errors.join(" | "),
                    "plan mutations were rejected during the turn",
                );
            }

            if let tauqe_protocol::ModelResult::Edit {
                ref summary,
                ref outcome,
                ..
            } = wf_result.result
            {
                state.out.send_event(&Event {
                    method: events::EDIT_FINISHED.to_string(),
                    params: serde_json::to_value(EditFinishedEvent {
                        operation_id: op_id.clone(),
                        outcome: outcome.clone(),
                    })
                    .ok(),
                });
                edit_finished_sent = true;

                if let Some(hash) = &outcome.commit_hash {
                    state.out.send_event(&Event {
                        method: events::GIT_COMMIT_CREATED.to_string(),
                        params: serde_json::to_value(GitCommitCreatedEvent {
                            commit_hash: hash.clone(),
                            summary: summary.clone(),
                            changed_files: outcome.changed_files.clone(),
                        })
                        .ok(),
                    });

                    // Reading the repository state shells out to git; offload
                    // it and resolve the repository from the authoritative
                    // shared root instead of the process CWD.
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
                            tracing::warn!(
                                error = %err,
                                "Failed to read repository state after commit"
                            );
                        }
                    }
                }
            }

            let ctx_state = state.context.read().await.get_state();
            state.out.send_event(&Event {
                method: events::CONTEXT_CHANGED.to_string(),
                params: Some(serde_json::json!({ "state": ctx_state })),
            });

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
                                is_save: wf_result.plan_saved,
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

            if should_report_aborted_edits(edit_events_sent, edit_finished_sent, &wf_result.result)
            {
                send_edit_aborted(
                    &state.out,
                    &op_id,
                    "Edits were streamed but no edit result was produced".to_string(),
                );
            }

            state.out.send_event(&Event {
                method: events::MODEL_RESULT.to_string(),
                params: Some(serde_json::json!(ModelResultEvent {
                    operation_id: op_id.clone(),
                    result: wf_result.result.clone(),
                    usage: Some(operation_totals.to_usage_info()),
                    session_total_cost: Some(snapshot.session.cost),
                    current_cost: Some(operation_totals.cost),
                })),
            });

            state.out.send_event(&Event {
                method: events::MODEL_FINISHED.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.clone(),
                    "full_text": wf_result.assistant_text.clone(),
                })),
            });

            // An interactive turn that applied code edits swept the transient
            // auto context layer away inside the FSM, so the stable prompt
            // prefix those files warmed no longer exists. The dialogue starts a
            // fresh provider session instead of being routed to a node holding
            // a stale prefix cache. An isolated step owns no dialogue session,
            // so it never rotates the interactive one.
            if intent.is_interactive_repl()
                && matches!(
                    &wf_result.result,
                    tauqe_protocol::ModelResult::Edit { outcome, .. } if outcome.applied
                )
            {
                state.reset_session_uuid().await;
            }

            let has_terminal_success =
                tauqe_core::edits::protocol::xml::extract_success(&raw_streamed_text).is_some()
                    || tauqe_core::edits::protocol::xml::extract_success(&wf_result.assistant_text)
                        .is_some();

            let turn_error = match &wf_result.result {
                tauqe_protocol::ModelResult::Edit { outcome, .. } if !outcome.applied => {
                    outcome.error.clone()
                }
                _ => None,
            };

            (
                ModelTurnResult {
                    operation_id: op_id,
                    result: Some(wf_result.result),
                    assistant_text: wf_result.assistant_text,
                    is_cancelled: false,
                    error: turn_error,
                    has_terminal_success,
                    blocked: wf_result.blocked,
                    abort: wf_result.abort,
                },
                operation_totals,
            )
        }
        Ok(Err(err)) => {
            let snapshot = flush_usage(state, &mut usage_guard).await;
            emit_legacy_usage_event(state, &op_id).await;
            if edit_events_sent && !edit_finished_sent {
                send_edit_aborted(&state.out, &op_id, format!("Workflow failed: {}", err));
            }
            state.out.send_event(&Event {
                method: events::MODEL_ERROR.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.clone(),
                    "message": err.to_string(),
                })),
            });

            (
                ModelTurnResult {
                    operation_id: op_id,
                    result: None,
                    assistant_text: raw_streamed_text,
                    is_cancelled: false,
                    error: Some(err.to_string()),
                    has_terminal_success: false,
                    blocked: None,
                    abort: None,
                },
                snapshot.prev,
            )
        }
        Err(join_err) => {
            let snapshot = flush_usage(state, &mut usage_guard).await;
            emit_legacy_usage_event(state, &op_id).await;
            if edit_events_sent && !edit_finished_sent {
                send_edit_aborted(
                    &state.out,
                    &op_id,
                    format!("Workflow task failed: {}", join_err),
                );
            }
            state.out.send_event(&Event {
                method: events::MODEL_ERROR.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.clone(),
                    "message": join_err.to_string(),
                })),
            });

            (
                ModelTurnResult {
                    operation_id: op_id,
                    result: None,
                    assistant_text: raw_streamed_text,
                    is_cancelled: false,
                    error: Some(join_err.to_string()),
                    has_terminal_success: false,
                    blocked: None,
                    abort: None,
                },
                snapshot.prev,
            )
        }
    };

    log_turn_finished(
        &turn_result.operation_id,
        &model,
        is_batch,
        turn_status(&turn_result),
        &final_usage,
        turn_result.error.as_deref(),
    );

    Ok(turn_result)
}

/// Initiates an asynchronous model turn in the background and immediately returns its operation ID.
///
/// Designed as a lightweight non-blocking wrapper for interactive single-prompt RPC requests.
pub async fn start_model_turn(
    state: &Arc<AppState>,
    prompt: String,
    intent: TurnIntent,
    custom_selection: Option<tauqe_protocol::ModelSelection>,
    plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
) -> Result<String, ResponseError> {
    let marker = state.next_turn_marker().await;
    let prepared = prepare_turn(
        state,
        intent,
        custom_selection,
        plan_context,
        None,
        false,
        marker,
        None,
        None,
    )
    .await?;
    let op_id = prepared.op_id.clone();
    let state_clone = Arc::clone(state);
    let workflow = prepared.app_config.develop.workflow;
    tokio::spawn(async move {
        let policy = build_apply_policy(&state_clone, workflow);
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
/// extends; without one an isolated step stays stateless.
pub struct StepTurnOptions {
    pub intent: TurnIntent,
    pub custom_selection: Option<tauqe_protocol::ModelSelection>,
    pub plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub marker: Option<String>,
    pub session_id: Option<String>,
    pub shared_operation: Option<Arc<OperationGuard>>,
}

/// Executes an isolated model turn to completion and awaits its outcome.
///
/// Returns the full `ModelTurnResult` representing the atomic turn lifecycle,
/// enabling server-side step orchestration to inspect step success
/// deterministically. Turns started here always execute inside a batch
/// supervisor, so they are marked as batch members to let clients suppress
/// per-step signals. A failure carries the identity of the operation the turn
/// belonged to, so the caller reports it under a registered operation instead of
/// an invented one.
pub async fn execute_model_turn(
    state: &Arc<AppState>,
    prompt: String,
    policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
    options: StepTurnOptions,
) -> Result<ModelTurnResult, TurnError> {
    let StepTurnOptions {
        intent,
        custom_selection,
        plan_context,
        reasoning_effort,
        marker,
        session_id,
        shared_operation,
    } = options;
    // A batch step borrows the batch-level registration, so the operation
    // identity exists before the turn can fail for any reason, including a
    // failure to construct the provider.
    let operation_id = shared_operation
        .as_ref()
        .map(|operation| format!("op-{}", operation.id()));
    let prepared = prepare_turn(
        state,
        intent,
        custom_selection,
        plan_context,
        reasoning_effort,
        true,
        marker,
        session_id,
        shared_operation,
    )
    .await
    .map_err(|error| TurnError {
        operation_id,
        error: Box::new(error),
    })?;
    run_prepared_turn(state, prompt, prepared, policy)
        .await
        .map_err(TurnError::from)
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
) {
    if matches!(status, "cancelled" | "error" | "edit_failed") {
        tracing::warn!(
            operation_id = %op_id,
            model = %model,
            is_batch,
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
    state.usage.lock().await.clone()
}

/// Mirrors the authoritative totals onto the legacy `model/usage` channel.
///
/// The aggregate snapshot travels through `model/usageTotals`; this event keeps
/// clients that predate it reading the same numbers, including the in-flight
/// operation while it is still running.
pub(crate) async fn emit_legacy_usage_event(state: &Arc<AppState>, op_id: &str) {
    let snapshot = state.usage.lock().await.clone();
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

pub(super) fn should_report_aborted_edits(
    edit_events_sent: bool,
    edit_finished_sent: bool,
    result: &tauqe_protocol::ModelResult,
) -> bool {
    if !edit_events_sent || edit_finished_sent {
        return false;
    }
    !matches!(result, tauqe_protocol::ModelResult::Answer { .. })
}

fn send_edit_aborted(out: &OutChannel, op_id: &str, error: String) {
    out.send_event(&Event {
        method: events::EDIT_FINISHED.to_string(),
        params: serde_json::to_value(EditFinishedEvent {
            operation_id: op_id.to_string(),
            outcome: EditOutcome {
                applied: false,
                error: Some(error),
                changed_files: Vec::new(),
                commit_hash: None,
            },
        })
        .ok(),
    });
}
