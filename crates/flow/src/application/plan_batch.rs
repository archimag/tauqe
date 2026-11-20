//! Application lifecycle of an implementation batch.
use crate::api::ApplicationEvent;
use crate::api::{Application, ContextClearScope, OperationGuard};
use crate::application::implementation_pipeline::{
    PlanImplementationOptions, PlanImplementationPipeline, PlanImplementationReport,
};
use crate::application::infrastructure::execution::PlanExecutionEnvironment;
use std::sync::Arc;
use tauqe_core::models::ModelSelection;
use tauqe_core::{
    plan::storage::PlanStorage, providers::StreamEvent, workflow::implementation::ExecutionScope,
};
use tokio::sync::mpsc;

/// Outcome of a headless batch of plan steps.
///
#[derive(Debug, Clone, PartialEq, Eq)]
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
    message: String,
}
#[derive(Debug, thiserror::Error)]
pub enum PlanLaunchError {
    #[error("Another operation is already running")]
    Busy,
    #[error(transparent)]
    Scope(#[from] crate::api::PlanExecutionError),
    #[error(transparent)]
    Queue(#[from] PlanQueueError),
    #[error("{0}")]
    Cancelled(String),
    #[error("{0}")]
    Verification(String),
}

pub struct PlanBatchStarted {
    pub plan_id: String,
    pub step_ids: Vec<String>,
    pub operation_id: String,
}

pub struct PlanPipelineStarted {
    pub plan_ids: Vec<String>,
    pub operation_id: String,
}

pub struct PlanBatchFinished {
    pub plan_id: String,
    pub outcome: BatchOutcome,
    pub executed: usize,
    pub scheduled: usize,
    pub elapsed: std::time::Duration,
}

impl Application {
    async fn prepare_plan_queue(&self, ids: &[String]) -> Result<Vec<String>, PlanQueueError> {
        use tauqe_core::plan::PlanScopeEvaluation;
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
            match plan.implementation.evaluate_scope(None) {
                PlanScopeEvaluation::Ready(_) => {}
                PlanScopeEvaluation::AllCompleted
                    if plan.implementation.items.is_empty() && !plan.concept.is_empty() => {}
                PlanScopeEvaluation::AllCompleted => continue,
                other => {
                    return Err(error(
                        PlanQueueFailure::Blocked,
                        format!("Plan '{}' cannot be executed: {:?}", plan.id, other),
                    ))
                }
            }
            scheduled.push(plan.id);
        }
        if scheduled.is_empty() {
            return Err(error(
                PlanQueueFailure::Completed,
                "Every plan in the pipeline is already completed".into(),
            ));
        }
        Ok(scheduled)
    }
    pub async fn execute_plan(
        &self,
        plan_id: String,
        options: PlanBatchOptions,
        observer: Arc<dyn crate::api::ApplicationObserver>,
    ) -> Result<PlanBatchStarted, PlanLaunchError> {
        let operation = Arc::new(
            self.try_begin_operation()
                .map_err(|_| PlanLaunchError::Busy)?,
        );
        let (plan, step_ids) = self
            .prepare_plan_execution(&plan_id, options.step_id.as_deref())
            .await?;
        let operation_id = self
            .prepare_plan_launch(&operation, observer.as_ref(), false)
            .await?;
        let started = PlanBatchStarted {
            plan_id: plan.id.clone(),
            step_ids,
            operation_id,
        };
        let application = self.clone();
        tokio::spawn(async move {
            application
                .run_plan_batch(&plan.id, &operation, &options, observer)
                .await;
        });
        Ok(started)
    }

    pub async fn execute_plan_pipeline(
        &self,
        ids: Vec<String>,
        model: Option<ModelSelection>,
        observer: Arc<dyn crate::api::ApplicationObserver>,
    ) -> Result<PlanPipelineStarted, PlanLaunchError> {
        let operation = Arc::new(
            self.try_begin_operation()
                .map_err(|_| PlanLaunchError::Busy)?,
        );
        let plan_ids = self.prepare_plan_queue(&ids).await?;
        let operation_id = self
            .prepare_plan_launch(&operation, observer.as_ref(), true)
            .await?;
        let started = PlanPipelineStarted {
            plan_ids: plan_ids.clone(),
            operation_id,
        };
        let application = self.clone();
        tokio::spawn(async move {
            let options = PlanBatchOptions {
                model,
                step_id: None,
            };
            for id in plan_ids {
                let outcome = application
                    .run_plan_batch(&id, &operation, &options, observer.clone())
                    .await;
                if !matches!(outcome, BatchOutcome::Completed { .. }) {
                    break;
                }
            }
        });
        Ok(started)
    }

    async fn prepare_plan_launch(
        &self,
        operation: &OperationGuard,
        observer: &dyn crate::api::ApplicationObserver,
        pipeline: bool,
    ) -> Result<String, PlanLaunchError> {
        use crate::api::PreflightVerdict;
        let root = self.repository_root().await;
        match crate::api::run_preflight_verification(&root, operation.cancel_receiver()).await {
            PreflightVerdict::Passed => {}
            PreflightVerdict::Cancelled => {
                let scope = if pipeline { "pipeline" } else { "batch" };
                return Err(PlanLaunchError::Cancelled(format!(
                    "Pre-flight verification was cancelled; {scope} execution did not start"
                )));
            }
            PreflightVerdict::Failed(report) => {
                let scope = if pipeline {
                    "Pipeline execution was blocked before starting any plan."
                } else {
                    "Batch execution was blocked before starting any step."
                };
                return Err(PlanLaunchError::Verification(format!("Pre-flight verification failed: the repository does not build cleanly. {scope}\n\n{report}")));
            }
        }
        let id = operation.publication_id();
        self.clear_discussion_focus().await.publish(&id, observer);
        Ok(id)
    }

    #[cfg(feature = "test-support")]
    pub async fn test_run_plan_batch(
        &self,
        plan_id: &str,
        operation: &Arc<OperationGuard>,
        options: &PlanBatchOptions,
        observer: Arc<dyn crate::api::ApplicationObserver>,
    ) -> BatchOutcome {
        self.run_plan_batch(plan_id, operation, options, observer)
            .await
    }

    async fn run_plan_batch(
        &self,
        plan_id: &str,
        operation: &Arc<OperationGuard>,
        options: &PlanBatchOptions,
        observer: Arc<dyn crate::api::ApplicationObserver>,
    ) -> BatchOutcome {
        use tauqe_core::workflow::implementation::PlanImplementationStop;
        let started = std::time::Instant::now();
        let report = self
            .execute_plan_batch(plan_id, operation, options, observer.clone())
            .await;
        let completed = report.succeeded;
        let outcome = match report.stop {
            PlanImplementationStop::Completed => BatchOutcome::Completed { steps: completed },
            PlanImplementationStop::Failed { step, reason } => BatchOutcome::Failed {
                completed,
                step_id: step.unwrap_or_else(|| plan_id.into()),
                reason,
            },
            PlanImplementationStop::Blocked { step, reason } => BatchOutcome::Blocked {
                completed,
                step_id: step.unwrap_or_else(|| plan_id.into()),
                reason,
            },
            PlanImplementationStop::Cancelled => BatchOutcome::Cancelled { completed },
            PlanImplementationStop::Stopped => BatchOutcome::Stopped { completed },
        };
        tracing::info!(
            plan_id,
            executed = report.executed,
            succeeded = completed,
            replans = report.replans,
            ?outcome,
            "Plan batch finished"
        );
        observer.publish(
            &operation.publication_id(),
            ApplicationEvent::PlanBatchFinished(PlanBatchFinished {
                plan_id: plan_id.into(),
                outcome: outcome.clone(),
                executed: report.executed,
                scheduled: report.scheduled.max(report.executed),
                elapsed: started.elapsed(),
            }),
        );
        outcome
    }

    /// Activates, executes, and reloads one durable horizon under the caller's operation.
    async fn execute_plan_batch(
        &self,
        plan_id: &str,
        operation: &Arc<OperationGuard>,
        options: &PlanBatchOptions,
        observer: Arc<dyn crate::api::ApplicationObserver>,
    ) -> PlanImplementationReport {
        assert!(
            self.owns_operation(operation),
            "operation belongs to another application"
        );
        let id = operation.publication_id();
        let root = self.repository_root().await;
        let active_root = root.clone();
        let active_id = plan_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            PlanStorage::save_active_id(&active_root, Some(&active_id))
        })
        .await;
        if let Ok(state) = self.clear_context(ContextClearScope::Transient).await {
            observer.publish(&id, ApplicationEvent::Context(state));
        }
        let snapshot = self.configuration().await;
        let config = snapshot.config.clone();
        let execution = PlanExecutionEnvironment::new(snapshot, operation);
        let host = Arc::new(
            crate::application::implementation_host::ApplicationImplementationHost::new(
                self.clone(),
                observer.clone(),
                operation.clone(),
                config.clone(),
                execution,
                root.clone(),
            ),
        );
        let (tx, mut rx) = mpsc::channel(32);
        let pipeline = PlanImplementationPipeline::new(
            &root,
            ExecutionScope::Plan {
                plan_id: plan_id.to_string(),
                step_id: options.step_id.clone(),
            },
            self,
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
        let relay = async {
            while let Some(event) = rx.recv().await {
                if matches!(event, StreamEvent::TierEscalated { .. }) {
                    observer.publish(&id, ApplicationEvent::Stream(event));
                }
            }
        };
        let (report, ()) = tokio::join!(run, relay);
        if let Ok(state) = self.clear_context(ContextClearScope::Transient).await {
            observer.publish(&id, ApplicationEvent::Context(state));
        }
        let documents = self.documents_at(&root);
        match documents.plan(plan_id).await {
            Ok(Some(plan)) => {
                observer.publish(
                    &id,
                    ApplicationEvent::PlanDocument {
                        plan,
                        is_save: false,
                    },
                );
                if let Ok(plans) = documents.plans().await {
                    observer.publish(
                        &id,
                        ApplicationEvent::PlanList {
                            plans,
                            active_id: Some(plan_id.into()),
                        },
                    );
                }
            }
            Ok(None) => tracing::warn!(plan_id, "the executed plan was not found"),
            Err(error) => tracing::warn!(plan_id, %error, "failed to reload the executed plan"),
        }
        report
    }
}
