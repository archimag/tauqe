//! Reference project driver for Rust code bases.
//!
//! TAUQE keeps every language-specific decision outside the engine: a project
//! driver reads `tauqe-project.toml`, formats source text in memory, runs disk
//! post-hooks, and executes named verification suites. This crate is the
//! reference implementation of that contract for Rust repositories, built and
//! tested on its own so the wire contract is dogfooded instead of assumed.
//!
//! The crate links only against the wire contract and has no engine dependency,
//! so a driver defect can never reach into the harness. Diagnostic parsers live
//! in [`diagnostics`]: they turn raw toolchain output into typed
//! [`Diagnostic`](tauqe_protocol::Diagnostic) values, so a verification-healing
//! round reads a file, a line, and a message instead of a compiler transcript.
//! Phase execution lives in [`runner`]: it reads or synthesizes the project
//! manifest and drives in-memory formatting, disk post-hooks, and named
//! verification suites.

#![cfg_attr(
    not(test),
    warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod diagnostics;
pub mod runner;
