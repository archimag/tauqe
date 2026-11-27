#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod blocking;
pub use tauqe_tools::atomic_write::write_atomic;
pub(crate) mod capabilities;
pub mod config;
pub mod context;
pub mod discussion;
pub(crate) mod docs;
pub mod edits;
pub mod escape;
pub mod execution;
pub mod git;
pub mod history;
pub mod llm;
pub mod locale;
pub mod mockups;
pub mod models;
pub mod outer_docs;
pub mod plan;
mod plan_item;
pub mod prompt;
pub mod providers;
// The Tree-sitter repository map is deterministic substrate and now lives in
// `tauqe-tools`, so `crate::repomap` resolves through this re-export while the
// transition lasts.
pub use tauqe_tools::repomap;
pub mod review;
pub use tauqe_tools::stop;
// Deterministic scratch paths shared by the unit tests of this crate.
#[cfg(test)]
pub(crate) mod test_paths;
pub mod toolchain;
pub mod workflow;
