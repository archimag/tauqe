//! Plan synthesis from admission through durable publication.
mod replan;

use super::PlanResources;
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::configuration::Configuration;
use crate::application::documents::Documents;
use crate::application::structured_task::{self, StructuredTaskError};
use crate::application::{PlanExecutionEnvironment, StructuredExecution};
use crate::workflows::architect::{
    self, publish_architect_started, ArchitectScope, PlanHorizonContour,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub use replan::{collect_plan_context_files, PlanHorizonError, PlanHorizonFailure};
use tauqe_core::execution::structured::StructuredError;
use tauqe_core::models::{ArchitectTier, ModelSelection};
use tauqe_core::plan::prompt::critique::plan_critique_contract;
use tauqe_core::plan::prompt::{
    build_plan_critique_prompt, build_plan_horizon_system_prompt, build_plan_horizon_user_prompt,
    build_plan_reconciliation_prompt, resolve_architect_tier, PlanCritiqueOutput,
    PlanRefinementOutput, StructuredRefineOutcome, StructuredRefineStatus,
};
use tauqe_core::plan::{find_duplicate_item_ids, prompt::format_architect_tier};
use tauqe_core::plan::{Plan, PlanItem, PlanSynthesisResult};
use tauqe_core::providers::{ChatMessage, StreamEvent};
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
struct TwoPhaseRequest {
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

struct Runtime<'a> {
    resources: &'a PlanResources,
    documents: Documents,
    observer: Arc<dyn ApplicationObserver>,
    exit_deadline: std::time::Duration,
}
pub enum PlanSynthesisInput {
    Create {
        directives: String,
        planner_model: Option<ModelSelection>,
        critic_model: Option<ModelSelection>,
        skip_critique: bool,
    },
    Refine {
        plan_id: String,
        directives: Option<String>,
        planner_model: Option<ModelSelection>,
        critic_model: Option<ModelSelection>,
        skip_critique: bool,
    },
    Replan {
        plan_id: String,
        step_id: String,
        reason: Option<String>,
        model: Option<ModelSelection>,
        directives: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum PlanSynthesisError {
    #[error("Another operation is already running")]
    Busy,
    #[error("Plan creation directives cannot be empty")]
    EmptyDirectives,
    #[error("{0}")]
    Load(String),
    #[error(transparent)]
    Preparation(#[from] crate::api::PlanPreparationError),
    #[error(transparent)]
    Execution(#[from] PlanHorizonError),
}

impl PlanResources {
    pub(crate) async fn synthesize(
        &self,
        configuration: &Configuration,
        input: PlanSynthesisInput,
        observer: Arc<dyn ApplicationObserver>,
        exit_deadline: std::time::Duration,
    ) -> Result<PlanSynthesisResult, PlanSynthesisError> {
        let operation = self
            .step
            .execution
            .operations
            .begin()
            .map_err(|_| PlanSynthesisError::Busy)?;
        let root = self.step.context.repository_root().await;
        let state = Runtime {
            resources: self,
            documents: Documents::new(&root),
            observer,
            exit_deadline,
        };
        let documents = &state.documents;
        let request = match input {
            PlanSynthesisInput::Create {
                directives,
                planner_model,
                critic_model,
                skip_critique,
            } => {
                let directives = directives.trim().to_owned();
                if directives.is_empty() {
                    return Err(PlanSynthesisError::EmptyDirectives);
                }
                let existing = documents.plans().await.map_err(PlanSynthesisError::Load)?;
                TwoPhaseRequest {
                    plan: created_plan_draft(&existing, &directives),
                    directives: Some(directives),
                    planner_model,
                    critic_model,
                    skip_critique,
                    creation: true,
                    operation_title: "Plan Creation",
                }
            }
            PlanSynthesisInput::Refine {
                plan_id,
                directives,
                planner_model,
                critic_model,
                skip_critique,
            } => TwoPhaseRequest {
                plan: documents.prepare_plan_refinement(plan_id).await?,
                directives,
                planner_model,
                critic_model,
                skip_critique,
                creation: false,
                operation_title: "Deep Plan Refinement",
            },
            PlanSynthesisInput::Replan {
                plan_id,
                step_id,
                reason,
                model,
                directives,
            } => {
                let prepared = documents.prepare_plan_replan(plan_id, step_id).await?;
                let environment =
                    PlanExecutionEnvironment::new(configuration.snapshot().await, &operation);
                return Ok(state
                    .execute_plan_horizon(
                        &operation,
                        &environment,
                        replan::PlanHorizonRequest {
                            plan: prepared.plan,
                            model,
                            instructions: directives,
                            diagnostics: Some(replan::PlanHorizonDiagnosticsOwned {
                                blocked_step_id: prepared.step.id,
                                blocked_reason: reason,
                                toolchain_status: None,
                            }),
                            operation_title: "Plan Replanning".into(),
                        },
                    )
                    .await?);
            }
        };
        let environment = PlanExecutionEnvironment::new(configuration.snapshot().await, &operation);
        Ok(execute_two_phase_plan_synthesis(&state, request, &operation, &environment).await?)
    }
}

async fn cleanup_transient_context(state: &Runtime<'_>) {
    if let Ok(snapshot) = state.resources.clear_transient_context().await {
        state
            .observer
            .publish("", ApplicationEvent::Context(snapshot));
    }
}
async fn preload_readonly_files(state: &Runtime<'_>, plan: &Plan) {
    let context = &state.resources.step.context;
    if let Ok(admission) = context
        .preload_declared_context(
            context.repository_root().await,
            Vec::new(),
            collect_plan_context_files(plan),
        )
        .await
    {
        if !admission.outcome.added.is_empty() {
            state
                .observer
                .publish("", ApplicationEvent::Context(admission.state));
        }
    }
}
async fn record_synthesis_history(
    state: &Runtime<'_>,
    prompt: String,
    paths: Vec<String>,
    response: String,
    summary: Option<String>,
) {
    let _ = state
        .resources
        .history
        .record_history_exchange(prompt, paths, response, summary)
        .await;
}
/// Runs the draft, critique, and reconciliation beats of one synthesis.
///
/// The pipeline is sequential and fail-fast: a beat that issues no horizon ends
/// the run through [`finish_wave_failure`], so the issued document is written
/// exactly once, by the last beat that produced a horizon.
async fn execute_two_phase_plan_synthesis(
    state: &Runtime<'_>,
    request: TwoPhaseRequest,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
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
    let (planner_tier, critic_tier) = {
        let cfg = environment.config();
        // A directed modification is not a replanning wave around a blocked step,
        // so the drafting beat starts from the base rung or from the explicit
        // developer selection, never from an escalated position.
        let (planner_tier, _) = resolve_architect_tier(
            cfg,
            plan.implementation.architect.as_ref(),
            planner_model.as_ref(),
            None,
            plan.implementation.last_replanned_step.as_deref(),
        );
        let critic_tier = critic_model
            .as_ref()
            .filter(|_| !skip_critique)
            .map(|selection| cfg.models.resolve_selection_architect_tier(selection));
        (planner_tier, critic_tier)
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
    preload_readonly_files(state, &plan).await;

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
        environment,
        compose_draft_task(&plan, directives.as_deref()),
        draft_header,
        planner_tier.clone(),
        ArchitectScope::Plan(plan.id.clone()),
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
            environment,
            &plan,
            &horizon.output,
            directives.as_deref(),
            critic_tier,
            &ArchitectScope::Plan(plan.id.clone()),
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
                        environment,
                        compose_reconciliation_task(
                            &plan,
                            &horizon.output,
                            &critique,
                            directives.as_deref(),
                        ),
                        header,
                        planner_tier.clone(),
                        ArchitectScope::Plan(plan.id.clone()),
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
        let persisted = state.documents.write_new_plan_horizon(draft, items).await;
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
        match state
            .documents
            .write_synthesized_horizon(plan.id.clone(), items, architect, None)
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

    publish_plan(state, "", &stored, creation).await;

    // The material the synthesis widened is released with the plan it produced.
    cleanup_transient_context(state).await;

    Ok(PlanSynthesisResult::Success {
        plan: Box::new(stored),
        explanation: Some(explanation),
    })
}

async fn publish_plan(state: &Runtime<'_>, op_id: &str, plan: &Plan, is_save: bool) {
    state.observer.publish(
        op_id,
        ApplicationEvent::PlanDocument {
            plan: plan.clone(),
            is_save,
        },
    );
    if let Ok((plans, active_id)) = state.documents.plan_list().await {
        state
            .observer
            .publish(op_id, ApplicationEvent::PlanList { plans, active_id });
    }
}

/// Runs one Planner beat and interprets its typed terminal status.
#[allow(clippy::too_many_arguments)]
async fn run_planner_wave(
    state: &Runtime<'_>,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    task: String,
    intro_header: String,
    tier: ArchitectTier,
    scope: ArchitectScope,
    budget_hint: &str,
) -> Result<PlannerWave, WaveFailure> {
    let op_id = format!("op-{}", crate::api::next_operation_id());
    publish_architect_started(state.observer.as_ref(), &op_id, &tier, false);

    // The contour works on its own clone of the context and owns the auto layer it
    // widens, so the adapter hands it a snapshot and merges the result back.
    let step = &state.resources.step;
    let context_snapshot = step.context.context_for_turn().await;
    let (result, summary) = architect::run_contour(
        &step.execution,
        &step.context,
        &step.usage,
        operation,
        environment,
        PlanHorizonContour {
            operation_id: op_id.clone(),
            task,
            context: context_snapshot,
            scope,
            tier,
            intro_header,
            budget_hint: budget_hint.to_string(),
        },
        state.observer.clone(),
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
        state
            .observer
            .publish(&op_id, ApplicationEvent::Finished(summary.assistant_text));
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
    state: &Runtime<'_>,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    plan: &Plan,
    draft: &PlanRefinementOutput,
    directives: Option<&str>,
    tier: &ArchitectTier,
    scope: &ArchitectScope,
) -> Result<PlanCritiqueOutput, WaveFailure> {
    let execution = match environment
        .for_structured(&state.resources.step.execution, operation, tier, scope)
        .await
    {
        Ok(execution) => execution,
        Err(error) => return Err(WaveFailure::Failed(error.to_string())),
    };

    let op_id = format!("op-{}", crate::api::next_operation_id());
    publish_architect_started(state.observer.as_ref(), &op_id, tier, false);
    tracing::info!(plan_id = %plan.id, model = %tier.model, "independent plan critique started");

    match run_critique_rounds(
        state, operation, &execution, plan, draft, directives, tier, &op_id,
    )
    .await
    {
        Ok(critique) => {
            let banner = critique_banner(&critique);
            state
                .observer
                .publish(&op_id, ApplicationEvent::Finished(banner));
            Ok(critique)
        }
        Err(CritiqueFailure::Cancelled) => {
            state
                .observer
                .publish(&op_id, ApplicationEvent::Stream(StreamEvent::Cancelled));
            Err(WaveFailure::Cancelled)
        }
        Err(CritiqueFailure::Failed(message)) => {
            state.observer.publish(
                &op_id,
                ApplicationEvent::Stream(StreamEvent::Error(message.clone())),
            );
            Err(WaveFailure::Failed(message))
        }
        Err(CritiqueFailure::Unreadable(message)) => {
            let banner =
                format!("\n\n---\n⛔ **[Independent Critique Unavailable]** {message}\n\n");
            state
                .observer
                .publish(&op_id, ApplicationEvent::Finished(banner));
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
    state: &Runtime<'_>,
    operation: &OperationGuard,
    execution: &StructuredExecution,
    plan: &Plan,
    draft: &PlanRefinementOutput,
    directives: Option<&str>,
    tier: &ArchitectTier,
    op_id: &str,
) -> Result<PlanCritiqueOutput, CritiqueFailure> {
    let budget = tier.attempts().max(1) as usize;
    let mut messages = vec![ChatMessage::user(build_plan_critique_prompt(
        plan, draft, directives,
    ))];
    let mut last_error = String::new();

    for attempt in 1..=budget {
        state.observer.publish(
            op_id,
            ApplicationEvent::Stream(StreamEvent::TurnPhase {
                phase: tauqe_core::workflow::turn::TurnPhase::Proposal,
                round: Some(attempt),
                max_rounds: Some(budget),
                detail: Some("Independent plan critique".into()),
            }),
        );

        match collect_structured_round(state, operation, execution, messages.clone(), op_id).await {
            Ok(critique) => return Ok(critique),
            Err(CritiqueFailure::Unreadable(err)) => {
                last_error = err;
                tracing::warn!(
                    plan_id = %plan.id,
                    attempt = attempt,
                    budget = budget,
                    error = %last_error,
                    "independent plan critique violated the response schema"
                );
                messages.push(ChatMessage::user(critique_healing_directive(&last_error)));
            }
            Err(other) => return Err(other),
        }
    }

    Err(CritiqueFailure::Unreadable(format!(
        "The independent critique did not answer a payload the response schema accepts after {budget} attempt(s): {last_error}"
    )))
}

/// Collects the typed payload of one structured provider round.
///
/// The critique is not streamed to the client, so the round only accumulates the
/// payload it will decode; a voided attempt drops the payload it had already
/// accumulated instead of publishing a withdrawal for text nobody saw.
async fn collect_structured_round(
    state: &Runtime<'_>,
    operation: &OperationGuard,
    execution: &StructuredExecution,
    messages: Vec<ChatMessage>,
    op_id: &str,
) -> Result<PlanCritiqueOutput, CritiqueFailure> {
    let publisher = state.observer.clone();
    let step = &state.resources.step;
    let usage = step
        .usage
        .begin(&step.context, op_id, move |id, snapshot| {
            publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()))
        })
        .await;
    let completion = structured_task::run_with_session(
        &step.execution,
        operation,
        execution,
        plan_critique_contract(),
        messages,
        op_id,
        state.exit_deadline,
        |event| {
            let usage = &usage;
            async move {
                if let StreamEvent::Usage(reported) = event {
                    usage.observe(&reported).await;
                }
            }
        },
    )
    .await;
    usage.flush().await;
    match completion {
        Ok(output) => Ok(output),
        Err(StructuredTaskError::Cancelled) => Err(CritiqueFailure::Cancelled),
        Err(StructuredTaskError::Request(
            error @ (StructuredError::Empty | StructuredError::Incomplete),
        )) => Err(CritiqueFailure::Unreadable(error.to_string())),
        Err(StructuredTaskError::Request(StructuredError::Decode(err))) => {
            Err(CritiqueFailure::Unreadable(err))
        }
        Err(StructuredTaskError::Request(StructuredError::Cancelled)) => {
            Err(CritiqueFailure::Cancelled)
        }
        Err(
            StructuredTaskError::Request(StructuredError::Failed(err))
            | StructuredTaskError::Stream(err),
        ) => Err(CritiqueFailure::Failed(err)),
        Err(StructuredTaskError::ForeignOperation) => Err(CritiqueFailure::Failed(
            "The independent critique requires an active operation for this session".into(),
        )),
        Err(StructuredTaskError::Task(join_error)) => Err(CritiqueFailure::Failed(format!(
            "The independent critique task terminated unexpectedly: {join_error}"
        ))),
        Err(StructuredTaskError::Orphaned) => {
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
    state: &Runtime<'_>,
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

/// Captures the prompt and the context paths a synthesis actually observed.
async fn capture_history_coordinates(
    state: &Runtime<'_>,
    operation_title: &str,
    plan: &Plan,
    directives: Option<&str>,
) -> (String, Vec<String>) {
    let context_paths: Vec<String> = state
        .resources
        .step
        .context
        .context_state()
        .await
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
