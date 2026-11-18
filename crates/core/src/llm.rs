//! Internal model interaction contracts, separate from the TUI RPC contract.

mod capability;

pub use capability::CapabilityTree;

mod classification;
mod kind;
pub use classification::command_capability;
pub use kind::LlmCapability;

pub mod command;
pub use command::{
    AbortCommand, BlockedCommand, CommitMessageCommand, ContextRequestCommand, CustomToolCommand,
    DirListCommand, DocRequestCommand, LlmCommand, OuterDocRequestCommand, RepoDirsCommand,
    RepoMapCommand, RepoSearchCommand, ScratchpadCommand, TextCommand, TurnErrorCommand,
    TurnSkipCommand,
};

mod result;
pub use result::ModelResult;

pub mod discussion;
mod topic;
pub use topic::DocTopic;
mod proposal;
pub use proposal::{
    ModelResultProposal, StructuredAbortProposal, StructuredBlockedProposal,
    StructuredOutcomeProposal,
};
