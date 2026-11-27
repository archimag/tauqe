//! Publication and telemetry around an already prepared step run.

use super::super::completion::{StepEditCompletion, StepEditDisposition};
use super::StepResources;
use crate::api::{next_operation_id, ApplicationEvent, ApplicationObserver};
use crate::application::edit_cycle::{EditCycle, EditTermination};
use crate::application::execution_scope::ExecutionScope;
use crate::application::usage_totals::UsageReceipt;
use std::future::Future;
use std::sync::Arc;
use tauqe_core::execution::executor::PromptIdentity;
use tauqe_core::execution::EventSink;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::turn::{ModelTurnPlanContext, TurnWorkContext};

pub(crate) struct StepRunPublication {
    pub identity: PromptIdentity,
    pub plan_context: Option<ModelTurnPlanContext>,
    pub work_context: Option<TurnWorkContext>,
    pub session_label: Option<String>,
}

impl StepResources {
    /// Deliver one publication window; the caller supplies its terminal edit facts.
    pub(crate) async fn publish_step_run<T, F, Fut>(
        &self,
        publication: StepRunPublication,
        observer: Arc<dyn ApplicationObserver>,
        scope: ExecutionScope,
        run: F,
        completion: impl FnOnce(&T) -> StepEditCompletion,
    ) -> Result<(T, UsageReceipt), String>
    where
        F: FnOnce(String, EventSink) -> Fut,
        Fut: Future<Output = T>,
    {
        let StepRunPublication {
            identity,
            plan_context,
            work_context,
            session_label,
        } = publication;
        let model = identity.model().clone();
        let role = identity.role();
        let effort = identity.effort();
        let alias = identity.alias().map(str::to_owned);
        let op_id = format!("op-{}", next_operation_id());
        tracing::info!(
            operation_id = %op_id,
            model = %model,
            role = ?role,
            effort = ?effort,
            work = work_kind(work_context.as_ref()),
            session = session_label.as_deref().unwrap_or("none"),
            "step run started"
        );
        observer.publish(
            &op_id,
            ApplicationEvent::Started {
                model,
                role,
                alias,
                effort,
                plan_context,
                work_context,
                is_batch: true,
            },
        );

        let (sink, mut event_rx) = EventSink::channel();
        let edit_observer = observer.clone();
        let edit_id = op_id.clone();
        let edit_cycle = Arc::new(EditCycle::new(move |event| {
            edit_observer.publish(&edit_id, ApplicationEvent::Edit(event));
        }));
        let relay = {
            let relay_op = op_id.clone();
            let relay_observer = observer.clone();
            let relay_edit_cycle = Arc::clone(&edit_cycle);
            tokio::spawn(async move {
                let mut assistant_text = String::new();
                while let Some(event) = event_rx.recv().await {
                    match &event {
                        StreamEvent::TextDelta(delta) => assistant_text.push_str(delta),
                        StreamEvent::ResetStream => assistant_text.clear(),
                        _ => {}
                    }
                    relay_edit_cycle.observe(&event);
                    relay_observer.publish(&relay_op, ApplicationEvent::Stream(event));
                }
                assistant_text
            })
        };

        // Opening the scope activates the already bound executor. Its worker
        // waits for the whole publication run, including disk recovery.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let publisher = observer.clone();
        let supervised = scope.run(
            &self.context,
            op_id.clone(),
            move |id, snapshot| {
                publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()))
            },
            async move {
                let _ = done_rx.await;
            },
            move |_| {
                let _ = started_tx.send(());
            },
            |outcome| async move { outcome },
        );
        tokio::pin!(supervised);
        tokio::select! {
            result = &mut supervised => {
                return Err(format!("Step accounting stopped before execution: {result:?}"));
            }
            result = started_rx => {
                result.map_err(|error| format!("Step accounting did not start: {error}"))?;
            }
        }
        let report = run(op_id.clone(), sink.clone()).await;
        drop(sink);
        let telemetry = relay.await;
        let _ = done_tx.send(());
        let receipt = match supervised.await {
            Ok((_, receipt)) => receipt,
            Err(error) => {
                let failure = format!("Step accounting window did not close: {error}");
                edit_cycle.close_outcome(
                    EditTermination::ProtocolFailure,
                    tauqe_core::edits::EditOutcome {
                        error: Some(failure.clone()),
                        ..Default::default()
                    },
                );
                return Err(failure);
            }
        };
        let completion = completion(&report);
        let termination = match completion.disposition {
            StepEditDisposition::Completed => EditTermination::Answered,
            StepEditDisposition::Refused => EditTermination::StagingRefused,
            StepEditDisposition::Cancelled => EditTermination::Cancelled,
            StepEditDisposition::Failed => EditTermination::ProtocolFailure,
        };
        edit_cycle.close_outcome(termination, completion.outcome);
        match telemetry {
            Ok(assistant_text) => {
                observer.publish(&op_id, ApplicationEvent::Finished(assistant_text));
            }
            Err(error) => {
                tracing::warn!(operation_id = %op_id, %error, "Step telemetry relay terminated unexpectedly")
            }
        }
        observer.publish(&op_id, ApplicationEvent::Usage(receipt.snapshot.clone()));
        Ok((report, receipt))
    }
}

fn work_kind(work_context: Option<&TurnWorkContext>) -> &'static str {
    match work_context {
        None => "none",
        Some(TurnWorkContext::Interactive) => "interactive",
        Some(TurnWorkContext::PlanStep { .. }) => "plan_step",
        Some(TurnWorkContext::ReviewGroupStep { .. }) => "review_group_step",
        Some(TurnWorkContext::ReviewStep { .. }) => "review_step",
        Some(TurnWorkContext::ReviewPhase { .. }) => "review_phase",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use crate::application::edit_cycle::EditCycleEvent;
    use crate::workflows::step::{completion::classify_step_edit_completion, run::StepLoopReport};
    use std::sync::Mutex;
    use tauqe_core::config::AppConfig;
    use tauqe_core::models::ModelRef;
    use tauqe_core::providers::ModelUsageInfo;

    struct Observer(Mutex<Vec<ApplicationEvent>>);

    impl ApplicationObserver for Observer {
        fn publish(&self, _: &str, event: ApplicationEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn publication() -> StepRunPublication {
        StepRunPublication {
            identity: PromptIdentity::new(ModelRef::openrouter("mock/model"), None, None, None),
            plan_context: None,
            work_context: None,
            session_label: None,
        }
    }

    #[tokio::test]
    async fn interrupted_review_publishes_its_reason_instead_of_answered_fallback() {
        use crate::workflows::review::{ReviewSequenceResult, ReviewSequenceStop, ReviewStepStop};
        use tauqe_core::edits::{EditOutcome, VerificationVerdict};
        use tauqe_core::workflow::implementation::{StepTurnResult, StepTurnSettlement};

        for exhausted in [false, true] {
            for prior_commit in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let (history, _) = tokio::sync::mpsc::unbounded_channel();
                let app =
                    Application::for_repository(AppConfig::default(), root.path().into(), history);
                let observer = Arc::new(Observer(Mutex::new(Vec::new())));
                let reason = if exhausted {
                    "Coder attempt budget exhausted after verification failed"
                } else {
                    "Mandatory verification failed in the S3 pacing test"
                };
                app.step_resources()
                    .publish_step_run(
                        publication(),
                        observer.clone(),
                        app.execution_session().prepare_scope(),
                        |_, sink| async move {
                            sink.emit(StreamEvent::EditStarted);
                            sink.emit(StreamEvent::TextDelta("Proposed metadata fixtures".into()));
                            ReviewSequenceResult {
                                executed_steps: if prior_commit {
                                    vec![StepTurnSettlement {
                                        result: StepTurnResult::Success {
                                            summary: "Earlier step".into(),
                                            commit: Some("confirmed".into()),
                                        },
                                        edit: Some(EditOutcome {
                                            applied: true,
                                            changed_files: vec!["committed.rs".into()],
                                            commit_hash: Some("confirmed".into()),
                                            verification: Some(VerificationVerdict::Passed),
                                            ..Default::default()
                                        }),
                                    }]
                                } else {
                                    Vec::new()
                                },
                                stop: ReviewSequenceStop::Interrupted(if exhausted {
                                    ReviewStepStop::Exhausted(reason.into())
                                } else {
                                    ReviewStepStop::Blocked(reason.into())
                                }),
                                usage: None,
                            }
                        },
                        ReviewSequenceResult::edit_completion,
                    )
                    .await
                    .unwrap();
                let events = observer.0.lock().unwrap();
                let finishes: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        ApplicationEvent::Edit(EditCycleEvent::Finished(outcome)) => Some(outcome),
                        _ => None,
                    })
                    .collect();
                assert_eq!(finishes.len(), 1);
                let outcome = finishes[0];
                assert!(!outcome.applied);
                assert_eq!(outcome.error.as_deref(), Some(reason));
                assert_eq!(
                    outcome.commit_hash.as_deref(),
                    prior_commit.then_some("confirmed")
                );
                assert_eq!(
                    outcome.verification,
                    prior_commit.then_some(VerificationVerdict::Passed)
                );
                if prior_commit {
                    assert_eq!(outcome.changed_files, ["committed.rs"]);
                } else {
                    assert!(outcome.changed_files.is_empty());
                }
            }
        }
    }

    #[tokio::test]
    async fn closes_open_edit_once_and_keeps_usage_after_failure_and_stream_reset() {
        let root = tempfile::tempdir().unwrap();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), history);
        let observer = Arc::new(Observer(Mutex::new(Vec::new())));
        let (report, receipt) = app
            .step_resources()
            .publish_step_run(
                publication(),
                observer.clone(),
                app.execution_session().prepare_scope(),
                |_, sink| async move {
                    sink.emit(StreamEvent::TextDelta("discarded".into()));
                    sink.emit(StreamEvent::EditStarted);
                    sink.emit(StreamEvent::ResetStream);
                    sink.emit(StreamEvent::EditStarted);
                    sink.emit(StreamEvent::Usage(ModelUsageInfo {
                        total_tokens: 7,
                        ..Default::default()
                    }));
                    sink.emit(StreamEvent::TextDelta("retained".into()));
                    StepLoopReport::Failed("failed run".into())
                },
                classify_step_edit_completion,
            )
            .await
            .unwrap();
        assert!(matches!(report, StepLoopReport::Failed(_)));
        assert_eq!(receipt.snapshot.operations, 1);
        assert_eq!(receipt.snapshot.session.total_tokens, 0);
        let events = observer.0.lock().unwrap();
        let finishes: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ApplicationEvent::Edit(EditCycleEvent::Finished(outcome)) => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(finishes.len(), 2, "one close per open edit attempt");
        assert_eq!(finishes[1].error.as_deref(), Some("failed run"));
        assert!(events
            .iter()
            .any(|event| matches!(event, ApplicationEvent::Finished(text) if text == "retained")));
        assert!(events.iter().any(|event| matches!(event, ApplicationEvent::UsageTotals(snapshot) if snapshot.cur.is_none() && snapshot.session.total_tokens == 0)));
        assert!(
            matches!(events.last(), Some(ApplicationEvent::Usage(snapshot)) if snapshot.session.total_tokens == 0)
        );
    }
}
