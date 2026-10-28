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
mod group;
mod item;
mod session;

#[cfg(test)]
mod tests;

pub use dto::{
    ReviewAnalyzeParams, ReviewAnalyzeResult, ReviewCancelParams, ReviewDeleteParams,
    ReviewDeleteResult, ReviewExecuteParams, ReviewGetParams, ReviewGetResult, ReviewListParams,
    ReviewListResult, ReviewRefineGroupParams, ReviewRefineGroupResult, ReviewRefineItemParams,
    ReviewRefineItemResult, ReviewStartParams, ReviewUpdateItemParams, ReviewUpdateItemResult,
};
pub use events::{
    ReviewAnalyzeFinishedEvent, ReviewAnalyzeStartedEvent, ReviewContentDeltaEvent,
    ReviewErrorEvent, ReviewFinishedEvent, ReviewGroupFinishedEvent, ReviewGroupStartedEvent,
    ReviewListChangedEvent, ReviewReasoningDeltaEvent, ReviewStartedEvent, ReviewUpdatedEvent,
};
pub use group::DEFAULT_REVIEW_GROUP_ID;
pub use group::{
    ArchitecturalIssueGroup, PostAnalysisMode, ReviewGroup, ReviewLaunchConfig, ReviewPostAnalysis,
};
pub use group::{LOCAL_REVIEW_GROUP_ID, LOCAL_REVIEW_GROUP_TITLE};
pub use item::{IssueStatus, IssueType, ReviewIssue, ReviewItem, ReviewItemStatus, ReviewSeverity};
pub use session::{ReviewSession, ReviewSummary};
