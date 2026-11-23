//! Directed replanning around an addressed leaf.
use super::{
    cleanup_transient_context, preload_readonly_files, publish_plan, record_synthesis_history,
    Runtime, BUDGET_HINT,
};
use crate::api::{ApplicationEvent, OperationGuard};
use crate::application::PlanExecutionEnvironment;
use crate::workflows::architect::{
    self, publish_architect_started, ArchitectScope, PlanHorizonContour,
};
use tauqe_core::models::{ModelRole, ModelSelection};
use tauqe_core::plan::find_duplicate_item_ids;
use tauqe_core::plan::prompt::{
    format_architect_escalation_note, PlanHorizonDiagnostics, StructuredRefineOutcome,
    StructuredRefineStatus,
};
use tauqe_core::plan::{Plan, PlanSynthesisResult};

use std::collections::HashSet;

use tauqe_core::plan::PlanItem;

use crate::api::normalize_preload_path;

/// Diagnostics describing a failed or blocked step for horizon synthesis.
pub(super) struct PlanHorizonDiagnosticsOwned {
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

pub(super) struct PlanHorizonRequest {
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
    pub(crate) message: String,
}

impl Runtime<'_> {
    pub(super) async fn execute_plan_horizon(
        &self,
        operation: &OperationGuard,
        environment: &PlanExecutionEnvironment,
        request: PlanHorizonRequest,
    ) -> Result<PlanSynthesisResult, PlanHorizonError> {
        let observer = &self.observer;
        let step = &self.resources.step;
        let PlanHorizonRequest {
            plan,
            diagnostics,
            model: custom_model,
            instructions,
            operation_title,
        } = request;
        tracing::info!(
            plan_id = %plan.id,
            operation = operation_title,
            replanning = diagnostics.is_some(),
            blocked_step = ?diagnostics.as_ref().map(|d| d.blocked_step_id.as_str()),
            "plan horizon synthesis started"
        );

        let (architect_tier, level_up, escalation_excluded) = {
            let cfg = environment.config();
            let (architect_tier, level_up) = tauqe_core::plan::prompt::resolve_architect_tier(
                cfg,
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
            (architect_tier, level_up, escalation_excluded)
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
        cleanup_transient_context(self).await;

        // Guarantee baseline visibility for both deep planning and replanning:
        // collect every existing step's declared target and reference files and
        // load them into the read-only auto layer before the first generation
        // round, so the architect sees the real code instead of inferring it from
        // the repo map alone.
        preload_readonly_files(self, &plan).await;

        // The user turn and its response are recorded together once the synthesis
        // reaches a terminal outcome, so a cancelled, failed, or rejected round
        // leaves no orphaned user turn in the transcript. The prompt and context
        // paths are captured here because they describe the state the Architect
        // actually saw.
        let (history_turn_prompt, history_context_paths) = {
            let ctx = step.context.context_state().await;
            let context_paths: Vec<String> = ctx.items.into_iter().map(|it| it.path).collect();
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
        let context_snapshot = step.context.context_for_turn().await;

        let op_id = format!("op-{}", crate::api::next_operation_id());

        publish_architect_started(observer.as_ref(), &op_id, &architect_tier, false);

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
        let mut task = tauqe_core::plan::prompt::build_plan_horizon_system_prompt();
        task.push_str("\n\n---\n\n");
        let diag_ref = diagnostics.as_ref().map(|d| PlanHorizonDiagnostics {
            blocked_step_id: &d.blocked_step_id,
            blocked_reason: d.blocked_reason.as_deref(),
            toolchain_status: d.toolchain_status.as_deref(),
        });
        task.push_str(&tauqe_core::plan::prompt::build_plan_horizon_user_prompt(
            &plan,
            diag_ref,
            instructions.as_deref(),
        ));

        let (contour_result, summary) = architect::run_contour(
            &step.execution,
            &step.context,
            &step.usage,
            operation,
            environment,
            PlanHorizonContour {
                operation_id: op_id.clone(),
                task,
                context: context_snapshot,
                scope: ArchitectScope::Plan(plan.id.clone()),
                tier: architect_tier,
                intro_header,
                budget_hint: BUDGET_HINT.to_string(),
            },
            observer.clone(),
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
                cleanup_transient_context(self).await;
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
            observer.publish(&op_id, ApplicationEvent::Finished(summary.assistant_text));
        }

        match status {
            StructuredRefineStatus::Cancelled => {
                // A cancelled synthesis leaves the transient context behind, so it
                // is released exactly as an ended step does.
                cleanup_transient_context(self).await;
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
                record_synthesis_history(
                    self,
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
                record_synthesis_history(
                    self,
                    history_turn_prompt,
                    history_context_paths,
                    message.clone(),
                    Some(format!("{}: Aborted ({})", operation_title, plan.title)),
                )
                .await;
                return Ok(PlanSynthesisResult::Blocked { reason: message });
            }
            StructuredRefineStatus::BudgetExceeded { diagnostic } => {
                record_synthesis_history(
                    self,
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
            cleanup_transient_context(self).await;
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
        let updated_plan = self
            .documents
            .write_synthesized_horizon(
                plan.id.clone(),
                output.items,
                tier,
                diagnostics.map(|d| d.blocked_step_id),
            )
            .await
            .map_err(|message| PlanHorizonError {
                kind: PlanHorizonFailure::Save,
                message,
            })?;

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
        record_synthesis_history(
            self,
            history_turn_prompt,
            history_context_paths,
            history_response,
            Some(format!("{}: {}", operation_title, updated_plan.title)),
        )
        .await;

        publish_plan(self, &op_id, &updated_plan, false).await;

        Ok(PlanSynthesisResult::Success {
            plan: Box::new(updated_plan),
            explanation: Some(output.explanation),
        })
    }
}
