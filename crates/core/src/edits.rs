//! Virtual staging, change-set application, and the model-facing edit surface.
//!
//! The deterministic substrate of this subsystem — the opaque line-anchor codec,
//! fuzzy line matching, the virtual-staging state machine, the in-memory
//! formatting hand-off, trust boundaries, write admission, and their typed
//! rejections — lives in `tauqe_tools::edits`, beside the pure path predicates it
//! resolves paths with and the sibling commit policy it applies a change set
//! through.
//!
//! What stays here is the cognitive surface: the structured and XML decoders
//! bound to capability tags, the stream filters that read model output while it
//! arrives, the live reconciliation of a validated change set with
//! `ContextManager`, and the adapter that hands the substrate a context snapshot
//! together with the one configuration check only this crate can make.
//!
//! The re-exports below keep the paths the rest of the engine already addresses
//! while the transition lasts.

mod apply;
pub(crate) mod atomic;
pub mod protocol;
mod stage;
pub(crate) mod strategy;
pub mod stream;

pub use apply::{apply_edit_proposal, apply_edit_proposal_with_config};
pub use protocol::{
    ContextRequest, EditProtocolFactory, MarkedXmlEditProtocol, StructuredEditProtocol,
    TerminalOutcome, VerifyOnSuccess, VerifyRequest, VerifyTarget, XmlEditProtocol,
};
pub use stage::{prepare_staged_edits, prepare_staged_edits_with_config, stage_and_validate_edits};
pub use tauqe_tools::edits::anchor;
pub use tauqe_tools::edits::fuzzy;
pub use tauqe_tools::edits::staging;
pub use tauqe_tools::edits::{EditError, FailedFileEdit, StageEditsResult, StagedEditsState};

pub(crate) use tauqe_tools::edits::fuzzy::apply_replace;
pub(crate) use tauqe_tools::edits::staging::apply_anchor_edits;

#[cfg(test)]
mod tests;
