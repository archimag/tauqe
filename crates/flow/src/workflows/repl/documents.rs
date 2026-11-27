//! Interactive document preparation and reversible final acceptance.

use super::{check_terminal, plan_diagnostic, response_text, DocumentDisposition, DocumentResult};
use crate::api::OperationGuard;
use crate::workflows::round::{run_round, RoundServices};
use crate::workflows::state::TurnState;
use std::path::Path;
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::execution::executor::{ExecuteInput, PromptMessage, RequestRound};
use tauqe_core::execution::AcceptedRoundId;
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, PromptExecutor, ResponseEnvelope,
};
use tauqe_core::llm::{LlmCapability, LlmCommand};
use tauqe_core::plan::storage::{PlanMutationMode, PlanStorage};
use tauqe_core::plan::{Plan, PlanRejection, PlanRejectionKind};
use tauqe_core::review::storage::{ReviewDecisionOutcome, ReviewStorage};
use tauqe_core::review::ReviewSession;

#[derive(Clone, Default)]
pub(crate) struct DocumentChanges {
    pub plans: Vec<Plan>,
    pub reviews: Vec<ReviewSession>,
}

impl DocumentChanges {
    pub fn is_empty(&self) -> bool {
        self.plans.is_empty() && self.reviews.is_empty()
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_plan_commands(
    mut envelope: ResponseEnvelope,
    mut source: Option<AcceptedRoundId>,
    root: &Path,
    focus: Option<&DiscussionTarget>,
    operation: LlmCapability,
    state: &mut TurnState,
    executor: &mut PromptExecutor,
    events: &EventSink,
    cancel: &CancellationToken,
    task: &str,
    owner: &OperationGuard,
    #[cfg(test)] write_gate: Option<(
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    )>,
) -> Result<DocumentResult, ExecutionError> {
    let mut plan_rejections = Vec::new();
    let mut plan_warnings = Vec::new();
    let mut corrections = 0;
    let mut batch = {
        let root = root.to_path_buf();
        match tauqe_core::blocking::run_blocking_async(move || {
            PlanStorage::prepare_plan_batch(&root)
        })
        .await
        .map_err(|error| ExecutionError::Other(error.to_string()))?
        {
            Ok(batch) => batch,
            Err(error) => {
                return Ok(DocumentResult {
                    disposition: DocumentDisposition::StorageFailed(format!(
                        "Plan preparation failed: {error:#}"
                    )),
                    plan_rejections,
                    plan_warnings,
                    changes: DocumentChanges::default(),
                })
            }
        }
    };
    let mode = if matches!(focus, Some(DiscussionTarget::Plan { .. })) {
        PlanMutationMode::Discussion
    } else {
        PlanMutationMode::Repl
    };
    loop {
        if cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        let updates: Vec<_> = envelope
            .take_any(&[LlmCapability::PlanCreation, LlmCapability::PlanUpdate])
            .into_iter()
            .filter_map(|command| match command {
                LlmCommand::Plan { update } => Some(update),
                _ => None,
            })
            .collect();
        let root_owned = root.to_path_buf();
        let (prepared, outcomes) = tauqe_core::blocking::run_blocking_async(move || {
            let outcomes =
                PlanStorage::prepare_plan_updates(&root_owned, &mut batch, &updates, mode);
            (batch, outcomes)
        })
        .await
        .map_err(|error| ExecutionError::Other(error.to_string()))?;
        batch = prepared;
        if cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        let mut defects = Vec::new();
        let mut storage_errors = Vec::new();
        for outcome in outcomes {
            plan_warnings.extend(outcome.warnings);
            for rejection in outcome.errors {
                if rejection.kind == PlanRejectionKind::PayloadDefect {
                    defects.push(rejection);
                } else {
                    plan_rejections.push(rejection);
                }
            }
            if let Some(error) = outcome.storage_error {
                storage_errors.push(error);
            }
        }
        if !storage_errors.is_empty() {
            plan_rejections.extend(defects);
            return Ok(DocumentResult {
                disposition: DocumentDisposition::StorageFailed(plan_diagnostic(
                    format!("Plan storage failed: {}", storage_errors.join("\n")),
                    &plan_rejections,
                    &[],
                )),
                plan_rejections,
                plan_warnings,
                changes: DocumentChanges::default(),
            });
        }
        if !plan_rejections.is_empty() {
            plan_rejections.extend(defects);
            return Ok(DocumentResult {
                disposition: DocumentDisposition::Refused(plan_diagnostic(
                    "Plan proposal refused.".into(),
                    &plan_rejections,
                    &[],
                )),
                plan_rejections,
                plan_warnings,
                changes: DocumentChanges::default(),
            });
        }
        if defects.is_empty() {
            let text = response_text(envelope, source)?;
            #[cfg(test)]
            if let Some((entered, release)) = write_gate.as_ref() {
                entered.notify_one();
                release.notified().await;
            }
            if cancel.is_cancelled() {
                return Err(ExecutionError::Cancelled);
            }
            // A storage call cannot be aborted once admitted. The batch checks
            // cancellation before acceptance, restoring every replacement if it wins.
            // Never abort the waiter while its blocking commit may be running.
            // Keep it cleared after acceptance so the committed result reaches
            // the supervisor even when cancellation arrives late.
            owner.clear_abort_handle();
            let root = root.to_path_buf();
            let cancel = cancel.clone();
            let committed = tauqe_core::blocking::run_blocking_async(move || {
                PlanStorage::commit_prepared_batch(&root, batch, &cancel)
            })
            .await
            .map_err(|error| ExecutionError::Other(error.to_string()))?;
            return Ok(DocumentResult {
                disposition: match &committed {
                    Ok(_) => DocumentDisposition::Saved(text),
                    Err(error) => {
                        DocumentDisposition::StorageFailed(format!("Plan storage failed: {error}"))
                    }
                },
                plan_rejections,
                plan_warnings,
                changes: DocumentChanges {
                    plans: committed.unwrap_or_default(),
                    reviews: Vec::new(),
                },
            });
        }
        corrections += 1;
        state.capabilities = tauqe_core::llm::repl::plan_correction_capabilities(operation);
        let repair = plan_correction_prompt(&defects, corrections);
        let mut empty_retries = 0;
        let mut feedback = repair;
        let correction_error = loop {
            if state.budget.exhausted() {
                break Some(ExecutionError::Other(
                    "The turn's provider-call budget was exhausted.".into(),
                ));
            }
            let admitted_before = executor.admitted_requests();
            let directives = state.budget.final_directive();
            let request_round = RequestRound {
                round: state.budget.spent + 1,
                max_rounds: state.budget.max_rounds.unwrap_or(usize::MAX),
                reason: None,
            };
            let reply = run_round(
                executor,
                ExecuteInput {
                    message: if admitted_before == 0 {
                        PromptMessage::Task(task.to_owned())
                    } else {
                        PromptMessage::Continuation
                    },
                    directives: directives.map(str::to_owned),
                    round: Some(request_round),
                    context_items: state.context.get_state().items,
                    history: state.history.clone(),
                    capabilities: state.capabilities.clone(),
                    brief: None,
                    feedback: Some(feedback.clone()),
                    staged: None,
                    created_paths: state.created_paths.clone(),
                    journal: None,
                },
                RoundServices {
                    context: &mut state.context,
                    journal: &mut state.journal,
                    repairing: false,
                    edits: None,
                },
                events,
                cancel,
            )
            .await;
            state
                .budget
                .charge(admitted_before, executor.admitted_requests());
            match reply {
                Ok(commands) => {
                    envelope = ResponseEnvelope::from_commands(&commands);
                    if tauqe_core::workflow::research::step::RESEARCH_CAPABILITIES
                        .iter()
                        .any(|cap| envelope.has(*cap))
                    {
                        break Some(ExecutionError::Other(
                            "The corrective response requested research, which is not allowed during plan correction.".into(),
                        ));
                    }
                    source = executor.last_accepted_round();
                    break check_terminal(&envelope).err().or_else(|| {
                            (!envelope.has(operation)).then(|| ExecutionError::Other(
                                "The corrective response did not supply the refused plan operation.".into(),
                            ))
                        });
                }
                Err(ExecutionError::EmptyResponse) if empty_retries < 2 => {
                    empty_retries += 1;
                    feedback = if empty_retries == 1 {
                        "Your previous reply produced no actionable result. Return the corrected plan through the active schema.".into()
                    } else {
                        String::new()
                    };
                }
                Err(ExecutionError::EmptyResponse) => {
                    break Some(ExecutionError::Other(
                        "The model produced an empty response after 2 additional attempt(s)."
                            .into(),
                    ))
                }
                Err(ExecutionError::Protocol(message)) => {
                    break Some(ExecutionError::Protocol(message))
                }
                Err(ExecutionError::Cancelled) => return Err(ExecutionError::Cancelled),
                Err(error) => break Some(error),
            }
        };
        if let Some(error) = correction_error {
            plan_rejections.extend(defects);
            return Ok(DocumentResult {
                disposition: DocumentDisposition::Refused(plan_diagnostic(
                    format!("Plan correction did not complete: {error}"),
                    &plan_rejections,
                    &[],
                )),
                plan_rejections,
                plan_warnings,
                changes: DocumentChanges::default(),
            });
        }
    }
}

fn plan_correction_prompt(defects: &[PlanRejection], correction: usize) -> String {
    let mut prompt = format!(
        "<response_directive>\n[Plan Correction {correction}] The plan mutations listed below were refused. Correct only those mutations and emit them through the plan branch of the action union; do not repeat mutations already prepared in memory (none have been saved yet).\n"
    );
    for defect in defects {
        prompt.push_str("\n<plan_rejection>\n");
        prompt.push_str(&tauqe_core::escape::escape_xml_text(&defect.message));
        prompt.push_str("\n</plan_rejection>");
    }
    prompt.push_str("\n</response_directive>");
    prompt
}

pub(super) async fn apply_review_commands(
    mut envelope: ResponseEnvelope,
    source: Option<AcceptedRoundId>,
    root: &Path,
    focus: Option<&DiscussionTarget>,
    cancel: &CancellationToken,
    owner: &OperationGuard,
) -> Result<DocumentResult, ExecutionError> {
    let commands = envelope.take(LlmCapability::ReviewUpdate);
    let text = response_text(envelope, source)?;
    let Some(focus) = focus.filter(|focus| focus.review_session_id().is_some()) else {
        return Ok(DocumentResult::review(DocumentDisposition::Refused(
            "Review decision refused: no authorized review focus.".into(),
        )));
    };
    let decisions: Vec<_> = commands
        .into_iter()
        .filter_map(|command| match command {
            LlmCommand::ReviewUpdate { update } => Some(update),
            _ => None,
        })
        .collect();
    let root_owned = root.to_path_buf();
    let focus_owned = focus.clone();
    let preparation = tauqe_core::blocking::run_blocking_async(move || {
        ReviewStorage::prepare_decisions(&root_owned, &focus_owned, &decisions)
    })
    .await
    .map_err(|error| ExecutionError::Other(error.to_string()))?;
    let (prepared, outcomes) = match preparation {
        Ok(prepared) => prepared,
        Err(error) => {
            return Ok(DocumentResult::review(DocumentDisposition::StorageFailed(
                format!("review storage failed: {error:#}"),
            )));
        }
    };
    if cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    let failures: Vec<_> = outcomes
        .iter()
        .enumerate()
        .filter_map(|(index, outcome)| {
            let reason = match outcome {
                ReviewDecisionOutcome::MissingSession => "review session not found",
                ReviewDecisionOutcome::MissingFinding => "finding not found in the review focus",
                ReviewDecisionOutcome::MissingGroup => "review group not found",
                ReviewDecisionOutcome::TargetMismatch => "proposal does not match the review focus",
                ReviewDecisionOutcome::EmptyGroupNote => {
                    "a group decision requires a non-empty note"
                }
                ReviewDecisionOutcome::Saved => return None,
            };
            Some(format!(
                "Review decision {} not saved: {reason}.",
                index + 1
            ))
        })
        .collect();
    if !failures.is_empty() {
        return Ok(DocumentResult::review(DocumentDisposition::Refused(
            failures.join("\n"),
        )));
    }
    let Some(prepared) = prepared else {
        return Ok(DocumentResult::review(DocumentDisposition::Refused(
            "Review session not found".into(),
        )));
    };
    owner.clear_abort_handle();
    let root = root.to_path_buf();
    let cancel = cancel.clone();
    let committed = tauqe_core::blocking::run_blocking_async(move || {
        ReviewStorage::commit_prepared_decisions(&root, prepared, &cancel)
    })
    .await
    .map_err(|error| ExecutionError::Other(error.to_string()))?;
    Ok(match committed {
        Ok(session) => DocumentResult {
            disposition: DocumentDisposition::Saved(text),
            plan_rejections: Vec::new(),
            plan_warnings: Vec::new(),
            changes: DocumentChanges {
                plans: Vec::new(),
                reviews: vec![session],
            },
        },
        Err(error) => DocumentResult::review(DocumentDisposition::StorageFailed(format!(
            "review storage failed: {error}"
        ))),
    })
}
