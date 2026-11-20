//! Interactive ask admission, execution, and completion.
use crate::api::{
    next_operation_id, Application, ApplicationEvent, ApplicationObserver, DiscussionPromptError,
    InteractiveExecutionRequest, InteractiveFinalizationInput, InteractiveFinalizationObserver,
    InteractiveFinalizationOutcome, InteractiveFinalizationUpdate, InteractiveStreamState,
    ModelAskInput, OperationGuard,
};
use crate::application::edit_cycle::{EditCycle, EditTermination};
use crate::application::infrastructure::execution::{DialogueExecution, ExecutionFactory};
use crate::application::turn_setup::TurnProviderError;
use std::sync::Arc;
use tauqe_core::edits::EditOutcome;
use tauqe_core::llm::ModelResult;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::turn::TurnWorkContext;

#[derive(Debug)]
pub struct AskStarted {
    pub operation_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error("Another operation is already running")]
    Busy,
    #[error(transparent)]
    Prompt(#[from] DiscussionPromptError),
    #[error(transparent)]
    Provider(#[from] TurnProviderError),
}

impl Application {
    pub(crate) async fn publish_review_snapshot(
        &self,
        root: &std::path::Path,
        id: &str,
        observer: &dyn ApplicationObserver,
    ) {
        let documents = self.documents_at(root);
        if let Ok(Some(session)) = documents.review(id).await {
            observer.publish("", ApplicationEvent::ReviewDocument(session));
        }
        if let Ok(reviews) = documents.reviews().await {
            let active_id = documents.active_review_id().await.ok().flatten();
            observer.publish("", ApplicationEvent::ReviewList { reviews, active_id });
        }
    }

    pub async fn ask(
        &self,
        input: ModelAskInput,
        observer: Arc<dyn ApplicationObserver>,
    ) -> Result<AskStarted, AskError> {
        let operation = Arc::new(self.try_begin_operation().map_err(|_| AskError::Busy)?);
        let root = self.repository_root().await;
        let prepared = self.prepare_model_ask(&root, input).await?;
        let operation_id = format!("op-{}", next_operation_id());
        if let Some(update) = prepared.focus_update {
            update.publish(&operation_id, observer.as_ref());
        }
        let factory = ExecutionFactory::bind(self, &operation).await?;
        let dialogue = factory.for_dialogue().await?;
        let resolved = &dialogue.model;
        let model = resolved.model.clone();
        let role = resolved.role;
        let alias = resolved.alias.clone();
        let effort = resolved.effort;
        let compaction = factory.compaction_environment();
        let application = self.clone();
        let id = operation_id.clone();
        tokio::spawn(async move {
            if let Err(error) = application.compact_history(&operation, &compaction).await {
                tracing::warn!(%error, "session history is unavailable for compaction");
            }
            if *operation.cancel_receiver().borrow() {
                observer.publish(&id, ApplicationEvent::Stream(StreamEvent::Cancelled));
                return;
            }
            let request = InteractiveExecutionRequest {
                capabilities: tauqe_core::control::repl_capabilities(prepared.focus.as_ref()),
                prompt: prepared.prompt,
                focus: prepared.focus,
            };
            tracing::info!(
                operation_id = %id,
                model = %model,
                role = ?role,
                effort = ?effort,
                "Model turn started"
            );
            observer.publish(
                &id,
                ApplicationEvent::Started {
                    model,
                    role,
                    alias,
                    effort,
                    plan_context: None,
                    work_context: Some(TurnWorkContext::Interactive),
                    is_batch: false,
                },
            );
            application
                .run_ask(operation, request, dialogue, observer, id)
                .await;
        });
        Ok(AskStarted { operation_id })
    }

    async fn run_ask(
        &self,
        operation: Arc<OperationGuard>,
        request: InteractiveExecutionRequest,
        dialogue: DialogueExecution,
        observer: Arc<dyn ApplicationObserver>,
        id: String,
    ) {
        let focus = request.focus.clone();
        let model = dialogue.model.model.clone();
        let execution = match self
            .start_interactive_execution(operation.clone(), request, dialogue)
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
        let mut events = execution.events;
        let mut cancel = operation.cancel_receiver();
        let mut stream = InteractiveStreamState::default();
        let mut published_error = false;
        let usage_observer = observer.clone();
        let mut usage = Some(
            self.begin_usage(&id, move |id, snapshot| {
                usage_observer.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()));
            })
            .await,
        );
        let edit_observer = observer.clone();
        let edit_id = id.clone();
        let cycle = EditCycle::new(move |event| {
            edit_observer.publish(&edit_id, ApplicationEvent::Edit(event));
        });
        while let Some(event) = events.recv().await {
            observer.ready(&event, &mut cancel).await;
            stream.observe(&event);
            observe_edit_event(&cycle, &event);
            if let StreamEvent::Usage(report) = &event {
                if let Some(guard) = &usage {
                    guard.observe(report).await;
                }
                observer.publish(&id, ApplicationEvent::Usage(self.usage_snapshot().await));
            }
            published_error = matches!(event, StreamEvent::Error(_));
            let terminal = published_error || matches!(event, StreamEvent::Cancelled);
            observer.publish(&id, ApplicationEvent::Stream(event));
            if terminal {
                break;
            }
        }
        let joined = execution.task.join(&id).await;
        let mut publication = FinalizationPublication {
            application: self,
            observer: observer.as_ref(),
            id: &id,
            cycle: &cycle,
        };
        let finalized = self
            .finalize_interactive_turn(
                &operation,
                InteractiveFinalizationInput {
                    joined,
                    stream,
                    turn_id: execution.turn_id,
                    focus,
                },
                &mut usage,
                &mut publication,
            )
            .await;
        match finalized {
            Ok(finalized) => match finalized.outcome {
                InteractiveFinalizationOutcome::Completed(completion) => {
                    let full_text = match &completion.result {
                        ModelResult::Answer { text } => text.clone(),
                        ModelResult::Edit { summary, .. } => summary.clone(),
                    };
                    tracing::info!(operation_id = %id, %model, usage = ?finalized.usage.prev,
                        error = ?completion.error, "Model turn finished");
                    observer.publish(
                        &id,
                        ApplicationEvent::Result {
                            completion,
                            usage: finalized.usage,
                        },
                    );
                    observer.publish(&id, ApplicationEvent::Finished(full_text));
                }
                InteractiveFinalizationOutcome::Interrupted { cancelled, error } => {
                    if !cancelled && !published_error {
                        if let Some(message) = &error {
                            observer.publish(
                                &id,
                                ApplicationEvent::Stream(StreamEvent::Error(message.clone())),
                            );
                        }
                    }
                    tracing::warn!(operation_id = %id, %model, cancelled, ?error,
                        usage = ?finalized.usage.prev, "Model turn interrupted");
                }
            },
            Err(error) => {
                tracing::error!(operation_id = %id, %error, "Interactive finalization failed")
            }
        }
    }
}

pub(crate) fn observe_edit_event(cycle: &EditCycle, event: &StreamEvent) {
    match event {
        StreamEvent::EditStarted | StreamEvent::EditFileStarted { .. } => {
            cycle.open();
        }
        StreamEvent::ResetStream => {
            cycle.close_unapplied(EditTermination::Withdrawn, Vec::new(), None);
        }
        StreamEvent::Cancelled => {
            cycle.close_unapplied(EditTermination::Cancelled, Vec::new(), None);
        }
        StreamEvent::Error(error) => close_failed(
            cycle,
            EditTermination::ProviderError,
            format!("Operation failed before edits were applied: {error}"),
        ),
        _ => {}
    }
}

fn close_failed(cycle: &EditCycle, termination: EditTermination, reason: String) {
    cycle.close_outcome(
        termination,
        EditOutcome {
            error: Some(reason),
            ..EditOutcome::default()
        },
    );
}

struct FinalizationPublication<'a> {
    application: &'a Application,
    observer: &'a dyn ApplicationObserver,
    id: &'a str,
    cycle: &'a EditCycle,
}

#[async_trait::async_trait]
impl InteractiveFinalizationObserver for FinalizationPublication<'_> {
    async fn publish(&mut self, update: InteractiveFinalizationUpdate<'_>) {
        let event = match update {
            InteractiveFinalizationUpdate::Cancelled => {
                self.cycle
                    .close_unapplied(EditTermination::Cancelled, Vec::new(), None);
                ApplicationEvent::Stream(StreamEvent::Cancelled)
            }
            InteractiveFinalizationUpdate::UsageFlushed(snapshot) => {
                ApplicationEvent::Usage(snapshot.clone())
            }
            InteractiveFinalizationUpdate::EditCompletion(completion) => {
                if let Some(error) = &completion.error {
                    close_failed(self.cycle, EditTermination::StagingRefused, error.clone());
                } else {
                    match &completion.result {
                        ModelResult::Answer { .. } => {
                            self.cycle.close_answered();
                        }
                        ModelResult::Edit {
                            outcome, summary, ..
                        } => {
                            if let Some(hash) = &outcome.commit_hash {
                                self.observer.publish(
                                    self.id,
                                    ApplicationEvent::Commit {
                                        hash: hash.clone(),
                                        summary: summary.clone(),
                                        changed_files: outcome.changed_files.clone(),
                                    },
                                );
                                if let Some(repository) =
                                    self.application.repository_after_turn_commit().await
                                {
                                    self.observer
                                        .publish(self.id, ApplicationEvent::Repository(repository));
                                }
                            }
                            self.cycle.open();
                            self.cycle
                                .close_outcome(EditTermination::Answered, outcome.clone());
                        }
                    }
                }
                return;
            }
            InteractiveFinalizationUpdate::Context(context) => ApplicationEvent::Context(context),
            InteractiveFinalizationUpdate::Plans(updates) => {
                if let Some(plan) = updates.active_plan {
                    self.observer.publish(
                        self.id,
                        ApplicationEvent::PlanDocument {
                            plan,
                            is_save: true,
                        },
                    );
                }
                ApplicationEvent::PlanList {
                    plans: updates.plans,
                    active_id: updates.active_id,
                }
            }
            InteractiveFinalizationUpdate::Review(updates) => {
                for _ in 0..updates.decisions_applied {
                    self.observer.publish(
                        self.id,
                        ApplicationEvent::ReviewDocument(updates.session.clone()),
                    );
                }
                return;
            }
        };
        self.observer.publish(self.id, event);
    }
}
