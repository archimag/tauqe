//! Interactive state, technical execution and document correction for one task.

use super::state::TurnState;
use super::turn::RoundError;
use super::turn::TurnOutcome;
use crate::application::discussion_prompt::DiscussionDocument;
use documents::{apply_plan_commands, apply_review_commands, DocumentChanges};
use std::path::Path;
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::edits::EditOutcome;
use tauqe_core::execution::AcceptedRoundId;
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, ReadyPromptExecutor, ResponseEnvelope,
};
use tauqe_core::llm::{LlmCapability, LlmCommand};
use tauqe_core::plan::PlanRejection;

pub(crate) mod documents;
pub(crate) mod lifecycle;
mod publication;
mod settlement;

pub(crate) enum ReplResult {
    Answer(ReplText),
    Edit {
        text: ReplText,
        outcome: EditOutcome,
    },
    Document(DocumentResult),
}

impl ReplResult {
    pub(crate) fn changes(&self) -> Option<&DocumentChanges> {
        match self {
            Self::Document(document) => Some(&document.changes),
            _ => None,
        }
    }
}

pub(crate) enum ReplText {
    Accepted {
        round: AcceptedRoundId,
        text: String,
    },
    Environment(String),
}

impl ReplText {
    fn source(&self) -> Option<AcceptedRoundId> {
        match self {
            Self::Accepted { round, .. } => Some(*round),
            Self::Environment(_) => None,
        }
    }

    fn into_parts(self) -> (String, Option<AcceptedRoundId>) {
        match self {
            Self::Accepted { round, text } => (text, Some(round)),
            Self::Environment(text) => (text, None),
        }
    }
}

impl ReplResult {
    pub(crate) fn source(&self) -> Option<AcceptedRoundId> {
        match self {
            Self::Answer(text) | Self::Edit { text, .. } => text.source(),
            Self::Document(document) => match &document.disposition {
                DocumentDisposition::Saved(text) => text.source(),
                DocumentDisposition::Refused(_) | DocumentDisposition::StorageFailed(_) => None,
            },
        }
    }
}

pub(crate) enum DocumentDisposition {
    Saved(ReplText),
    Refused(String),
    StorageFailed(String),
}

pub(crate) struct DocumentResult {
    disposition: DocumentDisposition,
    plan_rejections: Vec<PlanRejection>,
    plan_warnings: Vec<String>,
    changes: DocumentChanges,
}

impl DocumentResult {
    fn review(disposition: DocumentDisposition) -> Self {
        Self {
            disposition,
            plan_rejections: Vec::new(),
            plan_warnings: Vec::new(),
            changes: DocumentChanges::default(),
        }
    }
}

pub(super) fn format_discussion_task(document: &DiscussionDocument, utterance: &str) -> String {
    match document {
        DiscussionDocument::Plan {
            plan,
            focused_items,
        } => {
            tauqe_core::plan::prompt::format_plan_discussion_prompt(plan, focused_items, utterance)
        }
        DiscussionDocument::Review {
            session,
            focused_items,
        } => tauqe_core::review::prompt::format_review_discussion_prompt_items(
            session,
            focused_items,
            utterance,
        ),
        DiscussionDocument::ReviewGroup {
            session,
            group,
            findings,
            focused_steps,
            missing_steps,
        } => tauqe_core::review::prompt::format_review_group_discussion_prompt(
            session,
            group,
            findings,
            focused_steps,
            missing_steps,
            utterance,
        ),
    }
}

fn technical_diagnostic(outcome: &Result<TurnOutcome, RoundError>, diagnostic: &str) -> String {
    let primary = match outcome {
        Ok(TurnOutcome::Settled(artifact)) => check_terminal(&artifact.response)
            .err()
            .map(|error| error.to_string()),
        Ok(TurnOutcome::ProtocolDefect(response)) => Some(protocol_error(response).to_string()),
        Ok(TurnOutcome::Exhausted { reason }) => Some(reason.clone()),
        Err(error) => Some(error.to_string()),
    };
    match primary {
        Some(primary) => format!("{primary}\n\n{diagnostic}"),
        None => diagnostic.to_owned(),
    }
}

/// Interpret a settled technical turn using the same state and executor for
/// any document correction. File settlement belongs to the caller.
#[allow(clippy::too_many_arguments)]
pub(super) async fn continue_after_turn(
    outcome: Result<TurnOutcome, RoundError>,
    executor: &mut ReadyPromptExecutor,
    owner: &crate::api::OperationGuard,
    state: &mut TurnState,
    root: &Path,
    task: &str,
    focus: Option<&DiscussionTarget>,
    cancel: &CancellationToken,
    events: &EventSink,
    #[cfg(test)] write_gate: Option<(
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    )>,
) -> Result<ReplResult, ExecutionError> {
    let artifact = match outcome {
        Ok(TurnOutcome::Settled(artifact)) => artifact,
        Ok(TurnOutcome::ProtocolDefect(response)) => return Err(protocol_error(&response)),
        Ok(TurnOutcome::Exhausted { reason }) => return Err(ExecutionError::Other(reason)),
        Err(RoundError::Execution(error)) => return Err(error),
        Err(RoundError::BudgetExhausted { spent: 0 }) => {
            let spent = 0;
            let text = format!("**[Round Budget Reached]** The turn spent its whole budget of {spent} provider call(s). Ask again to continue from the material already gathered.");
            return Ok(ReplResult::Answer(ReplText::Environment(text)));
        }
        Err(RoundError::BudgetExhausted { spent }) => {
            return Err(ExecutionError::Other(format!(
                "The turn's provider-call budget was exhausted after {spent} call(s)."
            )));
        }
    };
    tracing::debug!(
        rounds = artifact.rounds_spent,
        "interactive technical turn settled"
    );
    let envelope = artifact.response;
    check_terminal(&envelope)?;
    let source = artifact.response_round;
    let result = if let Some(outcome) = artifact.edit {
        ReplResult::Edit {
            text: response_text(envelope, source)?,
            outcome,
        }
    } else if let Some(operation) = [LlmCapability::PlanCreation, LlmCapability::PlanUpdate]
        .into_iter()
        .find(|&capability| envelope.has(capability))
    {
        ReplResult::Document(
            apply_plan_commands(
                envelope,
                source,
                root,
                focus,
                operation,
                state,
                executor,
                events,
                cancel,
                task,
                owner,
                #[cfg(test)]
                write_gate,
            )
            .await?,
        )
    } else if envelope.has(LlmCapability::ReviewUpdate) {
        ReplResult::Document(
            apply_review_commands(envelope, source, root, focus, cancel, owner).await?,
        )
    } else {
        ReplResult::Answer(response_text(envelope, source)?)
    };
    if cancel.is_cancelled() && result.changes().is_none_or(DocumentChanges::is_empty) {
        return Err(ExecutionError::Cancelled);
    }
    Ok(result)
}

fn response_text(
    mut envelope: ResponseEnvelope,
    source: Option<AcceptedRoundId>,
) -> Result<ReplText, ExecutionError> {
    let message = extract_text_message(&mut envelope);
    envelope.take_any(&[
        LlmCapability::Scratchpad,
        LlmCapability::CommitMessage,
        LlmCapability::TurnSkip,
    ]);
    if !envelope.commands().is_empty() {
        return Err(ExecutionError::Other(
            "Unhandled commands remained in the interactive response.".into(),
        ));
    }
    Ok(match (source, message) {
        (Some(round), Some(text)) => ReplText::Accepted { round, text },
        _ => ReplText::Environment("Research completed.".into()),
    })
}

fn protocol_error(response: &ResponseEnvelope) -> ExecutionError {
    ExecutionError::Other(
        response
            .commands()
            .iter()
            .filter_map(|command| match command {
                LlmCommand::ProtocolError { message } => Some(message.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn check_terminal(response: &ResponseEnvelope) -> Result<(), ExecutionError> {
    for command in response.commands() {
        let reason = match command {
            LlmCommand::Blocked(blocked) => format!(
                "Execution blocked: {}\n{}",
                blocked.reason(),
                blocked.details()
            ),
            LlmCommand::Abort(abort) => format!(
                "Execution aborted: {}\n\nRecommended action:\n{}",
                abort.reason(),
                abort.recommendation()
            ),
            LlmCommand::TurnError(error) => error.error_message().to_owned(),
            LlmCommand::ProtocolError { message } => message.clone(),
            _ => continue,
        };
        return Err(ExecutionError::Other(reason));
    }
    Ok(())
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

fn plan_diagnostic(
    mut diagnostic: String,
    rejections: &[PlanRejection],
    _persisted: &[String],
) -> String {
    for rejection in rejections {
        diagnostic.push('\n');
        diagnostic.push_str(&rejection.message);
    }
    diagnostic
}

#[cfg(test)]
mod tests;
