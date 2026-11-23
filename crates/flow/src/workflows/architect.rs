//! Headless Architect task lifecycle, context merge, and usage accounting.
mod synthesis;

use self::synthesis::run_architect_synthesis;
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard, UsageGuard};
use crate::application::context::ContextSession;
use crate::application::usage::UsageSession;
use crate::application::{ExecutionSession, PlanExecutionEnvironment};
use std::sync::Arc;
use tauqe_core::context::ContextManager;
use tauqe_core::models::ArchitectTier;
use tauqe_core::plan::prompt::StructuredRefineOutcome;
use tauqe_core::providers::StreamEvent;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, Default)]
pub struct ContourRelaySummary {
    pub assistant_text: String,
    pub cancelled: bool,
    pub error: Option<String>,
}

pub struct PlanHorizonContour {
    pub operation_id: String,
    pub task: String,
    pub context: ContextManager,
    pub scope: ArchitectScope,
    pub tier: ArchitectTier,
    pub intro_header: String,
    pub budget_hint: String,
}

/// Identity of the plan or review audit whose Architect prefix is warmed.
#[derive(Clone)]
pub enum ArchitectScope {
    Plan(String),
    Review(String),
}

const CONTOUR_EVENT_CAPACITY: usize = 256;
/// Consumes the engine stream and accounts one synthesis across all its rounds.
async fn relay_architect_events(
    observer: Arc<dyn ApplicationObserver>,
    op_id: String,
    usage: UsageGuard,
    mut events_rx: mpsc::Receiver<StreamEvent>,
    mut cancel: watch::Receiver<bool>,
) -> ContourRelaySummary {
    let mut summary = ContourRelaySummary::default();
    while let Some(event) = events_rx.recv().await {
        observer.ready(&event, &mut cancel).await;
        match &event {
            StreamEvent::TextDelta(delta) => summary.assistant_text.push_str(delta),
            StreamEvent::ResetStream => summary.assistant_text.clear(),
            StreamEvent::Usage(reported) => usage.observe(reported).await,
            StreamEvent::Cancelled => summary.cancelled = true,
            StreamEvent::Error(message) => summary.error = Some(message.clone()),
            StreamEvent::Done => break,
            _ => {}
        }
        observer.publish(&op_id, ApplicationEvent::Stream(event));
    }
    usage.flush().await;
    summary
}
/// Run a read-only synthesis under the caller's admitted operation and frozen configuration.
pub(crate) async fn run_contour(
    resources: &ExecutionSession,
    shared_context: &ContextSession,
    usage_session: &UsageSession,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    work: PlanHorizonContour,
    observer: Arc<dyn ApplicationObserver>,
) -> (anyhow::Result<StructuredRefineOutcome>, ContourRelaySummary) {
    if !resources.owns(operation) {
        let message =
            "Architect synthesis requires an active operation for this session".to_string();
        return (
            Err(anyhow::anyhow!(message.clone())),
            ContourRelaySummary {
                error: Some(message),
                ..Default::default()
            },
        );
    }
    let PlanHorizonContour {
        operation_id,
        task,
        context,
        scope,
        tier,
        intro_header,
        budget_hint,
    } = work;
    let op_id = operation_id.as_str();
    let repo_root = context.repo_root().to_path_buf();
    let execution = match environment
        .for_architect_research(resources, operation, &tier, &scope)
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
            );
        }
    };
    let (events_tx, events_rx) = mpsc::channel::<StreamEvent>(CONTOUR_EVENT_CAPACITY);
    let cancel_rx = operation.cancel_receiver();
    // The relay waits out a client that stopped reading, so it shares the very
    // signal that interrupts the synthesis it relays.
    let relay_cancel = cancel_rx.clone();

    // The usage window wraps the whole synthesis, so every round and every tier
    // it climbs report into the same accounting window under the identifier the
    // surrounding operation already announced. The relay closes it.
    let publisher = observer.clone();
    let usage = usage_session
        .begin(shared_context, op_id, move |id, snapshot| {
            publisher.publish(id, ApplicationEvent::UsageTotals(snapshot.clone()))
        })
        .await;
    let relay = tokio::spawn(relay_architect_events(
        observer.clone(),
        op_id.to_string(),
        usage,
        events_rx,
        relay_cancel,
    ));

    let contour = tokio::spawn(async move {
        let mut contour_context = context;
        let result = run_architect_synthesis(
            execution,
            &mut contour_context,
            &task,
            &repo_root,
            tier,
            &intro_header,
            &budget_hint,
            events_tx,
            cancel_rx,
        )
        .await;
        (contour_context, result)
    });
    // A hard cancel reaches the task that really runs, not just the future that
    // waits for it, so the abort handle is installed for the whole synthesis and
    // dropped the moment the contour settles.
    operation.set_abort_handle(contour.abort_handle());
    let joined = contour.await;
    operation.clear_abort_handle();

    let summary = match relay.await {
        Ok(summary) => summary,
        Err(join_error) => {
            tracing::warn!(
                operation_id = op_id,
                error = %join_error,
                "Architect telemetry relay terminated unexpectedly"
            );
            ContourRelaySummary::default()
        }
    };

    let (contour_context, outcome) = match joined {
        Ok(joined) => joined,
        Err(join_error) => {
            // A contour that ignored the cooperative signal is force-aborted through
            // the operation slot, so its relayed `Cancelled` event never arrives and
            // the summary alone would classify the dead task as a provider failure.
            // The authoritative signal decides the classification: a raised hard
            // cancel surfaces as a cancellation, while a task that died without it
            // keeps its failure classification.
            let mut summary = summary;
            if *operation.cancel_receiver().borrow() {
                tracing::warn!(
                    operation_id = op_id,
                    error = %join_error,
                    "Architect contour terminated while the operation was cancelled"
                );
                summary.cancelled = true;
            }
            return (
                Err(anyhow::anyhow!(
                    "The architect synthesis task terminated unexpectedly: {join_error}"
                )),
                summary,
            );
        }
    };

    // The synthesis owns the auto context layer it widened: the material is the
    // Architect's investigation rather than the developer's dialogue, so it is
    // never kept. Everything the contour observed — the files research added
    // and the restored layers — is merged back so a partial run keeps the work
    // that was already paid for.
    let merged = shared_context.merge_turn_context(contour_context).await;
    match merged {
        Ok(ctx_state) => observer.publish(op_id, ApplicationEvent::Context(ctx_state)),
        Err(err) => {
            tracing::warn!(
                operation_id = op_id,
                error = %err,
                "Failed to merge the architect context back into the shared state"
            );
        }
    }

    (outcome, summary)
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

#[cfg(feature = "test-support")]
pub async fn test_relay_architect_events(
    observer: Arc<dyn ApplicationObserver>,
    op_id: String,
    usage: UsageGuard,
    events: mpsc::Receiver<StreamEvent>,
    cancel: watch::Receiver<bool>,
) -> ContourRelaySummary {
    relay_architect_events(observer, op_id, usage, events, cancel).await
}
