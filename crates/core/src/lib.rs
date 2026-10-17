#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub use tauqe_protocol as protocol;

pub(crate) mod blocking;
pub mod capabilities;
pub mod config;
pub mod context;
pub(crate) mod docs;
pub mod edits;
pub(crate) mod escape;
pub mod git;
pub mod history;
pub mod locale;
pub mod plan;
pub mod prompt;
pub mod providers;
pub mod repomap;
pub mod review;
pub mod toolchain;
pub mod workflow;

pub fn init() {
    // Core initialization
}
