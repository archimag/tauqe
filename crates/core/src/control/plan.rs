//! Plan execution control loops.
//!
//! [`run_plan_step_loop`](step::run_plan_step_loop) drives one leaf of a plan through the coder
//! ladder, delegating the actual code and verification work to
//! [`run_turn_loop`](super::turn::run_turn_loop).

pub mod fixed;
pub mod sequence;
pub mod step;

pub use sequence::{PlanSequenceArtifact, PlanSequenceLoopConfig, PlanSequenceStop};
pub use step::{PlanStepArtifact, PlanStepLoopConfig};
