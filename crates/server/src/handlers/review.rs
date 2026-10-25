//! Review handler facade.
//!
//! The review subsystem is decomposed into focused submodules: shared helpers
//! (`common`), session CRUD (`storage_ops`), primary audit synthesis
//! (`synthesis`), Architect-driven finding decomposition (`refine`), per-finding
//! bounded-turn execution (`step`), and the batch supervisor (`batch`). Every RPC
//! handler keeps its public name and signature so the dispatcher stays unchanged,
//! and the lifecycle and concurrency invariants — parallel-mutation rejection,
//! cancellation handling, and the decomposition status gate that refuses a
//! finished, rejected, or untriaged finding — are preserved across the split.

mod batch;
mod common;
mod refine;
mod step;
mod storage_ops;
mod synthesis;

pub use batch::handle_review_execute;
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
