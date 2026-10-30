mod admission;
pub mod anchor;
mod apply;
mod atomic;
mod change_set;
mod error;
mod format;
mod fuzzy;
mod protection;
pub mod protocol;
mod staging;
pub(crate) mod strategy;
pub mod stream;

pub use apply::{apply_edit_proposal, apply_edit_proposal_with_config};
pub use change_set::{
    stage_and_validate_edits, FailedFileEdit, StageEditsResult, StagedEditsState,
};
pub use error::EditError;
pub use format::{prepare_staged_edits, prepare_staged_edits_with_config};
pub use protocol::{EditProtocolFactory, XmlEditProtocol};
pub use stream::{EditStreamFilter, JsonStreamFilter, XmlStreamFilter};

pub(crate) use fuzzy::apply_replace;
pub(crate) use staging::apply_anchor_edits;

#[cfg(test)]
mod tests;
