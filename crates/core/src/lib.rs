#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub use tauqe_protocol as protocol;

pub mod blocking;
pub mod capabilities;
pub mod config;
pub mod context;
pub(crate) mod docs;
pub mod edits;
pub(crate) mod escape;
pub mod git;
pub mod history;
pub mod locale;
pub mod mockups;
pub mod plan;
pub mod prompt;
pub mod providers;
pub mod refs;
// The Tree-sitter repository map is deterministic substrate and now lives in
// `tauqe-tools`, so `crate::repomap` resolves through this re-export while the
// transition lasts.
pub use tauqe_tools::repomap;
pub mod review;
pub mod toolchain;
pub mod workflow;

pub fn init() {
    // Core initialization
}
