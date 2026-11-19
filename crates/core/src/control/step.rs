//! Execution of one isolated step turn.

use crate::control::context::LoopContext;
use crate::control::outcome::{FailureReport, LoopResult, SystemError};
use crate::control::turn::{run_turn_loop, TurnArtifact, TurnCompletion, TurnLoopConfig};

/// Runs one prepared task using a fresh journal, budget, and transcript.
pub async fn run_step_execution(
    context: &mut crate::context::ContextManager,
    policy: &mut dyn crate::workflow::policy::ApplyPolicy,
    cancel: &super::CancellationToken,
    events: &super::EventSink,
    execution: super::execution::TurnExecutionConfig<'_>,
    prompt: String,
) -> Result<LoopResult<TurnArtifact>, SystemError> {
    let config = TurnLoopConfig::from_app_config(execution.config);
    let mut inputs = super::execution::TurnInputs::default();
    let mut ctx = inputs.prepare_step(execution, Some(prompt), context, policy, cancel, events);
    run_step_turn(&mut ctx, &config).await
}

/// A step reports an exhausted turn to the workflow that owns its retry policy.
pub(crate) async fn run_step_turn(
    ctx: &mut LoopContext<'_>,
    config: &TurnLoopConfig,
) -> Result<LoopResult<TurnArtifact>, SystemError> {
    match run_turn_loop(ctx, None, config, TurnCompletion::ActionRequired).await {
        Err(SystemError::RoundBudgetExhausted { spent, .. }) => Ok(LoopResult::Exhausted(
            FailureReport::new("Turn round budget exhausted", spent, Vec::new()),
        )),
        outcome => outcome,
    }
}
