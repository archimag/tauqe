//! Architect plan horizon-synthesis engine.
//!
//! The single-phase horizon synthesis lives here. The two-phase contour that
//! drafts a horizon, audits it with an independent Architect entry, and reconciles
//! the audit before the plan is issued is orchestrated by `super::two_phase`,
//! which drives the same contour once per Planner beat and persists the result
//! through the same storage primitives.

use std::sync::Arc;

use tauqe_core::control::refine::{StructuredRefineOutcome, StructuredRefineStatus};
use tauqe_core::plan::prompt::PlanHorizonDiagnostics;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::providers::{format_session_id, SESSION_PREFIX_ARCHITECT};
use tauqe_core::workflow::WorkflowOptions;
use tauqe_protocol::{
    errors, events, ModelFinishedEvent, ModelRole, ModelSelection, ModelStartedEvent, Plan,
    PlanListChangedEvent, PlanRefineResult, PlanUpdatedEvent, Response,
};

use crate::handlers::architect::{format_architect_escalation_note, run_contour_synthesis};
use crate::handlers::common::{
    cleanup_transient_context, emit, preload_readonly_files, record_synthesis_history,
};
use crate::state::{AppState, OperationGuard};

use super::common::{find_duplicate_item_ids, offload_plan};
pub(super) use super::horizon::{collect_plan_context_files, PlanHorizonDiagnosticsOwned};

/// Unified engine for architect plan horizon synthesis.
///
/// Drives the structured Architect contour (`run_contour_synthesis`) instead of a
/// hand-written research loop: the contour owns the provider rounds, the schema
/// healing of an invalid payload, the tier ladder, the research journal, and the
/// typed terminal outcomes, while this adapter owns the RPC response, the repository
/// persistence, and the session-history transaction.
///
/// Enforces strict session history transactionality: `state.history.record_response` is
/// called strictly AFTER `replace_uncompleted_horizon` succeeds on disk. Any persistence
/// or validation error returns early without poisoning the interaction audit trail.
///
/// The destructive horizon write is additionally guarded by a final re-check of
/// the operation's cancellation signal, so a hard cancel delivered after the
/// contour's final provider round leaves the stored plan untouched and answers a
/// cancellation instead of a persisted success.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_plan_horizon_synthesis(
    req_id: tauqe_protocol::RequestId,
    state: &Arc<AppState>,
    plan: Plan,
    diagnostics: Option<PlanHorizonDiagnosticsOwned>,
    custom_model: Option<ModelSelection>,
    instructions: Option<String>,
    operation_title: &str,
    is_batch: bool,
    operation: &OperationGuard,
) -> Response {
    let repo_root = state.context.read().await.repo_root().to_path_buf();

    tracing::info!(
        plan_id = %plan.id,
        operation = operation_title,
        replanning = diagnostics.is_some(),
        blocked_step = ?diagnostics.as_ref().map(|d| d.blocked_step_id.as_str()),
        "plan horizon synthesis started"
    );

    let (architect_tier, level_up, escalation_excluded, options, models_config) = {
        let cfg = state.config.lock().await.config.clone();
        let (architect_tier, level_up) = tauqe_core::plan::prompt::resolve_architect_tier(
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
    cleanup_transient_context(state).await;

    // Guarantee baseline visibility for both deep planning and replanning:
    // collect every existing step's declared target and reference files and
    // load them into the read-only auto layer before the first generation
    // round, so the architect sees the real code instead of inferring it from
    // the repo map alone.
    let plan_files = collect_plan_context_files(&plan);
    preload_readonly_files(state, &repo_root, &plan_files).await;

    // The user turn and its response are recorded together once the synthesis
    // reaches a terminal outcome, so a cancelled, failed, or rejected round
    // leaves no orphaned user turn in the transcript. The prompt and context
    // paths are captured here because they describe the state the Architect
    // actually saw.
    let (history_turn_prompt, history_context_paths) = {
        let ctx = state.context.read().await;
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
    let context_snapshot = state.context.read().await.clone();

    let op_id = format!("op-{}", crate::state::next_operation_id());

    emit(
        &state.out,
        events::MODEL_STARTED,
        serde_json::to_value(ModelStartedEvent {
            operation_id: op_id.clone(),
            model: architect_tier.model.clone(),
            role: Some(ModelRole::Architect),
            alias: None,
            effort: architect_tier.effort,
            plan_context: None,
            work_context: None,
            is_batch,
        })
        .ok(),
    );

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

    let budget_hint = "The plan is too broad and should be manually decomposed or scoped down.";
    let (contour_result, summary) = run_contour_synthesis(
        state,
        operation,
        &op_id,
        task,
        context_snapshot,
        options,
        models_config,
        architect_tier,
        intro_header,
        budget_hint.to_string(),
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
            cleanup_transient_context(state).await;
            // A cancellation observed while the contour settled still ends the
            // turn as a cancellation instead of a provider failure.
            if summary.cancelled {
                return Response::err(
                    req_id,
                    errors::CANCELLED,
                    format!("{} was cancelled", operation_title),
                );
            }
            let message = summary.error.unwrap_or_else(|| err.to_string());
            return Response::err(req_id, errors::MODEL_ERROR, message);
        }
    };

    if !matches!(status, StructuredRefineStatus::Cancelled) {
        // The answer the turn relay accumulated is the single source of the
        // finished text, exactly as the retired manual loop published it.
        emit(
            &state.out,
            events::MODEL_FINISHED,
            serde_json::to_value(ModelFinishedEvent {
                operation_id: op_id.clone(),
                full_text: summary.assistant_text,
            })
            .ok(),
        );
    }

    match status {
        StructuredRefineStatus::Cancelled => {
            // A cancelled synthesis leaves the transient context behind, so it
            // is released exactly as an ended step does.
            cleanup_transient_context(state).await;
            return Response::err(
                req_id,
                errors::CANCELLED,
                format!("{} was cancelled", operation_title),
            );
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
                state,
                history_turn_prompt,
                history_context_paths,
                blocked_msg,
                Some(format!("{}: Blocked ({})", operation_title, plan.title)),
            )
            .await;
            return Response::ok_typed(req_id, &PlanRefineResult::Blocked { reason });
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
                state,
                history_turn_prompt,
                history_context_paths,
                message.clone(),
                Some(format!("{}: Aborted ({})", operation_title, plan.title)),
            )
            .await;
            return Response::ok_typed(req_id, &PlanRefineResult::Blocked { reason: message });
        }
        StructuredRefineStatus::BudgetExceeded { diagnostic } => {
            record_synthesis_history(
                state,
                history_turn_prompt,
                history_context_paths,
                diagnostic.clone(),
                Some(format!(
                    "{}: Token Budget Exceeded ({})",
                    operation_title, plan.title
                )),
            )
            .await;
            return Response::ok_typed(req_id, &PlanRefineResult::Blocked { reason: diagnostic });
        }
        StructuredRefineStatus::Synthesized => {}
    }

    let Some(output) = output else {
        return Response::err(
            req_id,
            errors::EMPTY_ITEMS,
            "Model returned success status but no horizon payload",
        );
    };

    if output.items.is_empty() {
        return Response::err(
            req_id,
            errors::EMPTY_ITEMS,
            "Model returned success status but an empty items array",
        );
    }

    if let Some(duplicate) = find_duplicate_item_ids(&output.items) {
        return Response::err(
            req_id,
            errors::INVALID_PLAN_TREE,
            format!(
                "Architect returned a plan with a duplicate or empty step ID: '{}'",
                duplicate
            ),
        );
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
        cleanup_transient_context(state).await;
        tracing::warn!(
            plan_id = %plan.id,
            operation = operation_title,
            "plan horizon synthesis was cancelled before the horizon write"
        );
        return Response::err(
            req_id,
            errors::CANCELLED,
            format!("{} was cancelled", operation_title),
        );
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
        Err(err) => return Response::err(req_id, errors::PLAN_SAVE_FAILED, err),
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
    record_synthesis_history(
        state,
        history_turn_prompt,
        history_context_paths,
        history_response,
        Some(format!("{}: {}", operation_title, updated_plan.title)),
    )
    .await;

    emit(
        &state.out,
        events::PLAN_UPDATED,
        serde_json::to_value(PlanUpdatedEvent {
            plan: updated_plan.clone(),
            is_save: false,
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

    Response::ok_typed(
        req_id,
        &PlanRefineResult::Success {
            plan: updated_plan,
            explanation: Some(output.explanation),
        },
    )
}
