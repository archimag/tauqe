//! Priority and ordered settlement of interactive execution after stream consumption.
use crate::api::{
    Application, InteractiveInterruption, InteractivePlanUpdates, InteractiveReviewUpdates,
    InteractiveTaskError, InteractiveTaskOutcome, InteractiveTurnCompletion, OperationGuard,
    UsageGuard, UsageTotalsSnapshot,
};
use crate::context::ContextState;
use crate::discussion::DiscussionTarget;
use crate::providers::StreamEvent;

#[derive(Default)]
pub struct InteractiveStreamState {
    text: String,
    cancelled: bool,
    error: Option<String>,
}
impl InteractiveStreamState {
    /// Observe only after transport pacing, matching the consumed attempt.
    pub fn observe(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::TextDelta(delta) => self.text.push_str(delta),
            StreamEvent::ResetStream => self.text.clear(),
            StreamEvent::Cancelled => self.cancelled = true,
            StreamEvent::Error(error) => self.error = Some(error.clone()),
            _ => {}
        }
    }
}
pub struct InteractiveFinalizationInput {
    pub joined: Result<InteractiveTaskOutcome, InteractiveTaskError>,
    pub stream: InteractiveStreamState,
    pub turn_id: Option<u64>,
    pub focus: Option<DiscussionTarget>,
}
pub enum InteractiveFinalizationUpdate<'a> {
    Cancelled,
    UsageFlushed(&'a UsageTotalsSnapshot),
    EditCompletion(&'a InteractiveTurnCompletion),
    Context(ContextState),
    Plans(InteractivePlanUpdates),
    Review(InteractiveReviewUpdates),
}
#[async_trait::async_trait]
pub trait InteractiveFinalizationObserver: Send {
    /// Adapt facts into transport events; never mutate history, context, or accounting.
    async fn publish(&mut self, update: InteractiveFinalizationUpdate<'_>);
}
pub enum InteractiveFinalizationOutcome {
    Completed(Box<InteractiveTurnCompletion>),
    Interrupted {
        cancelled: bool,
        error: Option<String>,
    },
}
pub struct FinalizedInteractiveTurn {
    pub outcome: InteractiveFinalizationOutcome,
    pub usage: UsageTotalsSnapshot,
}

impl Application {
    async fn flush_interactive_usage(
        &self,
        usage: &mut Option<UsageGuard>,
        observer: &mut dyn InteractiveFinalizationObserver,
    ) -> UsageTotalsSnapshot {
        if let Some(guard) = usage.take() {
            guard.flush().await;
        }
        let snapshot = self.usage_snapshot().await;
        observer
            .publish(InteractiveFinalizationUpdate::UsageFlushed(&snapshot))
            .await;
        snapshot
    }
    pub async fn finalize_interactive_turn(
        &self,
        operation: &OperationGuard,
        input: InteractiveFinalizationInput,
        usage: &mut Option<UsageGuard>,
        observer: &mut dyn InteractiveFinalizationObserver,
    ) -> Result<FinalizedInteractiveTurn, InteractiveTaskError> {
        if !self.owns_operation(operation) {
            return Err(InteractiveTaskError::OperationNotOwned);
        }
        let InteractiveFinalizationInput {
            joined,
            stream,
            turn_id,
            focus,
        } = input;
        let cancelled = stream.cancelled || *operation.cancel_receiver().borrow();
        let outcome = match joined {
            Err(error) => {
                let error = if cancelled {
                    "Operation cancelled".into()
                } else {
                    error.to_string()
                };
                let interruption = if cancelled {
                    InteractiveInterruption::Cancelled { turn_id }
                } else {
                    InteractiveInterruption::TaskFailure {
                        reason: error.clone(),
                    }
                };
                let _ = self
                    .persist_interactive_interruption(operation, interruption)
                    .await;
                let usage = self.flush_interactive_usage(usage, observer).await;
                if cancelled && !stream.cancelled {
                    observer
                        .publish(InteractiveFinalizationUpdate::Cancelled)
                        .await;
                }
                return Ok(FinalizedInteractiveTurn {
                    outcome: InteractiveFinalizationOutcome::Interrupted {
                        cancelled,
                        error: Some(error),
                    },
                    usage,
                });
            }
            Ok(outcome) => outcome,
        };
        if cancelled {
            if !stream.cancelled {
                observer
                    .publish(InteractiveFinalizationUpdate::Cancelled)
                    .await;
            }
            let _ = self
                .persist_interactive_interruption(
                    operation,
                    InteractiveInterruption::Cancelled { turn_id },
                )
                .await;
            let usage = self.flush_interactive_usage(usage, observer).await;
            return Ok(FinalizedInteractiveTurn {
                outcome: InteractiveFinalizationOutcome::Interrupted {
                    cancelled: true,
                    error: Some("Operation cancelled".into()),
                },
                usage,
            });
        }
        if let Some(error) = stream.error {
            let _ = self
                .persist_interactive_interruption(
                    operation,
                    InteractiveInterruption::StreamFailure {
                        streamed_text: stream.text,
                        error: Some(error.clone()),
                    },
                )
                .await;
            let usage = self.flush_interactive_usage(usage, observer).await;
            return Ok(FinalizedInteractiveTurn {
                outcome: InteractiveFinalizationOutcome::Interrupted {
                    cancelled: false,
                    error: Some(error),
                },
                usage,
            });
        }
        let usage = self.flush_interactive_usage(usage, observer).await;
        let completion = crate::api::classify_interactive_completion(outcome.result, stream.text);
        if let Err(error) = self.merge_turn_context(outcome.context).await {
            tracing::warn!(error=%error,"Failed to merge the turn context");
        }
        observer
            .publish(InteractiveFinalizationUpdate::EditCompletion(&completion))
            .await;
        let _ = self
            .persist_interactive_completion(operation, &completion)
            .await;
        observer
            .publish(InteractiveFinalizationUpdate::Context(
                self.context_state().await,
            ))
            .await;
        let root = self.repository_root().await;
        if let Some(updates) = self.interactive_plan_updates(&root, &completion).await {
            observer
                .publish(InteractiveFinalizationUpdate::Plans(updates))
                .await;
        }
        let root = self.repository_root().await;
        if let Some(updates) = self
            .interactive_review_updates(&root, &completion, focus.as_ref())
            .await
        {
            observer
                .publish(InteractiveFinalizationUpdate::Review(updates))
                .await;
        }
        Ok(FinalizedInteractiveTurn {
            outcome: InteractiveFinalizationOutcome::Completed(Box::new(completion)),
            usage,
        })
    }
}

#[cfg(test)]
mod tests;
