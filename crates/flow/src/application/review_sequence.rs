//! Ordered review horizon execution with one isolated unit per leaf.

use crate::api::{Application, OperationGuard};
use crate::application::infrastructure::execution::PlanExecutionEnvironment;
use crate::application::review_execution::{
    PlanSequenceArtifact, PlanSequenceStop, StepPersistence,
};
use crate::application::review_step;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::context::ContextManager;
use tauqe_core::control::context::{CancellationToken, EventSink};
use tauqe_core::control::outcome::{LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use tauqe_core::models::ModelSelection;

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    app: &Application,
    operation: &OperationGuard,
    environment: &PlanExecutionEnvironment,
    root: &Path,
    context: &mut ContextManager,
    persistence: Box<dyn StepPersistence>,
    steps: &[String],
    selection: Option<ModelSelection>,
    session_id: Option<String>,
    marker: Option<&str>,
    cancel: &CancellationToken,
    sink: &EventSink,
) -> Result<LoopResult<PlanSequenceArtifact>, SystemError> {
    let persistence: Arc<dyn StepPersistence> = persistence.into();
    let mut executed_steps = Vec::with_capacity(steps.len());
    for step in steps {
        if *operation.cancel_receiver().borrow() {
            return Err(SystemError::Cancelled);
        }
        if *operation.soft_stop_receiver().borrow() {
            return Ok(LoopResult::Success(LoopSuccess::new(
                PlanSequenceArtifact {
                    executed_steps,
                    stop: PlanSequenceStop::Stopped,
                },
                ResponseEnvelope::default(),
            )));
        }
        match review_step::run(
            app,
            operation,
            environment,
            root,
            context,
            &persistence,
            step,
            selection.clone(),
            session_id.clone(),
            marker,
            cancel,
            sink,
        )
        .await?
        {
            LoopResult::Success(success) => executed_steps.push(success.value),
            LoopResult::Diverged(response) => return Ok(LoopResult::Diverged(response)),
            LoopResult::Exhausted(report) => return Ok(LoopResult::Exhausted(report)),
        }
    }
    Ok(LoopResult::Success(LoopSuccess::new(
        PlanSequenceArtifact {
            executed_steps,
            stop: PlanSequenceStop::Completed,
        },
        ResponseEnvelope::default(),
    )))
}
