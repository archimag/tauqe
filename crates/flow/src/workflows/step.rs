//! Shared step execution: one technical attempt, bounded retries, and queue traversal.

pub(crate) mod attempts;
pub(crate) mod completion;
pub(crate) mod queue;
pub(crate) mod run;

use super::state::TurnState;
use super::turn::{run_technical_turn, TechnicalObservation, TechnicalTurnReport};
use crate::api::OperationGuard;
use std::path::PathBuf;
use tauqe_core::context::ContextManager;
use tauqe_core::execution::{EventSink, ReadyPromptExecutor};
use tauqe_core::llm::CapabilityTree;
use tauqe_core::prompt::TurnBrief;
use tauqe_core::workflow::policy::ApplyPolicy;

pub(crate) struct StepAttempt {
    pub root: PathBuf,
    pub task: String,
    pub context: ContextManager,
    pub capabilities: CapabilityTree,
    pub max_turn_rounds: usize,
    pub executor: ReadyPromptExecutor,
    pub events: EventSink,
    pub observation: Option<TechnicalObservation>,
}

impl StepAttempt {
    pub(crate) async fn run(
        self,
        operation: &OperationGuard,
        policy: Box<dyn ApplyPolicy>,
    ) -> Result<TechnicalTurnReport, String> {
        let Self {
            root,
            task,
            context,
            capabilities,
            max_turn_rounds,
            executor,
            events,
            observation,
        } = self;
        let state = TurnState::new(context, capabilities, None, Some(max_turn_rounds));
        run_technical_turn(
            operation,
            root,
            state,
            executor,
            policy,
            task,
            Some(TurnBrief::step()),
            events,
            observation,
        )
        .await
    }
}
