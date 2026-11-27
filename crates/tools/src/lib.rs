//! Deterministic substrate of the TAUQE harness.
//!
//! The crate owns the machinery the harness executes without model judgement:
//! the Tree-sitter repository map, the toolchain verification runners and task
//! runner integration, Git transactions, virtual staging of edit proposals, and
//! atomic file writes, and shared path-pattern utilities.
//!
//! Inputs, results, and shared path-pattern semantics belong to this crate.
//! Neither core nor the RPC protocol is a dependency.

pub mod atomic_write;
pub mod blocking;
pub mod edits;
pub mod git;
pub mod mockups;
pub mod path_patterns;
pub mod paths;
pub(crate) mod process;
pub mod repomap;
pub mod stop;
pub mod tokens;
pub mod toolchain;
