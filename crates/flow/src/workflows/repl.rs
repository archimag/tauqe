//! Interactive state, technical execution and document correction for one task.

use super::round::{RoundError, RoundInput, RoundWorkflow};
use super::state::TurnState;
use super::turn::{TurnDisk, TurnOutcome, TurnWorkflow};
use crate::api::{
    Application, ApplicationEvent, ApplicationObserver, InteractiveTask, InteractiveTaskError,
    InteractiveTaskOutcome, InteractiveTurnCompletion, OperationGuard,
};
use crate::application::discussion_prompt::{DiscussionDocument, PreparedModelAsk};
use crate::application::edit_cycle::{EditCycle, EditTermination};
use crate::application::interactive_completion::REPL_EDIT_SUMMARY;
use crate::application::interactive_task::InteractiveTaskReport;
use crate::application::turn_setup::ResolvedTurnModel;
use crate::application::CompactionEnvironment;
use documents::{DocumentExecutor, DocumentOutcome, DocumentWrite};
use publication::{
    close_failed, publish_documents, publish_edit, publish_interruption, relay_stream,
};
use std::path::PathBuf;
use std::sync::Arc;
use tauqe_core::discussion::DiscussionTarget;
use tauqe_core::edits::EditOutcome;
use tauqe_core::execution::transcript::AcceptedRoundId;
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, PromptExecutor, PromptOutcome, ResponseEnvelope,
};
use tauqe_core::history::TurnRoundRecord;
use tauqe_core::llm::{LlmCapability, LlmCommand, ModelResult};
use tauqe_core::plan::storage::PlanMutationMode;
use tauqe_core::plan::{PlanRejection, PlanRejectionKind};
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::storage::ReviewDecisionOutcome;
use tauqe_core::workflow::policy::ApplyPolicy;
use tauqe_core::workflow::research::step::ResearchLoopConfig;
use tauqe_core::workflow::WorkflowOptions;

pub(crate) mod documents;
mod publication;

pub(crate) enum ReplResult {
    Answer(ReplText),
    Edit {
        text: ReplText,
        outcome: EditOutcome,
    },
    Document(DocumentResult),
}

pub(crate) enum ReplText {
    Accepted {
        round: AcceptedRoundId,
        text: String,
    },
    Environment(String),
}

impl ReplText {
    fn into_parts(self) -> (String, Option<AcceptedRoundId>) {
        match self {
            Self::Accepted { round, text } => (text, Some(round)),
            Self::Environment(text) => (text, None),
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
}

impl DocumentResult {
    fn review(disposition: DocumentDisposition) -> Self {
        Self {
            disposition,
            plan_rejections: Vec::new(),
            plan_warnings: Vec::new(),
        }
    }
}

struct ReplTask {
    state: TurnState,
    root: PathBuf,
    task: String,
    focus: Option<DiscussionTarget>,
    documents: DocumentExecutor,
}

struct StartedRepl {
    task: InteractiveTask,
    events: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
    turn_id: u64,
    read_rounds: Box<dyn FnOnce() -> Vec<TurnRoundRecord> + Send>,
}

enum TaskSettlement {
    Returned(Box<InteractiveTaskOutcome>),
    Interrupted {
        cause: InterruptionCause,
        diagnostic: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterruptionCause {
    Cancelled,
    TaskFailed,
    StreamFailed,
    MissingEdit,
}

enum TurnPublication {
    Completed(Box<InteractiveTurnCompletion>),
    HistoryFailed {
        completion: Box<InteractiveTurnCompletion>,
        diagnostic: String,
    },
    Interrupted {
        cause: InterruptionCause,
        diagnostic: String,
        history_error: Option<String>,
    },
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    application: &Application,
    operation: Arc<OperationGuard>,
    prepared: PreparedModelAsk,
    model: ResolvedTurnModel,
    executor: PromptExecutor,
    policy: Box<dyn ApplyPolicy>,
    compaction: CompactionEnvironment,
    observer: Arc<dyn ApplicationObserver>,
) {
    let id = operation.publication_id();
    if let Err(error) = application.compact_history(&operation, &compaction).await {
        tracing::warn!(%error, "session history is unavailable for compaction");
    }
    if *operation.cancel_receiver().borrow() {
        observer.publish(&id, ApplicationEvent::Stream(StreamEvent::Cancelled));
        return;
    }
    observer.publish(
        &id,
        ApplicationEvent::Started {
            model: model.model.clone(),
            role: model.role,
            alias: model.alias.clone(),
            effort: model.effort,
            plan_context: None,
            work_context: Some(tauqe_core::workflow::turn::TurnWorkContext::Interactive),
            is_batch: false,
        },
    );

    let model_ref = model.model.clone();
    let execution = match start_task(
        application,
        operation.clone(),
        prepared,
        model,
        executor,
        policy,
    )
    .await
    {
        Ok(execution) => execution,
        Err(error) => {
            observer.publish(
                &id,
                ApplicationEvent::Stream(StreamEvent::Error(error.to_string())),
            );
            return;
        }
    };
    let mut cancel = operation.cancel_receiver();
    let usage_observer = observer.clone();
    let usage = application
        .begin_usage(&id, move |id, snapshot| {
            usage_observer.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()));
        })
        .await;
    let edit_observer = observer.clone();
    let edit_id = id.clone();
    let cycle = EditCycle::new(move |event| {
        edit_observer.publish(&edit_id, ApplicationEvent::Edit(event));
    });
    let stream = relay_stream(
        application,
        observer.as_ref(),
        &id,
        &cycle,
        &usage,
        execution.events,
        &mut cancel,
    )
    .await;

    let InteractiveTaskReport {
        task: joined,
        disk,
        documents,
    } = execution.task.join().await;
    let worker_failure = documents.join_error.map(|error| {
        tracing::error!(operation_id = %id, %error, "Document worker did not complete");
        format!("The document worker did not complete: {error}")
    });
    let documents = documents.receipts;
    let rounds = (execution.read_rounds)();
    let turn_id = execution.turn_id;
    let disk = disk.as_ref();
    let disk_notice = disk.and_then(|disk| disk.notice());
    let finalized_edit = disk.and_then(|disk| disk.finalized_edit());
    let task = classify_task(
        joined,
        stream.cancelled || *cancel.borrow(),
        stream.error.as_deref(),
        worker_failure.as_deref(),
        finalized_edit.as_ref(),
    );
    let publication = match task {
        TaskSettlement::Returned(task) => {
            let (mut completion, source) = completion_for(task.result);
            let history = record_completion(
                application,
                &operation,
                turn_id,
                rounds,
                &mut completion,
                source,
            )
            .await;
            if let Err(error) = application
                .context_session()
                .merge_turn_context(task.context)
                .await
            {
                tracing::warn!(%error, "Failed to merge the turn context");
            }
            let completion = Box::new(completion);
            match history {
                Ok(()) => TurnPublication::Completed(completion),
                Err(diagnostic) => TurnPublication::HistoryFailed {
                    completion,
                    diagnostic,
                },
            }
        }
        TaskSettlement::Interrupted {
            cause,
            mut diagnostic,
        } => {
            append_receipt_notices(&mut diagnostic, disk_notice.as_deref(), &documents);
            let message = match cause {
                InterruptionCause::Cancelled if diagnostic.is_empty() && rounds.is_empty() => None,
                InterruptionCause::Cancelled => {
                    let mut message = "Operation cancelled".to_owned();
                    append_notice(&mut message, &diagnostic);
                    Some(message)
                }
                InterruptionCause::TaskFailed => {
                    Some(format!("Workflow task failed: {diagnostic}"))
                }
                InterruptionCause::StreamFailed | InterruptionCause::MissingEdit => {
                    Some(diagnostic.clone())
                }
            };
            let history_error = record_interruption(
                application,
                turn_id,
                &rounds,
                message,
                finalized_edit.as_ref(),
            )
            .await
            .err();
            TurnPublication::Interrupted {
                cause,
                diagnostic,
                history_error,
            }
        }
    };

    usage.flush().await;
    let usage = application.usage_snapshot().await;
    observer.publish(&id, ApplicationEvent::Usage(usage.clone()));

    // Publish settled writes even when there is no ordinary terminal result.
    let edit = match &publication {
        TurnPublication::Completed(completion)
        | TurnPublication::HistoryFailed { completion, .. } => match &completion.result {
            ModelResult::Edit { outcome, .. } => Some(outcome),
            ModelResult::Answer { .. } => None,
        },
        TurnPublication::Interrupted { .. } => finalized_edit.as_ref(),
    };
    if let Some(edit) = edit {
        publish_edit(application, observer.as_ref(), &id, &cycle, edit).await;
        if edit.applied {
            application.clean_up_interactive_edit().await;
        }
    } else {
        match &publication {
            TurnPublication::Interrupted {
                cause: InterruptionCause::Cancelled,
                diagnostic,
                ..
            } => close_failed(&cycle, EditTermination::Cancelled, diagnostic.clone()),
            TurnPublication::Interrupted { diagnostic, .. }
            | TurnPublication::HistoryFailed { diagnostic, .. } => {
                close_failed(&cycle, EditTermination::Aborted, diagnostic.clone())
            }
            TurnPublication::Completed(completion) => {
                if let Some(error) = &completion.error {
                    close_failed(&cycle, EditTermination::StagingRefused, error.clone());
                } else {
                    cycle.close_answered();
                }
            }
        }
    }
    if !matches!(&publication, TurnPublication::Interrupted { .. })
        || edit.is_some_and(|edit| edit.applied)
    {
        observer.publish(
            &id,
            ApplicationEvent::Context(application.context_state().await),
        );
    }
    publish_documents(application, observer.as_ref(), &id, &documents).await;

    match publication {
        TurnPublication::Completed(completion) => {
            let full_text = match &completion.result {
                ModelResult::Answer { text } => text.clone(),
                ModelResult::Edit { summary, .. } => summary.clone(),
            };
            tracing::info!(operation_id = %id, model = %model_ref, usage = ?usage.prev,
                error = ?completion.error, "Model turn finished");
            observer.publish(&id, ApplicationEvent::Result { completion, usage });
            observer.publish(&id, ApplicationEvent::Finished(full_text));
        }
        TurnPublication::HistoryFailed {
            completion,
            mut diagnostic,
        } => {
            if let Some(error) = &completion.error {
                append_notice(&mut diagnostic, error);
            }
            append_receipt_notices(&mut diagnostic, disk_notice.as_deref(), &documents);
            publish_interruption(observer.as_ref(), &id, false, diagnostic);
        }
        TurnPublication::Interrupted {
            cause,
            mut diagnostic,
            history_error,
        } => {
            if let Some(error) = history_error {
                append_notice(&mut diagnostic, &error);
            }
            publish_interruption(
                observer.as_ref(),
                &id,
                cause == InterruptionCause::Cancelled,
                diagnostic,
            );
        }
    }
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
            // A failed finalization gets its own diagnostic response. The
            // accepted model reply stays among the rounds.
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

async fn record_completion(
    application: &Application,
    operation: &OperationGuard,
    turn_id: u64,
    mut rounds: Vec<TurnRoundRecord>,
    completion: &mut InteractiveTurnCompletion,
    source: Option<AcceptedRoundId>,
) -> Result<(), String> {
    let response_record =
        take_response_record(source, &mut rounds, &mut completion.history_response);
    record_rounds(application, turn_id, &rounds).await?;
    if let Err(error) = application
        .persist_interactive_completion(operation, completion)
        .await
    {
        let mut diagnostic = format!("Could not record the final response: {error}");
        if let Some(record) = response_record {
            rounds.push(record);
            if let Err(error) = application.set_history_turn_rounds(turn_id, rounds).await {
                append_notice(
                    &mut diagnostic,
                    &format!("Could not retain the final accepted round: {error}"),
                );
            }
        }
        return Err(diagnostic);
    }
    Ok(())
}

async fn record_interruption(
    application: &Application,
    turn_id: u64,
    rounds: &[TurnRoundRecord],
    message: Option<String>,
    edit: Option<&EditOutcome>,
) -> Result<(), String> {
    let result = if let Some(message) = message {
        record_rounds(application, turn_id, rounds).await?;
        application
            .record_interactive_response(message, edit.map(|edit| (edit, REPL_EDIT_SUMMARY)))
            .await
    } else {
        application
            .rollback_history_entry(turn_id, "Operation cancelled".into())
            .await
    };
    result.map_err(|error| format!("Could not record the interruption: {error}"))
}

fn classify_task(
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
                // A drained disk receipt can outlive the response that was to describe it.
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

fn take_response_record(
    source: Option<AcceptedRoundId>,
    rounds: &mut Vec<TurnRoundRecord>,
    response: &mut String,
) -> Option<TurnRoundRecord> {
    let source = source?;
    // Only the last record can move without reordering the dialogue.
    response.clear();
    if rounds
        .last()
        .is_some_and(|record| source.identifies(record))
    {
        let record = rounds.pop()?;
        response.clone_from(&record.message);
        Some(record)
    } else {
        None
    }
}

async fn record_rounds(
    application: &Application,
    turn_id: u64,
    rounds: &[TurnRoundRecord],
) -> Result<(), String> {
    if rounds.is_empty() {
        return Ok(());
    }
    application
        .set_history_turn_rounds(turn_id, rounds.to_vec())
        .await
        .map_err(|error| format!("Could not record accepted rounds: {error}"))
}

fn append_notice(diagnostic: &mut String, notice: &str) {
    if !notice.is_empty() && !diagnostic.contains(notice) {
        if !diagnostic.is_empty() {
            diagnostic.push_str("\n\n");
        }
        diagnostic.push_str(notice);
    }
}

fn append_receipt_notices(
    diagnostic: &mut String,
    disk_notice: Option<&str>,
    documents: &[Arc<DocumentOutcome>],
) {
    if let Some(notice) = disk_notice {
        append_notice(diagnostic, notice);
    }
    for receipt in documents {
        match receipt.as_ref() {
            DocumentOutcome::Plans(outcomes) => {
                for outcome in outcomes {
                    for plan in &outcome.applied {
                        append_notice(
                            diagnostic,
                            &format!("Plan document already saved: {}.", plan.id),
                        );
                    }
                    if let Some(error) = &outcome.storage_error {
                        append_notice(diagnostic, &format!("Plan storage failed: {error}"));
                    }
                }
            }
            DocumentOutcome::Review {
                item_id, result, ..
            } => match result {
                Ok(ReviewDecisionOutcome::Saved) => append_notice(
                    diagnostic,
                    &format!("Review decision already saved for finding {item_id}."),
                ),
                Err(error) => append_notice(diagnostic, &format!("Review storage failed: {error}")),
                Ok(_) => {}
            },
            DocumentOutcome::OffloadFailed(error) => {
                append_notice(diagnostic, &format!("Document worker failed: {error}"))
            }
        }
    }
}

async fn start_task(
    application: &Application,
    operation: Arc<OperationGuard>,
    prepared: PreparedModelAsk,
    model: ResolvedTurnModel,
    mut executor: PromptExecutor,
    policy: Box<dyn ApplyPolicy>,
) -> Result<StartedRepl, InteractiveTaskError> {
    if !application.owns_operation(&operation) {
        return Err(InteractiveTaskError::OperationNotOwned);
    }
    let root = application.repository_root().await;
    let task = match &prepared.discussion {
        Some(discussion) => format_discussion_task(&discussion.document, &prepared.utterance),
        None => prepared.utterance.clone(),
    };
    let history = application
        .prepare_interactive_history(&operation, prepared.utterance)
        .await
        .map_err(|error| {
            InteractiveTaskError::TaskFailed(format!(
                "Could not start interactive turn history: {error}"
            ))
        })?;
    let turn_id = history.turn_id;
    let config = &model.config;
    let focus = prepared.discussion.map(|discussion| discussion.target);
    let state = TurnState::new(
        application.context_for_turn().await,
        tauqe_core::llm::repl::repl_capabilities(focus.as_ref()),
        Some(history.history),
        Some(WorkflowOptions::from_app_config(config).max_turn_rounds()),
        config.context.max_scratchpad_chars(),
    );
    // Retain only accepted history outside the supervised task. It remains
    // available after destruction and transaction recovery.
    let read_rounds = Box::new(state.history_reader());
    let cancel = CancellationToken::from_receiver(operation.cancel_receiver());
    let (documents, document_worker) = DocumentExecutor::start(
        root.clone(),
        cancel.clone(),
        #[cfg(test)]
        application
            .interactive_execution()
            .document_write_gate
            .lock()
            .await
            .take(),
    );
    let mut repl = ReplTask {
        state,
        root,
        task,
        focus,
        documents,
    };
    let (sink, events) = EventSink::channel();
    let (disk, worker) = TurnDisk::start(repl.root.clone(), policy);
    let task = application.start_interactive_task(operation, move |context| async move {
        repl.state.context = context;
        let result = execute_task(&model, &mut executor, &disk, &mut repl, &cancel, &sink).await;
        InteractiveTaskOutcome {
            result,
            context: repl.state.context,
        }
    })?;
    Ok(StartedRepl {
        task: task
            .with_disk_worker(worker)
            .with_document_worker(document_worker),
        events,
        turn_id,
        read_rounds,
    })
}

fn format_discussion_task(document: &DiscussionDocument, utterance: &str) -> String {
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
        } => tauqe_core::review::prompt::format_review_group_discussion_prompt(
            session, group, findings, utterance,
        ),
    }
}

async fn execute_task(
    model: &ResolvedTurnModel,
    executor: &mut PromptExecutor,
    disk: &TurnDisk,
    repl: &mut ReplTask,
    cancel: &CancellationToken,
    events: &EventSink,
) -> Result<ReplResult, ExecutionError> {
    let ReplTask {
        state,
        root,
        task,
        focus,
        documents,
    } = repl;
    let config = &model.config;
    let project = executor.project(root, Some(&cancel.receiver())).await;
    let (repo_map, repo_dirs) = if config.context.include_repo_map {
        (
            executor
                .repo_map(root, config.context.repomap_token_budget())
                .await,
            None,
        )
    } else {
        (
            None,
            executor
                .repo_dirs(root, state.context.pinned_patterns())
                .await,
        )
    };
    events.emit(StreamEvent::RepoMapTokens(
        repo_map
            .as_deref()
            .or(repo_dirs.as_deref())
            .map(tauqe_core::prompt::estimate_tokens)
            .unwrap_or(0),
    ));
    let input = || RoundInput {
        task,
        repo_root: root,
        project: &project,
        repo_map: repo_map.as_deref(),
        repo_dirs: repo_dirs.as_deref(),
        outer_docs: &config.outer_docs,
        brief: None,
        feedback: None,
        reason: None,
        staged: None,
    };
    let mut round = RoundWorkflow {
        executor,
        events,
        cancel,
    };
    let turn = TurnWorkflow {
        round: RoundWorkflow {
            executor: &mut *round.executor,
            events,
            cancel,
        },
        disk,
        research: &ResearchLoopConfig::from_context(&config.context),
    }
    .run(state, input())
    .await;
    if let Some(diagnostic) = turn.rollback.diagnostic() {
        let primary = match &turn.outcome {
            Ok(TurnOutcome::Settled(artifact)) => check_terminal(&artifact.response)
                .err()
                .map(|error| error.to_string()),
            Ok(TurnOutcome::ProtocolDefect(response)) => Some(protocol_error(response).to_string()),
            Ok(TurnOutcome::Exhausted { reason }) => Some(reason.clone()),
            Err(error) => Some(error.to_string()),
        };
        let message = match primary {
            Some(primary) => format!("{primary}\n\n{diagnostic}"),
            None => diagnostic,
        };
        // Cancellation still wins as the terminal signal, but must not
        // suppress the recovery diagnostic before terminal publication.
        if turn.cancelled || cancel.is_cancelled() {
            events.emit_text_delta(format!("\n\n{message}\n\n"));
            return Err(ExecutionError::Cancelled);
        }
        return Err(ExecutionError::Other(message));
    }
    if turn.cancelled {
        return Err(ExecutionError::Cancelled);
    }
    let artifact = match turn.outcome {
        Ok(TurnOutcome::Settled(artifact)) => artifact,
        Ok(TurnOutcome::ProtocolDefect(response)) => return Err(protocol_error(&response)),
        Ok(TurnOutcome::Exhausted { reason }) => return Err(ExecutionError::Other(reason)),
        Err(RoundError::Execution(error)) => return Err(error),
        Err(RoundError::BudgetExhausted { spent }) => {
            let text = format!("**[Round Budget Reached]** The turn spent its whole budget of {spent} provider call(s). Ask again to continue from the material already gathered.");
            return Ok(ReplResult::Answer(ReplText::Environment(text)));
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
        if outcome.applied {
            tauqe_core::blocking::run_blocking(|| state.context.clear_transient());
            events.emit(StreamEvent::ContextChanged(state.context.get_state()));
        }
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
                documents,
                envelope,
                source,
                focus.as_ref(),
                operation,
                state,
                &mut round,
                input,
            )
            .await?,
        )
    } else if envelope.has(LlmCapability::ReviewUpdate) {
        ReplResult::Document(
            apply_review_commands(documents, envelope, source, focus.as_ref()).await?,
        )
    } else {
        ReplResult::Answer(response_text(envelope, source)?)
    };
    if cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn apply_plan_commands<'a>(
    documents: &DocumentExecutor,
    mut envelope: ResponseEnvelope,
    mut source: Option<AcceptedRoundId>,
    focus: Option<&DiscussionTarget>,
    operation: LlmCapability,
    state: &mut TurnState,
    round: &mut RoundWorkflow<'_>,
    input: impl Fn() -> RoundInput<'a>,
) -> Result<DocumentResult, ExecutionError> {
    let mut plan_rejections = Vec::new();
    let mut plan_warnings = Vec::new();
    let mut corrections = 0;
    let mut persisted = Vec::new();
    let disposition = loop {
        if round.cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        let receipt = write_plan_mutations(documents, &mut envelope, focus).await;
        let write_failed = |error| {
            DocumentDisposition::StorageFailed(plan_diagnostic(
                format!("Plan write failed: {error}"),
                &plan_rejections,
                &persisted,
            ))
        };
        let outcomes = match receipt.as_deref() {
            Ok(DocumentOutcome::Plans(outcomes)) => outcomes,
            Ok(DocumentOutcome::OffloadFailed(error)) => break write_failed(error.clone()),
            Err(ExecutionError::Cancelled) => return Err(ExecutionError::Cancelled),
            Err(error) => break write_failed(error.to_string()),
            Ok(DocumentOutcome::Review { .. }) => {
                return Err(ExecutionError::Other(
                    "The document worker returned a review receipt for a plan write".into(),
                ));
            }
        };
        let mut defects = Vec::new();
        let mut storage_errors = Vec::new();
        for outcome in outcomes {
            persisted.extend(outcome.applied.iter().map(|plan| plan.id.clone()));
            plan_warnings.extend(outcome.warnings.iter().cloned());
            for rejection in &outcome.errors {
                if rejection.kind == PlanRejectionKind::PayloadDefect {
                    defects.push(rejection.clone());
                } else {
                    plan_rejections.push(rejection.clone());
                }
            }
            if let Some(error) = &outcome.storage_error {
                storage_errors.push(error.as_str());
            }
        }
        if !storage_errors.is_empty() {
            plan_rejections.extend(defects);
            break DocumentDisposition::StorageFailed(plan_diagnostic(
                format!("Plan storage failed: {}", storage_errors.join("\n")),
                &plan_rejections,
                &persisted,
            ));
        }
        if defects.is_empty() {
            if !plan_rejections.is_empty() {
                break DocumentDisposition::Refused(plan_diagnostic(
                    "Plan proposal refused.".into(),
                    &plan_rejections,
                    &persisted,
                ));
            }
            break DocumentDisposition::Saved(response_text(envelope, source)?);
        }
        corrections += 1;
        state.capabilities = tauqe_core::llm::repl::plan_correction_capabilities(operation);
        let repair = plan_correction_prompt(&defects, corrections);
        let mut empty_retries = 0;
        let mut feedback = repair;
        let correction_error = loop {
            match round
                .run(
                    state,
                    RoundInput {
                        feedback: Some(&feedback),
                        ..input()
                    },
                )
                .await
            {
                Ok(reply) => {
                    match reply.result.outcome {
                        PromptOutcome::Completed(next) => {
                            envelope = next;
                            source = reply.accepted_round;
                            break check_terminal(&envelope).err().or_else(|| {
                                    (!envelope.has(operation)).then(|| ExecutionError::Other(
                                        "The corrective response did not supply the refused plan operation.".into(),
                                    ))
                                });
                        }
                        PromptOutcome::ProtocolError(response) => {
                            break Some(protocol_error(&response))
                        }
                        PromptOutcome::Empty if empty_retries < 2 => {
                            empty_retries += 1;
                            feedback = if empty_retries == 1 {
                                "Your previous reply produced no actionable result. Return the corrected plan through the active schema.".into()
                            } else {
                                String::new()
                            };
                        }
                        PromptOutcome::Empty => break Some(ExecutionError::Other(
                            "The model produced an empty response after 2 additional attempt(s)."
                                .into(),
                        )),
                    }
                }
                Err(RoundError::BudgetExhausted { .. }) => {
                    break Some(ExecutionError::Other(
                        "The turn's provider-call budget was exhausted.".into(),
                    ));
                }
                Err(RoundError::Execution(ExecutionError::Cancelled)) => {
                    return Err(ExecutionError::Cancelled)
                }
                Err(RoundError::Execution(error)) => break Some(error),
            }
        };
        if let Some(error) = correction_error {
            plan_rejections.extend(defects);
            break DocumentDisposition::Refused(plan_diagnostic(
                format!("Plan correction did not complete: {error}"),
                &plan_rejections,
                &persisted,
            ));
        }
    };
    Ok(DocumentResult {
        disposition,
        plan_rejections,
        plan_warnings,
    })
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

async fn write_plan_mutations(
    documents: &DocumentExecutor,
    envelope: &mut ResponseEnvelope,
    focus: Option<&DiscussionTarget>,
) -> Result<Arc<DocumentOutcome>, ExecutionError> {
    let updates: Vec<_> = envelope
        .take_any(&[LlmCapability::PlanCreation, LlmCapability::PlanUpdate])
        .into_iter()
        .filter_map(|command| match command {
            LlmCommand::Plan { update } => Some(update),
            _ => None,
        })
        .collect();
    let mode = if matches!(focus, Some(DiscussionTarget::Plan { .. })) {
        PlanMutationMode::Discussion
    } else {
        PlanMutationMode::Repl
    };
    documents
        .write(DocumentWrite::Plans { updates, mode })
        .await
}

fn plan_diagnostic(
    mut diagnostic: String,
    rejections: &[PlanRejection],
    persisted: &[String],
) -> String {
    for rejection in rejections {
        diagnostic.push('\n');
        diagnostic.push_str(&rejection.message);
    }
    if !persisted.is_empty() {
        diagnostic.push_str("\nPlan documents already saved: ");
        diagnostic.push_str(&persisted.join(", "));
    }
    diagnostic
}

fn plan_correction_prompt(defects: &[PlanRejection], correction: usize) -> String {
    let mut prompt = format!(
        "<response_directive>\n[Plan Correction {correction}] The plan mutations listed below were refused. Correct only those mutations and emit them through the plan branch of the action union; do not repeat mutations that were already saved.\n"
    );
    for defect in defects {
        prompt.push_str("\n<plan_rejection>\n");
        prompt.push_str(&tauqe_core::escape::escape_xml_text(&defect.message));
        prompt.push_str("\n</plan_rejection>");
    }
    prompt.push_str("\n</response_directive>");
    prompt
}

async fn apply_review_commands(
    documents: &DocumentExecutor,
    mut envelope: ResponseEnvelope,
    source: Option<AcceptedRoundId>,
    focus: Option<&DiscussionTarget>,
) -> Result<DocumentResult, ExecutionError> {
    let commands = envelope.take(LlmCapability::ReviewUpdate);
    let text = response_text(envelope, source)?;
    let Some(focus) = focus.filter(|focus| focus.review_session_id().is_some()) else {
        return Ok(DocumentResult::review(DocumentDisposition::Refused(
            "Review decision refused: no authorized review focus.".into(),
        )));
    };
    let mut diagnostics = Vec::new();
    let mut saved = Vec::new();
    let mut storage_failed = false;
    for command in commands {
        if let LlmCommand::ReviewUpdate { update } = command {
            let item_id = update.item_id;
            let receipt = documents
                .write(DocumentWrite::Review {
                    focus: focus.clone(),
                    decision: update,
                })
                .await;
            let diagnostic = match receipt.as_deref() {
                Ok(DocumentOutcome::Review {
                    item_id, result, ..
                }) => match result {
                    Ok(ReviewDecisionOutcome::Saved) => {
                        saved.push(item_id.to_string());
                        continue;
                    }
                    Ok(ReviewDecisionOutcome::MissingSession) => "review session not found".into(),
                    Ok(ReviewDecisionOutcome::MissingFinding) => {
                        "finding not found in the review focus".into()
                    }
                    Ok(ReviewDecisionOutcome::MissingGroup) => "review group not found".into(),
                    Ok(ReviewDecisionOutcome::TargetMismatch) => {
                        "proposal does not match the review focus".into()
                    }
                    Ok(ReviewDecisionOutcome::EmptyGroupNote) => {
                        "a group decision requires a non-empty note".into()
                    }
                    Err(error) => {
                        storage_failed = true;
                        format!("review storage failed: {error}")
                    }
                },
                Ok(DocumentOutcome::OffloadFailed(error)) => {
                    storage_failed = true;
                    format!("document worker failed: {error}")
                }
                Ok(DocumentOutcome::Plans(_)) => {
                    storage_failed = true;
                    "document worker returned a plan receipt instead of a review receipt".into()
                }
                Err(ExecutionError::Cancelled) => return Err(ExecutionError::Cancelled),
                Err(error) => {
                    storage_failed = true;
                    format!("document worker failed: {error}")
                }
            };
            diagnostics.push(format!(
                "Review decision for finding {item_id} not saved: {diagnostic}."
            ));
        }
    }
    if diagnostics.is_empty() {
        return Ok(DocumentResult::review(DocumentDisposition::Saved(text)));
    }
    if !saved.is_empty() {
        diagnostics.push(format!(
            "Review decisions already saved for findings: {}.",
            saved.join(", ")
        ));
    }
    let diagnostic = diagnostics.join("\n");
    Ok(DocumentResult::review(if storage_failed {
        DocumentDisposition::StorageFailed(diagnostic)
    } else {
        DocumentDisposition::Refused(diagnostic)
    }))
}

#[cfg(test)]
mod tests;
