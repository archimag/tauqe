use crate::api::{next_operation_id, Application, OperationGuard};
use crate::api::{ApplicationEvent, ApplicationObserver};
use crate::application::ask::observe_edit_event;
use crate::application::edit_cycle::{EditCycle, EditTermination};
use crate::application::step_dispatch::{StepLoopBody, StepLoopReport};
use std::sync::Arc;
use tauqe_core::control::context::EventSink;
use tauqe_core::models::ReasoningEffort;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::turn::TurnWorkContext;

/// Options of one step-endpoint execution through the control loops.
pub struct StepLoopOptions {
    /// Explicit model selection, or the active selection of the session.
    pub selection: Option<tauqe_core::models::ModelSelection>,
    /// Reasoning depth of the run.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Turn marker bound to the tagged control channel.
    pub marker: Option<String>,
    /// Provider session identifier shared by every turn of the run.
    pub session_id: Option<String>,
    /// Registration of the batch that owns the run.
    pub operation: Arc<OperationGuard>,
    /// Plan coordinates published with `model/started`.
    pub plan_context: Option<tauqe_core::workflow::turn::ModelTurnPlanContext>,
    /// Work the run executes, published with `model/started`.
    pub work_context: Option<TurnWorkContext>,
    /// Body the run executes.
    pub body: StepLoopBody,
}

/// Runs one step or sequence using its parent operation and settles its telemetry.
impl Application {
    pub(crate) async fn run_step_loop(
        &self,
        options: StepLoopOptions,
        observer: Arc<dyn ApplicationObserver>,
    ) -> StepLoopReport {
        let StepLoopOptions {
            selection,
            reasoning_effort,
            marker,
            session_id,
            operation,
            plan_context,
            work_context,
            body,
        } = options;

        let resolved = self
            .resolve_turn_model(selection, reasoning_effort, crate::api::TurnMode::Step)
            .await;
        let provider = match self.resolve_turn_provider(&resolved).await {
            Ok(provider) => provider,
            Err(error) => return StepLoopReport::Failed(error.to_string()),
        };
        let crate::api::ResolvedTurnModel {
            config,
            selection,
            model,
            role,
            effort,
            alias,
        } = resolved;
        let op_id = format!("op-{}", next_operation_id());
        tracing::info!(
            operation_id = %op_id,
            model = %model,
            role = ?role,
            effort = ?effort,
            work = work_kind(work_context.as_ref()),
            session = session_id.as_deref().unwrap_or("none"),
            "step run started"
        );
        observer.publish(
            &op_id,
            ApplicationEvent::Started {
                model: model.clone(),
                role,
                alias,
                effort,
                plan_context,
                work_context,
                is_batch: true,
            },
        );

        let execution = match self
            .prepare_step_execution(
                &operation,
                crate::api::StepExecutionRequest {
                    provider,
                    model,
                    config,
                    selection,
                    reasoning_effort: effort,
                    marker,
                    session_id,
                    body,
                },
            )
            .await
        {
            Ok(execution) => execution,
            Err(error) => return StepLoopReport::Failed(error.to_string()),
        };
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

        // Core owns task supervision, recovery, and merging the returned context.
        let report = self
            .execute_prepared_step(&operation, &op_id, execution, sink.clone())
            .await;
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
