//! Reports of isolated step and review sequence execution.

use crate::application::review_execution::PlanSequenceArtifact;
use tauqe_core::control::outcome::LoopResult;
use tauqe_core::control::turn::TurnArtifact;

/// Work accepted by the single-step endpoint.
pub enum StepLoopBody {
    Single(String),
}

/// Terminal report of a step or review horizon.
pub enum StepLoopReport {
    Turn(LoopResult<TurnArtifact>),
    Sequence(LoopResult<PlanSequenceArtifact>),
    Cancelled,
    Failed(String),
}
