//! Execution policy for a stored plan scope and an ordered pipeline of plans.

mod attempt;
mod preparation;
mod queue;
mod refinement;
mod run;
pub(crate) mod synthesis;

#[cfg(feature = "test-support")]
pub(crate) use preparation::prepare_implementation_step;
#[cfg(feature = "test-support")]
pub use preparation::StepPreparationError;
#[cfg(feature = "test-support")]
pub(crate) use run::run_batch;

use crate::api::{
    ApplicationObserver, ContextClearScope, ContextError, OperationGuard, PlanBatchOptions,
    PlanBatchStarted, PlanPipelineStarted, PlanQueueError, PlanQueueFailure,
};
use crate::application::configuration::Configuration;
use crate::application::dialogue::DialogueSession;
use crate::application::documents::Documents;
use crate::application::focus::FocusSession;
use crate::application::history::HistorySession;
use crate::application::preflight::{run_preflight_verification, PreflightVerdict};
use crate::application::PlanExecutionEnvironment;
use crate::workflows::step::run::StepResources;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::context::ContextState;
use tauqe_core::models::ModelSelection;
use tauqe_core::plan::{Plan, PlanScopeEvaluation};
use tauqe_core::workflow::implementation::{ImplementationFrame, StepFailure};

#[derive(Debug, thiserror::Error)]
pub enum PlanLaunchError {
    #[error("Another operation is already running")]
    Busy,
    #[error(transparent)]
    Scope(#[from] PlanExecutionError),
    #[error(transparent)]
    Queue(#[from] PlanQueueError),
    #[error("{0}")]
    Cancelled(String),
    #[error("{0}")]
    Verification(String),
}

/// Why a requested plan scope could not be admitted for execution.
#[derive(Debug, Clone, Copy)]
pub enum PlanExecutionFailure {
    NothingToExecute,
    NotFound,
    PlanLoadFailed,
    StepBlocked,
    StepBlockedByDependencies,
    StepCancelled,
    StepInDiscussion,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PlanExecutionError {
    pub kind: PlanExecutionFailure,
    pub(crate) message: String,
}

pub(crate) struct PlanResources {
    pub(crate) step: StepResources,
    pub(crate) history: HistorySession,
    pub(crate) dialogue: Arc<DialogueSession>,
}

impl PlanResources {
    async fn clear_transient_context(&self) -> Result<ContextState, ContextError> {
        self.step
            .context
            .clear_context(&self.dialogue, ContextClearScope::Transient)
            .await
    }

    pub(crate) async fn launch_batch(
        self,
        configuration: &Configuration,
        focus: &FocusSession,
        plan_id: String,
        options: PlanBatchOptions,
        observer: Arc<dyn ApplicationObserver>,
    ) -> Result<PlanBatchStarted, PlanLaunchError> {
        let operation = self
            .step
            .execution
            .operations
            .begin()
            .map_err(|_| PlanLaunchError::Busy)?;
        let root = self.step.context.repository_root().await;
        let documents = Documents::new(&root);
        let started = prepare_batch_launch(
            &documents,
            &root,
            &plan_id,
            options.step_id.as_deref(),
            &operation,
            focus,
            observer.as_ref(),
        )
        .await?;
        let plan_id = started.plan_id.clone();
        let execution = PlanExecutionEnvironment::new(configuration.snapshot().await, &operation);
        tokio::spawn(async move {
            run::run_batch(&self, &execution, &plan_id, &operation, &options, observer).await;
        });
        Ok(started)
    }

    pub(crate) async fn launch_pipeline(
        self,
        configuration: &Configuration,
        focus: &FocusSession,
        ids: Vec<String>,
        model: Option<ModelSelection>,
        observer: Arc<dyn ApplicationObserver>,
    ) -> Result<PlanPipelineStarted, PlanLaunchError> {
        let operation = self
            .step
            .execution
            .operations
            .begin()
            .map_err(|_| PlanLaunchError::Busy)?;
        let root = self.step.context.repository_root().await;
        let documents = Documents::new(&root);
        let started = prepare_pipeline_launch(
            &documents,
            &root,
            &ids,
            &operation,
            focus,
            observer.as_ref(),
        )
        .await?;
        let plan_ids = started.plan_ids.clone();
        let execution = PlanExecutionEnvironment::new(configuration.snapshot().await, &operation);
        tokio::spawn(async move {
            queue::run_pipeline(&self, &execution, plan_ids, model, &operation, observer).await;
        });
        Ok(started)
    }
}

async fn prepare_batch_launch(
    documents: &Documents,
    root: &Path,
    plan_id: &str,
    step_id: Option<&str>,
    operation: &OperationGuard,
    focus: &FocusSession,
    observer: &dyn ApplicationObserver,
) -> Result<PlanBatchStarted, PlanLaunchError> {
    let (plan_id, step_ids) = prepare_scope(documents, plan_id, step_id).await?;
    let operation_id = verify_launch(root, operation, focus, observer, false).await?;
    Ok(PlanBatchStarted {
        plan_id,
        step_ids,
        operation_id,
    })
}

async fn prepare_pipeline_launch(
    documents: &Documents,
    root: &Path,
    ids: &[String],
    operation: &OperationGuard,
    focus: &FocusSession,
    observer: &dyn ApplicationObserver,
) -> Result<PlanPipelineStarted, PlanLaunchError> {
    let plan_ids = prepare_queue(documents, ids).await?;
    let operation_id = verify_launch(root, operation, focus, observer, true).await?;
    Ok(PlanPipelineStarted {
        plan_ids,
        operation_id,
    })
}

async fn verify_launch(
    root: &Path,
    operation: &OperationGuard,
    focus: &FocusSession,
    observer: &dyn ApplicationObserver,
    pipeline: bool,
) -> Result<String, PlanLaunchError> {
    match run_preflight_verification(root, operation.cancel_receiver()).await {
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
    focus.clear().await.publish(&id, observer);
    Ok(id)
}

pub(crate) async fn resolve_frame(
    documents: &Documents,
    plan_id: &str,
    step_id: Option<&str>,
) -> Result<ImplementationFrame, StepFailure> {
    let plan = documents
        .plan(plan_id)
        .await
        .map_err(StepFailure::infrastructure)?
        .ok_or_else(|| StepFailure::infrastructure(format!("Plan '{plan_id}' not found")))?;
    Ok(ImplementationFrame::from_plan(plan, step_id))
}

async fn prepare_scope(
    documents: &Documents,
    plan_id: &str,
    step_id: Option<&str>,
) -> Result<(String, Vec<String>), PlanExecutionError> {
    use PlanExecutionFailure as Failure;
    use PlanScopeEvaluation as Scope;
    let error = |kind, message| PlanExecutionError { kind, message };
    let plan = documents
        .plan(plan_id)
        .await
        .map_err(|message| error(Failure::PlanLoadFailed, message))?
        .ok_or_else(|| error(Failure::NotFound, format!("Plan '{plan_id}' not found")))?;
    let (kind, message) = match plan.implementation.evaluate_scope(step_id) {
        Scope::Ready(steps) => {
            return Ok((plan.id, steps.into_iter().map(|step| step.id).collect()));
        }
        Scope::AllCompleted if needs_projection(&plan) => return Ok((plan.id, Vec::new())),
        Scope::BlockedByDiscussion(items) => (Failure::StepInDiscussion, format!(
            "Scope contains item(s) in DISCUSSION status: {}. Review them (change status to TODO) first",
            items.join(", ")
        )),
        Scope::BlockedByBlockers(items) => (Failure::StepBlocked, format!(
            "Scope contains step(s) in BLOCKED status: {}. Resolve the reported blockers and replan before executing",
            items.join(", ")
        )),
        Scope::BlockedByDependencies(items) => (Failure::StepBlockedByDependencies, format!(
            "Scope contains step(s) whose prerequisites are not yet DONE: {}. Complete the prerequisites first",
            items.join(", ")
        )),
        Scope::StepBlocked => (Failure::StepBlocked, "Target step is BLOCKED; resolve the reported blocker and replan before executing".into()),
        Scope::AllCompleted => (Failure::NothingToExecute, "All steps in this scope are already completed".into()),
        Scope::Cancelled => (Failure::StepCancelled, "Step is CANCELLED and cannot be executed".into()),
        Scope::NotFound => (Failure::NotFound, "Target item not found in plan".into()),
    };
    Err(error(kind, message))
}

async fn prepare_queue(
    documents: &Documents,
    ids: &[String],
) -> Result<Vec<String>, PlanQueueError> {
    use PlanQueueFailure as Failure;
    use PlanScopeEvaluation as Scope;
    let error = |kind, message| PlanQueueError { kind, message };
    if ids.is_empty() {
        return Err(error(
            Failure::Empty,
            "Pipeline must reference at least one plan".into(),
        ));
    }
    let mut scheduled = Vec::new();
    for id in ids {
        let plan = documents
            .plan(id)
            .await
            .map_err(|message| error(Failure::Load, message))?
            .ok_or_else(|| error(Failure::NotFound, format!("Plan '{id}' not found")))?;
        match plan.implementation.evaluate_scope(None) {
            Scope::Ready(_) => {}
            Scope::AllCompleted if needs_projection(&plan) => {}
            Scope::AllCompleted => continue,
            other => {
                return Err(error(
                    Failure::Blocked,
                    format!("Plan '{}' cannot be executed: {other:?}", plan.id),
                ));
            }
        }
        scheduled.push(plan.id);
    }
    if scheduled.is_empty() {
        return Err(error(
            Failure::Completed,
            "Every plan in the pipeline is already completed".into(),
        ));
    }
    Ok(scheduled)
}

fn needs_projection(plan: &Plan) -> bool {
    plan.implementation.items.is_empty() && !plan.concept.is_empty()
}

#[cfg(test)]
pub(crate) use run::PlanLeaf;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApplicationEvent;
    use crate::application::operation::OperationRegistry;
    use std::sync::Arc;
    use tauqe_core::plan::{storage::PlanStorage, ConceptItem, PlanItem, PlanItemStatus};

    struct NoPublication;

    impl ApplicationObserver for NoPublication {
        fn publish(&self, _: &str, _: ApplicationEvent) {
            panic!("refused plan launch published an event");
        }
    }

    #[tokio::test]
    async fn missing_plan_is_refused_before_preflight_without_a_facade() {
        let dir = tempfile::tempdir().unwrap();
        let documents = Documents::new(dir.path());
        let registry = Arc::new(OperationRegistry::default());
        let operation = registry.begin().unwrap();
        // If preflight runs first it returns Cancelled, not a scope error.
        registry.hard_cancel();
        let result = prepare_batch_launch(
            &documents,
            dir.path(),
            "missing",
            None,
            &operation,
            &FocusSession::default(),
            &NoPublication,
        )
        .await;
        assert!(matches!(
            result,
            Err(PlanLaunchError::Scope(PlanExecutionError {
                kind: PlanExecutionFailure::NotFound,
                ..
            }))
        ));
        assert_eq!(registry.current_id(), Some(operation.id()));
    }

    #[tokio::test]
    async fn admission_keeps_subtree_order_and_projects_concept_only_plans() {
        let dir = tempfile::tempdir().unwrap();
        let documents = Documents::new(dir.path());
        let leaf = |id: &str, status| PlanItem {
            id: id.into(),
            title: id.into(),
            files: vec!["src/lib.rs".into()],
            status,
            ..PlanItem::default()
        };
        let store = |id: &str, concept, items| {
            PlanStorage::save_plan(
                dir.path(),
                &Plan::from_parts(id.into(), id.into(), None, 0, 0, concept, items),
            )
            .unwrap();
        };
        let mut dependent = leaf("2", PlanItemStatus::Todo);
        dependent.depends_on = vec!["1".into()];
        store(
            "ready",
            vec![],
            vec![PlanItem {
                children: vec![dependent, leaf("1", PlanItemStatus::Todo)],
                ..leaf("branch", PlanItemStatus::Todo)
            }],
        );
        store("done", vec![], vec![leaf("1", PlanItemStatus::Done)]);
        store(
            "concept",
            vec![ConceptItem {
                id: "c-1".into(),
                title: "Invariant".into(),
                details: None,
                children: vec![],
            }],
            vec![],
        );

        assert_eq!(
            prepare_scope(&documents, "ready", Some("branch"))
                .await
                .unwrap()
                .1,
            ["1", "2"]
        );
        assert!(matches!(
            prepare_scope(&documents, "ready", Some("2"))
                .await
                .unwrap_err()
                .kind,
            PlanExecutionFailure::StepBlockedByDependencies
        ));
        assert!(prepare_scope(&documents, "concept", None)
            .await
            .unwrap()
            .1
            .is_empty());
        assert!(matches!(
            prepare_scope(&documents, "concept", Some("1"))
                .await
                .unwrap_err()
                .kind,
            PlanExecutionFailure::NotFound
        ));
        assert_eq!(
            prepare_queue(
                &documents,
                &["done".into(), "concept".into(), "ready".into()]
            )
            .await
            .unwrap(),
            ["concept", "ready"]
        );
        assert!(matches!(
            prepare_queue(&documents, &["done".into()])
                .await
                .unwrap_err()
                .kind,
            PlanQueueFailure::Completed
        ));
    }
}
