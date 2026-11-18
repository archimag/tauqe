//! Owned edit vocabulary, model-output proposals, and application outcomes.
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
pub mod operation;
mod proposal;
pub use operation::{
    normalize_repo_path, EditCreate, EditDelete, EditLineAnchor, EditMove, EditMoveDir,
    EditOperation, EditOverwrite, EditReplace,
};
pub use proposal::{ChangeConversionError, EditProposal, StructuredChangeProposal};
fn deserialize_null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum EditFileOp {
    Replace,
    Create,
    Delete,
    Move,
    /// Moves a directory subtree rather than one file.
    ///
    /// A directory is never a file of the context block, so the operation names
    /// the source and the destination directory, and no replaced lines, range
    /// bounds, or payload belong to it. The staging engine expands it into the
    /// per-file moves of the subtree before any byte reaches disk.
    MoveDir,
    Overwrite,
}

impl std::convert::TryFrom<String> for EditFileOp {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl EditFileOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Create => "create",
            Self::Delete => "delete",
            Self::Move => "move",
            Self::MoveDir => "move_dir",
            Self::Overwrite => "overwrite",
        }
    }
}

impl fmt::Display for EditFileOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for EditFileOp {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "replace" => Ok(Self::Replace),
            "create" => Ok(Self::Create),
            "delete" => Ok(Self::Delete),
            "move" => Ok(Self::Move),
            "move_dir" => Ok(Self::MoveDir),
            "overwrite" => Ok(Self::Overwrite),
            other => Err(format!(
                "Unknown change operation '{}'. Expected replace, create, delete, move, move_dir, or overwrite",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum EditFileStatus {
    Ok,
    Error,
}

impl std::convert::TryFrom<String> for EditFileStatus {
    type Error = String;

    fn try_from(value: String) -> Result<Self, String> {
        value.parse()
    }
}

impl EditFileStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for EditFileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for EditFileStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ok" => Ok(Self::Ok),
            "error" => Ok(Self::Error),
            other => Err(format!(
                "Unknown edit file status '{}'. Expected ok or error",
                other
            )),
        }
    }
}

/// Result of applying an edit proposal.
///
/// Captures the applied state, diagnostics, changed files, commit hash, and
/// verification verdict consumed by the turn result and server publication.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EditOutcome {
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
    /// Explicit verdict of the verification run that guarded this change set.
    ///
    /// Absent when the change set never reached verification — a rejected
    /// staging attempt, an aborted turn, or a failure to apply — so a client
    /// distinguishes "not verified" from a project that declares no gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationVerdict>,
}

/// Explicit verdict of the project verification run that guarded a change set.
///
/// A project declares its verification lifecycle itself, so a repository that
/// declares none is a legitimate project state rather than a failed gate. The
/// verdict therefore mirrors the engine status instead of a boolean: a passed
/// run, a failed run, an absent declaration, and an interrupted run are four
/// different facts about a change set, and a client renders a commit that was
/// never verified as unverified instead of inventing a success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationVerdict {
    /// The project declares a verification lifecycle and its command passed.
    Passed,
    /// The project declares a verification lifecycle and its command failed.
    Failed,
    /// The project declares no verification lifecycle, so nothing ran.
    NotConfigured,
    /// The verification run was interrupted before it finished.
    Cancelled,
}
