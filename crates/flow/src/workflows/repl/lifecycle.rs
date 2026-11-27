//! Interactive operation coordination; technical lifetimes stay with their owners.

use super::format_discussion_task;
use super::publication::{
    close_failed, publish_documents, publish_edit, publish_interruption, relay_stream,
};
use super::settlement::{
    append_notice, append_receipt_notices, classify, Classified, Decision, InterruptionCause,
};
use crate::api::{
    ApplicationEvent, ApplicationObserver, InteractiveTask, InteractiveTaskError,
    InteractiveTaskOutcome, InteractiveTurnCompletion, OperationGuard,
};
use crate::application::context::ContextSession;
use crate::application::discussion_prompt::PreparedModelAsk;
use crate::application::edit_cycle::{EditCycle, EditTermination};
use crate::application::execution_scope::ExecutionScope;
use crate::application::history::HistorySession;
use crate::application::interactive_task::{InteractiveExecution, InteractiveTaskResult};
use crate::application::ExecutionSession;
use crate::workflows::state::TurnState;
use crate::workflows::turn::{run_technical_turn, TechnicalObservation};
use std::sync::Arc;
use tauqe_core::config::AppConfig;
use tauqe_core::execution::task::TaskOutcome;
use tauqe_core::execution::usage::UsageBinding;
use tauqe_core::execution::{EventSink, ExecutionError, ReadyPromptExecutor};
use tauqe_core::llm::ModelResult;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::policy::ApplyPolicy;

pub(crate) struct LifecycleServices {
    pub(crate) context: ContextSession,
    pub(crate) interactive: InteractiveExecution,
    pub(crate) execution: ExecutionSession,
    pub(crate) history: HistorySession,
}

enum TurnPublication {
    Completed(Box<InteractiveTurnCompletion>),
    SettlementFailed {
        completion: Box<InteractiveTurnCompletion>,
        diagnostic: String,
    },
    Interrupted {
        cause: InterruptionCause,
        diagnostic: String,
        history_error: Option<String>,
    },
}

struct WindowSettlement {
    publication: TurnPublication,
    late_diagnostic: String,
    model_ref: tauqe_core::models::ModelRef,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    services: LifecycleServices,
    operation: Arc<OperationGuard>,
    prepared: PreparedModelAsk,
    max_turn_rounds: usize,
    config: Arc<AppConfig>,
    scope: ExecutionScope,
    executor: ReadyPromptExecutor,
    policy: Box<dyn ApplyPolicy>,
    observer: Arc<dyn ApplicationObserver>,
) {
    let id = operation.publication_id();
    if operation.hard_cancellation().is_cancelled() {
        observer.publish(&id, ApplicationEvent::Stream(StreamEvent::Cancelled));
        return;
    }
    let identity = executor.identity();
    if !services
        .context
        .activate_manual_publication(&operation, observer.clone())
    {
        publish_interruption(
            observer.as_ref(),
            &id,
            false,
            "Could not bind interactive context publication".into(),
        );
        return;
    }
    observer.publish(
        &id,
        ApplicationEvent::Started {
            model: identity.model().clone(),
            role: identity.role(),
            alias: identity.alias().map(str::to_owned),
            effort: identity.effort(),
            plan_context: None,
            work_context: Some(tauqe_core::workflow::turn::TurnWorkContext::Interactive),
            is_batch: false,
        },
    );

    let usage = scope.binding();
    let publisher = observer.clone();
    let window_observer = observer.clone();
    let window_id = id.clone();
    let context = services.context.clone();
    #[cfg(test)]
    let terminal_gate = services.interactive.terminal_gate.clone();
    let work_operation = operation.clone();
    let settled = scope
        .run(
            &context,
            id.clone(),
            move |id, snapshot| {
                publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()));
            },
            async move {
                run_window(
                    services,
                    work_operation,
                    prepared,
                    max_turn_rounds,
                    config,
                    usage,
                    executor,
                    policy,
                    window_observer,
                    window_id,
                )
                .await
            },
            |_| {},
            |result| async move { result },
        )
        .await;
    let (settlement, receipt) = match settled {
        Ok((TaskOutcome::Completed(settlement), receipt)) => (settlement, receipt),
        Ok((TaskOutcome::Failed(error), _)) => {
            context.close_research_publication(operation.id());
            let cancelled = operation.hard_cancellation().is_cancelled();
            publish_interruption(
                observer.as_ref(),
                &id,
                cancelled,
                format!("Interactive accounting task did not complete: {error}"),
            );
            return;
        }
        Ok((TaskOutcome::Cancelled | TaskOutcome::Orphaned, _)) => {
            context.close_research_publication(operation.id());
            let cancelled = operation.hard_cancellation().is_cancelled();
            publish_interruption(
                observer.as_ref(),
                &id,
                cancelled,
                "Interactive accounting task did not complete".into(),
            );
            return;
        }
        Err(error) => {
            context.close_research_publication(operation.id());
            let cancelled = operation.hard_cancellation().is_cancelled();
            publish_interruption(observer.as_ref(), &id, cancelled, error);
            return;
        }
    };
    let usage = receipt.snapshot;
    observer.publish(&id, ApplicationEvent::Usage(usage.clone()));
    #[cfg(test)]
    InteractiveExecution::pause_at(&terminal_gate).await;
    let WindowSettlement {
        publication,
        late_diagnostic,
        model_ref,
    } = settlement;
    // The terminal choice is fixed here; no await or watch borrow spans publication.
    let cancelled = operation.hard_cancellation().is_cancelled();
    if cancelled {
        let mut diagnostic = late_diagnostic;
        match &publication {
            TurnPublication::Completed(completion)
            | TurnPublication::SettlementFailed { completion, .. } => {
                if let Some(error) = &completion.error {
                    append_notice(&mut diagnostic, error);
                }
            }
            TurnPublication::Interrupted {
                diagnostic: error,
                history_error,
                ..
            } => {
                append_notice(&mut diagnostic, error);
                if let Some(error) = history_error {
                    append_notice(&mut diagnostic, error);
                }
            }
        }
        if let TurnPublication::SettlementFailed {
            diagnostic: error, ..
        } = &publication
        {
            append_notice(&mut diagnostic, error);
        }
        publish_interruption(observer.as_ref(), &id, true, diagnostic);
        return;
    }
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
        TurnPublication::SettlementFailed {
            completion,
            mut diagnostic,
        } => {
            if let Some(error) = &completion.error {
                append_notice(&mut diagnostic, error);
            }
            append_notice(&mut diagnostic, &late_diagnostic);
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

#[allow(clippy::too_many_arguments)]
async fn run_window(
    services: LifecycleServices,
    operation: Arc<OperationGuard>,
    prepared: PreparedModelAsk,
    max_turn_rounds: usize,
    config: Arc<AppConfig>,
    usage: UsageBinding,
    executor: ReadyPromptExecutor,
    policy: Box<dyn ApplyPolicy>,
    observer: Arc<dyn ApplicationObserver>,
    id: String,
) -> WindowSettlement {
    let model_ref = executor.identity().model().clone();
    let (sink, events) = EventSink::channel();
    let cancel = operation.hard_cancellation();
    let pacing_cancel = operation.hard_cancellation();
    let edit_observer = observer.clone();
    let edit_id = id.clone();
    let cycle = EditCycle::new(move |event| {
        edit_observer.publish(&edit_id, ApplicationEvent::Edit(event));
    });
    // Publish the stream while preparation, including Scribe, is running.
    let preparation = start_task(
        &services,
        operation.clone(),
        prepared,
        max_turn_rounds,
        config,
        usage,
        executor,
        policy,
        sink.clone(),
        observer.clone(),
    );
    let relay = relay_stream(observer.as_ref(), &id, &cycle, events, &pacing_cancel);
    drop(sink);
    let (task, stream) = tokio::join!(preparation, relay);
    let task = match task {
        Ok(task) => task,
        Err(error) => {
            let diagnostic = error.to_string();
            close_failed(
                &cycle,
                if cancel.is_cancelled() {
                    EditTermination::Cancelled
                } else {
                    EditTermination::Aborted
                },
                diagnostic.clone(),
            );
            services.context.close_research_publication(operation.id());
            return WindowSettlement {
                publication: TurnPublication::Interrupted {
                    cause: if cancel.is_cancelled() {
                        InterruptionCause::Cancelled
                    } else {
                        InterruptionCause::StreamFailed
                    },
                    diagnostic,
                    history_error: None,
                },
                late_diagnostic: String::new(),
                model_ref,
            };
        }
    };
    let report = task.join().await;
    #[cfg(test)]
    InteractiveExecution::pause_at(&services.interactive.report_gate).await;
    let Classified {
        open,
        decision,
        documents,
        disk_notice,
        finalized_edit,
    } = classify(
        report,
        stream.cancelled || cancel.is_cancelled(),
        stream.error.as_deref(),
    );
    let mut late_diagnostic = String::new();
    append_receipt_notices(&mut late_diagnostic, disk_notice.as_deref(), &documents);
    let mut returned_context = None;
    let mut publication = match decision {
        Decision::Completed {
            completion,
            context,
            history,
        } => {
            returned_context = Some(*context);
            let history_result = open
                .complete(&operation, history)
                .await
                .map_err(|error| error.to_string());
            match history_result {
                Ok(()) => TurnPublication::Completed(completion),
                Err(diagnostic) => TurnPublication::SettlementFailed {
                    completion,
                    diagnostic,
                },
            }
        }
        Decision::Interrupted {
            cause,
            diagnostic,
            history,
        } => {
            let history_error = open
                .interrupt(&operation, history)
                .await
                .err()
                .map(|error| error.to_string());
            TurnPublication::Interrupted {
                cause,
                diagnostic,
                history_error,
            }
        }
    };
    let edit = match &publication {
        TurnPublication::Completed(completion)
        | TurnPublication::SettlementFailed { completion, .. } => match &completion.result {
            ModelResult::Edit { outcome, .. } => Some(outcome.clone()),
            ModelResult::Answer { .. } => None,
        },
        TurnPublication::Interrupted { .. } => finalized_edit,
    };
    let (_, context_errors) = services
        .context
        .settle_turn_context(
            returned_context,
            edit.as_ref().is_some_and(|edit| edit.applied),
        )
        .await;
    for error in context_errors {
        publication = match publication {
            TurnPublication::Completed(completion) => TurnPublication::SettlementFailed {
                completion,
                diagnostic: error,
            },
            TurnPublication::SettlementFailed {
                completion,
                mut diagnostic,
            } => {
                append_notice(&mut diagnostic, &error);
                TurnPublication::SettlementFailed {
                    completion,
                    diagnostic,
                }
            }
            TurnPublication::Interrupted {
                cause,
                mut diagnostic,
                history_error,
            } => {
                append_notice(&mut diagnostic, &error);
                TurnPublication::Interrupted {
                    cause,
                    diagnostic,
                    history_error,
                }
            }
        };
    }
    if let Some(edit) = edit.as_ref() {
        publish_edit(&services.context, observer.as_ref(), &id, &cycle, edit).await;
    } else {
        match &publication {
            TurnPublication::Interrupted {
                cause: InterruptionCause::Cancelled,
                diagnostic,
                ..
            } => close_failed(&cycle, EditTermination::Cancelled, diagnostic.clone()),
            TurnPublication::Interrupted { diagnostic, .. }
            | TurnPublication::SettlementFailed { diagnostic, .. } => {
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
    services.context.close_research_publication(operation.id());
    publish_documents(&services.context, observer.as_ref(), &id, &documents).await;
    WindowSettlement {
        publication,
        late_diagnostic,
        model_ref,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn start_task(
    services: &LifecycleServices,
    operation: Arc<OperationGuard>,
    prepared: PreparedModelAsk,
    max_turn_rounds: usize,
    config: Arc<AppConfig>,
    usage: UsageBinding,
    executor: ReadyPromptExecutor,
    policy: Box<dyn ApplyPolicy>,
    sink: EventSink,
    observer: Arc<dyn ApplicationObserver>,
) -> Result<InteractiveTask, InteractiveTaskError> {
    if !services.execution.owns(&operation) {
        return Err(InteractiveTaskError::OperationNotOwned);
    }
    let prepared_task = match &prepared.discussion {
        Some(discussion) => format_discussion_task(&discussion.document, &prepared.utterance),
        None => prepared.utterance.clone(),
    };
    let (serial, history) = services
        .interactive
        .prepare(
            &services.context,
            &services.execution,
            &operation,
            &services.history,
            config,
            usage,
            prepared.utterance,
            &sink,
        )
        .await?;
    let mut turn_context = history.context;
    let listener = services
        .context
        .activate_research_publication(
            &operation,
            operation.publication_id(),
            observer.clone(),
            &turn_context,
        )
        .ok_or_else(|| {
            InteractiveTaskError::TaskFailed(
                "Could not bind interactive turn context publication".into(),
            )
        })?;
    turn_context.set_listener(Some(listener));
    let root = turn_context.repo_root().to_path_buf();
    let open = history.open;
    let focus = prepared.discussion.map(|discussion| discussion.target);
    let mut state = TurnState::new(
        turn_context,
        tauqe_core::llm::repl::repl_capabilities(focus.as_ref()),
        Some(history.history),
        Some(max_turn_rounds),
    );
    let read_rounds = Box::new(executor.history_reader());
    let cancel = operation.hard_cancellation();
    #[cfg(test)]
    let write_gate = services.interactive.document_write_gate.lock().await.take();
    #[cfg(test)]
    let continuation_gate = services.interactive.continuation_gate.clone();
    let context = std::mem::take(&mut state.context);
    let observed_context = services.context.clone();
    let observed_id = operation.publication_id();
    let task = services.interactive.start_prepared(
        operation.clone(),
        serial,
        context,
        open,
        read_rounds,
        move |context, operation| async move {
            state.context = context;
            let technical = run_technical_turn(
                &operation,
                root.clone(),
                state,
                executor,
                policy,
                prepared_task.clone(),
                None,
                sink.clone(),
                Some(TechnicalObservation {
                    session: observed_context.clone(),
                    operation_id: operation.id(),
                }),
            )
            .await;
            let report = match technical {
                Ok(report) => report,
                Err(error) => {
                    operation.clear_abort_handle();
                    return InteractiveTaskResult {
                        task: Err(InteractiveTaskError::TaskFailed(error.clone())),
                        settlement: tauqe_core::workflow::policy::owned::Settlement::RolledBack(
                            tauqe_core::edits::transaction::RollbackOutcome::Incomplete {
                                policy_error: None,
                                backup_error: error.clone(),
                            },
                        ),
                        diagnostic: Some(error),
                    };
                }
            };
            let settlement = report.settlement;
            let diagnostic = report.diagnostic;
            // File settlement is complete, but document continuation has not started.
            #[cfg(test)]
            InteractiveExecution::pause_at(&continuation_gate).await;
            let cancelled = report.cancelled || cancel.is_cancelled();
            let task = if let Some(recovery) = diagnostic.as_deref() {
                let message = super::technical_diagnostic(&report.outcome, recovery);
                if cancelled {
                    sink.emit_text_delta(format!("\n\n{message}\n\n"));
                    Ok(InteractiveTaskOutcome {
                        result: Err(ExecutionError::Cancelled),
                        context: report.context,
                    })
                } else {
                    Ok(InteractiveTaskOutcome {
                        result: Err(ExecutionError::Other(message)),
                        context: report.context,
                    })
                }
            } else if cancelled {
                Ok(InteractiveTaskOutcome {
                    result: Err(ExecutionError::Cancelled),
                    context: report.context,
                })
            } else if let Some(mut continuation) = report.continuation {
                let Some(listener) = observed_context.activate_research_publication(
                    &operation,
                    observed_id,
                    observer,
                    &continuation.state.context,
                ) else {
                    return InteractiveTaskResult {
                        task: Err(InteractiveTaskError::TaskFailed(
                            "Could not bind interactive document context publication".into(),
                        )),
                        settlement,
                        diagnostic,
                    };
                };
                continuation.state.context.set_listener(Some(listener));
                let continuation_cancel = cancel.clone();
                let continuation_sink = sink.clone();
                let continuation_owner = operation.clone();
                let next = tokio::spawn(async move {
                    let mut state = continuation.state;
                    let mut executor = continuation.executor;
                    let result = super::continue_after_turn(
                        report.outcome,
                        &mut executor,
                        &continuation_owner,
                        &mut state,
                        &root,
                        &prepared_task,
                        focus.as_ref(),
                        &continuation_cancel,
                        &continuation_sink,
                        #[cfg(test)]
                        write_gate,
                    )
                    .await;
                    InteractiveTaskOutcome {
                        result,
                        context: state.context,
                    }
                });
                operation.set_abort_handle(next.abort_handle());
                let result = next.await.map_err(InteractiveTaskError::Interrupted);
                operation.clear_abort_handle();
                result
            } else {
                Ok(InteractiveTaskOutcome {
                    result: Err(ExecutionError::Other(
                        "The technical turn stopped without a continuation".into(),
                    )),
                    context: report.context,
                })
            };
            InteractiveTaskResult {
                task,
                settlement,
                diagnostic,
            }
        },
    );
    Ok(task)
}
