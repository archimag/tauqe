//! Plan execution control loops.
//!
//! [`run_plan_step_loop`] drives one leaf of a plan through the coder
//! ladder, delegating the actual code and verification work to
//! [`run_turn_loop`](super::turn::run_turn_loop).

pub mod fixed;
pub mod sequence;
pub mod step;

pub use fixed::{run_fixed_plan_loop, FixedPlanArtifact, FixedPlanLoopConfig};
pub use sequence::{
    run_plan_sequence_loop, PlanSequenceArtifact, PlanSequenceLoopConfig, PlanSequenceStop,
};
pub use step::{run_plan_step_loop, PlanStepArtifact, PlanStepLoopConfig};
