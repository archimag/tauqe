use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauqe_core::prompt::json::JsonPromptProtocol;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::prompt::xml::XmlPromptProtocol;
use tauqe_core::providers::{create_provider, LlmProvider, StreamEvent};
use tauqe_core::workflow::fsm::is_discussion_prompt;
use tauqe_core::workflow::{ExecutionMode, SingleExecution};
use tauqe_protocol::{
    events, DiscussionClearFocusResult, DiscussionFocus, DiscussionGetFocusResult,
    DiscussionRequest, DiscussionSetFocusParams, DiscussionSetFocusResult, DiscussionTarget,
    EditFileDoneEvent, EditFileRetryingEvent, EditFileStartedEvent, EditFinishedEvent,
    EditHunkEvent, EditOutcome, EditStartedEvent, Event, GitCommitCreatedEvent, ModelAskParams,
    ModelResultEvent, ModelStartedEvent, ModelUsageInfo, ReasoningEffort, Request, Response,
    ResponseError,
};
use tauqe_protocol::model::{CancelMode, ModelCancelParams};

use crate::handlers::offload;
use crate::state::{next_operation_id, AppState, OperationGuard};
use crate::transport::OutChannel;

pub async fn handle_model_ask(req: Request, state: &Arc<AppState>) -> Response {
    let params: ModelAskParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: "INVALID_PARAMS".to_string(),
                    message: "Missing or invalid prompt".to_string(),
                    data: None,
                }),
            };
        }
    };

    let prompt = match build_model_ask_prompt(state, params).await {
        Ok(prompt) => prompt,
        Err(err) => {
            return Response {
                id: req.id,
                result: None,
                error: Some(err),
            };
        }
    };

    match start_model_turn(state, prompt, None, None).await {
        Ok(op_id) => Response {
            id: req.id,
            result: Some(serde_json::json!({ "operation_id": op_id })),
            error: None,
        },
        Err(err) => Response {
            id: req.id,
            result: None,
            error: Some(err),
        },
    }
}

/// Resolves the turn prompt for a model ask request.
///
/// A structured discussion turn carries its target explicitly, so the server
/// generates the discussion prompt from those typed fields and replaces the
/// focus anchor. A plain ask continues the anchored discussion instead: while a
/// focus is active, the current state of the focused plan or review session is
/// injected into the prompt on every turn. Neither path inspects the prompt text
/// to decide the mode, so text that merely resembles a discussion heading can
/// never enter discussion mode.
async fn build_model_ask_prompt(
    state: &Arc<AppState>,
    params: ModelAskParams,
) -> Result<String, ResponseError> {
    match params {
        ModelAskParams::Discussion(discussion) => {
            let prompt = build_discussion_prompt(
                state,
                &discussion.target,
                &discussion.focused_items,
                &discussion.comment,
            )
            .await?;
            // An explicit discussion target immediately replaces the focus
            // anchor without resetting the REPL history.
            state
                .set_discussion_focus(focus_from_target(&discussion))
                .await;
            Ok(prompt)
        }
        ModelAskParams::Prompt { text } => {
            if text.trim().is_empty() {
                return Err(ResponseError {
                    code: tauqe_protocol::errors::INVALID_PARAMS.to_string(),
                    message: "Model ask requires a non-empty prompt or a discussion target"
                        .to_string(),
                    data: None,
                });
            }
            match focus_to_target(&state.discussion_focus().await) {
                Some((target, focused_items)) => {
                    match build_discussion_prompt(state, &target, &focused_items, &text).await {
                        Ok(prompt) => Ok(prompt),
                        Err(err) => {
                            // The focused entity is gone (for example the plan
                            // was deleted elsewhere): drop the stale anchor and
                            // answer the raw prompt instead of failing the turn.
                            tracing::warn!(
                                "Discussion focus target could not be loaded ({}); clearing focus",
                                err.message
                            );
                            state.clear_discussion_focus().await;
                            Ok(text)
                        }
                    }
                }
                None => Ok(text),
            }
        }
    }
}

/// Maps an explicit discussion target onto the focus anchor it establishes.
fn focus_from_target(discussion: &DiscussionRequest) -> DiscussionFocus {
    DiscussionFocus::from_target(discussion.target.clone(), discussion.focused_items.clone())
}

/// Maps an active focus anchor onto the discussion target it addresses.
///
/// Returns `None` for the free (unfocused) mode, so a plain prompt stays as-is.
fn focus_to_target(focus: &DiscussionFocus) -> Option<(DiscussionTarget, Vec<String>)> {
    focus
        .target()
        .map(|target| (target.clone(), focus.focused_items().to_vec()))
}

/// Builds the discussion prompt for a typed target, loading the referenced plan
/// or review session from storage.
async fn build_discussion_prompt(
    state: &Arc<AppState>,
    target: &DiscussionTarget,
    focused_items: &[String],
    comment: &str,
) -> Result<String, ResponseError> {
    let repo_root = state.repo_root().await;
    match target {
        DiscussionTarget::Plan { id } => {
            let loaded = {
                let repo_root = repo_root.clone();
                let id = id.clone();
                offload(move || tauqe_core::plan::storage::PlanStorage::load_plan(&repo_root, &id))
                    .await
                    .map_err(internal_error)?
            };
            let plan = match loaded {
                Ok(Some(plan)) => plan,
                Ok(None) => return Err(not_found(format!("Plan '{}' not found", id))),
                Err(err) => return Err(internal_error(format!("{:#}", err))),
            };
            Ok(tauqe_core::plan::prompt::format_plan_discussion_prompt(
                &plan,
                focused_items,
                comment,
            ))
        }
        DiscussionTarget::Review { id } => {
            let loaded = {
                let repo_root = repo_root.clone();
                let id = id.clone();
                offload(move || {
                    tauqe_core::review::storage::ReviewStorage::load_review(&repo_root, &id)
                })
                .await
                .map_err(internal_error)?
            };
            let session = match loaded {
                Ok(Some(session)) => session,
                Ok(None) => return Err(not_found(format!("Review session '{}' not found", id))),
                Err(err) => return Err(internal_error(format!("{:#}", err))),
            };
            let focused: Vec<u32> = focused_items
                .iter()
                .filter_map(|raw| raw.trim().parse::<u32>().ok())
                .collect();
            Ok(tauqe_core::review::prompt::format_review_discussion_prompt_items(
                &session, &focused, comment,
            ))
        }
    }
}

fn not_found(message: String) -> ResponseError {
    ResponseError {
        code: tauqe_protocol::errors::NOT_FOUND.to_string(),
        message,
        data: None,
    }
}

fn internal_error(message: String) -> ResponseError {
    ResponseError {
        code: tauqe_protocol::errors::INTERNAL_ERROR.to_string(),
        message,
        data: None,
    }
}

pub async fn handle_model_select(req: Request, state: &Arc<AppState>) -> Response {
    if state.is_operation_active() {
        return Response {
            id: req.id,
            result: None,
            error: Some(ResponseError {
                code: "OPERATION_IN_PROGRESS".to_string(),
                message: "Cannot change model while an operation is in progress".to_string(),
                data: None,
            }),
        };
    }

    let params: tauqe_protocol::ModelSelectParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: "INVALID_PARAMS".to_string(),
                    message: "Missing or invalid model selection".to_string(),
                    data: None,
                }),
            };
        }
    };

    let (selection, effective_model, roles) = {
        let mut guard = state.config.lock().await;
        // Restrict the selection to the configured model pools: a role resolves
        // to its base model and an explicit model must already be configured,
        // so a client can only activate a model it was offered.
        let effective = guard.config.resolve_model(&params.selection);
        let available = guard.config.available_models();
        if !available.contains(&effective) {
            return Response {
                id: req.id,
                result: None,
                error: Some(ResponseError {
                    code: "INVALID_MODEL".to_string(),
                    message: format!(
                        "Unknown model '{}'. Available models: {}",
                        effective,
                        available
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    data: None,
                }),
            };
        }
        guard.selection = params.selection.clone();
        let roles = crate::handlers::config::effective_roles_summary(&guard.config);
        (guard.selection.clone(), effective, roles)
    };

    state.out.send_event(&Event {
        method: events::MODEL_CHANGED.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelChangedEvent {
            selection: selection.clone(),
            effective_model: effective_model.clone(),
            roles: Some(roles.clone()),
        })
        .ok(),
    });

    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelSelectResult {
            selection,
            effective_model,
            roles: Some(roles),
        },
    )
}

pub async fn handle_model_current(req: Request, state: &Arc<AppState>) -> Response {
    let guard = state.config.lock().await;
    let selection = guard.selection.clone();
    let effective_model = guard.config.resolve_model(&selection);
    let available_models = guard.config.available_models();
    let roles = crate::handlers::config::effective_roles_summary(&guard.config);

    Response::ok_typed(
        req.id,
        &tauqe_protocol::ModelCurrentResult {
            selection,
            effective_model,
            roles: Some(roles),
            available_models,
        },
    )
}

/// Sets the active discussion focus anchor and broadcasts the change.
pub async fn handle_discussion_set_focus(req: Request, state: &Arc<AppState>) -> Response {
    let params: DiscussionSetFocusParams = match req.params.and_then(|p| serde_json::from_value(p).ok())
    {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                tauqe_protocol::errors::INVALID_PARAMS,
                "Missing or invalid discussion focus",
            );
        }
    };
    let focus = state.set_discussion_focus(params.focus).await;
    Response::ok_typed(req.id, &DiscussionSetFocusResult { focus })
}

/// Returns the active discussion focus anchor.
pub async fn handle_discussion_get_focus(req: Request, state: &Arc<AppState>) -> Response {
    let focus = state.discussion_focus().await;
    Response::ok_typed(req.id, &DiscussionGetFocusResult { focus })
}

/// Clears the active discussion focus anchor, returning the resulting focus.
pub async fn handle_discussion_clear_focus(req: Request, state: &Arc<AppState>) -> Response {
    let focus = state.clear_discussion_focus().await;
    Response::ok_typed(req.id, &DiscussionClearFocusResult { focus })
}

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
}

impl ModelTurnResult {
    /// Evaluates if the step executed successfully according to atomic completion invariants.
    ///
    /// A step is considered successful strictly when:
    /// 1. The turn was not cancelled and encountered no unhandled errors.
    /// 2. Code edits were verified and applied (`Edit` with `applied: true`).
    /// 3. Or an explicit terminal success tag was emitted (`<success ... />`).
    pub fn is_success(&self) -> bool {
        if self.is_cancelled || self.error.is_some() {
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

struct PreparedTurn {
    provider: Arc<dyn LlmProvider>,
    model: tauqe_protocol::ModelRef,
    app_config: tauqe_core::config::AppConfig,
    selection: tauqe_protocol::ModelSelection,
    reasoning_effort: Option<ReasoningEffort>,
    op_id: String,
    /// Whether the turn runs as a member of a batch supervisor. Recorded so the
    /// turn's terminal log can distinguish batch steps from interactive turns.
    is_batch: bool,
    /// Operation backing this turn. A turn that starts on its own owns the
    /// registration; a batch step shares the batch-level registration so the
    /// slot never lapses between steps.
    operation: Arc<OperationGuard>,
    /// Active turn marker binding structural protocol tags to this turn.
    marker: Option<String>,
}

/// Builds a real network provider from configuration, mapping failures to a
/// `NO_API_KEY` response error.
fn build_configured_provider(
    config: &tauqe_core::config::AppConfig,
    kind: tauqe_protocol::ProviderKind,
) -> Result<Arc<dyn LlmProvider>, ResponseError> {
    create_provider(kind, config)
        .map(Arc::from)
        .map_err(|err| ResponseError {
            code: "NO_API_KEY".to_string(),
            message: err.to_string(),
            data: None,
        })
}

/// Grace period granted to a workflow task to unwind cooperatively after a
/// cancellation or stream error before a supervisor force-aborts it.
pub(crate) const CANCEL_UNWIND_GRACE: std::time::Duration =
    std::time::Duration::from_secs(30);

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
        tauqe_protocol::WorkflowKind::Git => {
            Box::new(tauqe_core::workflow::git_policy::GitTransactionPolicy::with_guard(
                Arc::clone(&state.git_transaction_active),
            ))
        }
    }
}

/// Resolves the provider and model for a turn, emits `model/started`, and
/// registers the cancellable active operation. `is_batch` marks turns that run
/// as members of a batch supervisor so clients can suppress per-step signals.
async fn prepare_turn(
    state: &Arc<AppState>,
    custom_selection: Option<tauqe_protocol::ModelSelection>,
    plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    reasoning_effort: Option<ReasoningEffort>,
    is_batch: bool,
    marker: Option<String>,
    shared_operation: Option<Arc<OperationGuard>>,
) -> Result<PreparedTurn, ResponseError> {
    // Acquire (or borrow) the active operation first: registration is atomic,
    // so a concurrent turn fails with BUSY instead of silently clobbering the
    // running operation's cancel handle.
    let operation = match shared_operation {
        Some(operation) => operation,
        None => Arc::new(state.try_begin_operation().map_err(|()| ResponseError {
            code: "OPERATION_IN_PROGRESS".to_string(),
            message: "Another operation is already running".to_string(),
            data: None,
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

    let role = match &selection {
        tauqe_protocol::ModelSelection::Role(r) => Some(*r),
        tauqe_protocol::ModelSelection::Specific(_) => None,
    };
    // Report the running model as structured data: the configured alias (when
    // present) and the effective reasoning effort, so the client can render the
    // active role and model without re-deriving them from configuration.
    let entry = resolve_model_entry(&app_config, &selection);
    let alias = entry.as_ref().and_then(|entry| entry.alias.clone());
    let effort = reasoning_effort.or_else(|| entry.as_ref().and_then(|entry| entry.effort));

    tracing::info!(
        operation_id = %op_id,
        model = %model,
        role = ?role,
        alias = alias.as_deref().unwrap_or("none"),
        effort = ?effort,
        is_batch,
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
        is_batch,
        operation,
        marker,
    })
}

/// Resolves the prompt protocol for a turn.
///
/// Discussion turns (plan or review refinement) are always routed through the
/// structured discussion protocol, regardless of the configured
/// `develop.protocol`. The discussion system prompt instructs the model to
/// answer with the discussion JSON schema, so pairing it with an XML stream
/// filter would leak the raw JSON scaffolding into the REPL. Non-discussion
/// turns keep using the configured base protocol. The active turn marker is
/// bound onto the chosen protocol when its wire format supports it; the
/// structured discussion protocol encodes its structure in JSON Schema and
/// therefore declines the marker.
fn resolve_turn_protocol(
    prompt: &str,
    develop_protocol: tauqe_protocol::EditProtocolKind,
    marker: Option<&str>,
) -> Box<dyn PromptProtocol> {
    let base_protocol: Box<dyn PromptProtocol> = if is_discussion_prompt(prompt) {
        Box::new(JsonPromptProtocol::discussion())
    } else {
        match develop_protocol {
            tauqe_protocol::EditProtocolKind::Structured => {
                Box::new(JsonPromptProtocol::default())
            }
            tauqe_protocol::EditProtocolKind::Xml => Box::new(XmlPromptProtocol),
        }
    };
    match marker {
        Some(marker) => base_protocol
            .with_turn_marker(marker)
            .unwrap_or(base_protocol),
        None => base_protocol,
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
        is_batch,
        operation,
        marker,
    } = prepared;

    let cancel_rx = operation.cancel_receiver();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);

    let protocol =
        resolve_turn_protocol(&prompt, app_config.develop.protocol, marker.as_deref());

    let mut workflow_opts = tauqe_core::workflow::WorkflowOptions::from_app_config(&app_config);
    workflow_opts.reasoning_effort = reasoning_effort.or_else(|| match &selection {
        tauqe_protocol::ModelSelection::Role(role) => app_config.models.resolve_effort(*role),
        tauqe_protocol::ModelSelection::Specific(_) => None,
    });
    workflow_opts.initial_selection = Some(selection);
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
                provider.as_ref(),
                &model_clone,
                &mut local_context,
                &mut history,
                protocol.as_ref(),
                wf_tx,
                wf_rx,
            )
            .await;
        state_clone.context.write().await.merge_turn_context(&local_context);
        res
    };

    let wf_task = tokio::spawn(workflow_future);
    operation.set_abort_handle(wf_task.abort_handle());

    let mut raw_streamed_text = String::new();
    let mut usage_info = None;
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
                accumulate_usage(&mut usage_info, usage.clone());
                let op_accumulated_cost = usage_info.as_ref().and_then(|u| u.cost);
                let current_session_cost = {
                    let base_cost = *state.total_cost.lock().await;
                    base_cost + op_accumulated_cost.unwrap_or(0.0)
                };
                state.out.send_event(&Event {
                    method: events::MODEL_USAGE.to_string(),
                    params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                        operation_id: op_id.clone(),
                        usage,
                        session_total_cost: current_session_cost,
                        current_cost: op_accumulated_cost,
                    })
                    .ok(),
                });
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
        // the cancel signal and unwinds its own transaction. Force-aborting is a
        // last resort with a generous grace period, and it is skipped while a
        // git transaction is still open, because an abort lands at the next
        // `.await` and could otherwise strand a half-finished checkpoint or step
        // commit until startup crash recovery runs.
        let abort_handle = wf_task.abort_handle();
        let stalled = tokio::time::timeout(CANCEL_UNWIND_GRACE, wf_task)
            .await
            .is_err();
        if stalled {
            if state.git_transaction_active.load(Ordering::SeqCst) {
                tracing::warn!(
                    operation_id = %op_id,
                    grace_secs = CANCEL_UNWIND_GRACE.as_secs(),
                    "Turn did not unwind within the grace period; deferring abort while a git transaction is open",
                );
            } else {
                tracing::warn!(
                    operation_id = %op_id,
                    grace_secs = CANCEL_UNWIND_GRACE.as_secs(),
                    "Turn did not unwind within the grace period; aborting workflow task",
                );
                abort_handle.abort();
            }
        }

        // Flush usage accumulated before the turn was cancelled or failed:
        // otherwise a turn that consumed tokens but never completed would
        // silently under-report the session cost.
        let usage_to_send = usage_info.unwrap_or_default();
        let cost = usage_to_send.cost.unwrap_or(0.0);
        let total_cost = {
            let mut total = state.total_cost.lock().await;
            *total += cost;
            *total
        };
        state.out.send_event(&Event {
            method: events::MODEL_USAGE.to_string(),
            params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                operation_id: op_id.clone(),
                usage: usage_to_send.clone(),
                session_total_cost: total_cost,
                current_cost: Some(cost),
            })
            .ok(),
        });

        let error = if is_cancelled {
            Some("Operation cancelled".to_string())
        } else {
            stream_error_msg
        };

        let status = if is_cancelled { "cancelled" } else { "error" };
        log_turn_finished(
            &op_id,
            &model,
            is_batch,
            status,
            &usage_to_send,
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
        });
    }

    // Aggregated usage of the completed turn, captured for the terminal log.
    let mut final_usage = ModelUsageInfo::default();
    let turn_result = match wf_task.await {
        Ok(Ok(wf_result)) => {
            let usage_to_send = usage_info.unwrap_or_default();
            final_usage = usage_to_send.clone();
            let cost = usage_to_send.cost.unwrap_or(0.0);
            let total_cost = {
                let mut total = state.total_cost.lock().await;
                *total += cost;
                *total
            };

            state.out.send_event(&Event {
                method: events::MODEL_USAGE.to_string(),
                params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
                    operation_id: op_id.clone(),
                    usage: usage_to_send.clone(),
                    session_total_cost: total_cost,
                    current_cost: Some(cost),
                })
                .ok(),
            });

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

            if should_report_aborted_edits(edit_events_sent, edit_finished_sent, &wf_result.result) {
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
                    usage: Some(usage_to_send),
                    session_total_cost: Some(total_cost),
                    current_cost: Some(cost),
                })),
            });

            state.out.send_event(&Event {
                method: events::MODEL_FINISHED.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.clone(),
                    "full_text": wf_result.assistant_text.clone(),
                })),
            });

            let has_terminal_success =
                tauqe_core::edits::protocol::xml::extract_success(&raw_streamed_text)
                    .is_some()
                    || tauqe_core::edits::protocol::xml::extract_success(
                        &wf_result.assistant_text,
                    )
                    .is_some();

            let turn_error = match &wf_result.result {
                tauqe_protocol::ModelResult::Edit { outcome, .. } if !outcome.applied => {
                    outcome.error.clone()
                }
                _ => None,
            };

            ModelTurnResult {
                operation_id: op_id,
                result: Some(wf_result.result),
                assistant_text: wf_result.assistant_text,
                is_cancelled: false,
                error: turn_error,
                has_terminal_success,
                blocked: wf_result.blocked,
            }
        }
        Ok(Err(err)) => {
            if edit_events_sent && !edit_finished_sent {
                send_edit_aborted(
                    &state.out,
                    &op_id,
                    format!("Workflow failed: {}", err),
                );
            }
            state.out.send_event(&Event {
                method: events::MODEL_ERROR.to_string(),
                params: Some(serde_json::json!({
                    "operation_id": op_id.clone(),
                    "message": err.to_string(),
                })),
            });

            ModelTurnResult {
                operation_id: op_id,
                result: None,
                assistant_text: raw_streamed_text,
                is_cancelled: false,
                error: Some(err.to_string()),
                has_terminal_success: false,
                blocked: None,
            }
        }
        Err(join_err) => {
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

            ModelTurnResult {
                operation_id: op_id,
                result: None,
                assistant_text: raw_streamed_text,
                is_cancelled: false,
                error: Some(join_err.to_string()),
                has_terminal_success: false,
                blocked: None,
            }
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
    custom_selection: Option<tauqe_protocol::ModelSelection>,
    plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
) -> Result<String, ResponseError> {
    let marker = state.next_turn_marker().await;
    let prepared =
        prepare_turn(state, custom_selection, plan_context, None, false, marker, None).await?;
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
/// new one, so the batch slot never lapses between steps.
pub struct StepTurnOptions {
    pub custom_selection: Option<tauqe_protocol::ModelSelection>,
    pub plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub marker: Option<String>,
    pub shared_operation: Option<Arc<OperationGuard>>,
}

/// Executes an isolated model turn to completion and awaits its outcome.
///
/// Returns the full `ModelTurnResult` representing the atomic turn lifecycle,
/// enabling server-side step orchestration to inspect step success
/// deterministically. Turns started here always execute inside a batch
/// supervisor, so they are marked as batch members to let clients suppress
/// per-step signals.
pub async fn execute_model_turn(
    state: &Arc<AppState>,
    prompt: String,
    policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
    options: StepTurnOptions,
) -> Result<ModelTurnResult, ResponseError> {
    let StepTurnOptions {
        custom_selection,
        plan_context,
        reasoning_effort,
        marker,
        shared_operation,
    } = options;
    let prepared = prepare_turn(
        state,
        custom_selection,
        plan_context,
        reasoning_effort,
        true,
        marker,
        shared_operation,
    )
    .await?;
    run_prepared_turn(state, prompt, prepared, policy).await
}

pub async fn handle_model_cancel(req: Request, state: &Arc<AppState>) -> Response {
    // The mode is optional so clients that predate two-mode cancellation keep
    // sending an empty object and receive the default immediate abort.
    let params: ModelCancelParams = req
        .params
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();

    // A soft stop is advisory: the batch supervisor finishes the current member
    // (its turn, compiler verification, and Git commit) and then stops before
    // launching the next one. The operation stays registered so its cancel and
    // soft-stop channels remain live until the batch unwinds on its own.
    if params.mode == Some(CancelMode::Soft) {
        state.soft_stop_active_operation();
        return Response {
            id: req.id,
            result: Some(serde_json::json!({ "cancelled": true })),
            error: None,
        };
    }

    // A hard stop signals cancellation but keeps the operation slot registered:
    // the cancelled workflow keeps mutating state (rolling back its git
    // transaction, releasing the session lock) until it observes the cancel
    // signal and drops its guard. Clearing the slot immediately would let a
    // client start `git/undo`, `config/set`, or a new turn against a half-unwound
    // turn. A watchdog force-aborts a stalled task after the grace period.
    state.hard_cancel_active_operation();
    Response {
        id: req.id,
        result: Some(serde_json::json!({ "cancelled": true })),
        error: None,
    }
}

pub async fn handle_model_clear_history(req: Request, state: &Arc<AppState>) -> Response {
    let mut history = state.history.lock().await;
    let _ = history.clear();
    drop(history);
    // Clearing the dialogue session also detaches the discussion focus so the
    // REPL returns to the free, unfocused mode.
    state.clear_discussion_focus().await;
    Response {
        id: req.id,
        result: Some(serde_json::json!({ "cleared": true })),
        error: None,
    }
}

/// Resolves the configured role entry backing a turn's model selection.
///
/// A role selection maps to the base entry of that role's list. An explicit
/// selection (such as a coder escalation rung) carries no role of its own, so
/// its entry is recovered by matching the resolved model against the configured
/// role lists, which is where escalation rungs come from.
fn resolve_model_entry(
    config: &tauqe_core::config::AppConfig,
    selection: &tauqe_protocol::ModelSelection,
) -> Option<tauqe_protocol::ModelEntry> {
    match selection {
        tauqe_protocol::ModelSelection::Role(role) => {
            config.models.role_entries(*role).into_iter().next()
        }
        tauqe_protocol::ModelSelection::Specific(model) => [
            tauqe_protocol::ModelRole::Primary,
            tauqe_protocol::ModelRole::Coder,
            tauqe_protocol::ModelRole::Architect,
            tauqe_protocol::ModelRole::Reviewer,
            tauqe_protocol::ModelRole::Scribe,
        ]
        .into_iter()
        .flat_map(|role| config.models.role_entries(role))
        .find(|entry| &entry.model == model),
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

/// Records the terminal outcome of a model turn with its aggregated usage.
///
/// Only metadata is logged — identifiers, the final status, and aggregated
/// token and cost counters — never streamed deltas, hunks, or prompt bodies,
/// upholding the Anti-Bloat logging invariant.
fn log_turn_finished(
    op_id: &str,
    model: &tauqe_protocol::ModelRef,
    is_batch: bool,
    status: &'static str,
    usage: &ModelUsageInfo,
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
            cost = usage.cost.unwrap_or(0.0),
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
            cost = usage.cost.unwrap_or(0.0),
            "Model turn finished",
        );
    }
}

fn accumulate_usage(accum: &mut Option<ModelUsageInfo>, new_usage: ModelUsageInfo) {
    if let Some(existing) = accum.as_mut() {
        // Token counters come from untrusted provider usage reports and add up
        // across a multi-round turn. Saturating addition keeps a malformed or
        // runaway report from panicking in debug builds or wrapping in release.
        existing.prompt_tokens = existing.prompt_tokens.saturating_add(new_usage.prompt_tokens);
        existing.completion_tokens = existing
            .completion_tokens
            .saturating_add(new_usage.completion_tokens);
        existing.total_tokens = existing.total_tokens.saturating_add(new_usage.total_tokens);

        if let Some(r) = new_usage.reasoning_tokens {
            let slot = existing.reasoning_tokens.get_or_insert(0);
            *slot = slot.saturating_add(r);
        }
        if let Some(c) = new_usage.cached_tokens {
            let slot = existing.cached_tokens.get_or_insert(0);
            *slot = slot.saturating_add(c);
        }
        if let Some(c) = new_usage.cost {
            *existing.cost.get_or_insert(0.0) += c;
        }
    } else {
        *accum = Some(new_usage);
    }
}

fn should_report_aborted_edits(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_turn_result_success_evaluation() {
        // 1. Successful edit with applied: true
        let ok_edit = ModelTurnResult {
            operation_id: "op-1".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "Fixed bug".to_string(),
                edits: vec![],
                proposal: None,
                outcome: tauqe_protocol::EditOutcome {
                    applied: true,
                    error: None,
                    changed_files: vec!["src/lib.rs".to_string()],
                    commit_hash: Some("abc".to_string()),
                },
            }),
            assistant_text: "Done.".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: false,
            blocked: None,
        };
        assert!(ok_edit.is_success());

        // 2. Failed edit with applied: false
        let failed_edit = ModelTurnResult {
            operation_id: "op-2".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "Failed".to_string(),
                edits: vec![],
                proposal: None,
                outcome: tauqe_protocol::EditOutcome {
                    applied: false,
                    error: Some("compiler error".to_string()),
                    changed_files: vec![],
                    commit_hash: None,
                },
            }),
            assistant_text: "Failed.".to_string(),
            is_cancelled: false,
            error: Some("compiler error".to_string()),
            has_terminal_success: false,
            blocked: None,
        };
        assert!(!failed_edit.is_success());

        // 3. Conversational answer without terminal success tag -> false (c-2 invariant)
        let conversational_answer = ModelTurnResult {
            operation_id: "op-3".to_string(),
            result: Some(tauqe_protocol::ModelResult::Answer {
                text: "I think you should change file X.".to_string(),
            }),
            assistant_text: "I think you should change file X.".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: false,
            blocked: None,
        };
        assert!(!conversational_answer.is_success());

        // 4. Conversational answer WITH terminal success tag -> true
        let verified_answer = ModelTurnResult {
            operation_id: "op-4".to_string(),
            result: Some(tauqe_protocol::ModelResult::Answer {
                text: "Inspection verified cleanly.".to_string(),
            }),
            assistant_text: "Inspection verified cleanly.".to_string(),
            is_cancelled: false,
            error: None,
            has_terminal_success: true,
            blocked: None,
        };
        assert!(verified_answer.is_success());

        // 5. Cancelled turn -> false
        let cancelled_turn = ModelTurnResult {
            operation_id: "op-5".to_string(),
            result: None,
            assistant_text: "".to_string(),
            is_cancelled: true,
            error: Some("Operation cancelled".to_string()),
            has_terminal_success: true,
            blocked: None,
        };
        assert!(!cancelled_turn.is_success());

        // 6. A semantically blocked turn is never a success.
        let blocked_turn = ModelTurnResult {
            operation_id: "op-6".to_string(),
            result: Some(tauqe_protocol::ModelResult::Edit {
                summary: "Execution blocked".to_string(),
                edits: vec![],
                proposal: None,
                outcome: tauqe_protocol::EditOutcome {
                    applied: false,
                    error: Some("precondition missing".to_string()),
                    changed_files: vec![],
                    commit_hash: None,
                },
            }),
            assistant_text: "Blocked.".to_string(),
            is_cancelled: false,
            error: Some("precondition missing".to_string()),
            has_terminal_success: false,
            blocked: Some("precondition missing".to_string()),
        };
        assert!(!blocked_turn.is_success());
    }

    #[test]
    fn test_should_report_aborted_edits() {
        let answer = tauqe_protocol::ModelResult::Answer { text: "text".to_string() };
        let edit = tauqe_protocol::ModelResult::Edit {
            summary: "sum".to_string(),
            edits: vec![],
            proposal: None,
            outcome: tauqe_protocol::EditOutcome {
                applied: false,
                error: None,
                changed_files: vec![],
                commit_hash: None,
            },
        };
        
        // Never report if no edit events were sent
        assert!(!should_report_aborted_edits(false, false, &answer));
        assert!(!should_report_aborted_edits(false, false, &edit));
        
        // Never report if already sent
        assert!(!should_report_aborted_edits(true, true, &answer));
        assert!(!should_report_aborted_edits(true, true, &edit));
        
        // If edit events were sent but not finished, report ONLY if it's not an Answer
        assert!(!should_report_aborted_edits(true, false, &answer));
        assert!(should_report_aborted_edits(true, false, &edit));
    }

    #[tokio::test]
    async fn test_handle_model_current_and_select_roles_summary() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = tauqe_core::config::AppConfig::default();
        config.models.scribe = Some(tauqe_protocol::ModelRef::openrouter("test/scribe"));
        config.models.coder =
            Some(vec![tauqe_protocol::ModelRef::openrouter("test/coder").into()]);
        config.models.architect =
            Some(vec![tauqe_protocol::ModelRef::openrouter("test/architect").into()]);
        config.models.primary = vec![tauqe_protocol::ModelRef::openrouter("test/primary").into()];
        config.models.reviewer =
            Some(vec![tauqe_protocol::ModelRef::openrouter("test/reviewer").into()]);

        let state = AppState::new(
            config,
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        // 1. Initial selection defaults to Primary
        let req_current = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::MODEL_CURRENT.to_string(),
            params: None,
        };
        let res_current = handle_model_current(req_current, &state).await;
        assert!(res_current.error.is_none());
        let current_val: tauqe_protocol::ModelCurrentResult =
            serde_json::from_value(res_current.result.unwrap()).unwrap();
        assert_eq!(
            current_val.selection,
            tauqe_protocol::ModelSelection::Role(tauqe_protocol::ModelRole::Primary)
        );
        let roles = current_val.roles.unwrap();
        assert_eq!(roles.scribe, tauqe_protocol::ModelRef::openrouter("test/scribe"));
        assert_eq!(roles.coder, tauqe_protocol::ModelRef::openrouter("test/coder"));
        assert_eq!(roles.architect, tauqe_protocol::ModelRef::openrouter("test/architect"));
        assert_eq!(roles.primary, tauqe_protocol::ModelRef::openrouter("test/primary"));
        assert_eq!(roles.reviewer, tauqe_protocol::ModelRef::openrouter("test/reviewer"));

        // 2. Select Coder role
        let req_select = Request {
            id: tauqe_protocol::RequestId::Number(2),
            method: tauqe_protocol::methods::MODEL_SELECT.to_string(),
            params: Some(serde_json::json!({
                "selection": "coder"
            })),
        };
        let res_select = handle_model_select(req_select, &state).await;
        assert!(res_select.error.is_none());
        let select_val: tauqe_protocol::ModelSelectResult =
            serde_json::from_value(res_select.result.unwrap()).unwrap();
        assert_eq!(
            select_val.selection,
            tauqe_protocol::ModelSelection::Role(tauqe_protocol::ModelRole::Coder)
        );
        assert_eq!(
            select_val.effective_model,
            tauqe_protocol::ModelRef::openrouter("test/coder")
        );
        let selected_roles = select_val.roles.unwrap();
        assert_eq!(selected_roles.primary, tauqe_protocol::ModelRef::openrouter("test/primary"));
    }

    #[tokio::test]
    async fn test_turn_preparation_no_api_key() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        let res_start = start_model_turn(&state, "test prompt".to_string(), None, None).await;
        assert!(res_start.is_err());
        assert_eq!(res_start.unwrap_err().code, "NO_API_KEY");

        let policy = Box::new(tauqe_core::workflow::policy::DirectApplyPolicy::new());
        let res_exec = execute_model_turn(
            &state,
            "test prompt".to_string(),
            policy,
            StepTurnOptions {
                custom_selection: None,
                plan_context: None,
                reasoning_effort: None,
                marker: None,
                shared_operation: None,
            },
        )
        .await;
        assert!(res_exec.is_err());
        assert_eq!(res_exec.unwrap_err().code, "NO_API_KEY");
    }

    #[test]
    fn test_discussion_turn_forces_structured_protocol() {
        // Plan and review discussions must select the structured discussion
        // protocol even when the configured base protocol is XML.
        let plan_prompt = "# Plan Discussion: [p-1] Refine plan\n".to_string();
        let review_prompt = format!(
            "{} [rev-1] Fix issue\n",
            tauqe_core::review::REVIEW_DISCUSSION_PREFIX
        );

        for prompt in [plan_prompt, review_prompt] {
            let protocol = resolve_turn_protocol(&prompt, tauqe_protocol::EditProtocolKind::Xml, None);

            // The discussion protocol carries plan capabilities while hiding
            // file editing and toolchain verification from the model.
            assert!(protocol.supports(tauqe_protocol::LlmCapability::PlanCreation));
            assert!(protocol.supports(tauqe_protocol::LlmCapability::PlanUpdate));
            assert!(!protocol.supports(tauqe_protocol::LlmCapability::FileEdits));
            assert!(!protocol.supports(tauqe_protocol::LlmCapability::ToolchainVerify));

            // The model must be told to answer with the discussion JSON schema.
            match protocol.response_format(&[]) {
                Some(tauqe_core::providers::ResponseFormat::JsonSchema { json_schema, .. }) => {
                    assert_eq!(json_schema.name, "discussion_response");
                }
                _ => panic!("discussion turn must advertise the discussion_response schema"),
            }
        }
    }

    #[test]
    fn test_non_discussion_turn_respects_configured_protocol() {
        // A plain develop turn keeps the configured base protocol.
        let xml_protocol = resolve_turn_protocol(
            "Change compute to 42",
            tauqe_protocol::EditProtocolKind::Xml,
            None,
        );
        assert!(xml_protocol.supports(tauqe_protocol::LlmCapability::FileEdits));
        assert!(xml_protocol.supports(tauqe_protocol::LlmCapability::ToolchainVerify));

        let json_protocol = resolve_turn_protocol(
            "Change compute to 42",
            tauqe_protocol::EditProtocolKind::Structured,
            None,
        );
        assert!(json_protocol.supports(tauqe_protocol::LlmCapability::FileEdits));
        assert!(json_protocol.supports(tauqe_protocol::LlmCapability::ToolchainVerify));
        // The structured edits protocol never advertises plan capabilities.
        assert!(!json_protocol.supports(tauqe_protocol::LlmCapability::PlanUpdate));
    }

    #[tokio::test]
    async fn test_handle_model_cancel_soft_keeps_operation_active() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        let operation = state.try_begin_operation().expect("register operation");
        let soft_rx = operation.soft_stop_receiver();
        let cancel_rx = operation.cancel_receiver();
        assert!(!*soft_rx.borrow());
        assert!(!*cancel_rx.borrow());

        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::MODEL_CANCEL.to_string(),
            params: Some(serde_json::json!({ "mode": "soft" })),
        };
        let res = handle_model_cancel(req, &state).await;
        assert!(res.error.is_none());

        // A soft stop raises the graceful signal but leaves the operation
        // registered so the batch can finish its current member.
        assert!(*soft_rx.borrow());
        assert!(!*cancel_rx.borrow());
        assert!(state.is_operation_active());
    }

    #[tokio::test]
    async fn test_handle_model_cancel_hard_signals_and_keeps_operation_active() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        let operation = state.try_begin_operation().expect("register operation");
        let soft_rx = operation.soft_stop_receiver();
        let cancel_rx = operation.cancel_receiver();

        // Legacy clients omit the mode entirely; the default is an immediate
        // abort.
        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::MODEL_CANCEL.to_string(),
            params: None,
        };
        let res = handle_model_cancel(req, &state).await;
        assert!(res.error.is_none());

        // A hard stop delivers the cancel signal but keeps the operation slot
        // registered until the workflow unwinds and drops its guard, so a
        // concurrent request cannot race a half-unwound turn.
        assert!(*cancel_rx.borrow());
        assert!(!*soft_rx.borrow());
        assert!(state.is_operation_active());
    }
}
