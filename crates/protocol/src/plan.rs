//! Project plan domain model exposed behind a single facade.
//!
//! The plan subsystem is decomposed into focused submodules: the graph leaf
//! types (`item`), the plan aggregate with deterministic status computation
//! (`node`), scope scheduling (`scope`), Markdown rendering (`render`), the
//! shared plan-discussion prompt helpers (`prompt`), and the wire DTOs (`dto`).
//! Every public item is re-exported here, so external paths such as
//! `tauqe_protocol::plan::Plan` remain stable. Keeping the submodules private
//! avoids leaking their names through the crate-root glob re-exports.

mod dto;
mod item;
mod node;
mod prompt;
mod render;
mod scope;

#[cfg(test)]
mod tests;

pub use dto::*;
pub use item::*;
pub use node::*;
pub use prompt::*;
pub use scope::*;
