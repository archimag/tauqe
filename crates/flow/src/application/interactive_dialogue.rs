//! Interactive interpretation of a bounded technical turn.

use crate::application::infrastructure::execution::DialogueExecution;
use crate::application::turn_execution::InteractiveExecutionRequest;
use std::path::Path;
use tauqe_core::context::ContextManager;
use tauqe_core::control::context::{CancellationToken, EventSink};
use tauqe_core::control::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use tauqe_core::control::repl::ReplArtifact;
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::history::HistorySnapshot;
use tauqe_core::llm::{CapabilityTree, LlmCapability, LlmCommand, TextCommand};
use tauqe_core::plan::storage::{PlanMutationMode, PlanStorage};
use tauqe_core::plan::{PlanRejection, PlanRejectionKind};
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::storage::ReviewStorage;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    dialogue: &mut DialogueExecution,
    root: &Path,
    context: &mut ContextManager,
    cancel: &CancellationToken,
    events: &EventSink,
    request: InteractiveExecutionRequest,
    history: Option<&HistorySnapshot>,
) -> Result<LoopResult<ReplArtifact>, SystemError> {
    let InteractiveExecutionRequest {
        capabilities,
        prompt,
        focus,
    } = request;
    let turn = match dialogue
        .execute(
            root,
            context,
            cancel,
            events,
            prompt,
            capabilities.clone(),
            history,
        )
        .await
    {
        Err(SystemError::RoundBudgetExhausted { spent, last_answer }) => {
            let answer = last_answer.unwrap_or_else(|| {
                "The turn reached its round budget before producing an answer.".to_owned()
            });
            let text = format!(
                "{answer}\n\n---\n**[Round Budget Reached]** The turn spent its whole budget of {spent} provider call(s). \
                 The answer above is the last one the model produced; ask again to continue from the material the turn already gathered."
            );
            let envelope =
                ResponseEnvelope::from_commands(&[LlmCommand::Text(TextCommand { content: text })]);
            LoopResult::Success(LoopSuccess::new(
                tauqe_core::control::turn::TurnArtifact::without_change_set(
                    spent,
                    envelope.clone(),
                ),
                envelope,
            ))
        }
        result => result?,
    };

    let (mut envelope, artifact) = match turn {
        LoopResult::Success(success) => (success.response, success.value),
        LoopResult::Diverged(env) => return Ok(LoopResult::Diverged(env)),
        LoopResult::Exhausted(report) => return Ok(LoopResult::Exhausted(report)),
    };
    if !artifact.modified_paths.is_empty() && artifact.commit_error.is_none() {
        tauqe_core::blocking::run_blocking(|| context.clear_transient());
        events.emit(StreamEvent::ContextChanged(context.get_state()));
    }
    let message = extract_text_message(&mut envelope);
    let (plans_applied, plan_rejections, plan_warnings) = apply_plan_commands(
        dialogue,
        root,
        context,
        cancel,
        events,
        &capabilities,
        history,
        &mut envelope,
        focus.as_ref(),
    )
    .await?;
    let review_decisions_applied = apply_review_commands(root, &mut envelope, focus.as_ref()).await;
    let _ = envelope.take_any(&[
        LlmCapability::CommitMessage,
        LlmCapability::TurnError,
        LlmCapability::TurnSkip,
    ]);
    let remaining = envelope.remaining_capabilities();
    if !remaining.is_empty() {
        tracing::warn!(repo_root = %root.display(), ?remaining, "Unhandled capabilities remaining in REPL response envelope");
    }
    Ok(LoopResult::Success(LoopSuccess::new(
        ReplArtifact {
            message,
            modified_paths: artifact.modified_paths,
            plans_applied,
            plan_rejections,
            plan_warnings,
            review_decisions_applied,
            envelope: envelope.clone(),
            commit_hash: artifact.commit_hash,
            verification: artifact.verification,
            commit_error: artifact.commit_error,
            rounds_spent: artifact.rounds_spent,
        },
        envelope,
    )))
}

fn extract_text_message(envelope: &mut ResponseEnvelope) -> Option<String> {
    let mut combined = String::new();
    for command in envelope.take(LlmCapability::TextMessage) {
        if let LlmCommand::Text(text) = command {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&text.content);
        }
    }
    (!combined.is_empty()).then_some(combined)
}

async fn apply_plan_mutations(
    root: &Path,
    envelope: &mut ResponseEnvelope,
    focus: Option<&DiscussionTarget>,
) -> (bool, Vec<PlanRejection>, Vec<String>) {
    let mut commands = envelope.take(LlmCapability::PlanCreation);
    commands.extend(envelope.take(LlmCapability::PlanUpdate));
    let updates: Vec<_> = commands
        .into_iter()
        .filter_map(|command| match command {
            LlmCommand::Plan { update } => Some(update),
            _ => None,
        })
        .collect();
    if updates.is_empty() {
        return (false, Vec::new(), Vec::new());
    }
    let mode = if matches!(focus, Some(DiscussionTarget::Plan { .. })) {
        PlanMutationMode::Discussion
    } else {
        PlanMutationMode::Repl
    };
    let root = root.to_path_buf();
    let outcomes = tauqe_core::blocking::run_blocking_async(move || {
        PlanStorage::apply_discussion_plan_updates_with_mode(&root, &updates, mode)
    })
    .await
    .unwrap_or_default();
    let mut applied = false;
    let mut rejections = Vec::new();
    let mut warnings = Vec::new();
    for outcome in outcomes {
        applied |= !outcome.applied.is_empty();
        rejections.extend(outcome.errors);
        warnings.extend(outcome.warnings);
    }
    (applied, rejections, warnings)
}

#[allow(clippy::too_many_arguments)]
async fn apply_plan_commands(
    dialogue: &mut DialogueExecution,
    root: &Path,
    context: &mut ContextManager,
    cancel: &CancellationToken,
    events: &EventSink,
    capabilities: &CapabilityTree,
    history: Option<&HistorySnapshot>,
    envelope: &mut ResponseEnvelope,
    focus: Option<&DiscussionTarget>,
) -> Result<(bool, Vec<PlanRejection>, Vec<String>), SystemError> {
    let mut plans_applied = false;
    let mut plan_rejections = Vec::new();
    let mut plan_warnings = Vec::new();
    let mut corrections = 0usize;
    loop {
        let (applied, rejections, warnings) = apply_plan_mutations(root, envelope, focus).await;
        plans_applied |= applied;
        plan_warnings.extend(warnings);
        if applied && corrections > 0 {
            tracing::info!(repo_root = %root.display(), corrections, "a corrective round repaired the plan payload");
        }
        let mut defects = Vec::new();
        for rejection in rejections {
            if rejection.kind == PlanRejectionKind::PayloadDefect {
                defects.push(rejection.message);
            } else {
                plan_rejections.push(rejection);
            }
        }
        if defects.is_empty() {
            return Ok((plans_applied, plan_rejections, plan_warnings));
        }
        corrections += 1;
        tracing::warn!(repo_root = %root.display(), correction = corrections, defects = defects.len(), "plan mutation payload refused; asking for a corrected payload");
        events.emit(StreamEvent::TextDelta(format!(
            "\n\n---\n**[Plan Correction {}]** The plan payload was refused before it reached storage; asking for a corrected payload...\n\n",
            corrections
        )));
        let repair = plan_correction_prompt(&defects, corrections);
        match dialogue
            .continue_round(
                root,
                context,
                cancel,
                events,
                capabilities.clone(),
                history,
                &repair,
            )
            .await
        {
            Ok(next) => *envelope = next,
            Err(SystemError::RoundBudgetExhausted { .. }) => {
                tracing::warn!(repo_root = %root.display(), corrections, defects = defects.len(), "the plan mutation stayed defective when turn round budget was exhausted");
                events.emit(StreamEvent::TextDelta(
                    "\n\n---\n**[Plan Correction Limit Reached]** The turn round budget was exhausted during plan correction; the refusal is reported below.\n\n".to_string(),
                ));
                plan_rejections.extend(defects.into_iter().map(PlanRejection::payload_defect));
                return Ok((plans_applied, plan_rejections, plan_warnings));
            }
            Err(error) => return Err(error),
        }
    }
}

fn plan_correction_prompt(defects: &[String], correction: usize) -> String {
    let mut prompt = format!(
        "<response_directive>\n[Plan Correction {correction}] The plan payload you declared was refused before it reached plan storage, so the stored plan is unchanged. Repair the payload and emit it again through the plan branch of the action union.\n"
    );
    for defect in defects {
        prompt.push_str("\n<plan_rejection>\n");
        prompt.push_str(&tauqe_core::escape::escape_xml_text(defect));
        prompt.push_str("\n</plan_rejection>");
    }
    prompt.push_str("\n</response_directive>");
    prompt
}

async fn apply_review_commands(
    root: &Path,
    envelope: &mut ResponseEnvelope,
    focus: Option<&DiscussionTarget>,
) -> Vec<u32> {
    let commands = envelope.take(LlmCapability::ReviewUpdate);
    if commands.is_empty() {
        return Vec::new();
    }
    let Some(review_id) = focus.and_then(DiscussionTarget::review_session_id) else {
        tracing::warn!(
            "Review update commands were emitted without an active review focus; ignoring"
        );
        return Vec::new();
    };
    let mut updated = Vec::new();
    for command in commands {
        if let LlmCommand::ReviewUpdate { update } = command {
            let item_id = update.item_id;
            let root = root.to_path_buf();
            let id = review_id.to_owned();
            match tauqe_core::blocking::run_blocking_async(move || {
                ReviewStorage::apply_decision(&root, &id, &update)
            })
            .await
            {
                Ok(Ok(Some(_))) => updated.push(item_id),
                Ok(Ok(None)) => {
                    tracing::warn!(item_id, "Review item not found in session for decision")
                }
                Ok(Err(error)) => {
                    tracing::error!(item_id, %error, "Failed to apply review decision")
                }
                Err(error) => tracing::error!(item_id, %error, "Failed to apply review decision"),
            }
        }
    }
    updated
}

#[cfg(test)]
mod tests;
