//! Hierarchical Functional Control Loops (`crates/core/src/control`).
//!
//! This module provides the functional foundation for Tauqe execution workflows
//! (`run_*_loop`) replacing monolithic FSM and scattered pipeline state machines.
//!
//! # Core Principles
//! - **Functional Decomposition:** Nested, plain `async` functions (`run_<domain>_loop`).
//! - **LoopContext:** Shared execution context carrying cancellation, event sinks,
//!   protocol healing limits, and dynamic capability sets.
//! - **Three-Tier Outcome & Envelope:** Strict distinction between infrastructure failures
//!   ([`SystemError`]), successful artifacts with the response envelope ([`LoopSuccess`]),
//!   valid alternative model responses ([`LoopResult::Diverged`]), and retry exhaustion
//!   ([`LoopResult::Exhausted`]).

pub mod cache;
pub mod context;
pub mod execution;
pub mod history;
pub mod outcome;
pub mod plan;
pub mod prompt;
pub mod refine;
pub mod repl;
pub mod step;
mod transcript;
pub mod turn;

pub use context::{CancellationToken, EventSink, LoopContext};
pub use history::compact_history_if_needed_with_budget;
pub use outcome::{FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
pub use plan::{PlanStepArtifact, PlanStepLoopConfig};
pub use prompt::PromptBuilder;
pub use repl::{repl_capabilities, ReplArtifact};
#[cfg(test)]
pub(crate) use repl::{run_repl_loop, ReplLoopConfig};
pub use turn::{run_turn_loop, ResearchLoopConfig, TurnArtifact, TurnLoopConfig};
