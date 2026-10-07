#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub use tauqe_protocol as protocol;

pub mod config;
pub mod context;
pub mod edits;
pub mod git;
pub mod history;
pub mod prompt;
pub mod providers;
pub mod repomap;
pub mod toolchain;
pub mod workflow;

pub fn init() {
    // Core initialization
}
