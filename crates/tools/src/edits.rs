//! Virtual staging and the deterministic algorithms that resolve a change set.
//!
//! Nothing here touches the cognitive layers: the substrate works from edit
//! operations, resolved project commands, explicit context paths, and the pure path
//! predicates of [`crate::paths`]. A change set is applied to memory before any
//! byte reaches disk, a refused member restores exactly the paths it touched,
//! and the files an attempt staged leave through the declared project filter.
//!
//! What the substrate deliberately does not own: decoding model output, the
//! streaming filters that read it while it arrives, and the live reconciliation
//! of a validated change set with the context engine. Those belong to
//! `tauqe-core`, which builds on this crate.

mod admission;
pub mod anchor;
pub mod apply_patch;
mod change_set;
pub mod error;
mod format;
pub mod fuzzy;
mod line_endings;
pub mod operation;
mod protection;
pub mod staging;

pub use change_set::{
    stage_and_validate_edits_with, FailedFileEdit, StageEditsResult, StagedConfigValidator,
    StagedEditsState,
};
pub use error::EditError;
pub use format::prepare_edit_batches;
