//! Application lifecycle of an implementation batch.
#[cfg(feature = "test-support")]
use crate::api::OperationGuard;
use crate::api::{Application, PlanLaunchError};
#[cfg(feature = "test-support")]
use crate::workflows::plan;
use crate::workflows::plan::PlanResources;
use std::sync::Arc;
use tauqe_core::models::ModelSelection;

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
    pub(crate) message: String,
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
    pub(crate) fn plan_resources(&self) -> PlanResources {
        PlanResources {
            step: self.step_resources(),
            history: self.history_session().clone(),
            dialogue: self.dialogue_session().clone(),
        }
    }

    pub async fn execute_plan(
        &self,
        plan_id: String,
        options: PlanBatchOptions,
        observer: Arc<dyn crate::api::ApplicationObserver>,
    ) -> Result<PlanBatchStarted, PlanLaunchError> {
        self.plan_resources()
            .launch_batch(
                self.configuration_session(),
                self.focus_session(),
                plan_id,
                options,
                observer,
            )
            .await
    }

    pub async fn execute_plan_pipeline(
        &self,
        ids: Vec<String>,
        model: Option<ModelSelection>,
        observer: Arc<dyn crate::api::ApplicationObserver>,
    ) -> Result<PlanPipelineStarted, PlanLaunchError> {
        self.plan_resources()
            .launch_pipeline(
                self.configuration_session(),
                self.focus_session(),
                ids,
                model,
                observer,
            )
            .await
    }

    #[cfg(feature = "test-support")]
    pub async fn test_run_plan_batch(
        &self,
        plan_id: &str,
        operation: &Arc<OperationGuard>,
        options: &PlanBatchOptions,
        observer: Arc<dyn crate::api::ApplicationObserver>,
    ) -> BatchOutcome {
        let execution = super::PlanExecutionEnvironment::new(self.configuration().await, operation);
        plan::run_batch(
            &self.plan_resources(),
            &execution,
            plan_id,
            operation,
            options,
            observer,
        )
        .await
    }

    #[cfg(feature = "test-support")]
    pub async fn test_prepare_implementation_step(
        &self,
        operation: &OperationGuard,
        root: &std::path::Path,
        config: &tauqe_core::config::AppConfig,
        request: &tauqe_core::workflow::implementation::StepTurnRequest,
        publish: impl Fn(tauqe_core::context::ContextState) + Send,
    ) -> Result<String, plan::StepPreparationError> {
        let execution = super::PlanExecutionEnvironment::new(
            super::configuration::ConfigurationSnapshot {
                config: config.clone(),
                selection: config.default_selection(),
            },
            operation,
        );
        plan::prepare_implementation_step(
            &self.plan_resources(),
            operation,
            root,
            config,
            &execution.compaction_environment(),
            request,
            publish,
        )
        .await
    }
}
