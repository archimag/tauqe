//! Review handler facade.
//!
//! The review subsystem is decomposed into focused submodules: shared helpers
//! (`common`), session CRUD (`storage_ops`), the deterministic audit-window
//! projection every launch consumes (`window`), primary audit synthesis
//! (`synthesis`), cross-group post-analysis (`analysis`), Architect-driven
//! group root-cause decomposition (`refine`), per-finding bounded-turn execution
//! (`step`), unified-plan execution of an architectural review group
//! (`group_exec`), and the batch supervisor (`batch`). Every RPC handler keeps
//! its public name and signature so the dispatcher stays unchanged,
//! and the lifecycle and concurrency invariants — parallel-mutation rejection,
//! cancellation handling, and the decomposition status gate that refuses a
//! finished, rejected, or untriaged finding — are preserved across the split.

mod analysis;
mod batch;
mod common;
// The executor of an architectural group's unified plan: just-in-time Architect
// synthesis, the ordered walk over the group horizon, and the group-wide finding
// lifecycle it publishes.
#[allow(dead_code)]
mod group_exec;
// The grouped preset launch audits one group per batch member, so the supervisor
// is wired directly into `review/start` and shares the audit-window projection
// with the plain launch instead of carrying its own expansion.
mod groups;
// The read-only projection a client resolves consent against before launching.
mod preview;
// The Architect decomposition of an architectural group into its unified
// remediation subplan, shared by the just-in-time synthesis and the replanning
// wave.
mod refine;
mod step;
mod storage_ops;
mod synthesis;
// The deterministic projection of the audit window a launch reads.
mod window;

pub use analysis::handle_review_analyze;
pub use batch::handle_review_execute;
pub use preview::handle_review_preview;
pub use storage_ops::{
    handle_review_delete, handle_review_get, handle_review_list, handle_review_update_item,
};
pub use synthesis::{handle_review_cancel, handle_review_start};

// The integration tests import the batch supervisor and the shared context
// helpers directly from their submodules, so no test-only re-exports are
// needed here.
#[cfg(test)]
mod tests;
