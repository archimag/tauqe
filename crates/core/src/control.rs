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

pub mod context;
pub mod edit;
pub mod outcome;
pub mod prompt;
pub mod repl;
pub mod research;
pub mod staging;

pub use context::{CancellationToken, EventSink, LoopContext};
pub use edit::{run_edit_loop, EditArtifact, EditLoopConfig};
pub use outcome::{FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
pub use prompt::PromptBuilder;
pub use repl::{repl_capabilities, run_repl_loop, ReplArtifact, ReplLoopConfig};
pub use research::{
    handle_research_commands, run_research_loop, ResearchArtifact, ResearchFeedback,
    ResearchLoopConfig, RESEARCH_CAPABILITIES,
};
pub use staging::{run_staging_loop, StagingArtifact, StagingLoopConfig};
