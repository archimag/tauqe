//! Shared technical execution units (`crates/core/src/control`).
//!
//! Flow scenarios select work, capabilities, context, optional history, and domain
//! outcomes. This module assembles coherent model requests and executes bounded
//! turns, structured requests, and structured research without scheduling plan
//! steps or persisting review decisions.
//!
//! An [`execution::ExecutionUnit`] retains research, transcript, cache, and round
//! budget for continuations of one turn; a new unit isolates the next step.
//! [`structured::StructuredContract`] binds a read-only request's schema,
//! decoder, visibility, and completion rule. [`research_unit::StructuredResearchUnit`]
//! retains the journal and cache across Architect research rounds. Outcomes keep
//! infrastructure failures ([`SystemError`]) separate from success
//! ([`LoopSuccess`]), valid divergence ([`LoopResult::Diverged`]), and exhaustion
//! ([`LoopResult::Exhausted`]).

pub mod cache;
pub mod context;
pub mod execution;
#[cfg(test)]
mod history;
pub mod outcome;
pub mod prompt;
pub mod refine;
pub mod repl;
pub(crate) mod request;
pub mod research_unit;
pub mod step;
pub mod structured;
mod transcript;
pub mod turn;

pub use context::{CancellationToken, EventSink, LoopContext};
pub use outcome::{FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
pub use prompt::PromptBuilder;
pub use repl::{repl_capabilities, ReplArtifact};
pub use turn::{run_turn_loop, ResearchLoopConfig, TurnArtifact, TurnLoopConfig};
