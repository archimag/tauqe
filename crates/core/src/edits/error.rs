use thiserror::Error;

/// Every rejection a staged change set can produce.
///
/// A read-only context file is deliberately not among them. The harness
/// auto-promotes such a file to editable once a change set that touches it is
/// applied, so auto-promotion is the single documented enforcement path and the
/// enum never advertises a rejection the engine cannot produce.
///
/// A path declared in `[project.boundaries.protected]` is the opposite case:
/// the declaration exists precisely to keep a turn away from that path, so it
/// can never be promoted and every operation that would touch it is rejected as
/// [`EditError::ProtectedPath`] during staging.
#[derive(Debug, Error)]
pub enum EditError {
    #[error("File '{0}' is not in context. Only files explicitly in context can be edited.")]
    NotInContext(String),

    #[error("File '{0}' does not exist on disk.")]
    FileNotFound(String),

    #[error("Target path '{0}' already exists.")]
    DestinationExists(String),

    #[error("Target path '{0}' is invalid: {1}")]
    InvalidPath(String, String),

    #[error("In file '{path}': old_text not found (0 matches). Code might be stale or incorrect.")]
    NoMatch { path: String },

    #[error("In file '{path}': old_text matches {count} times. Replacement must be uniquely identifiable (exactly 1 match).")]
    AmbiguousMatch { path: String, count: usize },

    #[error("IO error for '{0}': {1}")]
    Io(String, #[source] std::io::Error),

    #[error("In file '{path}': invalid project configuration: {message}")]
    InvalidConfig { path: String, message: String },

    #[error(
        "Path '{0}' is excluded by the repository ignore rules (.gitignore or .git/info/exclude). Choose a path the repository tracks."
    )]
    IgnoredByGit(String),

    #[error(
        "Path '{0}' is inside the mockups sandbox but the repository ignore rules do not exclude it. The sandbox is admitted only while Git ignores it: a mockup Git would track joins the change set and aborts the Git transaction."
    )]
    MockupsNotIgnored(String),

    #[error(
        "Path '{0}' is declared in [project.boundaries.protected] and must never be created, modified, moved, or deleted by a turn: it stays tracked by Git and readable. Request it as a read-only reference instead."
    )]
    ProtectedPath(String),

    #[error("In file '{path}': {source}")]
    InvalidAnchor {
        path: String,
        #[source]
        source: crate::edits::anchor::AnchorError,
    },

    #[error(
        "In file '{path}': anchor '{anchor}' names no line of the file, which has {lines} line(s). Copy an anchor of this file from the <context> block."
    )]
    AnchorOutOfRange {
        path: String,
        anchor: String,
        lines: usize,
    },

    #[error(
        "In file '{path}': the range '{start}'..'{end}' runs backwards. The start anchor must name a line at or above the end anchor."
    )]
    ReversedAnchorRange {
        path: String,
        start: String,
        end: String,
    },

    #[error(
        "In file '{path}': the range '{start}'..'{end}' overlaps the range '{other_start}'..'{other_end}'. A change set must address disjoint line ranges."
    )]
    OverlappingAnchorRanges {
        path: String,
        start: String,
        end: String,
        other_start: String,
        other_end: String,
    },

    #[error("In file '{path}': a 'replace' operation addressed no replacement range")]
    ReplaceWithoutRange { path: String },
}
