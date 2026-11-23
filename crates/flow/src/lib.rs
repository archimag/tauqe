//! Application sessions and scenarios for TAUQE.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod api;
mod application;
mod workflows;
