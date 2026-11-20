//! Publication and telemetry around an already prepared step run.

use crate::api::{next_operation_id, Application, ApplicationEvent, ApplicationObserver};
use crate::application::ask::observe_edit_event;
use crate::application::edit_cycle::{EditCycle, EditTermination};
use crate::application::step_dispatch::StepLoopReport;
use std::future::Future;
use std::sync::Arc;
use tauqe_core::control::context::EventSink;
use tauqe_core::models::{ModelRef, ModelRole, ReasoningEffort};
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::turn::{ModelTurnPlanContext, TurnWorkContext};

pub(crate) struct StepRunPublication {
    pub model: ModelRef,
    pub role: Option<ModelRole>,
    pub effort: Option<ReasoningEffort>,
    pub alias: Option<String>,
    pub plan_context: Option<ModelTurnPlanContext>,
    pub work_context: Option<TurnWorkContext>,
    pub session_label: Option<String>,
}

impl Application {
    /// Surround prepared work with its publication and accounting lifecycle.
    /// The caller retains ownership of task supervision and context adoption.
    pub(crate) async fn run_prepared_step<F, Fut>(
        &self,
        publication: StepRunPublication,
        observer: Arc<dyn ApplicationObserver>,
        run: F,
    ) -> StepLoopReport
    where
        F: FnOnce(String, EventSink) -> Fut,
        Fut: Future<Output = StepLoopReport>,
    {
        let StepRunPublication {
            model,
            role,
            effort,
            alias,
            plan_context,
            work_context,
            session_label,
        } = publication;
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
            let usage_observer = observer.clone();
            let usage_guard = self
                .begin_usage(&op_id, move |id, snapshot| {
                    usage_observer.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()));
                })
                .await;
            let relay_edit_cycle = Arc::clone(&edit_cycle);
            tokio::spawn(async move {
                let mut assistant_text = String::new();
                while let Some(event) = event_rx.recv().await {
                    match &event {
                        StreamEvent::TextDelta(delta) => assistant_text.push_str(delta),
                        StreamEvent::ResetStream => assistant_text.clear(),
                        StreamEvent::Usage(report) => usage_guard.observe(report).await,
                        _ => {}
                    }
                    observe_edit_event(&relay_edit_cycle, &event);
                    relay_observer.publish(&relay_op, ApplicationEvent::Stream(event));
                }
                (assistant_text, usage_guard)
            })
        };

        let report = run(op_id.clone(), sink.clone()).await;
        drop(sink);
        let telemetry = relay.await;
        let completion = crate::api::classify_step_edit_completion(&report);
        let termination = match completion.disposition {
            crate::api::StepEditDisposition::Completed => EditTermination::Answered,
            crate::api::StepEditDisposition::Refused => EditTermination::StagingRefused,
            crate::api::StepEditDisposition::Cancelled => EditTermination::Cancelled,
            crate::api::StepEditDisposition::Failed => EditTermination::ProtocolFailure,
        };
        edit_cycle.close_outcome(termination, completion.outcome);
        match telemetry {
            Ok((assistant_text, usage_guard)) => {
                observer.publish(&op_id, ApplicationEvent::Finished(assistant_text));
                usage_guard.flush().await;
            }
            Err(error) => {
                tracing::warn!(operation_id = %op_id, %error, "Step telemetry relay terminated unexpectedly")
            }
        }
        observer.publish(&op_id, ApplicationEvent::Usage(self.usage_snapshot().await));
        report
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
    use crate::application::edit_cycle::EditCycleEvent;
    use std::sync::Mutex;
    use tauqe_core::config::AppConfig;
    use tauqe_core::providers::ModelUsageInfo;

    struct Observer(Mutex<Vec<ApplicationEvent>>);

    impl ApplicationObserver for Observer {
        fn publish(&self, _: &str, event: ApplicationEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn publication() -> StepRunPublication {
        StepRunPublication {
            model: ModelRef::openrouter("mock/model"),
            role: None,
            effort: None,
            alias: None,
            plan_context: None,
            work_context: None,
            session_label: None,
        }
    }

    #[tokio::test]
    async fn closes_open_edit_once_and_keeps_usage_after_failure_and_stream_reset() {
        let root = tempfile::tempdir().unwrap();
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.path().into(), history);
        let observer = Arc::new(Observer(Mutex::new(Vec::new())));
        let report = app
            .run_prepared_step(publication(), observer.clone(), |_, sink| async move {
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
            })
            .await;
        assert!(matches!(report, StepLoopReport::Failed(_)));
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
        assert!(events.iter().any(|event| matches!(event, ApplicationEvent::UsageTotals(snapshot) if snapshot.cur.is_none() && snapshot.session.total_tokens == 7)));
        assert!(
            matches!(events.last(), Some(ApplicationEvent::Usage(snapshot)) if snapshot.session.total_tokens == 7)
        );
    }
}
