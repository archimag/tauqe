//! Pure classification of an interactive report into a terminal and journal decision.

use super::{DocumentDisposition, ReplResult};
use crate::api::{InteractiveTaskError, InteractiveTaskOutcome, InteractiveTurnCompletion};
use crate::application::interactive_history::{CompleteTurn, InterruptTurn, OpenInteractiveTurn};
use crate::application::interactive_task::InteractiveTaskReport;
use crate::workflows::repl::documents::DocumentChanges;
use tauqe_core::context::ContextManager;
use tauqe_core::edits::transaction::RollbackOutcome;
use tauqe_core::edits::EditOutcome;
use tauqe_core::execution::AcceptedRoundId;
use tauqe_core::execution::ExecutionError;
use tauqe_core::history::TurnRoundRecord;
use tauqe_core::llm::ModelResult;
use tauqe_core::workflow::policy::owned::Settlement;

pub(super) enum TaskSettlement {
    Returned(Box<InteractiveTaskOutcome>),
    Interrupted {
        cause: InterruptionCause,
        diagnostic: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InterruptionCause {
    Cancelled,
    TaskFailed,
    StreamFailed,
    MissingEdit,
}

pub(super) struct Completed {
    pub completion: InteractiveTurnCompletion,
    pub history: CompleteTurn,
}

pub(super) struct Interrupted {
    pub cause: InterruptionCause,
    pub diagnostic: String,
    pub history: InterruptTurn,
}

pub(super) enum Decision {
    Completed {
        completion: Box<InteractiveTurnCompletion>,
        context: Box<ContextManager>,
        history: CompleteTurn,
    },
    Interrupted {
        cause: InterruptionCause,
        diagnostic: String,
        history: InterruptTurn,
    },
}

pub(super) struct Classified {
    pub open: OpenInteractiveTurn,
    pub decision: Decision,
    pub documents: DocumentChanges,
    pub disk_notice: Option<String>,
    pub finalized_edit: Option<EditOutcome>,
}

/// Decide from the complete report without performing journal or context IO.
pub(super) fn classify(
    report: InteractiveTaskReport,
    cancelled: bool,
    stream_error: Option<&str>,
) -> Classified {
    let InteractiveTaskReport {
        preparation: open,
        task,
        rounds,
        source,
        settlement,
        diagnostic,
        documents,
    } = report;
    let recovery_unknown = matches!(
        &settlement,
        Settlement::RolledBack(
            RollbackOutcome::Incomplete { .. }
                | RollbackOutcome::RestoredFromBackup {
                    policy_error: Some(_)
                }
        )
    );
    let finalized_edit = match &settlement {
        Settlement::Finalized { edit, .. } => Some(edit.clone()),
        _ => None,
    };
    let cancelled = cancelled
        || matches!(
            &settlement,
            Settlement::Finalized {
                cancelled: true,
                ..
            } | Settlement::NoChanges { cancelled: true }
        );
    let disk_notice = diagnostic;
    let decision = match classify_task(task, cancelled, stream_error, None, finalized_edit.as_ref())
    {
        TaskSettlement::Returned(_task) if recovery_unknown => {
            let interrupted = interrupt(
                InterruptionCause::TaskFailed,
                String::new(),
                rounds,
                disk_notice.as_deref(),
                &documents,
                finalized_edit.clone(),
                true,
            );
            Decision::Interrupted {
                cause: interrupted.cause,
                diagnostic: interrupted.diagnostic,
                history: interrupted.history,
            }
        }
        TaskSettlement::Returned(task) => {
            let completed = complete(task.result, rounds, source);
            Decision::Completed {
                completion: Box::new(completed.completion),
                context: Box::new(task.context),
                history: completed.history,
            }
        }
        TaskSettlement::Interrupted { cause, diagnostic } => {
            let interrupted = interrupt(
                cause,
                diagnostic,
                rounds,
                disk_notice.as_deref(),
                &documents,
                finalized_edit.clone(),
                recovery_unknown,
            );
            Decision::Interrupted {
                cause: interrupted.cause,
                diagnostic: interrupted.diagnostic,
                history: interrupted.history,
            }
        }
    };
    Classified {
        open,
        decision,
        documents,
        disk_notice,
        finalized_edit,
    }
}

pub(super) fn complete(
    result: Result<ReplResult, ExecutionError>,
    rounds: Vec<TurnRoundRecord>,
    source: Option<AcceptedRoundId>,
) -> Completed {
    let (completion, _) = completion_for(result);
    // A failed finalization has its own diagnostic response. Keep the
    // accepted reply among the rounds instead of moving it to Response.
    let source = source.filter(|_| completion.error.is_none());
    let history = CompleteTurn {
        rounds,
        source,
        response: completion.history_response.clone(),
        edit: match &completion.result {
            ModelResult::Edit { outcome, .. } => Some(outcome.clone()),
            ModelResult::Answer { .. } => None,
        },
    };
    Completed {
        completion,
        history,
    }
}

pub(super) fn interrupt(
    cause: InterruptionCause,
    mut diagnostic: String,
    rounds: Vec<TurnRoundRecord>,
    disk_notice: Option<&str>,
    documents: &DocumentChanges,
    finalized_edit: Option<EditOutcome>,
    recovery_unknown: bool,
) -> Interrupted {
    append_receipt_notices(&mut diagnostic, disk_notice, documents);
    if recovery_unknown {
        append_notice(
            &mut diagnostic,
            "The edit transaction's recovery is unconfirmed.",
        );
    }
    let message = match cause {
        InterruptionCause::Cancelled if diagnostic.is_empty() && rounds.is_empty() => None,
        InterruptionCause::Cancelled => {
            let mut message = "Operation cancelled".to_owned();
            append_notice(&mut message, &diagnostic);
            Some(message)
        }
        InterruptionCause::TaskFailed => Some(format!("Workflow task failed: {diagnostic}")),
        InterruptionCause::StreamFailed | InterruptionCause::MissingEdit => {
            Some(diagnostic.clone())
        }
    };
    let history = InterruptTurn {
        rounds,
        reason: message.unwrap_or_else(|| "Operation cancelled".into()),
        diagnostic: diagnostic.clone(),
        cancelled: cause == InterruptionCause::Cancelled,
        confirmed_edit: finalized_edit,
        confirmed_documents: !documents.is_empty(),
    };
    Interrupted {
        cause,
        diagnostic,
        history,
    }
}

pub(super) fn classify_task(
    joined: Result<InteractiveTaskOutcome, InteractiveTaskError>,
    cancellation_signalled: bool,
    stream_error: Option<&str>,
    worker_failure: Option<&str>,
    finalized_edit: Option<&EditOutcome>,
) -> TaskSettlement {
    use InterruptionCause::{Cancelled, MissingEdit, StreamFailed, TaskFailed};

    let (cause, mut diagnostic, task) = match joined {
        Err(error) => (
            if cancellation_signalled {
                Cancelled
            } else {
                TaskFailed
            },
            error.to_string(),
            None,
        ),
        Ok(task) => {
            let (cause, diagnostic) = if cancellation_signalled
                || matches!(task.result, Err(ExecutionError::Cancelled))
            {
                (Cancelled, String::new())
            } else if let Some(error) = worker_failure.filter(|_| task.result.is_ok()) {
                (TaskFailed, error.to_owned())
            } else if let Some(error) = stream_error {
                (StreamFailed, error.to_owned())
            } else if finalized_edit.is_some()
                && !matches!(task.result, Ok(ReplResult::Edit { .. }))
            {
                let diagnostic = if task.result.is_ok() {
                    "The task did not return the finalized edit.".into()
                } else {
                    String::new()
                };
                (MissingEdit, diagnostic)
            } else {
                return TaskSettlement::Returned(Box::new(task));
            };
            (cause, diagnostic, Some(task))
        }
    };
    if cause == Cancelled {
        if let Some(error) = stream_error {
            append_notice(&mut diagnostic, error);
        }
    }
    if let Some(error) = task.and_then(|task| task.result.err()) {
        if !matches!(error, ExecutionError::Cancelled) {
            let error = error.to_string();
            if cause == Cancelled || !stream_error.is_some_and(|stream| error.ends_with(stream)) {
                append_notice(&mut diagnostic, &error);
            }
        }
    }
    if let Some(error) = worker_failure {
        append_notice(&mut diagnostic, error);
    }
    TaskSettlement::Interrupted { cause, diagnostic }
}

fn completion_for(
    result: Result<ReplResult, ExecutionError>,
) -> (InteractiveTurnCompletion, Option<AcceptedRoundId>) {
    match result {
        Ok(ReplResult::Answer(text)) => {
            let (text, source) = text.into_parts();
            (InteractiveTurnCompletion::answer(text), source)
        }
        Ok(ReplResult::Edit { text, outcome }) => {
            let (text, source) = text.into_parts();
            let source = if outcome.error.is_some() {
                None
            } else {
                source
            };
            (InteractiveTurnCompletion::edit(text, outcome), source)
        }
        Ok(ReplResult::Document(document)) => {
            let (mut completion, source) = match document.disposition {
                DocumentDisposition::Saved(text) => {
                    let (text, source) = text.into_parts();
                    (InteractiveTurnCompletion::answer(text), source)
                }
                DocumentDisposition::Refused(error) | DocumentDisposition::StorageFailed(error) => {
                    (InteractiveTurnCompletion::failed(error), None)
                }
            };
            completion.plan_rejections = document.plan_rejections;
            completion.plan_warnings = document.plan_warnings;
            (completion, source)
        }
        Err(error) => (InteractiveTurnCompletion::failed(error.to_string()), None),
    }
}

pub(super) fn append_notice(diagnostic: &mut String, notice: &str) {
    if !notice.is_empty() && !diagnostic.contains(notice) {
        if !diagnostic.is_empty() {
            diagnostic.push_str("\n\n");
        }
        diagnostic.push_str(notice);
    }
}

pub(super) fn append_receipt_notices(
    diagnostic: &mut String,
    disk_notice: Option<&str>,
    documents: &DocumentChanges,
) {
    if let Some(notice) = disk_notice {
        append_notice(diagnostic, notice);
    }
    for plan in &documents.plans {
        append_notice(
            diagnostic,
            &format!("Plan document already saved: {}.", plan.id),
        );
    }
    for review in &documents.reviews {
        append_notice(
            diagnostic,
            &format!("Review session already saved: {}.", review.id),
        );
    }
}
