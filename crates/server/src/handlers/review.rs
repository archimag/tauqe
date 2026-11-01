//! Review handler facade.
//!
//! The review subsystem is decomposed into focused submodules: shared helpers
//! (`common`), session CRUD (`storage_ops`), the deterministic audit-window
//! projection every launch consumes (`window`), primary audit synthesis
//! (`synthesis`), cross-group post-analysis (`analysis`), the grouped preset
//! launch (`groups`), the read-only projection a client resolves consent against
//! (`preview`), and the batch supervisor (`batch`). Every RPC handler keeps its
//! public name and signature so the dispatcher stays unchanged, and the
//! lifecycle and concurrency invariants — parallel-mutation rejection,
//! cancellation handling, and the status gate that refuses a finished, rejected,
//! or untriaged finding — are preserved across the split.
//!
//! Review execution itself is not implemented here. Every step turn, every
//! Architect wave, every ladder rung, and every durable finding transition
//! belongs to the engine's unified implementation pipeline
//! (`tauqe_core::workflow::implementation`), which the batch supervisor drives
//! through `ServerImplementationHost`. A review run and a plan run therefore
//! share one implementation instead of keeping two parallel runtimes.

mod analysis;
mod batch;
mod common;
mod groups;
mod preview;
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

// A dialogue anchored on a review session publishes the decision it recorded
// through the same path every other review mutation uses.
pub(crate) use common::publish_review;

// The integration tests import the batch supervisor and the shared context
// helpers directly from their submodules, so no test-only re-exports are
// needed here.
#[cfg(test)]
mod tests;
