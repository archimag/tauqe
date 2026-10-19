use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauqe_core::prompt::json::JsonPromptProtocol;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::prompt::xml::XmlPromptProtocol;
use tauqe_core::providers::{
    create_provider, format_session_id, LlmProvider, StreamEvent, SESSION_PREFIX_REPL,
};
use tauqe_core::workflow::{ExecutionMode, SingleExecution};
use tauqe_protocol::model::{CancelMode, ModelCancelParams, UsageTotals, UsageTotalsSnapshot};
use tauqe_protocol::{
    events, DiscussionClearFocusResult, DiscussionFocus, DiscussionGetFocusResult,
    DiscussionRequest, DiscussionSetFocusParams, DiscussionSetFocusResult, DiscussionTarget,
    EditFileDoneEvent, EditFileRetryingEvent, EditFileStartedEvent, EditFinishedEvent,
    EditHunkEvent, EditOutcome, EditStartedEvent, Event, GitCommitCreatedEvent, ModelAskParams,
    ModelResultEvent, ModelStartedEvent, ReasoningEffort, Request, Response, ResponseError,
    TurnIntent,
};

use crate::handlers::offload;
use crate::state::{next_operation_id, AppState, OperationGuard};
use crate::transport::OutChannel;
use crate::usage::UsageGuard;

pub async fn handle_model_ask(req: Request, state: &Arc<AppState>) -> Response {
    let params: ModelAskParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                tauqe_protocol::errors::INVALID_PARAMS,
                "Missing or invalid prompt",
            );
        }
    };

    let (prompt, intent) = match build_model_ask_prompt(state, params).await {
        Ok(resolved) => resolved,
        Err(err) => {
            return Response {
                id: req.id,
                result: None,
                error: Some(err),
            };
        }
    };

    match start_model_turn(state, prompt, intent, None, None).await {
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

/// Resolves the turn prompt and its declared intent for a model ask request.
///
/// A structured discussion turn carries its target explicitly, so the server
/// generates the discussion prompt from those typed fields and replaces the
/// focus anchor. A plain ask continues the anchored discussion instead: while a
/// focus is active, the current state of the focused plan or review session is
/// injected into the prompt on every turn. The intent is derived from the
/// resolved target rather than from the prompt text, so text that merely
/// resembles a discussion heading can never switch the turn mode.
async fn build_model_ask_prompt(
    state: &Arc<AppState>,
    params: ModelAskParams,
) -> Result<(String, TurnIntent), ResponseError> {
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
            Ok((prompt, intent_for_target(&discussion.target)))
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
                        Ok(prompt) => Ok((prompt, intent_for_target(&target))),
                        Err(err) => {
                            // The focused entity is gone (for example the plan
                            // was deleted elsewhere): drop the stale anchor and
                            // answer the raw prompt instead of failing the turn.
                            tracing::warn!(
                                "Discussion focus target could not be loaded ({}); clearing focus",
                                err.message
                            );
                            state.clear_discussion_focus().await;
                            Ok((text, TurnIntent::InteractiveRepl))
                        }
                    }
                }
                None => Ok((text, TurnIntent::InteractiveRepl)),
            }
        }
    }
}

/// Maps a discussion target onto the turn intent it establishes.
///
/// A plan discussion is strictly read-only: the model reasons about the plan
/// and never touches files. A review finding dialogue stays interactive,
/// because the developer may ask the model to apply the fix directly.
fn intent_for_target(target: &DiscussionTarget) -> TurnIntent {
    match target {
        DiscussionTarget::Plan { .. } => TurnIntent::ReadonlyDiscussion,
        DiscussionTarget::Review { .. } => TurnIntent::InteractiveRepl,
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
        return Response::err(
            req.id,
            tauqe_protocol::errors::OPERATION_IN_PROGRESS,
            "Cannot change model while an operation is in progress",
        );
    }

    let params: tauqe_protocol::ModelSelectParams = match req.params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(p) => p,
        None => {
            return Response::err(
                req.id,
                tauqe_protocol::errors::INVALID_PARAMS,
                "Missing or invalid model selection",
            );
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
            return Response::err(
                req.id,
                tauqe_protocol::errors::INVALID_MODEL,
                format!(
                    "Unknown model '{}'. Available models: {}",
                    effective,
                    available
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
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
    create_provider(kind, config).map(Arc::from).map_err(|err| {
        ResponseError::new(tauqe_protocol::errors::NO_API_KEY, err.to_string())
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
/// The active turn marker is bound onto the chosen protocol when its wire format
/// supports it; the structured discussion protocol encodes its structure in JSON
/// Schema and therefore declines the marker.
fn resolve_turn_protocol(
    intent: TurnIntent,
    develop_protocol: tauqe_protocol::EditProtocolKind,
    marker: Option<&str>,
) -> Box<dyn PromptProtocol> {
    let base_protocol: Box<dyn PromptProtocol> = if intent.is_readonly_discussion() {
        Box::new(JsonPromptProtocol::discussion())
    } else if intent.is_isolated_step() {
        match develop_protocol {
            tauqe_protocol::EditProtocolKind::Structured => {
                JsonPromptProtocol::with_capabilities(
                    tauqe_protocol::CapabilitySet::isolated_step_structured(),
                )
            }
            tauqe_protocol::EditProtocolKind::Xml => {
                XmlPromptProtocol::with_capabilities(
                    tauqe_protocol::CapabilitySet::isolated_step_xml(),
                )
            }
        }
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

/// Resolves the provider session identifier that binds a turn to a warm prompt
/// prefix.
///
/// Only a dialogue turn carries a session: interactive REPL turns and focused
/// discussions share one identifier, so successive rounds reuse the backend's
/// cached prefix. A turn without a dialogue receives `None`, so an isolated step
/// never inherits — or poisons — the interactive cache.
fn resolve_session_id(intent: TurnIntent, dialogue_session: String) -> Option<String> {
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

    let protocol =
        resolve_turn_protocol(intent, app_config.develop.protocol, marker.as_deref());

    let mut workflow_opts = tauqe_core::workflow::WorkflowOptions::from_app_config(&app_config);
    workflow_opts.reasoning_effort = reasoning_effort.or_else(|| match &selection {
        tauqe_protocol::ModelSelection::Role(role) => app_config.models.resolve_effort(*role),
        tauqe_protocol::ModelSelection::Specific(_) => None,
    });
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

    let wf_task = tokio::spawn(workflow_future);
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

        // Close the usage window exactly once on this terminal path: tokens and
        // time already consumed by a cancelled or failed turn are still charged,
        // and the partial report never leaks into the next operation.
        let snapshot = flush_usage(state, &mut usage_guard).await;
        emit_legacy_usage_event(state, &op_id).await;

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
        });
    }

    // Each arm yields the turn result together with the usage totals of the
    // operation it just closed, so the terminal log never re-derives them.
    let (turn_result, final_usage) = match wf_task.await {
        Ok(Ok(wf_result)) => {
            let snapshot = flush_usage(state, &mut usage_guard).await;
            emit_legacy_usage_event(state, &op_id).await;
            let operation_totals = snapshot.prev.clone();

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

            (
                ModelTurnResult {
                    operation_id: op_id,
                    result: Some(wf_result.result),
                    assistant_text: wf_result.assistant_text,
                    is_cancelled: false,
                    error: turn_error,
                    has_terminal_success,
                    blocked: wf_result.blocked,
                },
                operation_totals,
            )
        }
        Ok(Err(err)) => {
            let snapshot = flush_usage(state, &mut usage_guard).await;
            emit_legacy_usage_event(state, &op_id).await;
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

            (
                ModelTurnResult {
                    operation_id: op_id,
                    result: None,
                    assistant_text: raw_streamed_text,
                    is_cancelled: false,
                    error: Some(err.to_string()),
                    has_terminal_success: false,
                    blocked: None,
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
/// per-step signals.
pub async fn execute_model_turn(
    state: &Arc<AppState>,
    prompt: String,
    policy: Box<dyn tauqe_core::workflow::policy::ApplyPolicy>,
    options: StepTurnOptions,
) -> Result<ModelTurnResult, ResponseError> {
    let StepTurnOptions {
        intent,
        custom_selection,
        plan_context,
        reasoning_effort,
        marker,
        session_id,
        shared_operation,
    } = options;
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
    // Clearing history while a turn is compacting or recording it would race
    // the running operation, so the mutation is refused for its duration.
    if let Some(resp) = crate::handlers::reject_if_operation_active(
        &req.id,
        state,
        "Cannot clear history while a model operation is in progress",
    ) {
        return resp;
    }

    // Clearing the history truncates `history.jsonl` behind an `fsync` barrier,
    // so the mutation runs on the blocking pool with the exclusive history
    // guard held instead of stalling an async worker.
    let cleared = crate::handlers::with_history_mutation(state, |history| history.clear()).await;
    if let Err(err) = cleared {
        tracing::warn!(error = %err, "Failed to clear the interaction history");
    }
    // Clearing the dialogue session also detaches the discussion focus so the
    // REPL returns to the free, unfocused mode, and rotates the provider session
    // identifier so the discarded transcript can never warm the prompt-prefix
    // cache of the next dialogue.
    state.clear_discussion_focus().await;
    state.reset_session_uuid().await;
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
async fn flush_usage(
    state: &Arc<AppState>,
    guard: &mut Option<UsageGuard>,
) -> UsageTotalsSnapshot {
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

    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};

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

        let res_start = start_model_turn(
            &state,
            "test prompt".to_string(),
            TurnIntent::InteractiveRepl,
            None,
            None,
        )
        .await;
        assert!(res_start.is_err());
        assert_eq!(res_start.unwrap_err().code, "NO_API_KEY");

        let policy = Box::new(tauqe_core::workflow::policy::DirectApplyPolicy::new());
        let res_exec = execute_model_turn(
            &state,
            "test prompt".to_string(),
            policy,
            StepTurnOptions {
                intent: TurnIntent::IsolatedStep,
                custom_selection: None,
                plan_context: None,
                reasoning_effort: None,
                marker: None,
                session_id: None,
                shared_operation: None,
            },
        )
        .await;
        assert!(res_exec.is_err());
        assert_eq!(res_exec.unwrap_err().code, "NO_API_KEY");
    }

    #[test]
    fn test_plan_discussion_turn_forces_structured_protocol() {
        // A read-only plan discussion must select the structured discussion
        // protocol even when the configured base protocol is XML.
        let intent = intent_for_target(&DiscussionTarget::Plan {
            id: "p-1".to_string(),
        });
        assert!(intent.is_readonly_discussion());
        let protocol =
            resolve_turn_protocol(intent, tauqe_protocol::EditProtocolKind::Xml, None);

        // The discussion protocol carries plan capabilities while hiding file
        // editing and toolchain verification from the model.
        assert!(protocol.supports(tauqe_protocol::LlmCapability::PlanCreation));
        assert!(protocol.supports(tauqe_protocol::LlmCapability::PlanUpdate));
        assert!(!protocol.supports(tauqe_protocol::LlmCapability::FileEdits));
        assert!(!protocol.supports(tauqe_protocol::LlmCapability::ToolchainVerify));

        // The model must be told to answer with the discussion JSON schema.
        match protocol.response_format(&[]) {
            Some(tauqe_core::providers::ResponseFormat::JsonSchema { json_schema, .. }) => {
                assert_eq!(json_schema.name, "discussion_response");
            }
            _ => panic!("plan discussion turn must advertise the discussion_response schema"),
        }
    }

    #[test]
    fn test_review_focus_turn_keeps_edit_capability() {
        // A review focus dialogue keeps dual capability (addressed concept c-3):
        // the developer may reason about a finding and directly request a code
        // fix, so the turn keeps the file-edit and toolchain-verification
        // capabilities of the configured protocol instead of collapsing into the
        // text-only discussion schema.
        let intent = intent_for_target(&DiscussionTarget::Review {
            id: "rev-1".to_string(),
        });
        assert!(intent.is_interactive_repl());

        for kind in [
            tauqe_protocol::EditProtocolKind::Xml,
            tauqe_protocol::EditProtocolKind::Structured,
        ] {
            let protocol = resolve_turn_protocol(intent, kind, None);
            assert!(
                protocol.supports(tauqe_protocol::LlmCapability::FileEdits),
                "review focus must keep code editing"
            );
            assert!(
                protocol.supports(tauqe_protocol::LlmCapability::ToolchainVerify),
                "review focus must keep toolchain verification"
            );
        }
    }

    #[test]
    fn test_non_discussion_turn_respects_configured_protocol() {
        // A plain develop turn keeps the configured base protocol.
        let xml_protocol = resolve_turn_protocol(
            TurnIntent::InteractiveRepl,
            tauqe_protocol::EditProtocolKind::Xml,
            None,
        );
        assert!(xml_protocol.supports(tauqe_protocol::LlmCapability::FileEdits));
        assert!(xml_protocol.supports(tauqe_protocol::LlmCapability::ToolchainVerify));

        let json_protocol = resolve_turn_protocol(
            TurnIntent::InteractiveRepl,
            tauqe_protocol::EditProtocolKind::Structured,
            None,
        );
        assert!(json_protocol.supports(tauqe_protocol::LlmCapability::FileEdits));
        assert!(json_protocol.supports(tauqe_protocol::LlmCapability::ToolchainVerify));
        // The structured edits protocol never advertises plan capabilities.
        assert!(!json_protocol.supports(tauqe_protocol::LlmCapability::PlanUpdate));
    }

    #[test]
    fn test_isolated_step_turn_narrows_protocol() {
        let xml_proto = resolve_turn_protocol(
            TurnIntent::IsolatedStep,
            tauqe_protocol::EditProtocolKind::Xml,
            None,
        );
        assert!(xml_proto.supports(tauqe_protocol::LlmCapability::FileEdits));
        assert!(!xml_proto.supports(tauqe_protocol::LlmCapability::PlanCreation));
        assert!(!xml_proto.supports(tauqe_protocol::LlmCapability::PlanUpdate));

        let structured_proto = resolve_turn_protocol(
            TurnIntent::IsolatedStep,
            tauqe_protocol::EditProtocolKind::Structured,
            None,
        );
        assert!(structured_proto.supports(tauqe_protocol::LlmCapability::FileEdits));
        assert!(!structured_proto.supports(tauqe_protocol::LlmCapability::PlanCreation));
        assert!(!structured_proto.supports(tauqe_protocol::LlmCapability::PlanUpdate));
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

    #[test]
    fn test_resolve_session_id_scopes_the_dialogue_session() {
        // An interactive turn joins the running dialogue session.
        assert_eq!(
            resolve_session_id(TurnIntent::InteractiveRepl, "abc".to_string()),
            Some(format_session_id(SESSION_PREFIX_REPL, "abc"))
        );
        // A focused discussion is still a dialogue.
        assert!(resolve_session_id(TurnIntent::ReadonlyDiscussion, "abc".to_string()).is_some());
        // An isolated step carries no dialogue and stays stateless.
        assert_eq!(
            resolve_session_id(TurnIntent::IsolatedStep, "abc".to_string()),
            None
        );
    }

    #[tokio::test]
    async fn test_isolated_step_forwards_the_caller_session() {
        let repo = FocusTestRepo::new();
        let root = repo.path().to_path_buf();

        let mock = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            "Nothing to change.",
        )]));
        let provider: Arc<dyn LlmProvider> = mock.clone();

        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = tauqe_core::config::AppConfig::default();
        config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
        config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
        config.models.auto_level_up = false;
        let state = AppState::new(config, root.clone(), hist_tx, out);
        *state.provider_override.lock().await = Some(provider);

        state
            .context
            .write()
            .await
            .add_file("src/target.rs", tauqe_protocol::ContextAccess::Editable)
            .unwrap();

        // A review finding turn names its own session, so the audit's cached
        // prefix survives the turn instead of collapsing into a stateless call.
        let session = format_session_id(tauqe_core::providers::SESSION_PREFIX_REVIEW, "rev-7");
        let policy = build_apply_policy(&state, tauqe_protocol::WorkflowKind::Naive);
        let _ = execute_model_turn(
            &state,
            "Verify the finding".to_string(),
            policy,
            StepTurnOptions {
                intent: TurnIntent::IsolatedStep,
                custom_selection: None,
                plan_context: None,
                reasoning_effort: None,
                marker: None,
                session_id: Some(session),
                shared_operation: None,
            },
        )
        .await
        .expect("isolated turn executes");

        let request = mock.last_request().expect("recorded provider request");
        assert_eq!(request.session_id.as_deref(), Some("tauqe-review-rev-7"));
    }

    #[tokio::test]
    async fn test_clear_history_rotates_the_dialogue_session() {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        let before = state.session_uuid().await;
        let req = Request {
            id: tauqe_protocol::RequestId::Number(1),
            method: tauqe_protocol::methods::MODEL_CLEAR_HISTORY.to_string(),
            params: None,
        };
        let res = handle_model_clear_history(req, &state).await;
        assert!(res.error.is_none());
        assert_ne!(
            before,
            state.session_uuid().await,
            "clearing the history must start a fresh dialogue session"
        );
    }

    /// Temporary git repository hosting a single committed source file.
    struct FocusTestRepo(std::path::PathBuf);

    impl FocusTestRepo {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "tauqe-model-focus-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::create_dir_all(root.join("src"));
            tauqe_core::git::init_repository(Some(&root), true).unwrap();
            std::fs::write(root.join("src/target.rs"), "pub fn target() {}\n").unwrap();
            tauqe_core::git::cmd::stage_files(&root, &["src/target.rs".to_string()]).unwrap();
            tauqe_core::git::cmd::commit_with_developer_author(&root, "Add target", false)
                .unwrap();
            Self(root)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for FocusTestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn test_review_focus_repl_applies_code_edits() {
        let repo = FocusTestRepo::new();
        let root = repo.path().to_path_buf();

        let session = tauqe_protocol::ReviewSession {
            id: "rev-focus".to_string(),
            title: "Focused Review".to_string(),
            created_at: 100,
            model: tauqe_protocol::ModelRef::openrouter("test/reviewer"),
            description: None,
            user_prompt: None,
            target_files: vec!["src/target.rs".to_string()],
            items: vec![tauqe_protocol::ReviewItem {
                id: 1,
                title: "Fix target".to_string(),
                model: None,
                issue_type: tauqe_protocol::IssueType::Architecture,
                severity: tauqe_protocol::ReviewSeverity::Warning,
                status: tauqe_protocol::ReviewItemStatus::Todo,
                recommendations: vec![],
                files: vec!["src/target.rs".to_string()],
                readonly_files: vec![],
                subplan: Vec::new(),
                subplan_architect: None,
                resolution_notes: None,
                body: "target needs fixing".to_string(),
            }],
            raw_markdown: String::new(),
        };
        tauqe_core::review::ReviewStorage::save_session(&root, &session).unwrap();

        let edit = "<tauqe_edits summary=\"Apply the focused fix\">\n<edit path=\"src/target.rs\">\n<search>\npub fn target() {}\n</search>\n<replace>\npub fn target() { /* focused fix */ }\n</replace>\n</edit>\n</tauqe_edits>";
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockLlmProvider::scripted([MockResponse::text(edit)]));

        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = tauqe_core::config::AppConfig::default();
        config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
        config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
        config.models.auto_level_up = false;
        let state = AppState::new(config, root.clone(), hist_tx, out);
        *state.provider_override.lock().await = Some(provider);

        // The finding's file must be editable so the requested change can land.
        state
            .context
            .write()
            .await
            .add_file("src/target.rs", tauqe_protocol::ContextAccess::Editable)
            .unwrap();

        // The REPL is anchored on the finding under discussion.
        state
            .set_discussion_focus(DiscussionFocus::review("rev-focus", vec!["1".to_string()]))
            .await;

        // The focused prompt carries the finding context and keeps edit capability.
        let (prompt, intent) = build_model_ask_prompt(
            &state,
            ModelAskParams::Prompt {
                text: "Please apply the fix for this finding".to_string(),
            },
        )
        .await
        .expect("focused prompt is built");
        assert!(prompt.starts_with(tauqe_core::review::REVIEW_DISCUSSION_PREFIX));
        assert!(intent.is_interactive_repl());
        let protocol = resolve_turn_protocol(intent, tauqe_protocol::EditProtocolKind::Xml, None);
        assert!(protocol.supports(tauqe_protocol::LlmCapability::FileEdits));

        // Running the focused turn applies the requested code change.
        let policy = build_apply_policy(&state, tauqe_protocol::WorkflowKind::Naive);
        let turn = execute_model_turn(
            &state,
            prompt,
            policy,
            StepTurnOptions {
                intent: TurnIntent::InteractiveRepl,
                custom_selection: None,
                plan_context: None,
                reasoning_effort: None,
                marker: None,
                session_id: None,
                shared_operation: None,
            },
        )
        .await
        .expect("focused turn executes");

        assert!(
            matches!(
                &turn.result,
                Some(tauqe_protocol::ModelResult::Edit { outcome, .. }) if outcome.applied
            ),
            "a focused review turn must apply code edits, got {:?}",
            turn.result
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/target.rs")).unwrap(),
            "pub fn target() { /* focused fix */ }\n"
        );
    }

    /// Builds a state whose only purpose is to observe the usage ledger of a
    /// single turn: a Naive workflow over an XML protocol with a scripted
    /// provider and no model escalation.
    async fn usage_state(root: &std::path::Path, provider: Arc<dyn LlmProvider>) -> Arc<AppState> {
        let (out, _rx) = crate::transport::OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = tauqe_core::config::AppConfig::default();
        config.develop.workflow = tauqe_protocol::WorkflowKind::Naive;
        config.develop.protocol = tauqe_protocol::EditProtocolKind::Xml;
        config.models.auto_level_up = false;
        let state = AppState::new(config, root.to_path_buf(), hist_tx, out);
        *state.provider_override.lock().await = Some(provider);
        state
            .context
            .write()
            .await
            .add_file("src/target.rs", tauqe_protocol::ContextAccess::Editable)
            .unwrap();
        state
    }

    /// A completed turn folds its usage into the session exactly once and leaves
    /// no in-flight window behind, so the next operation cannot inherit it.
    #[tokio::test]
    async fn a_completed_turn_flushes_its_usage_exactly_once() {
        let repo = FocusTestRepo::new();
        let usage = tauqe_protocol::ModelUsageInfo {
            prompt_tokens: 12,
            completion_tokens: 8,
            total_tokens: 20,
            cost: Some(0.5),
            ..tauqe_protocol::ModelUsageInfo::default()
        };
        let provider: Arc<dyn LlmProvider> = Arc::new(MockLlmProvider::scripted([
            MockResponse::text("Nothing to change.").with_usage(usage),
        ]));
        let state = usage_state(repo.path(), provider).await;

        let policy = build_apply_policy(&state, tauqe_protocol::WorkflowKind::Naive);
        let turn = execute_model_turn(
            &state,
            "Report the current state".to_string(),
            policy,
            StepTurnOptions {
                intent: TurnIntent::IsolatedStep,
                custom_selection: None,
                plan_context: None,
                reasoning_effort: None,
                marker: None,
                session_id: None,
                shared_operation: None,
            },
        )
        .await
        .expect("turn executes");
        assert!(!turn.is_cancelled);
        assert!(turn.error.is_none(), "unexpected turn error: {:?}", turn.error);

        let snapshot = state.usage.lock().await.clone();
        assert_eq!(snapshot.operations, 1, "one terminal flush per turn");
        assert!(snapshot.cur.is_none(), "the terminal flush closes the window");
        assert_eq!(snapshot.session.total_tokens, 20);
        assert_eq!(snapshot.prev.total_tokens, 20);
        assert!((snapshot.session.cost - 0.5).abs() < f64::EPSILON);
    }

    /// A cancelled turn still closes its window exactly once, so the tokens and
    /// time it consumed are charged and no window is left open.
    #[tokio::test]
    async fn a_cancelled_turn_flushes_its_window_exactly_once() {
        let repo = FocusTestRepo::new();
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockLlmProvider::scripted([MockResponse::text(
                "This answer never reaches the developer.",
            )]));
        let state = usage_state(repo.path(), provider).await;

        let operation = Arc::new(state.try_begin_operation().expect("register operation"));
        state.hard_cancel_active_operation();

        let policy = build_apply_policy(&state, tauqe_protocol::WorkflowKind::Naive);
        let turn = execute_model_turn(
            &state,
            "Cancelled prompt".to_string(),
            policy,
            StepTurnOptions {
                intent: TurnIntent::InteractiveRepl,
                custom_selection: None,
                plan_context: None,
                reasoning_effort: None,
                marker: None,
                session_id: None,
                shared_operation: Some(operation),
            },
        )
        .await
        .expect("cancelled turn returns a result");
        assert!(
            turn.is_cancelled,
            "the pre-cancelled turn reports cancellation"
        );

        let snapshot = state.usage.lock().await.clone();
        assert_eq!(
            snapshot.operations, 1,
            "a cancelled turn flushes exactly once"
        );
        assert!(
            snapshot.cur.is_none(),
            "cancellation must close the usage window"
        );
    }
}
