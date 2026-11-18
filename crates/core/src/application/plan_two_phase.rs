//! Headless draft, independent critique, reconciliation, and persistence.
use std::borrow::Cow;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::{
    abort_and_reap_with_deadline, await_with_deadline, Application, Awaited, OperationGuard,
    UsageTotalsSnapshot,
};
use crate::application::plan_horizon::{
    collect_plan_context_files, PlanHorizonContour, PlanHorizonError, PlanHorizonFailure,
    PlanHorizonObserver,
};
use crate::config::{AppConfig, ModelsConfig};
use crate::control::refine::{StructuredRefineOutcome, StructuredRefineStatus};
use crate::models::{ArchitectTier, ModelSelection, ProviderKind};
use crate::plan::prompt::{
    build_plan_critique_prompt, build_plan_critique_system_prompt,
    build_plan_horizon_system_prompt, build_plan_horizon_user_prompt,
    build_plan_reconciliation_prompt, parse_plan_critique_output, plan_critique_response_schema,
    plan_critique_response_schema_definition, resolve_architect_tier, PlanCritiqueOutput,
    PlanRefinementOutput,
};
use crate::plan::storage::PlanStorage;
use crate::plan::{find_duplicate_item_ids, prompt::format_architect_tier};
use crate::plan::{Plan, PlanItem, PlanSynthesisResult};
use crate::providers::{
    format_session_id, ChatMessage, JsonSchemaDefinition, LlmProvider, ResponseFormat, StreamEvent,
    SESSION_PREFIX_ARCHITECT,
};
use crate::workflow::WorkflowOptions;
use tokio::sync::mpsc;
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
pub struct TwoPhaseRequest {
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

/// Publication and provider bindings for independent plan synthesis.
#[async_trait::async_trait]
pub trait TwoPhaseObserver: PlanHorizonObserver {
    async fn critic_provider(
        &self,
        config: &AppConfig,
        provider: ProviderKind,
    ) -> Result<Arc<dyn LlmProvider>, String>;
    fn usage_totals(&self, id: &str, snapshot: &UsageTotalsSnapshot);
    fn critique_round(&self, id: &str, round: usize, budget: usize);
    fn cancelled(&self, id: &str);
    fn failed(&self, id: &str, message: &str);
    async fn publish_issued(&self, plan: &Plan, creation: bool);
}
struct Runtime {
    application: Application,
    observer: Arc<dyn TwoPhaseObserver>,
    exit_deadline: std::time::Duration,
}
impl Application {
    pub async fn prepare_plan_creation(&self, directives: &str) -> Result<Plan, String> {
        let root = self.repository_root().await;
        let existing = offload_plan(move || PlanStorage::load_all(&root)).await?;
        Ok(created_plan_draft(&existing, directives))
    }
    pub async fn execute_two_phase_plan(
        &self,
        operation: &OperationGuard,
        request: TwoPhaseRequest,
        observer: Arc<dyn TwoPhaseObserver>,
        exit_deadline: std::time::Duration,
    ) -> Result<PlanSynthesisResult, PlanHorizonError> {
        if !self.owns_operation(operation) {
            return Err(PlanHorizonError {
                kind: PlanHorizonFailure::Model,
                message: "Plan synthesis requires an active operation for this session".into(),
            });
        }
        execute_two_phase_plan_synthesis(
            &Runtime {
                application: self.clone(),
                observer,
                exit_deadline,
            },
            request,
            operation,
        )
        .await
    }
}
async fn offload_plan<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("blocking task failed: {e}"))?
        .map_err(|e| format!("{e:#}"))
}
async fn cleanup_transient_context(state: &Runtime) {
    if let Ok(snapshot) = state
        .application
        .clear_context(crate::api::ContextClearScope::Transient)
        .await
    {
        state.observer.context_changed(snapshot);
    }
}
async fn preload_readonly_files(state: &Runtime, root: &std::path::Path, files: &[String]) {
    if let Ok(admission) = state
        .application
        .preload_declared_context(root.into(), Vec::new(), files.to_vec())
        .await
    {
        if !admission.outcome.added.is_empty() {
            state.observer.context_changed(admission.state);
        }
    }
}
async fn record_synthesis_history(
    state: &Runtime,
    prompt: String,
    paths: Vec<String>,
    response: String,
    summary: Option<String>,
) {
    let _ = state
        .application
        .record_history_exchange(prompt, paths, response, summary)
        .await;
}
/// Runs the draft, critique, and reconciliation beats of one synthesis.
///
/// The pipeline is sequential and fail-fast: a beat that issues no horizon ends
/// the run through [`finish_wave_failure`], so the issued document is written
/// exactly once, by the last beat that produced a horizon.
async fn execute_two_phase_plan_synthesis(
    state: &Runtime,
    request: TwoPhaseRequest,
    operation: &OperationGuard,
) -> Result<PlanSynthesisResult, PlanHorizonError> {
    let TwoPhaseRequest {
        plan,
        directives,
        planner_model,
        critic_model,
        skip_critique,
        creation,
        operation_title,
    } = request;
    let repo_root = state.application.repository_root().await;

    let (config, planner_tier, planner_options, models, critic_tier, critic_session) = {
        let cfg = state.application.configuration().await.config.clone();
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
        return Err(PlanHorizonError {
            kind: PlanHorizonFailure::InvalidTree,
            message: format!(
                "The synthesized plan contains a duplicate or empty step ID: '{duplicate}'"
            ),
        });
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
        return Err(PlanHorizonError {
            kind: PlanHorizonFailure::Cancelled,
            message: format!("{} was cancelled", operation_title),
        });
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
                return Err(PlanHorizonError {
                    kind: PlanHorizonFailure::Save,
                    message: err,
                });
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
                return Err(PlanHorizonError {
                    kind: PlanHorizonFailure::Save,
                    message: err,
                });
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

    state.observer.publish_issued(&stored, creation).await;

    // The material the synthesis widened is released with the plan it produced.
    cleanup_transient_context(state).await;

    Ok(PlanSynthesisResult::Success {
        plan: stored,
        explanation: Some(explanation),
    })
}

/// Runs one Planner beat and interprets its typed terminal status.
#[allow(clippy::too_many_arguments)]
async fn run_planner_wave(
    state: &Runtime,
    operation: &OperationGuard,
    task: String,
    intro_header: String,
    options: WorkflowOptions,
    models: ModelsConfig,
    tier: ArchitectTier,
    budget_hint: &str,
) -> Result<PlannerWave, WaveFailure> {
    let op_id = format!("op-{}", crate::api::next_operation_id());
    state.observer.started(&op_id, &tier);

    // The contour works on its own clone of the context and owns the auto layer it
    // widens, so the adapter hands it a snapshot and merges the result back.
    let context_snapshot = state.application.context_for_turn().await;
    let (result, summary) = state
        .observer
        .run(
            operation,
            PlanHorizonContour {
                operation_id: op_id.clone(),
                task,
                context: context_snapshot,
                options,
                models_config: models,
                tier,
                intro_header,
                budget_hint: budget_hint.to_string(),
            },
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
        state.observer.finished(&op_id, summary.assistant_text);
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
    state: &Runtime,
    operation: &OperationGuard,
    config: &AppConfig,
    plan: &Plan,
    draft: &PlanRefinementOutput,
    directives: Option<&str>,
    tier: &ArchitectTier,
    session_id: Option<String>,
) -> Result<PlanCritiqueOutput, WaveFailure> {
    let provider = match state
        .observer
        .critic_provider(config, tier.model.provider)
        .await
    {
        Ok(provider) => provider,
        Err(message) => return Err(WaveFailure::Failed(message)),
    };

    let op_id = format!("op-{}", crate::api::next_operation_id());
    state.observer.started(&op_id, tier);
    tracing::info!(plan_id = %plan.id, model = %tier.model, "independent plan critique started");

    match run_critique_rounds(
        state, operation, provider, plan, draft, directives, tier, session_id, &op_id,
    )
    .await
    {
        Ok(critique) => {
            let banner = critique_banner(&critique);
            state.observer.finished(&op_id, banner);
            Ok(critique)
        }
        Err(CritiqueFailure::Cancelled) => {
            state.observer.cancelled(&op_id);
            Err(WaveFailure::Cancelled)
        }
        Err(CritiqueFailure::Failed(message)) => {
            state.observer.failed(&op_id, &message);
            Err(WaveFailure::Failed(message))
        }
        Err(CritiqueFailure::Unreadable(message)) => {
            let banner =
                format!("\n\n---\n⛔ **[Independent Critique Unavailable]** {message}\n\n");
            state.observer.finished(&op_id, banner);
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
    state: &Runtime,
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
        state.observer.critique_round(op_id, attempt, budget);

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
    state: &Runtime,
    operation: &OperationGuard,
    provider: Arc<dyn LlmProvider>,
    tier: &ArchitectTier,
    messages: Vec<ChatMessage>,
    session_id: Option<String>,
    op_id: &str,
) -> Result<String, CritiqueFailure> {
    let publisher = state.observer.clone();
    let usage = state
        .application
        .begin_usage(op_id, move |id, snapshot| {
            publisher.usage_totals(id, snapshot)
        })
        .await;
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
    // task-exit confirmation (`crate::api::abort_and_reap_with_deadline`, governed by the
    // task-exit deadline), so an abandoned provider call cannot outlive the critique
    // and a task that never reaches a suspension point cannot park it.
    if *cancel_rx.borrow() || cancelled {
        let _ =
            abort_and_reap_with_deadline(op_id, "plan-critique-stream", state.exit_deadline, task)
                .await;
        usage.flush().await;
        return Err(CritiqueFailure::Cancelled);
    }
    if let Some(message) = stream_error {
        let _ =
            abort_and_reap_with_deadline(op_id, "plan-critique-stream", state.exit_deadline, task)
                .await;
        usage.flush().await;
        return Err(CritiqueFailure::Failed(message));
    }
    // A completed round is awaited through the shared bounded task-exit
    // confirmation (`crate::api::await_with_deadline`, governed by the task-exit
    // deadline): a task that stops yielding after its last event becomes a typed
    // stream failure carrying the recorded expiry diagnostic instead of parking the
    // critique forever.
    let completion =
        await_with_deadline(op_id, "plan-critique-stream", state.exit_deadline, task).await;
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
                state.exit_deadline.as_millis()
            );
            Err(CritiqueFailure::Failed(message))
        }
    }
}

/// Converts a beat that issued no horizon into the terminal response of the run.
async fn finish_wave_failure(
    state: &Runtime,
    operation_title: &str,
    plan: &Plan,
    history_turn_prompt: &str,
    history_context_paths: &[String],
    failure: WaveFailure,
) -> Result<PlanSynthesisResult, PlanHorizonError> {
    cleanup_transient_context(state).await;
    match failure {
        WaveFailure::Cancelled => {
            tracing::warn!(
                plan_id = %plan.id,
                operation = operation_title,
                "two-phase plan synthesis was cancelled"
            );
            Err(PlanHorizonError {
                kind: PlanHorizonFailure::Cancelled,
                message: format!("{} was cancelled", operation_title),
            })
        }
        WaveFailure::Failed(message) => Err(PlanHorizonError {
            kind: PlanHorizonFailure::Model,
            message,
        }),
        WaveFailure::EmptyHorizon => Err(PlanHorizonError {
            kind: PlanHorizonFailure::EmptyItems,
            message: ("The synthesis reported a horizon status without any steps").into(),
        }),
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
            Ok(PlanSynthesisResult::Blocked { reason: message })
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
    state: &Runtime,
    operation_title: &str,
    plan: &Plan,
    directives: Option<&str>,
) -> (String, Vec<String>) {
    let context_paths: Vec<String> = state
        .application
        .context_for_turn()
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
