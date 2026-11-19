//! Capability classification model shared by every prompt protocol.
//!
//! A protocol advertises the interaction capabilities it supports; this module
//! declares the capabilities and how a turn regime groups them in the prompt.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Declarative set of interaction capabilities a prompt protocol may enable.
///
/// Capabilities decouple the concrete protocol syntax (XML markers, JSON schema)
/// from the core business logic. A protocol advertises the capabilities it
/// supports, and its decoder produces only the corresponding
/// [`crate::llm::LlmCommand`] variants. This keeps the execution
/// pipeline independent of any single wire format and makes protocols
/// interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmCapability {
    /// Free-form conversational Markdown text.
    TextMessage,
    /// Replace text or an anchored line range in an existing file.
    ///
    /// Projected through `PatchStrategy`: `EditReplace` under
    /// `SearchReplace`, or `EditLineAnchor` under `LineAnchor`.
    EditReplace,
    /// Create a new file.
    EditCreate,
    /// Delete an existing file.
    EditDelete,
    /// Move or rename an existing file.
    EditMove,
    /// Move or rename a directory subtree.
    EditMoveDir,
    /// Overwrite an existing file.
    EditOverwrite,
    /// Request to add a file to the working context.
    ContextRequest,
    /// Search repository files using Rust/ripgrep regex.
    RepoSearch,
    /// Request the compact repository directory tree.
    RepoDirs,
    /// Request a targeted symbol map of one subdirectory.
    RepoMap,
    /// Request a tracked-file listing of one directory.
    DirList,
    /// Requests for system documentation of this harness by topic.
    DocRequest,
    /// Requests for external project documentation by its logical identifier.
    ///
    /// The identifier names a document the project declared in
    /// `[[outer_docs]]`, never a cache path, so only declared material is
    /// reachable and the harness alone decides where a snapshot lives. The
    /// request belongs to the research phase of every contour that investigates
    /// the project domain, an isolated step included, because a step reads the
    /// contracts it was pointed at; the internal documentation of the harness
    /// itself stays closed to a step.
    OuterDocRequest,
    /// Creation of a new project plan.
    PlanCreation,
    /// Modification of an existing project plan.
    PlanUpdate,
    /// A decision recorded for one finding of a discussed review session.
    ///
    /// The decision belongs to the review discussion regime: it states the
    /// status the engineer settled on for one finding together with the
    /// reasoning behind it, and it travels exclusively through the structured
    /// response schema, because the capability owns no control tag. The harness
    /// applies it against the session the turn names and never against whichever
    /// session happens to be active.
    ReviewUpdate,
    /// Ephemeral working-memory notes for multi-round reasoning.
    Scratchpad,
    /// Terminal commit message stated by the model when a turn succeeds.
    CommitMessage,
    /// Terminal error stated by the model when a turn fails.
    TurnError,
    /// Terminal skip reason stated by the model when the work is already satisfied.
    TurnSkip,
    /// Explicit semantic blocker emitted when preconditions or assumptions are violated.
    Blocked,
    /// Explicit unconditional abort emitted when external host environment actions or developer intervention is required.
    Abort,
    CustomTools,
}

impl LlmCapability {
    /// Every capability a built-in protocol profile may advertise.
    ///
    /// The order matches the enum declaration so iteration, prompt
    /// composition, and tests stay deterministic.
    pub(crate) const ALL: &'static [LlmCapability] = &[
        Self::TextMessage,
        Self::EditReplace,
        Self::EditCreate,
        Self::EditDelete,
        Self::EditMove,
        Self::EditMoveDir,
        Self::EditOverwrite,
        Self::ContextRequest,
        Self::RepoSearch,
        Self::RepoDirs,
        Self::RepoMap,
        Self::DirList,
        Self::DocRequest,
        Self::OuterDocRequest,
        Self::PlanCreation,
        Self::PlanUpdate,
        Self::ReviewUpdate,
        Self::Scratchpad,
        Self::CommitMessage,
        Self::TurnError,
        Self::TurnSkip,
        Self::Blocked,
        Self::Abort,
        Self::CustomTools,
    ];

    /// Canonical lower-case identifier used on the wire.
    const fn as_str(self) -> &'static str {
        match self {
            Self::TextMessage => "text_message",
            Self::EditReplace => "edit_replace",
            Self::EditCreate => "edit_create",
            Self::EditDelete => "edit_delete",
            Self::EditMove => "edit_move",
            Self::EditMoveDir => "edit_move_dir",
            Self::EditOverwrite => "edit_overwrite",
            Self::ContextRequest => "context_request",
            Self::RepoSearch => "repo_search",
            Self::RepoDirs => "repo_dirs",
            Self::RepoMap => "repo_map",
            Self::DirList => "dir_list",
            Self::DocRequest => "doc_request",
            Self::OuterDocRequest => "outer_doc_request",
            Self::PlanCreation => "plan_creation",
            Self::PlanUpdate => "plan_update",
            Self::ReviewUpdate => "review_update",
            Self::Scratchpad => "scratchpad",
            Self::CommitMessage => "commit_message",
            Self::TurnError => "turn_error",
            Self::TurnSkip => "turn_skip",
            Self::Blocked => "blocked",
            Self::Abort => "abort",
            Self::CustomTools => "custom_tools",
        }
    }
}

impl fmt::Display for LlmCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_llm_capability_serde_roundtrip() {
        let cases = [
            (LlmCapability::TextMessage, "\"text_message\""),
            (LlmCapability::EditReplace, "\"edit_replace\""),
            (LlmCapability::EditCreate, "\"edit_create\""),
            (LlmCapability::EditDelete, "\"edit_delete\""),
            (LlmCapability::EditMove, "\"edit_move\""),
            (LlmCapability::EditMoveDir, "\"edit_move_dir\""),
            (LlmCapability::EditOverwrite, "\"edit_overwrite\""),
            (LlmCapability::ContextRequest, "\"context_request\""),
            (LlmCapability::RepoSearch, "\"repo_search\""),
            (LlmCapability::RepoDirs, "\"repo_dirs\""),
            (LlmCapability::RepoMap, "\"repo_map\""),
            (LlmCapability::DirList, "\"dir_list\""),
            (LlmCapability::DocRequest, "\"doc_request\""),
            (LlmCapability::OuterDocRequest, "\"outer_doc_request\""),
            (LlmCapability::PlanCreation, "\"plan_creation\""),
            (LlmCapability::PlanUpdate, "\"plan_update\""),
            (LlmCapability::ReviewUpdate, "\"review_update\""),
            (LlmCapability::Scratchpad, "\"scratchpad\""),
            (LlmCapability::CommitMessage, "\"commit_message\""),
            (LlmCapability::TurnError, "\"turn_error\""),
            (LlmCapability::TurnSkip, "\"turn_skip\""),
            (LlmCapability::Blocked, "\"blocked\""),
            (LlmCapability::Abort, "\"abort\""),
            (LlmCapability::CustomTools, "\"custom_tools\""),
        ];

        for (capability, expected) in cases {
            assert_eq!(serde_json::to_string(&capability).unwrap(), expected);
            let decoded: LlmCapability = serde_json::from_str(expected).unwrap();
            assert_eq!(decoded, capability);
        }
    }
}
