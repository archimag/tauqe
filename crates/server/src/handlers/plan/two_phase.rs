//! Two-phase Architect plan synthesis contour.
//!
//! A plan is issued in three beats. The Planner drafts the horizon from the
//! immutable concept — or from the developer directives a new plan starts with —
//! an independent Architect entry audits that draft, and the Planner reconciles
//! the audit into the plan it issues. The split exists to break the cognitive
//! inertia of a model reviewing itself: an audit that inherits the assumptions of
//! the draft is not an audit.
//!
//! The module owns the beats and their order; it owns neither the rounds nor the
//! persistence barrier. Each Planner beat is one run of the shared structured
//! Architect contour (`crate::handlers::architect::run_contour_synthesis`), which
//! carries the provider rounds, the schema healing of an invalid payload, the tier
//! ladder, the research journal, and the typed terminal outcomes. The critique
//! beat is a single read-only structured round, because a critic authors no plan
//! and widens no context. The issued document then reaches disk through
//! `PlanStorage` exactly as a single-phase synthesis does, so no beat bypasses the
//! write barrier.
//!
//! The regime is declared, never inferred: whether the critique runs is decided by
//! the typed request (`critic_model` together with `skip_critique`), and whether
//! the run creates a plan or modifies a stored one is a typed flag of the same
//! request.
//!
//! The critique beat runs only when the request names a critic entry, because an
//! auditor is a deliberate choice of an independent configuration rather than a
//! second sampling of the drafting model. A request that names none issues the
//! draft alone, exactly as the single-phase contour of `super::engine` does.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tauqe_core::config::{AppConfig, ModelsConfig};
use tauqe_core::plan::prompt::{
    build_plan_critique_prompt, build_plan_critique_system_prompt,
    build_plan_horizon_system_prompt, build_plan_horizon_user_prompt,
    build_plan_reconciliation_prompt, parse_plan_critique_output, plan_critique_response_schema,
    plan_critique_response_schema_definition, resolve_architect_tier, PlanCritiqueOutput,
    PlanRefinementOutput,
};
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::{
    format_session_id, ChatMessage, JsonSchemaDefinition, LlmProvider, ResponseFormat, StreamEvent,
    SESSION_PREFIX_ARCHITECT,
};
use tauqe_core::workflow::fsm::structured::{StructuredRefineOutcome, StructuredRefineStatus};
use tauqe_core::workflow::WorkflowOptions;
use tauqe_protocol::{
    errors, events, ArchitectTier, ModelErrorEvent, ModelFinishedEvent, ModelRole, ModelSelection,
    ModelStartedEvent, Plan, PlanCreatePipelineParams, PlanItem, PlanListChangedEvent,
    PlanRefineResult, PlanUpdatedEvent, Request, RequestId, Response, TurnPhase, TurnPhaseEvent,
};
use tokio::sync::mpsc;

use crate::handlers::architect::{
    format_architect_tier, resolve_architect_provider, run_contour_synthesis,
};
use crate::handlers::common::{
    cleanup_transient_context, emit, preload_readonly_files, record_synthesis_history,
};
use crate::reap::{abort_and_reap, await_bounded, Awaited};
use crate::state::{AppState, OperationGuard};

use super::common::{find_duplicate_item_ids, offload_plan};
use super::engine::collect_plan_context_files;

/// Longest plan title the pipeline derives from developer directives.
const DERIVED_TITLE_MAX_CHARS: usize = 120;

/// Title of a plan whose directives carry no readable line.
const FALLBACK_PLAN_TITLE: &str = "Untitled plan";

/// Hint appended to a breached context-budget diagnostic.
const BUDGET_HINT: &str = "The plan is too broad and should be manually decomposed or scoped down.";

/// One two-phase plan synthesis request.
///
/// The request is typed end to end: it states which plan the pipeline operates
/// on, which directives the issued document must satisfy, and which of the two
/// Architect configurations produce and audit the horizon. Nothing about the
/// regime is inferred from the wording of a directive.
pub(super) struct TwoPhaseRequest {
    /// Plan the pipeline operates on.
    ///
    /// A directed modification carries the stored document; a creation carries
    /// the identity draft the handler derived from the directives.
    pub plan: Plan,
    /// Engineering directives the issued plan must satisfy.
    pub directives: Option<String>,
    /// Explicit Planner selection of the drafting beat.
    pub planner_model: Option<ModelSelection>,
    /// Explicit Architect selection of the critique beat.
    pub critic_model: Option<ModelSelection>,
    /// Whether the critique beat is opted out of.
    pub skip_critique: bool,
    /// Whether the pipeline issues a new plan instead of modifying a stored one.
    pub creation: bool,
    /// Operation label the run reports in telemetry and session history.
    pub operation_title: &'static str,
}

/// One Planner beat that produced a horizon the pipeline may issue.
struct PlannerWave {
    /// Payload the beat answered with.
    output: PlanRefinementOutput,
    /// Architect configuration that produced the payload.
    tier: ArchitectTier,
}

/// Category of a beat verdict that issued no horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerdictKind {
    /// The beat refused to plan and stated the exact contradiction.
    Blocked,
    /// The beat reported a host condition only the developer can repair.
    Aborted,
    /// The beat demanded more context than the configured budget allows.
    BudgetExceeded,
}

impl VerdictKind {
    /// Label the verdict is recorded under in telemetry and session history.
    fn label(&self) -> &'static str {
        match self {
            Self::Blocked => "Blocked",
            Self::Aborted => "Aborted",
            Self::BudgetExceeded => "Token Budget Exceeded",
        }
    }
}

/// Terminal failure of a beat that never produced a usable horizon.
enum WaveFailure {
    /// The operation was cancelled: the run answers a cancellation.
    Cancelled,
    /// The beat could not run at all: a provider or transport failure.
    Failed(String),
    /// The beat reported a horizon status without any step.
    EmptyHorizon,
    /// The beat reached a typed verdict instead of a horizon.
    Verdict { kind: VerdictKind, message: String },
}

/// Terminal failure of the independent critique beat.
enum CritiqueFailure {
    /// The operation was cancelled while the critic ran.
    Cancelled,
    /// The critic could not run at all: a provider or transport failure.
    Failed(String),
    /// The critic answered no payload the response schema accepts.
    Unreadable(String),
}

/// Handles `plan/createPipeline`: the two-phase creation of a new plan.
///
/// The directives are the only coordinate a creation carries, so the plan
/// identity is derived from them deterministically and the issued document is
/// written by the pipeline instead of being supplied by the caller.
pub async fn handle_plan_create_pipeline(req: Request, state: &Arc<AppState>) -> Response {
    let params: PlanCreatePipelineParams = match req.params {
        Some(value) => match serde_json::from_value::<PlanCreatePipelineParams>(value) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid plan create pipeline parameters: {err}"),
                )
            }
        },
        None => {
            return Response::err(
                req.id,
                errors::INVALID_PARAMS,
                "Missing plan create pipeline parameters",
            )
        }
    };

    let directives = params.directives.trim().to_string();
    if directives.is_empty() {
        return Response::err(
            req.id,
            errors::INVALID_PARAMS,
            "Plan creation directives cannot be empty",
        );
    }

    let operation = match state.try_begin_operation() {
        Ok(guard) => guard,
        Err(()) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Another operation is already running",
            )
        }
    };

    let repo_root = state.context.read().await.repo_root().to_path_buf();
    let load_root = repo_root.clone();
    let existing = match offload_plan(move || PlanStorage::load_all(&load_root)).await {
        Ok(plans) => plans,
        Err(err) => return Response::err(req.id, errors::PLAN_LOAD_FAILED, err),
    };
    let plan = created_plan_draft(&existing, &directives);

    tracing::info!(
        plan_id = %plan.id,
        critique = params.critic_model.is_some() && !params.skip_critique,
        "plan creation pipeline requested"
    );

    execute_two_phase_plan_synthesis(
        req.id,
        state,
        TwoPhaseRequest {
            plan,
            directives: Some(directives),
            planner_model: params.planner_model,
            critic_model: params.critic_model,
            skip_critique: params.skip_critique,
            creation: true,
            operation_title: "Plan Creation",
        },
        &operation,
    )
    .await
}

/// Runs the draft, critique, and reconciliation beats of one synthesis.
///
/// The pipeline is sequential and fail-fast: a beat that issues no horizon ends
/// the run through [`finish_wave_failure`], so the issued document is written
/// exactly once, by the last beat that produced a horizon.
pub(super) async fn execute_two_phase_plan_synthesis(
    req_id: RequestId,
    state: &Arc<AppState>,
    request: TwoPhaseRequest,
    operation: &OperationGuard,
) -> Response {
    let TwoPhaseRequest {
        plan,
        directives,
        planner_model,
        critic_model,
        skip_critique,
        creation,
        operation_title,
    } = request;
    let repo_root = state.context.read().await.repo_root().to_path_buf();

    let (config, planner_tier, planner_options, models, critic_tier, critic_session) = {
        let cfg = state.config.lock().await.config.clone();
        // A directed modification is not a replanning wave around a blocked step,
        // so the drafting beat starts from the base rung or from the explicit
        // developer selection, never from an escalated position.
        let (planner_tier, _) = resolve_architect_tier(
            &cfg,
            plan.implementation.architect.as_ref(),
            planner_model.as_ref(),
            None,
            plan.implementation.last_replanned_step.as_deref(),
        );
        let mut planner_options = WorkflowOptions::from_app_config(&cfg);
        // One synthesis is one isolated design dialogue, so its cache is keyed by
        // the plan and never by the interactive REPL session.
        planner_options.session_id = Some(format_session_id(SESSION_PREFIX_ARCHITECT, &plan.id));
        let critic_tier = critic_model
            .as_ref()
            .filter(|_| !skip_critique)
            .map(|selection| cfg.models.resolve_selection_architect_tier(selection));
        // A promoted critic addresses a backend node warmed by that very model,
        // so the critique session never shares an identifier across ladder rungs.
        let critic_session = critic_tier.as_ref().and_then(|tier| {
            planner_options.session_id.as_ref().map(|session| {
                let index = cfg
                    .models
                    .architect_tier_index(Some(tier))
                    .unwrap_or_default();
                format_session_id(session, &format!("critique{index}"))
            })
        });
        let models = cfg.models.clone();
        (
            cfg,
            planner_tier,
            planner_options,
            models,
            critic_tier,
            critic_session,
        )
    };

    tracing::info!(
        plan_id = %plan.id,
        operation = operation_title,
        creation = creation,
        critique = critic_tier.is_some(),
        "two-phase plan synthesis started"
    );

    // The synthesis starts from a released transient context: only the files the
    // plan already declares reach the drafting beat, so a modification never
    // inherits the material a previous turn discovered.
    cleanup_transient_context(state).await;
    preload_readonly_files(state, &repo_root, &collect_plan_context_files(&plan)).await;

    let (history_turn_prompt, history_context_paths) =
        capture_history_coordinates(state, operation_title, &plan, directives.as_deref()).await;

    let draft_header = format!(
        "# {}: [{}] {}\n*Drafting the plan horizon with {}...*\n\n---\n\n",
        operation_title,
        plan.id,
        plan.title,
        format_architect_tier(&planner_tier)
    );
    let mut horizon = match run_planner_wave(
        state,
        operation,
        compose_draft_task(&plan, directives.as_deref()),
        draft_header,
        planner_options.clone(),
        models.clone(),
        planner_tier.clone(),
        BUDGET_HINT,
    )
    .await
    {
        Ok(wave) => wave,
        Err(failure) => {
            return finish_wave_failure(
                req_id,
                state,
                operation_title,
                &plan,
                &history_turn_prompt,
                &history_context_paths,
                failure,
            )
            .await
        }
    };

    if let Some(critic_tier) = critic_tier.as_ref() {
        match run_critique_wave(
            state,
            operation,
            &config,
            &plan,
            &horizon.output,
            directives.as_deref(),
            critic_tier,
            critic_session,
        )
        .await
        {
            Ok(critique) => {
                if !critique.is_approved() {
                    let header = format!(
                        "# {}: [{}] {}\n*Reconciling the independent critique with {}...*\n\n---\n\n",
                        operation_title,
                        plan.id,
                        plan.title,
                        format_architect_tier(&planner_tier)
                    );
                    match run_planner_wave(
                        state,
                        operation,
                        compose_reconciliation_task(
                            &plan,
                            &horizon.output,
                            &critique,
                            directives.as_deref(),
                        ),
                        header,
                        planner_options,
                        models,
                        planner_tier.clone(),
                        BUDGET_HINT,
                    )
                    .await
                    {
                        Ok(wave) => horizon = wave,
                        Err(failure) => {
                            return finish_wave_failure(
                                req_id,
                                state,
                                operation_title,
                                &plan,
                                &history_turn_prompt,
                                &history_context_paths,
                                failure,
                            )
                            .await
                        }
                    }
                }
            }
            Err(failure) => {
                return finish_wave_failure(
                    req_id,
                    state,
                    operation_title,
                    &plan,
                    &history_turn_prompt,
                    &history_context_paths,
                    failure,
                )
                .await
            }
        }
    }

    let explanation = horizon.output.explanation.trim().to_string();
    let items = horizon.output.items;
    let architect = horizon.tier;

    if let Some(duplicate) = find_duplicate_item_ids(&items) {
        cleanup_transient_context(state).await;
        return Response::err(
            req_id,
            errors::INVALID_PLAN_TREE,
            format!("The synthesized plan contains a duplicate or empty step ID: '{duplicate}'"),
        );
    }

    // The write below is destructive for the uncompleted horizon and exclusive
    // for a creation, so the authoritative cancellation signal is re-checked at
    // the seam where the document reaches disk.
    if *operation.cancel_receiver().borrow() {
        cleanup_transient_context(state).await;
        tracing::warn!(
            plan_id = %plan.id,
            operation = operation_title,
            "two-phase plan synthesis was cancelled before the plan write"
        );
        return Response::err(
            req_id,
            errors::CANCELLED,
            format!("{} was cancelled", operation_title),
        );
    }

    let (stored, dropped_refs) = if creation {
        let mut items = items;
        // A newly created plan carries no concept tree yet, so a reference the
        // draft emitted cannot resolve. The repair is reported, never silent.
        let dropped = strip_unresolved_concept_refs(&mut items);
        let mut draft = plan.clone();
        draft.updated_at = unix_now();
        let save_root = repo_root.clone();
        let persisted = offload_plan(move || -> anyhow::Result<Plan> {
            draft.implementation.items = PlanStorage::merge_subplan(&[], items)?;
            PlanStorage::save_plan(&save_root, &draft)?;
            Ok(draft)
        })
        .await;
        match persisted {
            Ok(plan) => (plan, dropped),
            Err(err) => {
                cleanup_transient_context(state).await;
                return Response::err(req_id, errors::PLAN_SAVE_FAILED, err);
            }
        }
    } else {
        let save_root = repo_root.clone();
        let plan_id = plan.id.clone();
        match offload_plan(move || {
            PlanStorage::replace_uncompleted_horizon(&save_root, &plan_id, items, architect, None)
        })
        .await
        {
            Ok(plan) => (plan, 0usize),
            Err(err) => {
                cleanup_transient_context(state).await;
                return Response::err(req_id, errors::PLAN_SAVE_FAILED, err);
            }
        }
    };

    let explanation = if dropped_refs > 0 {
        let note = format!(
            "Dropped {dropped_refs} unresolved concept reference(s): a newly created plan carries no concept tree yet."
        );
        if explanation.is_empty() {
            note
        } else {
            format!("{explanation}\n\n{note}")
        }
    } else {
        explanation
    };

    let (total_items, completed_items) = stored.implementation.count_stats();
    let headline = if creation {
        "created"
    } else {
        "horizon synthesized"
    };
    tracing::info!(
        plan_id = %stored.id,
        total_items = total_items,
        completed_items = completed_items,
        creation = creation,
        "two-phase plan synthesis issued a plan"
    );

    let history_response = format!(
        "Plan [{}] {} ({} total, {} completed).\n\n{}",
        stored.id, headline, total_items, completed_items, explanation
    );
    record_synthesis_history(
        state,
        history_turn_prompt,
        history_context_paths,
        history_response,
        Some(format!("{}: {}", operation_title, stored.title)),
    )
    .await;

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent {
            plan: stored.clone(),
            is_save: creation,
        })
        .ok(),
    );

    let list_root = repo_root.clone();
    if let Ok(plans) = offload_plan(move || PlanStorage::load_all(&list_root)).await {
        let active_root = repo_root.clone();
        let active_id = offload_plan(move || PlanStorage::load_active_id(&active_root))
            .await
            .ok()
            .flatten();
        emit(
            &state.out,
            events::PLAN_LIST_CHANGED,
            serde_json::to_value(PlanListChangedEvent { plans, active_id }).ok(),
        );
    }

    // The material the synthesis widened is released with the plan it produced.
    cleanup_transient_context(state).await;

    Response::ok_typed(
        req_id,
        &PlanRefineResult::Success {
            plan: stored,
            explanation: Some(explanation),
        },
    )
}

/// Runs one Planner beat and interprets its typed terminal status.
#[allow(clippy::too_many_arguments)]
async fn run_planner_wave(
    state: &Arc<AppState>,
    operation: &OperationGuard,
    task: String,
    intro_header: String,
    options: WorkflowOptions,
    models: ModelsConfig,
    tier: ArchitectTier,
    budget_hint: &str,
) -> Result<PlannerWave, WaveFailure> {
    let op_id = format!("op-{}", crate::state::next_operation_id());
    emit_model_window_start(state, &op_id, &tier);

    // The contour works on its own clone of the context and owns the auto layer it
    // widens, so the adapter hands it a snapshot and merges the result back.
    let context_snapshot = state.context.read().await.clone();
    let (result, summary) = run_contour_synthesis(
        state,
        operation,
        &op_id,
        task,
        context_snapshot,
        options,
        models,
        tier,
        intro_header,
        budget_hint.to_string(),
    )
    .await;

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            if summary.cancelled {
                return Err(WaveFailure::Cancelled);
            }
            return Err(WaveFailure::Failed(
                summary.error.unwrap_or_else(|| err.to_string()),
            ));
        }
    };

    if !matches!(outcome.status, StructuredRefineStatus::Cancelled) {
        emit(
            &state.out,
            events::MODEL_FINISHED,
            serde_json::to_value(ModelFinishedEvent {
                operation_id: op_id,
                full_text: summary.assistant_text,
            })
            .ok(),
        );
    }

    let StructuredRefineOutcome {
        output,
        tier,
        status,
    } = outcome;
    match status {
        StructuredRefineStatus::Synthesized => match output {
            Some(output) if output.items.is_empty() => Err(WaveFailure::EmptyHorizon),
            Some(output) => Ok(PlannerWave { output, tier }),
            None => Err(WaveFailure::EmptyHorizon),
        },
        StructuredRefineStatus::Cancelled => Err(WaveFailure::Cancelled),
        StructuredRefineStatus::Blocked { reason } => Err(WaveFailure::Verdict {
            kind: VerdictKind::Blocked,
            message: reason,
        }),
        StructuredRefineStatus::Aborted(report) => Err(WaveFailure::Verdict {
            kind: VerdictKind::Aborted,
            message: report.to_message(),
        }),
        StructuredRefineStatus::BudgetExceeded { diagnostic } => Err(WaveFailure::Verdict {
            kind: VerdictKind::BudgetExceeded,
            message: diagnostic,
        }),
    }
}

/// Runs the independent critique of one draft horizon.
///
/// The beat is read-only: the critic receives the whole coordinate it needs (the
/// requirement, the completed foundation, and the draft) and answers with the
/// audit payload. A payload that violates the response schema is regenerated on
/// the same rung within the attempt budget the rung declares; an exhausted budget
/// blocks the plan instead of issuing it unaudited.
#[allow(clippy::too_many_arguments)]
async fn run_critique_wave(
    state: &Arc<AppState>,
    operation: &OperationGuard,
    config: &AppConfig,
    plan: &Plan,
    draft: &PlanRefinementOutput,
    directives: Option<&str>,
    tier: &ArchitectTier,
    session_id: Option<String>,
) -> Result<PlanCritiqueOutput, WaveFailure> {
    let provider = match resolve_architect_provider(state, config, tier.model.provider).await {
        Ok(provider) => provider,
        Err(message) => return Err(WaveFailure::Failed(message)),
    };

    let op_id = format!("op-{}", crate::state::next_operation_id());
    emit_model_window_start(state, &op_id, tier);
    tracing::info!(plan_id = %plan.id, model = %tier.model, "independent plan critique started");

    match run_critique_rounds(
        state, operation, provider, plan, draft, directives, tier, session_id, &op_id,
    )
    .await
    {
        Ok(critique) => {
            let banner = critique_banner(&critique);
            emit(
                &state.out,
                events::MODEL_FINISHED,
                serde_json::to_value(ModelFinishedEvent {
                    operation_id: op_id,
                    full_text: banner,
                })
                .ok(),
            );
            Ok(critique)
        }
        Err(CritiqueFailure::Cancelled) => {
            emit(
                &state.out,
                events::MODEL_CANCELLED,
                Some(serde_json::json!({ "operation_id": op_id })),
            );
            Err(WaveFailure::Cancelled)
        }
        Err(CritiqueFailure::Failed(message)) => {
            emit(
                &state.out,
                events::MODEL_ERROR,
                serde_json::to_value(ModelErrorEvent {
                    operation_id: op_id,
                    message: message.clone(),
                })
                .ok(),
            );
            Err(WaveFailure::Failed(message))
        }
        Err(CritiqueFailure::Unreadable(message)) => {
            let banner =
                format!("\n\n---\n⛔ **[Independent Critique Unavailable]** {message}\n\n");
            emit(
                &state.out,
                events::MODEL_FINISHED,
                serde_json::to_value(ModelFinishedEvent {
                    operation_id: op_id,
                    full_text: banner,
                })
                .ok(),
            );
            Err(WaveFailure::Verdict {
                kind: VerdictKind::Blocked,
                message,
            })
        }
    }
}

/// Runs the critique rounds of one beat within the rung's attempt budget.
#[allow(clippy::too_many_arguments)]
async fn run_critique_rounds(
    state: &Arc<AppState>,
    operation: &OperationGuard,
    provider: Arc<dyn LlmProvider>,
    plan: &Plan,
    draft: &PlanRefinementOutput,
    directives: Option<&str>,
    tier: &ArchitectTier,
    session_id: Option<String>,
    op_id: &str,
) -> Result<PlanCritiqueOutput, CritiqueFailure> {
    let budget = tier.attempts().max(1) as usize;
    let mut messages = vec![
        ChatMessage::system(build_plan_critique_system_prompt()),
        ChatMessage::user(build_plan_critique_prompt(plan, draft, directives)),
    ];
    let mut last_error = String::new();

    for attempt in 1..=budget {
        emit(
            &state.out,
            events::TURN_PHASE,
            serde_json::to_value(TurnPhaseEvent {
                operation_id: op_id.to_string(),
                phase: TurnPhase::Proposal,
                round: Some(attempt),
                max_rounds: Some(budget),
                detail: Some("Independent plan critique".to_string()),
            })
            .ok(),
        );

        let raw = collect_structured_round(
            state,
            operation,
            Arc::clone(&provider),
            tier,
            messages.clone(),
            session_id.clone(),
            op_id,
        )
        .await?;

        match parse_plan_critique_output(&raw) {
            Ok(critique) => return Ok(critique),
            Err(err) => {
                last_error = err.to_string();
                tracing::warn!(
                    plan_id = %plan.id,
                    attempt = attempt,
                    budget = budget,
                    error = %last_error,
                    "independent plan critique violated the response schema"
                );
                messages.push(ChatMessage::user(critique_healing_directive(&last_error)));
            }
        }
    }

    Err(CritiqueFailure::Unreadable(format!(
        "The independent critique did not answer a payload the response schema accepts after {budget} attempt(s): {last_error}"
    )))
}

/// Collects the raw text of one structured provider round.
///
/// The critique is not streamed to the client, so the round only accumulates the
/// payload it will decode; a voided attempt drops the payload it had already
/// accumulated instead of publishing a withdrawal for text nobody saw.
async fn collect_structured_round(
    state: &Arc<AppState>,
    operation: &OperationGuard,
    provider: Arc<dyn LlmProvider>,
    tier: &ArchitectTier,
    messages: Vec<ChatMessage>,
    session_id: Option<String>,
    op_id: &str,
) -> Result<String, CritiqueFailure> {
    let usage = state.begin_usage(op_id).await;
    let (tx, mut rx) = mpsc::channel(64);
    let cancel_rx = operation.cancel_receiver();
    let stream_cancel = cancel_rx.clone();
    let model_name = tier.model.name.clone();
    let effort = tier.effort;
    let response_format = critique_response_format();
    let task = tokio::spawn(async move {
        provider
            .stream_chat_with_session(
                &model_name,
                messages,
                Some(response_format),
                effort,
                session_id.as_deref(),
                tx,
                stream_cancel,
            )
            .await
    });
    // The round is reachable only through the operation slot, so a hard cancel
    // and the shutdown drain abort a provider that ignores the signal.
    operation.set_abort_handle(task.abort_handle());

    let mut raw = String::new();
    let mut stream_error: Option<String> = None;
    let mut cancelled = false;
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::TextDelta(delta) => raw.push_str(&delta),
            StreamEvent::Usage(reported) => usage.observe(&reported).await,
            StreamEvent::ResetStream => raw.clear(),
            StreamEvent::Cancelled => {
                cancelled = true;
                break;
            }
            StreamEvent::Error(message) => {
                stream_error = Some(message);
                break;
            }
            _ => {}
        }
    }
    operation.clear_abort_handle();

    // Both terminal rejections reap the streamed round through the shared bounded
    // task-exit confirmation (`crate::reap::abort_and_reap`, governed by the
    // task-exit deadline), so an abandoned provider call cannot outlive the critique
    // and a task that never reaches a suspension point cannot park it.
    if *cancel_rx.borrow() || cancelled {
        let _ = abort_and_reap(op_id, "plan-critique-stream", task).await;
        usage.flush().await;
        return Err(CritiqueFailure::Cancelled);
    }
    if let Some(message) = stream_error {
        let _ = abort_and_reap(op_id, "plan-critique-stream", task).await;
        usage.flush().await;
        return Err(CritiqueFailure::Failed(message));
    }
    // A completed round is awaited through the shared bounded task-exit
    // confirmation (`crate::reap::await_bounded`, governed by the task-exit
    // deadline): a task that stops yielding after its last event becomes a typed
    // stream failure carrying the recorded expiry diagnostic instead of parking the
    // critique forever.
    let completion = await_bounded(op_id, "plan-critique-stream", task).await;
    usage.flush().await;
    match completion {
        Awaited::Joined(Ok(Ok(()))) => Ok(raw),
        Awaited::Joined(Ok(Err(err))) => Err(CritiqueFailure::Failed(err.to_string())),
        Awaited::Joined(Err(join_error)) => Err(CritiqueFailure::Failed(format!(
            "The independent critique task terminated unexpectedly: {join_error}"
        ))),
        Awaited::Orphaned => {
            let message = format!(
                "The independent critique task did not stop within the {} ms task-exit deadline",
                crate::reap::task_exit_deadline().as_millis()
            );
            Err(CritiqueFailure::Failed(message))
        }
    }
}

/// Converts a beat that issued no horizon into the terminal response of the run.
async fn finish_wave_failure(
    req_id: RequestId,
    state: &Arc<AppState>,
    operation_title: &str,
    plan: &Plan,
    history_turn_prompt: &str,
    history_context_paths: &[String],
    failure: WaveFailure,
) -> Response {
    cleanup_transient_context(state).await;
    match failure {
        WaveFailure::Cancelled => {
            tracing::warn!(
                plan_id = %plan.id,
                operation = operation_title,
                "two-phase plan synthesis was cancelled"
            );
            Response::err(
                req_id,
                errors::CANCELLED,
                format!("{} was cancelled", operation_title),
            )
        }
        WaveFailure::Failed(message) => Response::err(req_id, errors::MODEL_ERROR, message),
        WaveFailure::EmptyHorizon => Response::err(
            req_id,
            errors::EMPTY_ITEMS,
            "The synthesis reported a horizon status without any steps",
        ),
        WaveFailure::Verdict { kind, message } => {
            let label = kind.label();
            tracing::warn!(
                plan_id = %plan.id,
                verdict = label,
                reason = %message,
                "two-phase plan synthesis reached a verdict without a horizon"
            );
            record_synthesis_history(
                state,
                history_turn_prompt.to_string(),
                history_context_paths.to_vec(),
                message.clone(),
                Some(format!("{}: {} ({})", operation_title, label, plan.title)),
            )
            .await;
            Response::ok_typed(req_id, &PlanRefineResult::Blocked { reason: message })
        }
    }
}

/// Composes the task statement of the drafting beat.
fn compose_draft_task(plan: &Plan, directives: Option<&str>) -> String {
    let mut task = build_plan_horizon_system_prompt();
    task.push_str("\n\n---\n\n");
    task.push_str(&build_plan_horizon_user_prompt(plan, None, directives));
    task
}

/// Composes the task statement of the reconciliation beat.
fn compose_reconciliation_task(
    plan: &Plan,
    draft: &PlanRefinementOutput,
    critique: &PlanCritiqueOutput,
    directives: Option<&str>,
) -> String {
    let mut task = build_plan_horizon_system_prompt();
    task.push_str("\n\n---\n\n");
    task.push_str(&build_plan_reconciliation_prompt(
        plan, draft, critique, directives,
    ));
    task
}

/// Renders the developer-facing summary of one independent audit.
fn critique_banner(critique: &PlanCritiqueOutput) -> String {
    let mut banner = format!(
        "\n\n---\n**[Independent Critique]** `{}`\n",
        critique.verdict.as_str()
    );
    let summary = critique.summary.trim();
    if !summary.is_empty() {
        banner.push_str(&format!("\n{summary}\n"));
    }
    for finding in &critique.findings {
        let target = finding
            .target
            .as_deref()
            .map(str::trim)
            .filter(|target| !target.is_empty())
            .map(|target| format!("#{target}"))
            .unwrap_or_else(|| "(whole draft)".to_string());
        banner.push_str(&format!(
            "\n- [{}] {}: {}",
            finding.category.as_str(),
            target,
            finding.detail.trim()
        ));
    }
    banner.push_str("\n\n");
    banner
}

/// Directive appended to the critique conversation after a schema violation.
fn critique_healing_directive(error: &str) -> String {
    format!(
        "<response_directive>\nYour previous reply did not satisfy the response schema ({error}). Regenerate the whole payload strictly according to the specified JSON schema: `verdict`, `summary`, and `findings` are required, the verdict accepts only `approved` and `changes_required`, and every finding requires `category`, `target`, and `detail`.\n</response_directive>"
    )
}

/// Projects the declarative critique definition into a provider response format.
fn critique_response_format() -> ResponseFormat {
    let definition = plan_critique_response_schema_definition();
    let name = definition
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("plan_critique")
        .to_string();
    let description = definition
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let strict = definition
        .get("strict")
        .and_then(serde_json::Value::as_bool);
    let schema = definition
        .get("schema")
        .cloned()
        .unwrap_or_else(plan_critique_response_schema);
    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaDefinition {
            name,
            description,
            schema: Cow::Owned(schema),
            strict,
        },
    }
}

/// Captures the prompt and the context paths a synthesis actually observed.
async fn capture_history_coordinates(
    state: &Arc<AppState>,
    operation_title: &str,
    plan: &Plan,
    directives: Option<&str>,
) -> (String, Vec<String>) {
    let context_paths: Vec<String> = state
        .context
        .read()
        .await
        .get_state()
        .items
        .into_iter()
        .map(|item| item.path)
        .collect();
    let mut prompt = format!("# {}: [{}] {}\n", operation_title, plan.id, plan.title);
    if let Some(directives) = directives.map(str::trim).filter(|value| !value.is_empty()) {
        prompt.push_str(&format!("\nDirectives: {directives}\n"));
    }
    (prompt, context_paths)
}

/// Opens a model window for one beat of the pipeline.
fn emit_model_window_start(state: &Arc<AppState>, operation_id: &str, tier: &ArchitectTier) {
    emit(
        &state.out,
        events::MODEL_STARTED,
        serde_json::to_value(ModelStartedEvent {
            operation_id: operation_id.to_string(),
            model: tier.model.clone(),
            role: Some(ModelRole::Architect),
            alias: None,
            effort: tier.effort,
            plan_context: None,
            work_context: None,
            is_batch: false,
        })
        .ok(),
    );
}

/// Builds the identity of the plan a creation pipeline issues.
///
/// The directives are the only coordinate the request carries, so the title is
/// their first readable line and the identifier is its storage slug. A slug a
/// stored plan already holds is suffixed deterministically, so a second creation
/// never overwrites an existing plan.
fn created_plan_draft(existing: &[Plan], directives: &str) -> Plan {
    let title = plan_title_from_directives(directives);
    let id = unique_plan_id(existing, &slugify(&title));
    let now = unix_now();
    Plan::from_parts(
        id,
        title,
        Some(directives.trim().to_string()),
        now,
        now,
        Vec::new(),
        Vec::new(),
    )
}

/// Reads the title of a created plan from developer directives.
fn plan_title_from_directives(directives: &str) -> String {
    let line = directives
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| FALLBACK_PLAN_TITLE.to_string());
    if line.chars().count() <= DERIVED_TITLE_MAX_CHARS {
        line
    } else {
        line.chars().take(DERIVED_TITLE_MAX_CHARS).collect()
    }
}

/// Canonical storage slug of a plan title.
///
/// The identifier is what plan storage maps onto a file name, so it keeps only
/// the characters the storage sanitizer preserves: ASCII letters, digits, and the
/// dash that separates the words of the title.
fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut separated = false;
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            separated = false;
        } else if !separated && !slug.is_empty() {
            slug.push('-');
            separated = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "plan".to_string()
    } else {
        slug.to_string()
    }
}

/// Returns `base` or the first free suffixed spelling of it.
///
/// The bound is the number of stored plans, so the pigeonhole principle
/// guarantees a free candidate without scanning the storage a second time.
fn unique_plan_id(existing: &[Plan], base: &str) -> String {
    let taken = |candidate: &str| {
        existing
            .iter()
            .any(|plan| plan.id.eq_ignore_ascii_case(candidate))
    };
    if !taken(base) {
        return base.to_string();
    }
    for suffix in 2..=existing.len() + 2 {
        let candidate = format!("{base}-{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", existing.len() + 2)
}

/// Removes concept references a plan without a concept tree cannot resolve.
fn strip_unresolved_concept_refs(items: &mut [PlanItem]) -> usize {
    let mut dropped = 0usize;
    for item in items.iter_mut() {
        dropped += item.concept_refs.len();
        item.concept_refs.clear();
        dropped += strip_unresolved_concept_refs(&mut item.children);
    }
    dropped
}

/// Current Unix time in whole seconds.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a stored plan carrying only the identity a lookup reads.
    fn stored(id: &str) -> Plan {
        Plan::from_parts(
            id.to_string(),
            id.to_string(),
            None,
            0,
            0,
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn a_slug_keeps_only_the_characters_the_storage_sanitizer_preserves() {
        assert_eq!(
            slugify("Plan: two-phase / planning"),
            "plan-two-phase-planning"
        );
        assert_eq!(slugify("  "), "plan");
        assert_eq!(slugify("Типизированные диалоги"), "plan");
    }

    #[test]
    fn the_title_of_a_created_plan_is_the_first_readable_line() {
        assert_eq!(
            plan_title_from_directives("\n\n  Harden   the turn   boundary \nsecond line"),
            "Harden the turn boundary"
        );
        assert_eq!(plan_title_from_directives("   \n\t"), FALLBACK_PLAN_TITLE);
    }

    #[test]
    fn a_taken_slug_is_suffixed_instead_of_overwriting_a_stored_plan() {
        let existing = vec![stored("plan")];
        assert_eq!(unique_plan_id(&existing, "plan"), "plan-2");
        assert_eq!(unique_plan_id(&existing, "other"), "other");

        let pair = vec![stored("plan"), stored("plan-2")];
        assert_eq!(unique_plan_id(&pair, "plan"), "plan-3");
    }

    #[test]
    fn a_created_plan_drops_concept_references_it_cannot_resolve() {
        let mut item = PlanItem {
            id: "1".to_string(),
            title: "Step".to_string(),
            concept_refs: vec!["c-1".to_string()],
            files: vec!["src/lib.rs".to_string()],
            children: vec![PlanItem {
                id: "1.1".to_string(),
                title: "Child".to_string(),
                concept_refs: vec!["c-2".to_string()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let dropped = strip_unresolved_concept_refs(std::slice::from_mut(&mut item));

        assert_eq!(dropped, 2);
        assert!(item.concept_refs.is_empty());
        assert!(item.children[0].concept_refs.is_empty());
    }
}
