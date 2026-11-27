//! Plan synthesis from admission through durable publication.
mod replan;

use super::PlanResources;
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::configuration::{Configuration, ConfigurationSnapshot};
use crate::application::documents::Documents;
use crate::application::PlanExecutionEnvironment;
use crate::workflows::architect::refinement::{
    synthesize_horizon, HorizonSynthesisRequest, WaveFailure,
};
use crate::workflows::architect::ArchitectScope;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub use replan::{collect_plan_context_files, PlanHorizonError, PlanHorizonFailure};
use tauqe_core::models::ModelSelection;
use tauqe_core::plan::prompt::{build_plan_horizon_system_prompt, build_plan_horizon_user_prompt};
use tauqe_core::plan::{Plan, PlanItem, PlanSynthesisResult};
/// Longest plan title the pipeline derives from developer directives.
const DERIVED_TITLE_MAX_CHARS: usize = 120;

/// Title of a plan whose directives carry no readable line.
const FALLBACK_PLAN_TITLE: &str = "Untitled plan";

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
        // Keep the operation and its publication owner alive after the RPC waiter exits.
        let resources = PlanResources {
            step: crate::workflows::step::run::StepResources {
                context: self.step.context.clone(),
                execution: self.step.execution.clone(),
            },
            history: self.history.clone(),
        };
        let snapshot = configuration.snapshot().await;
        tokio::spawn(async move {
            let context = resources.step.context.clone();
            let bound = context.activate_manual_publication(&operation, observer.clone());
            let result = if bound {
                resources
                    .synthesize_owned(snapshot, input, observer, exit_deadline, &operation)
                    .await
            } else {
                Err(PlanSynthesisError::Load(
                    "Could not bind plan synthesis context publication".into(),
                ))
            };
            if bound {
                if let Err(error) = context.clear_transient_context().await {
                    tracing::warn!(%error, "Failed to clear plan synthesis context");
                }
                context.close_manual_publication(&operation);
            }
            result
        })
        .await
        .map_err(|error| {
            PlanSynthesisError::Load(format!("Plan synthesis supervisor failed: {error}"))
        })?
    }

    async fn synthesize_owned(
        &self,
        snapshot: ConfigurationSnapshot,
        input: PlanSynthesisInput,
        observer: Arc<dyn ApplicationObserver>,
        exit_deadline: std::time::Duration,
        operation: &OperationGuard,
    ) -> Result<PlanSynthesisResult, PlanSynthesisError> {
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
                let environment = PlanExecutionEnvironment::new(snapshot, operation);
                return Ok(state
                    .execute_plan_horizon(
                        operation,
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
        let environment = PlanExecutionEnvironment::new(snapshot, operation);
        Ok(execute_two_phase_plan_synthesis(&state, request, operation, &environment).await?)
    }
}

async fn cleanup_transient_context(state: &Runtime<'_>) {
    let _ = state.resources.clear_transient_context().await;
}
async fn preload_readonly_files(state: &Runtime<'_>, plan: &Plan) {
    let context = &state.resources.step.context;
    let _ = context
        .preload_declared_context(
            context.repository_root().await,
            Vec::new(),
            collect_plan_context_files(plan),
        )
        .await;
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
    execute_two_phase_plan_synthesis_inner(state, request, operation, environment).await
}

async fn execute_two_phase_plan_synthesis_inner(
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
    tracing::info!(
        plan_id = %plan.id,
        operation = operation_title,
        creation = creation,
        critique = critic_model.is_some() && !skip_critique,
        "two-phase plan synthesis started"
    );

    // The synthesis starts from a released transient context: only the files the
    // plan already declares reach the drafting beat, so a modification never
    // inherits the material a previous turn discovered.
    cleanup_transient_context(state).await;
    preload_readonly_files(state, &plan).await;

    let (history_turn_prompt, history_context_paths) =
        capture_history_coordinates(state, operation_title, &plan, directives.as_deref()).await;

    let horizon = match synthesize_horizon(
        &state.resources.step.execution,
        &state.resources.step.context,
        operation,
        environment,
        state.observer.clone(),
        state.exit_deadline,
        HorizonSynthesisRequest {
            draft_task: compose_draft_task(&plan, directives.as_deref()),
            directives: directives.clone(),
            scope: ArchitectScope::Plan(plan.id.clone()),
            planner_model,
            critic_model,
            skip_critique,
            operation_title,
            plan: plan.clone(),
        },
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

    let explanation = horizon.output.explanation.trim().to_string();
    let items = horizon.output.items;
    let architect = horizon.tier;
    let _receipts = horizon.receipts;
    let terminal = horizon.terminal;

    // The write below is destructive for the uncompleted horizon and exclusive
    // for a creation, so the authoritative cancellation signal is re-checked at
    // the seam where the document reaches disk.
    if operation.hard_cancellation().is_cancelled() {
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
    terminal.publish(state.observer.as_ref());

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

/// Converts a beat that issued no horizon into the terminal response of the run.
async fn finish_wave_failure(
    state: &Runtime<'_>,
    operation_title: &str,
    plan: &Plan,
    history_turn_prompt: &str,
    history_context_paths: &[String],
    failure: WaveFailure,
) -> Result<PlanSynthesisResult, PlanHorizonError> {
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
        WaveFailure::InvalidTree(duplicate) => Err(PlanHorizonError {
            kind: PlanHorizonFailure::InvalidTree,
            message: format!(
                "The synthesized plan contains a duplicate or empty step ID: '{duplicate}'"
            ),
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
