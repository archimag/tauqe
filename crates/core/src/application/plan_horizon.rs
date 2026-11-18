//! Architect horizon preparation, execution policy, validation, and persistence.
use crate::api::{Application, OperationGuard};
use crate::config::ModelsConfig;
use crate::context::ContextManager;
use crate::context::ContextState;
use crate::control::refine::{StructuredRefineOutcome, StructuredRefineStatus};
use crate::models::{ArchitectTier, ModelRole, ModelSelection};
use crate::plan::prompt::{format_architect_escalation_note, PlanHorizonDiagnostics};
use crate::plan::{find_duplicate_item_ids, storage::PlanStorage};
use crate::plan::{Plan, PlanSynthesisResult};
use crate::providers::{format_session_id, SESSION_PREFIX_ARCHITECT};
use crate::workflow::WorkflowOptions;
use std::sync::Arc;

use std::collections::HashSet;

use crate::plan::PlanItem;

use crate::api::normalize_preload_path;

/// Diagnostics describing a failed or blocked step for horizon synthesis.
pub struct PlanHorizonDiagnosticsOwned {
    pub blocked_step_id: String,
    pub blocked_reason: Option<String>,
    pub toolchain_status: Option<String>,
}

/// Recursively collects every declared target (`files`) and reference
/// (`readonly_files`) path across the whole plan, deduplicating normalized
/// paths while preserving first-seen order.
///
/// Leaves are the authoritative holders of declared files (see the leaf-only
/// file invariant), but the walk visits every node so a plan carrying residual
/// parent files still contributes them. The result feeds
/// `preload_readonly_files`, giving the architect visibility of every file an
/// existing step already targets or references rather than a single step's.
pub fn collect_plan_context_files(plan: &Plan) -> Vec<String> {
    fn walk(items: &[PlanItem], out: &mut Vec<String>, seen: &mut HashSet<String>) {
        for item in items {
            for path in item.files.iter().chain(item.readonly_files.iter()) {
                let normalized = normalize_preload_path(path);
                if normalized.is_empty() {
                    continue;
                }
                if seen.insert(normalized.to_string()) {
                    out.push(normalized.to_string());
                }
            }
            walk(&item.children, out, seen);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(&plan.implementation.items, &mut out, &mut seen);
    out
}

#[derive(Debug, Clone, Default)]
pub struct ContourRelaySummary {
    pub assistant_text: String,
    pub cancelled: bool,
    pub error: Option<String>,
}
pub struct PlanHorizonContour {
    pub operation_id: String,
    pub task: String,
    pub context: ContextManager,
    pub options: WorkflowOptions,
    pub models_config: ModelsConfig,
    pub tier: ArchitectTier,
    pub intro_header: String,
    pub budget_hint: String,
}
#[async_trait::async_trait]
pub trait PlanHorizonObserver: Send + Sync {
    fn context_changed(&self, state: ContextState);
    fn started(&self, id: &str, tier: &ArchitectTier);
    fn finished(&self, id: &str, text: String);
    async fn run(
        &self,
        operation: &OperationGuard,
        work: PlanHorizonContour,
    ) -> (Result<StructuredRefineOutcome, String>, ContourRelaySummary);
    async fn publish(&self, plan: &Plan);
}
pub struct PlanHorizonRequest {
    pub plan: Plan,
    pub diagnostics: Option<PlanHorizonDiagnosticsOwned>,
    pub model: Option<ModelSelection>,
    pub instructions: Option<String>,
    pub operation_title: String,
}
#[derive(Debug, Clone, Copy)]
pub enum PlanHorizonFailure {
    Cancelled,
    Model,
    EmptyItems,
    InvalidTree,
    Save,
}
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PlanHorizonError {
    pub kind: PlanHorizonFailure,
    pub message: String,
}
async fn offload_plan<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("blocking task failed: {e}"))?
        .map_err(|e| format!("{e:#}"))
}
async fn cleanup_context(app: &Application, observer: &Arc<dyn PlanHorizonObserver>) {
    if let Ok(state) = app
        .clear_context(crate::api::ContextClearScope::Transient)
        .await
    {
        observer.context_changed(state);
    }
}
async fn preload_readonly_files(
    app: &Application,
    observer: &Arc<dyn PlanHorizonObserver>,
    root: &std::path::Path,
    files: &[String],
) {
    if let Ok(admission) = app
        .preload_declared_context(root.into(), Vec::new(), files.to_vec())
        .await
    {
        if !admission.outcome.added.is_empty() {
            observer.context_changed(admission.state);
        }
    }
}
async fn record_history(
    app: &Application,
    prompt: String,
    paths: Vec<String>,
    message: String,
    summary: Option<String>,
) {
    let _ = app
        .record_history_exchange(prompt, paths, message, summary)
        .await;
}
impl Application {
    pub async fn execute_plan_horizon(
        &self,
        operation: &OperationGuard,
        request: PlanHorizonRequest,
        observer: Arc<dyn PlanHorizonObserver>,
    ) -> Result<PlanSynthesisResult, PlanHorizonError> {
        if !self.owns_operation(operation) {
            return Err(PlanHorizonError {
                kind: PlanHorizonFailure::Model,
                message: "Plan synthesis requires an active operation for this session".into(),
            });
        }
        let application = self;
        let PlanHorizonRequest {
            plan,
            diagnostics,
            model: custom_model,
            instructions,
            operation_title,
        } = request;
        let repo_root = application.repository_root().await;

        tracing::info!(
            plan_id = %plan.id,
            operation = operation_title,
            replanning = diagnostics.is_some(),
            blocked_step = ?diagnostics.as_ref().map(|d| d.blocked_step_id.as_str()),
            "plan horizon synthesis started"
        );

        let (architect_tier, level_up, escalation_excluded, options, models_config) = {
            let cfg = application.configuration().await.config.clone();
            let (architect_tier, level_up) = crate::plan::prompt::resolve_architect_tier(
                &cfg,
                plan.implementation.architect.as_ref(),
                custom_model.as_ref(),
                diagnostics.as_ref().map(|d| d.blocked_step_id.as_str()),
                plan.implementation.last_replanned_step.as_deref(),
            );
            // A manual run on a configuration the automatic ladder excludes cannot
            // escalate, which the published note states instead of leaving the
            // developer to assume a silent downgrade.
            let escalation_excluded = cfg.models.excluded_from_escalation(
                ModelRole::Architect,
                &architect_tier.model,
                architect_tier.effort,
            );
            // Every operational budget of the synthesis — research rounds, file and
            // repomap budgets, the eager-map switch, and the research limits — belongs
            // to the turn, so the contour reads them from one options value instead of
            // a per-request parameter set.
            let mut options = WorkflowOptions::from_app_config(&cfg);
            // Horizon synthesis is an isolated design dialogue: its cache is keyed by
            // the plan, never by the interactive REPL session. The contour rotates the
            // identifier itself when it promotes another model (crates/core/Conventions.md §9.9).
            options.session_id = Some(format_session_id(SESSION_PREFIX_ARCHITECT, &plan.id));
            (
                architect_tier,
                level_up,
                escalation_excluded,
                options,
                cfg.models.clone(),
            )
        };

        // The resolved rung is recorded with the synthesized horizon so a later
        // wave can reproduce or escalate it instead of re-deriving one. A replan
        // wave escalates that recorded configuration rather than retrying the one
        // that already failed, so the transition is journalled once here.
        if let Some(level) = &level_up {
            tracing::warn!(
                plan_id = %plan.id,
                operation = operation_title,
                blocked_step_id = diagnostics
                    .as_ref()
                    .map(|d| d.blocked_step_id.as_str())
                    .unwrap_or_default(),
                from_tier = ?level.from,
                to_tier = ?level.to,
                "architect level up"
            );
        }

        // The synthesis starts from a released transient context: only the files it
        // declares below reach the Architect, so a replanning wave never inherits
        // the material that already failed.
        cleanup_context(application, &observer).await;

        // Guarantee baseline visibility for both deep planning and replanning:
        // collect every existing step's declared target and reference files and
        // load them into the read-only auto layer before the first generation
        // round, so the architect sees the real code instead of inferring it from
        // the repo map alone.
        let plan_files = collect_plan_context_files(&plan);
        preload_readonly_files(application, &observer, &repo_root, &plan_files).await;

        // The user turn and its response are recorded together once the synthesis
        // reaches a terminal outcome, so a cancelled, failed, or rejected round
        // leaves no orphaned user turn in the transcript. The prompt and context
        // paths are captured here because they describe the state the Architect
        // actually saw.
        let (history_turn_prompt, history_context_paths) = {
            let ctx = application.context_for_turn().await;
            let context_paths: Vec<String> = ctx
                .get_state()
                .items
                .into_iter()
                .map(|it| it.path)
                .collect();
            let mut history_turn_prompt =
                format!("# {}: [{}] {}\n", operation_title, plan.id, plan.title);
            if let Some(ref diag) = diagnostics {
                history_turn_prompt.push_str(&format!("\nBlocked Step: #{}", diag.blocked_step_id));
                if let Some(ref r) = diag.blocked_reason {
                    history_turn_prompt.push_str(&format!("\nReason: {}", r));
                }
            }
            if let Some(ins) = &instructions {
                let trimmed = ins.trim();
                if !trimmed.is_empty() {
                    history_turn_prompt.push_str(&format!("\nDirectives: {}\n", trimmed));
                }
            }
            (history_turn_prompt, context_paths)
        };

        // The contour works on its own clone of the context and owns the auto layer it
        // widens, so the adapter hands it a snapshot and the shared launch skeleton
        // merges the result back once the synthesis settles.
        let context_snapshot = application.context_for_turn().await;

        let op_id = format!("op-{}", crate::api::next_operation_id());

        observer.started(&op_id, &architect_tier);

        let level_up_note = format_architect_escalation_note(
            &architect_tier,
            level_up.as_ref(),
            escalation_excluded && diagnostics.is_some(),
        );

        let intro_header = match &diagnostics {
        Some(diag) => format!(
            "# {}: [{}] {}\n*Replanning uncompleted horizon around blocked step `{}`...*{}\n\n---\n\n",
            operation_title, plan.id, plan.title, diag.blocked_step_id, level_up_note
        ),
        None => format!(
            "# {}: [{}] {}\n*Analyzing codebase contours and decomposing architectural plan...*{}\n\n---\n\n",
            operation_title, plan.id, plan.title, level_up_note
        ),
    };

        // The domain framing of the synthesis is task content, so it precedes the plan
        // prompt of every round. The protocol carries the response contract, and the turn
        // kernel renders the context block and the repository outline of each round in its
        // stable head, so this fragment restates neither of them.
        let mut task = crate::plan::prompt::build_plan_horizon_system_prompt();
        task.push_str("\n\n---\n\n");
        let diag_ref = diagnostics.as_ref().map(|d| PlanHorizonDiagnostics {
            blocked_step_id: &d.blocked_step_id,
            blocked_reason: d.blocked_reason.as_deref(),
            toolchain_status: d.toolchain_status.as_deref(),
        });
        task.push_str(&crate::plan::prompt::build_plan_horizon_user_prompt(
            &plan,
            diag_ref,
            instructions.as_deref(),
        ));

        let budget_hint = "The plan is too broad and should be manually decomposed or scoped down.";
        let (contour_result, summary) = observer
            .run(
                operation,
                PlanHorizonContour {
                    operation_id: op_id.clone(),
                    task,
                    context: context_snapshot,
                    options,
                    models_config,
                    tier: architect_tier,
                    intro_header,
                    budget_hint: budget_hint.to_string(),
                },
            )
            .await;

        let StructuredRefineOutcome {
            output,
            tier,
            status,
        } = match contour_result {
            Ok(outcome) => outcome,
            Err(err) => {
                // The contour owns the transient layers it widened and the launch
                // skeleton merged everything else back, so the transient context is
                // released before the turn reports its failure.
                cleanup_context(application, &observer).await;
                // A cancellation observed while the contour settled still ends the
                // turn as a cancellation instead of a provider failure.
                if summary.cancelled {
                    return Err(PlanHorizonError {
                        kind: PlanHorizonFailure::Cancelled,
                        message: format!("{} was cancelled", operation_title),
                    });
                }
                let message = summary.error.unwrap_or_else(|| err.to_string());
                return Err(PlanHorizonError {
                    kind: PlanHorizonFailure::Model,
                    message,
                });
            }
        };

        if !matches!(status, StructuredRefineStatus::Cancelled) {
            // The answer the turn relay accumulated is the single source of the
            // finished text, exactly as the retired manual loop published it.
            observer.finished(&op_id, summary.assistant_text);
        }

        match status {
            StructuredRefineStatus::Cancelled => {
                // A cancelled synthesis leaves the transient context behind, so it
                // is released exactly as an ended step does.
                cleanup_context(application, &observer).await;
                return Err(PlanHorizonError {
                    kind: PlanHorizonFailure::Cancelled,
                    message: format!("{} was cancelled", operation_title),
                });
            }
            StructuredRefineStatus::Blocked { reason } => {
                tracing::warn!(
                    plan_id = %plan.id,
                    reason = %reason,
                    "architect reported blockers"
                );
                let blocked_msg = format!(
                    "Architect reported blockers for plan [{}]:\n\n{}",
                    plan.id, reason
                );
                record_history(
                    application,
                    history_turn_prompt,
                    history_context_paths,
                    blocked_msg,
                    Some(format!("{}: Blocked ({})", operation_title, plan.title)),
                )
                .await;
                return Ok(PlanSynthesisResult::Blocked { reason });
            }
            StructuredRefineStatus::Aborted(report) => {
                // An abort states that the work cannot be completed in the current
                // environment, so the developer receives the exact host action
                // beside the recorded verdict instead of a contradiction the
                // Architect would have had to invent.
                let message = report.to_message();
                tracing::warn!(
                    plan_id = %plan.id,
                    reason = %report.reason,
                    "architect reported an abort"
                );
                record_history(
                    application,
                    history_turn_prompt,
                    history_context_paths,
                    message.clone(),
                    Some(format!("{}: Aborted ({})", operation_title, plan.title)),
                )
                .await;
                return Ok(PlanSynthesisResult::Blocked { reason: message });
            }
            StructuredRefineStatus::BudgetExceeded { diagnostic } => {
                record_history(
                    application,
                    history_turn_prompt,
                    history_context_paths,
                    diagnostic.clone(),
                    Some(format!(
                        "{}: Token Budget Exceeded ({})",
                        operation_title, plan.title
                    )),
                )
                .await;
                return Ok(PlanSynthesisResult::Blocked { reason: diagnostic });
            }
            StructuredRefineStatus::Synthesized => {}
        }

        let Some(output) = output else {
            return Err(PlanHorizonError {
                kind: PlanHorizonFailure::EmptyItems,
                message: ("Model returned success status but no horizon payload").into(),
            });
        };

        if output.items.is_empty() {
            return Err(PlanHorizonError {
                kind: PlanHorizonFailure::EmptyItems,
                message: ("Model returned success status but an empty items array").into(),
            });
        }

        if let Some(duplicate) = find_duplicate_item_ids(&output.items) {
            return Err(PlanHorizonError {
                kind: PlanHorizonFailure::InvalidTree,
                message: format!(
                    "Architect returned a plan with a duplicate or empty step ID: '{}'",
                    duplicate
                ),
            });
        }

        // The operation slot is held for the whole synthesis, so a hard cancel may
        // land after the contour's final provider round resolved and before this
        // write. The horizon below destructively replaces the plan's uncompleted
        // steps, so the authoritative signal is re-checked here, at the same seam
        // the review subsystem guards before it touches its session on disk. A
        // raised signal releases the transient context, answers a cancellation, and
        // leaves the stored plan untouched instead of persisting a horizon for an
        // operation the developer already aborted.
        if *operation.cancel_receiver().borrow() {
            cleanup_context(application, &observer).await;
            tracing::warn!(
                plan_id = %plan.id,
                operation = operation_title,
                "plan horizon synthesis was cancelled before the horizon write"
            );
            return Err(PlanHorizonError {
                kind: PlanHorizonFailure::Cancelled,
                message: format!("{} was cancelled", operation_title),
            });
        }

        // Replace the uncompleted horizon while preserving the immutable Done foundation.
        let new_steps = output.items.len();
        let horizon_root = repo_root.clone();
        let horizon_plan_id = plan.id.clone();
        let horizon_items = output.items;
        // The rung that actually produced the horizon is recorded as its provenance, so a
        // later wave reproduces or escalates it instead of re-deriving one.
        let horizon_architect = tier;
        // The step whose horizon this wave reprojects is persisted with the new
        // horizon, so the next wave reads from disk whether this step already
        // failed a replanning round.
        let horizon_replanned_step = diagnostics.as_ref().map(|d| d.blocked_step_id.clone());
        let updated_plan = match offload_plan(move || {
            PlanStorage::replace_uncompleted_horizon(
                &horizon_root,
                &horizon_plan_id,
                horizon_items,
                horizon_architect,
                horizon_replanned_step,
            )
        })
        .await
        {
            Ok(p) => p,
            Err(err) => {
                return Err(PlanHorizonError {
                    kind: PlanHorizonFailure::Save,
                    message: err,
                })
            }
        };

        // Strict history transactionality: record the assistant response ONLY after
        // the synthesized horizon has been atomically written to disk.
        let (total_items, completed_items) = updated_plan.implementation.count_stats();
        tracing::info!(
            plan_id = %updated_plan.id,
            total_items = total_items,
            completed_items = completed_items,
            new_steps = new_steps,
            "plan horizon synthesized"
        );
        let history_response = format!(
            "Plan [{}] horizon synthesized successfully ({} total, {} completed).\n\n{}",
            updated_plan.id, total_items, completed_items, output.explanation
        );
        record_history(
            application,
            history_turn_prompt,
            history_context_paths,
            history_response,
            Some(format!("{}: {}", operation_title, updated_plan.title)),
        )
        .await;

        observer.publish(&updated_plan).await;

        Ok(PlanSynthesisResult::Success {
            plan: updated_plan,
            explanation: Some(output.explanation),
        })
    }
}
