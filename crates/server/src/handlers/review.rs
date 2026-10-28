//! Review handler facade.
//!
//! The review subsystem is decomposed into focused submodules: shared helpers
//! (`common`), session CRUD (`storage_ops`), primary audit synthesis
//! (`synthesis`), cross-group post-analysis (`analysis`), Architect-driven
//! finding decomposition (`refine`), per-finding
//! bounded-turn execution (`step`), and the batch supervisor (`batch`). Every RPC
//! handler keeps its public name and signature so the dispatcher stays unchanged,
//! and the lifecycle and concurrency invariants — parallel-mutation rejection,
//! cancellation handling, and the decomposition status gate that refuses a
//! finished, rejected, or untriaged finding — are preserved across the split.

mod analysis;
mod batch;
mod common;
// The grouped launch flow wires the supervisor into `review/start` in the
// following review step; until then it is exercised by its own tests, so the
// dead-code lint is silenced here instead of by a stub caller.
#[allow(dead_code)]
mod groups;
mod refine;
mod step;
mod storage_ops;
mod synthesis;

pub use analysis::handle_review_analyze;
pub use batch::handle_review_execute;
// Exposed for the grouped launch flow of the following review step; the
// supervisor tests are the only consumer of the re-export until then.
#[allow(unused_imports)]
pub(crate) use groups::run_group_audits;
pub use refine::handle_review_refine_item;
pub use storage_ops::{
    handle_review_delete, handle_review_get, handle_review_list, handle_review_update_item,
};
pub use synthesis::{handle_review_cancel, handle_review_start};

// The integration tests import the batch supervisor and the shared context
// helpers directly from their submodules, so no test-only re-exports are
// needed here.
#[cfg(test)]
mod tests;
