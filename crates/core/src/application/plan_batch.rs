//! Application lifecycle of an implementation batch.
use crate::context::ContextState;
use crate::models::ModelSelection;
use crate::plan::Plan;
use crate::{
    api::{Application, ContextClearScope, OperationGuard},
    config::AppConfig,
    plan::storage::PlanStorage,
    providers::StreamEvent,
    workflow::implementation::{
        ExecutionScope, ImplementationHost, PlanImplementationOptions, PlanImplementationPipeline,
        PlanImplementationReport,
    },
};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::mpsc;

/// Outcome of a headless batch of plan steps.
///
#[derive(Debug, PartialEq, Eq)]
pub enum BatchOutcome {
    Completed {
        steps: usize,
    },
    Failed {
        completed: usize,
        step_id: String,
        reason: String,
    },
    Blocked {
        completed: usize,
        step_id: String,
        reason: String,
    },
    Cancelled {
        completed: usize,
    },
    Stopped {
        completed: usize,
    },
}

/// Execution options a plan batch forwards to every step it runs.
///
/// The coder selection names the ladder rung a step starts from. It travels from
/// the RPC parameters, so a batch never invents a starting model of its own.
#[derive(Debug, Clone, Default)]
pub struct PlanBatchOptions {
    /// Coder selection the batch starts from, resolved against the coder ladder.
    pub model: Option<ModelSelection>,
    /// Targeted step or subtree to execute, if not the entire plan.
    pub step_id: Option<String>,
}

/// Passive publication and host bindings supplied by the caller.
#[async_trait::async_trait]
pub trait PlanBatchObserver: Send + Sync {
    fn context_changed(&self, state: ContextState);
    fn host(&self, config: AppConfig, root: PathBuf) -> Arc<dyn ImplementationHost>;
    async fn relay(&self, events: mpsc::Receiver<StreamEvent>);
    async fn publish(&self, plan: Plan);
}
#[derive(Debug, Clone, Copy)]
pub enum PlanQueueFailure {
    Empty,
    Load,
    NotFound,
    Blocked,
    Completed,
}
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PlanQueueError {
    pub kind: PlanQueueFailure,
    pub message: String,
}
pub struct PreparedPlanQueue {
    pub scheduled: Vec<(String, Vec<String>)>,
}
#[async_trait::async_trait]
pub trait PlanQueueObserver: PlanBatchObserver {
    fn batch_finished(
        &self,
        id: &str,
        elapsed: std::time::Duration,
        report: &PlanImplementationReport,
    );
}
impl Application {
    pub async fn prepare_plan_queue(
        &self,
        ids: &[String],
    ) -> Result<PreparedPlanQueue, PlanQueueError> {
        use crate::plan::PlanScopeEvaluation;
        let error = |kind, message| PlanQueueError { kind, message };
        if ids.is_empty() {
            return Err(error(
                PlanQueueFailure::Empty,
                "Pipeline must reference at least one plan".into(),
            ));
        }
        let root = self.repository_root().await;
        let mut scheduled = Vec::new();
        for id in ids {
            let load_root = root.clone();
            let load_id = id.clone();
            let plan =
                tokio::task::spawn_blocking(move || PlanStorage::load_plan(&load_root, &load_id))
                    .await
                    .map_err(|e| {
                        error(PlanQueueFailure::Load, format!("blocking task failed: {e}"))
                    })?
                    .map_err(|e| error(PlanQueueFailure::Load, format!("{e:#}")))?
                    .ok_or_else(|| {
                        error(PlanQueueFailure::NotFound, format!("Plan '{id}' not found"))
                    })?;
            let steps = match plan.implementation.evaluate_scope(None) {
                PlanScopeEvaluation::Ready(steps) => {
                    steps.into_iter().map(|step| step.id).collect()
                }
                PlanScopeEvaluation::AllCompleted
                    if plan.implementation.items.is_empty() && !plan.concept.is_empty() =>
                {
                    Vec::new()
                }
                PlanScopeEvaluation::AllCompleted => continue,
                other => {
                    return Err(error(
                        PlanQueueFailure::Blocked,
                        format!("Plan '{}' cannot be executed: {:?}", plan.id, other),
                    ))
                }
            };
            scheduled.push((plan.id, steps));
        }
        if scheduled.is_empty() {
            return Err(error(
                PlanQueueFailure::Completed,
                "Every plan in the pipeline is already completed".into(),
            ));
        }
        Ok(PreparedPlanQueue { scheduled })
    }
    /// Executes a validated queue under one operation and stops on the first unsettled horizon.
    pub async fn execute_plan_queue(
        &self,
        queue: PreparedPlanQueue,
        operation: Arc<OperationGuard>,
        options: PlanBatchOptions,
        observer: Arc<dyn PlanQueueObserver>,
    ) {
        for (id, _) in queue.scheduled {
            let started = std::time::Instant::now();
            let report = self
                .execute_plan_batch(&id, &operation, &options, observer.clone())
                .await;
            observer.batch_finished(&id, started.elapsed(), &report);
            if !matches!(
                report.stop,
                crate::workflow::implementation::PlanImplementationStop::Completed
            ) {
                tracing::warn!(plan_id = id, stop = ?report.stop, "Pipeline stopped");
                break;
            }
            tracing::info!(
                plan_id = id,
                succeeded = report.succeeded,
                "Pipeline plan completed"
            );
        }
    }
    /// Activates, executes, and reloads one durable horizon under the caller's operation.
    pub async fn execute_plan_batch(
        &self,
        plan_id: &str,
        operation: &Arc<OperationGuard>,
        options: &PlanBatchOptions,
        observer: Arc<dyn PlanBatchObserver>,
    ) -> PlanImplementationReport {
        assert!(
            self.owns_operation(operation),
            "operation belongs to another application"
        );
        let root = self.repository_root().await;
        let active_root = root.clone();
        let active_id = plan_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            PlanStorage::save_active_id(&active_root, Some(&active_id))
        })
        .await;
        if let Ok(state) = self.clear_context(ContextClearScope::Transient).await {
            observer.context_changed(state);
        }
        let config = self.configuration().await.config.clone();
        let host = observer.host(config.clone(), root.clone());
        let (tx, rx) = mpsc::channel(32);
        let pipeline = PlanImplementationPipeline::new(
            &root,
            ExecutionScope::Plan {
                plan_id: plan_id.to_string(),
                step_id: options.step_id.clone(),
            },
            host.as_ref(),
            config,
            PlanImplementationOptions {
                selection: options.model.clone(),
                instructions: None,
                marker: None,
            },
        );
        let cancel = operation.cancel_receiver();
        let soft_stop = operation.soft_stop_receiver();
        let run = async {
            let report = pipeline.run(&tx, &cancel, &soft_stop).await;
            drop(tx);
            report
        };
        let (report, ()) = tokio::join!(run, observer.relay(rx));
        if let Ok(state) = self.clear_context(ContextClearScope::Transient).await {
            observer.context_changed(state);
        }
        let load_id = plan_id.to_string();
        match tokio::task::spawn_blocking(move || PlanStorage::load_plan(&root, &load_id)).await {
            Ok(Ok(Some(plan))) => observer.publish(plan).await,
            Ok(Ok(None)) => tracing::warn!(plan_id, "the executed plan was not found"),
            Ok(Err(error)) => tracing::warn!(plan_id, %error, "failed to reload the executed plan"),
            Err(error) => tracing::warn!(plan_id, %error, "failed to reload the executed plan"),
        }
        report
    }
}
