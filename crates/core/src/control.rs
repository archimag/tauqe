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
pub mod outcome;
pub mod plan;
pub mod prompt;
pub mod refine;
pub mod repl;
pub mod turn;

pub use context::{CancellationToken, EventSink, LoopContext};
pub use outcome::{FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
pub use plan::{run_plan_step_loop, PlanStepArtifact, PlanStepLoopConfig};
pub use prompt::PromptBuilder;
pub use repl::{repl_capabilities, run_repl_loop, ReplArtifact, ReplLoopConfig};
pub use turn::{
    handle_research_commands, run_edit_loop, run_research_loop, run_staging_loop, run_turn_loop,
    EditArtifact, EditLoopConfig, ResearchArtifact, ResearchFeedback, ResearchLoopConfig,
    StagingArtifact, StagingLoopConfig, TurnArtifact, TurnLoopConfig, EDIT_CAPABILITIES,
    RESEARCH_CAPABILITIES,
};
