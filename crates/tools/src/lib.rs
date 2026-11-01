//! Deterministic substrate of the TAUQE harness.
//!
//! The crate owns the machinery the harness executes without model judgement:
//! the Tree-sitter repository map, the toolchain verification runners and task
//! runner integration, Git transactions, virtual staging of edit proposals, and
//! atomic file writes.
//!
//! Dependencies flow one way only: `tauqe-tools` reads the shared wire contract
//! from `tauqe-protocol` and never reaches into `tauqe-core`, which builds its
//! cognitive layers on top of this substrate.

pub mod atomic_write;
pub mod blocking;
pub mod edits;
pub mod git;
pub mod mockups;
pub mod paths;
pub mod repomap;
pub mod tokens;
pub mod toolchain;
