//! Code review domain model exposed behind a single facade.
//!
//! The review subsystem is decomposed into focused submodules: the finding
//! leaf types (`item`), the session aggregate (`session`), the wire DTOs
//! (`dto`), and the streaming events (`events`). Every public item is
//! re-exported here through an explicit list, so external paths such as
//! `tauqe_protocol::review::ReviewIssue` remain stable and an accidental name
//! collision between two submodules becomes a compile error instead of a
//! silently ambiguous glob. Keeping the submodules private avoids leaking
//! their names onto the crate root.

mod dto;
mod events;
mod item;
mod session;

#[cfg(test)]
mod tests;

pub use dto::{
    ReviewCancelParams, ReviewDeleteParams, ReviewDeleteResult, ReviewExecuteParams,
    ReviewGetParams, ReviewGetResult, ReviewListParams, ReviewListResult, ReviewRefineItemParams,
    ReviewRefineItemResult, ReviewStartParams, ReviewUpdateItemParams, ReviewUpdateItemResult,
};
pub use events::{
    ReviewContentDeltaEvent, ReviewErrorEvent, ReviewFinishedEvent, ReviewListChangedEvent,
    ReviewReasoningDeltaEvent, ReviewStartedEvent, ReviewUpdatedEvent,
};
pub use item::{
    IssueStatus, IssueType, ReviewIssue, ReviewItem, ReviewItemStatus, ReviewSeverity,
};
pub use session::{ReviewSession, ReviewSummary};
