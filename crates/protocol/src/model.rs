//! Model, role, effort, and turn-lifecycle wire contracts.
//!
//! The model subsystem is decomposed into focused submodules: provider and
//! model references (`reference`), semantic roles (`role`), reasoning effort
//! (`effort`), configured entries and coder tiers (`entry`), selection DTOs
//! (`selection`), turn parameters and lifecycle events (`turn`), usage
//! accounting (`usage`), and batch completion summaries (`batch`). Every public
//! item is re-exported here, so external paths such as
//! `tauqe_protocol::model::ModelRef` remain stable. Keeping the submodules
//! private avoids leaking their names through the crate-root glob re-exports.

mod batch;
mod effort;
mod entry;
mod reference;
mod role;
mod selection;
mod turn;
mod usage;

pub use batch::*;
pub use effort::*;
pub use entry::*;
pub use reference::*;
pub use role::*;
pub use selection::*;
pub use turn::*;
pub use usage::*;
