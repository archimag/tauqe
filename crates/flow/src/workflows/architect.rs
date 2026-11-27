//! Headless Architect task lifecycle, context merge, and usage accounting.
pub(crate) mod refinement;
mod synthesis;

use self::synthesis::run_architect_synthesis;
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use crate::application::context::ContextSession;
use crate::application::usage_totals::UsageReceipt;
use crate::application::{ExecutionSession, PlanExecutionEnvironment};
use std::sync::Arc;
use tauqe_core::context::ContextManager;
use tauqe_core::execution::task::{self, TaskOutcome};
use tauqe_core::models::ArchitectTier;
use tauqe_core::plan::find_duplicate_item_ids;
use tauqe_core::plan::prompt::{
    AbortReport, PlanRefinementOutput, StructuredRefineOutcome, StructuredRefineStatus,
};
use tauqe_core::providers::StreamEvent;
use tauqe_core::stop::HardCancellation;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default)]
pub struct ContourRelaySummary {
    pub assistant_text: String,
    pub cancelled: bool,
    pub error: Option<String>,
    pub terminal_stream_published: bool,
    pub(crate) receipt: Option<UsageReceipt>,
}

pub struct PlanHorizonContour {
    pub operation_id: String,
    pub task: String,
    pub context: ContextManager,
    pub scope: ArchitectScope,
    pub tier: ArchitectTier,
    pub intro_header: String,
}

/// Research retained only for the lifetime of the owning operation.
pub(crate) struct ArchitectResearchState {
    pub(crate) context: ContextManager,
    pub(crate) execution: crate::application::ArchitectResearchExecution,
}

impl std::fmt::Debug for ArchitectResearchState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ArchitectResearchState { research: <private> }")
    }
}

pub(crate) enum HorizonVerdict {
    Synthesized {
        output: PlanRefinementOutput,
        tier: ArchitectTier,
    },
    Blocked {
        reason: String,
        tier: ArchitectTier,
    },
    Aborted {
        report: AbortReport,
        tier: ArchitectTier,
    },
    EmptyHorizon {
        tier: ArchitectTier,
    },
    InvalidTree {
        duplicate: String,
        tier: ArchitectTier,
    },
    Cancelled,
    Failed(String),
}

/// Admit a contour's horizon once, independently of the caller's persistence policy.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_horizon_wave(
    resources: &ExecutionSession,
    shared_context: &ContextSession,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    work: PlanHorizonContour,
    observer: Arc<dyn ApplicationObserver>,
) -> (HorizonVerdict, ContourRelaySummary) {
    let (result, summary) = run_contour(
        resources,
        shared_context,
        operation,
        environment,
        work,
        observer.clone(),
    )
    .await;
    let _ = shared_context.clear_transient_context().await;
    let verdict = match result {
        _ if summary.cancelled || operation.hard_cancellation().is_cancelled() => {
            HorizonVerdict::Cancelled
        }
        Err(error) => {
            HorizonVerdict::Failed(summary.error.clone().unwrap_or_else(|| error.to_string()))
        }
        Ok(StructuredRefineOutcome {
            output,
            tier,
            status,
        }) => match status {
            StructuredRefineStatus::Cancelled => HorizonVerdict::Cancelled,
            StructuredRefineStatus::Blocked { reason } => HorizonVerdict::Blocked { reason, tier },
            StructuredRefineStatus::Aborted(report) => HorizonVerdict::Aborted { report, tier },
            StructuredRefineStatus::Synthesized => match output {
                Some(output) if !output.items.is_empty() => {
                    if let Some(duplicate) = find_duplicate_item_ids(&output.items) {
                        HorizonVerdict::InvalidTree { duplicate, tier }
                    } else {
                        HorizonVerdict::Synthesized { output, tier }
                    }
                }
                _ => HorizonVerdict::EmptyHorizon { tier },
            },
        },
    };
    (verdict, summary)
}

/// Identity of the plan or review audit whose Architect prefix is warmed.
#[derive(Clone)]
pub enum ArchitectScope {
    Plan(String),
    Review(String),
}

const CONTOUR_EVENT_CAPACITY: usize = 256;
/// Relays the engine stream without owning the accounting window.
async fn relay_architect_events(
    observer: Arc<dyn ApplicationObserver>,
    op_id: String,
    mut events_rx: mpsc::Receiver<StreamEvent>,
    cancel: HardCancellation,
) -> ContourRelaySummary {
    let mut summary = ContourRelaySummary::default();
    while let Some(event) = events_rx.recv().await {
        observer.ready(&event, &cancel).await;
        match &event {
            StreamEvent::TextDelta(delta) => summary.assistant_text.push_str(delta),
            StreamEvent::ResetStream => summary.assistant_text.clear(),
            StreamEvent::Cancelled => {
                summary.cancelled = true;
                summary.terminal_stream_published = true;
            }
            StreamEvent::Error(message) => {
                summary.error = Some(message.clone());
                summary.terminal_stream_published = true;
            }
            StreamEvent::Done => break,
            _ => {}
        }
        observer.publish(&op_id, ApplicationEvent::Stream(event));
    }
    summary
}
/// Run a read-only synthesis under the caller's admitted operation and frozen configuration.
pub(crate) async fn run_contour(
    resources: &ExecutionSession,
    shared_context: &ContextSession,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    work: PlanHorizonContour,
    observer: Arc<dyn ApplicationObserver>,
) -> (anyhow::Result<StructuredRefineOutcome>, ContourRelaySummary) {
    let (result, summary, mut state) = run_contour_with_state_inner(
        resources,
        shared_context,
        operation,
        environment,
        work,
        observer.clone(),
        None,
        true,
    )
    .await;
    if let Some(state) = state.as_mut() {
        tauqe_core::blocking::run_blocking(|| state.context.clear_transient());
    }
    (result, summary)
}

/// Return the detached state only after the worker and accounting have drained.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_contour_with_state(
    resources: &ExecutionSession,
    shared_context: &ContextSession,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    work: PlanHorizonContour,
    observer: Arc<dyn ApplicationObserver>,
    previous: Option<ArchitectResearchState>,
) -> (
    anyhow::Result<StructuredRefineOutcome>,
    ContourRelaySummary,
    Option<ArchitectResearchState>,
) {
    run_contour_with_state_inner(
        resources,
        shared_context,
        operation,
        environment,
        work,
        observer,
        previous,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_contour_with_state_inner(
    resources: &ExecutionSession,
    shared_context: &ContextSession,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    work: PlanHorizonContour,
    observer: Arc<dyn ApplicationObserver>,
    previous: Option<ArchitectResearchState>,
    cleanup_shared: bool,
) -> (
    anyhow::Result<StructuredRefineOutcome>,
    ContourRelaySummary,
    Option<ArchitectResearchState>,
) {
    if !resources.owns(operation) {
        let message =
            "Architect synthesis requires an active operation for this session".to_string();
        return (
            Err(anyhow::anyhow!(message.clone())),
            ContourRelaySummary {
                error: Some(message),
                ..Default::default()
            },
            previous,
        );
    }
    let PlanHorizonContour {
        operation_id,
        task,
        context,
        scope,
        tier,
        intro_header,
    } = work;
    let op_id = operation_id.as_str();
    let repo_root = context.repo_root().to_path_buf();
    let continuing = previous.is_some();
    let mut state = if let Some(state) = previous {
        state
    } else {
        let execution = match environment
            .for_architect_research(resources, operation, &tier, &scope, &repo_root)
            .await
        {
            Ok(execution) => execution,
            Err(err) => {
                observer.publish(
                    op_id,
                    ApplicationEvent::Stream(StreamEvent::Error(err.to_string())),
                );
                return (
                    Err(anyhow::anyhow!(err.to_string())),
                    ContourRelaySummary {
                        error: Some(err.to_string()),
                        ..Default::default()
                    },
                    None,
                );
            }
        };
        ArchitectResearchState { context, execution }
    };
    if state.context.repo_root() != repo_root {
        return (
            Err(anyhow::anyhow!(
                "Architect research belongs to another repository"
            )),
            ContourRelaySummary::default(),
            Some(state),
        );
    }
    let Some(listener) = shared_context.activate_research_publication(
        operation,
        operation_id.clone(),
        observer.clone(),
        &state.context,
    ) else {
        let message = "Could not bind architect context publication".to_string();
        return (
            Err(anyhow::anyhow!(message.clone())),
            ContourRelaySummary {
                error: Some(message),
                ..Default::default()
            },
            Some(state),
        );
    };
    state.context.set_listener(Some(listener));
    let (events_tx, events_rx) = mpsc::channel::<StreamEvent>(CONTOUR_EVENT_CAPACITY);
    // The relay waits out a client that stopped reading, so it shares the very
    // signal that interrupts the synthesis it relays.
    let relay_cancel = operation.hard_cancellation();
    let synthesis_cancel = operation.hard_cancellation();

    // All research rounds and tier changes retain the same request-bound scope.
    let scope = resources.prepare_scope();
    let binding = if continuing {
        state.execution.rebind_usage(scope.binding())
    } else {
        state.execution.bind_usage(scope.binding())
    };
    if let Err(error) = binding {
        let message = error.to_string();
        state.context.set_listener(None);
        shared_context.close_research_publication(operation.id());
        return (
            Err(anyhow::anyhow!(message.clone())),
            ContourRelaySummary {
                error: Some(message),
                ..Default::default()
            },
            Some(state),
        );
    }
    let relay = tokio::spawn(relay_architect_events(
        observer.clone(),
        op_id.to_string(),
        events_rx,
        relay_cancel,
    ));

    let context_session = shared_context.clone();
    let publication_operation = operation.id();
    let context_op_id = op_id.to_string();
    let publisher = observer.clone();
    let (handle_tx, handle_rx) = tokio::sync::oneshot::channel();
    let supervised = scope.run(
        shared_context,
        op_id.to_string(),
        move |id, snapshot| publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone())),
        async move {
            let result = run_architect_synthesis(
                &mut state,
                &task,
                &repo_root,
                tier,
                &intro_header,
                events_tx,
                synthesis_cancel,
            )
            .await;
            (state, result)
        },
        move |handle| {
            let _ = handle_tx.send(handle);
        },
        move |joined| async move {
            if let TaskOutcome::Completed((state, _)) = &joined {
                match context_session
                    .merge_turn_context(state.context.clone())
                    .await
                {
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        operation_id = context_op_id,
                        error = %error,
                        "Failed to merge the architect context back into the shared state"
                    ),
                }
            }
            if cleanup_shared {
                if let Err(error) = context_session.clear_transient_context().await {
                    tracing::warn!(error = %error, "Failed to clear architect transient context");
                }
            }
            context_session.close_research_publication(publication_operation);
            joined
        },
    );
    tokio::pin!(supervised);
    let joined = tokio::select! {
        result = &mut supervised => result,
        handle = handle_rx => {
            if let Ok(handle) = handle {
                operation.set_abort_handle(handle);
            }
            supervised.await
        }
    };
    operation.clear_abort_handle();

    let mut summary = match task::join(relay).await {
        TaskOutcome::Completed(summary) => summary,
        other => {
            tracing::warn!(
                operation_id = op_id,
                error = ?other,
                "Architect telemetry relay terminated unexpectedly"
            );
            ContourRelaySummary {
                error: Some(format!(
                    "Architect telemetry relay terminated unexpectedly: {other:?}"
                )),
                ..Default::default()
            }
        }
    };

    let joined = match joined {
        Ok((joined, receipt)) => {
            summary.receipt = Some(receipt);
            joined
        }
        Err(error) => return (Err(anyhow::anyhow!(error)), summary, None),
    };
    let (state, outcome) = match joined {
        TaskOutcome::Completed((mut state, outcome)) => {
            state.context.set_listener(None);
            (Some(state), outcome)
        }
        other => {
            // A contour that ignored the cooperative signal is force-aborted through
            // the operation slot, so its relayed `Cancelled` event never arrives and
            // the summary alone would classify the dead task as a provider failure.
            // The authoritative signal decides the classification: a raised hard
            // cancel surfaces as a cancellation, while a task that died without it
            // keeps its failure classification.
            let mut summary = summary;
            if operation.hard_cancellation().is_cancelled() {
                tracing::warn!(
                    operation_id = op_id,
                    error = ?other,
                    "Architect contour terminated while the operation was cancelled"
                );
                summary.cancelled = true;
            }
            return (
                Err(anyhow::anyhow!(
                    "The architect synthesis task terminated unexpectedly: {other:?}"
                )),
                summary,
                None,
            );
        }
    };

    if let Some(error) = &summary.error {
        return (Err(anyhow::anyhow!(error.clone())), summary, state);
    }
    (outcome, summary, state)
}

pub(crate) fn publish_architect_started(
    observer: &dyn ApplicationObserver,
    id: &str,
    tier: &tauqe_core::models::ArchitectTier,
    is_batch: bool,
) {
    observer.publish(
        id,
        ApplicationEvent::Started {
            model: tier.model.clone(),
            role: Some(tauqe_core::models::ModelRole::Architect),
            alias: None,
            effort: tier.effort,
            plan_context: None,
            work_context: None,
            is_batch,
        },
    );
}

/// Finish a horizon publication without repeating a terminal stream event.
pub(crate) fn publish_horizon_terminal(
    observer: &dyn ApplicationObserver,
    id: &str,
    verdict: &HorizonVerdict,
    summary: &ContourRelaySummary,
) {
    match verdict {
        HorizonVerdict::Cancelled if !summary.terminal_stream_published => {
            observer.publish(id, ApplicationEvent::Stream(StreamEvent::Cancelled));
        }
        HorizonVerdict::Failed(reason) if !summary.terminal_stream_published => {
            observer.publish(
                id,
                ApplicationEvent::Stream(StreamEvent::Error(reason.clone())),
            );
        }
        HorizonVerdict::Cancelled | HorizonVerdict::Failed(_) => {}
        _ => observer.publish(
            id,
            ApplicationEvent::Finished(summary.assistant_text.clone()),
        ),
    }
    if let Some(receipt) = &summary.receipt {
        observer.publish(id, ApplicationEvent::Usage(receipt.snapshot.clone()));
    }
}

#[cfg(feature = "test-support")]
pub async fn test_relay_architect_events(
    observer: Arc<dyn ApplicationObserver>,
    op_id: String,
    events: mpsc::Receiver<StreamEvent>,
    cancel: HardCancellation,
) -> ContourRelaySummary {
    relay_architect_events(observer, op_id, events, cancel).await
}

#[cfg(test)]
mod context_tests;
