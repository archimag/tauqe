use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

pub mod proposal;

pub use proposal::{
    normalize_repo_path, ChangeConversionError, EditOperation, EditProposal,
    StructuredChangeProposal,
};

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
    pub fn as_str(&self) -> &'static str {
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

// Structured Edit Events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditStartedEvent {
    pub operation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFileStartedEvent {
    pub operation_id: String,
    pub path: String,
    pub op_type: EditFileOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditHunkEvent {
    pub operation_id: String,
    pub path: String,
    pub hunk_index: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFileDoneEvent {
    pub operation_id: String,
    pub path: String,
    pub status: EditFileStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub hunks_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFileRetryingEvent {
    pub operation_id: String,
    pub path: String,
    pub attempt: usize,
    pub max_retries: usize,
    pub reason: String,
}

/// Result of applying an edit proposal.
///
/// Shared by the turn result ([`crate::ModelResult::Edit`]) and the streamed
/// completion event ([`EditFinishedEvent`]) so the applied state, diagnostics,
/// changed files, commit hash, and verification verdict have a single
/// authoritative definition instead of being duplicated field-by-field across
/// two wire types.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFinishedEvent {
    pub operation_id: String,
    #[serde(flatten)]
    pub outcome: EditOutcome,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edit_file_op_serde_and_parsing() {
        let op = EditFileOp::Replace;
        assert_eq!(serde_json::to_string(&op).unwrap(), "\"replace\"");
        assert_eq!(
            serde_json::from_str::<EditFileOp>("\"create\"").unwrap(),
            EditFileOp::Create
        );
        assert_eq!(
            serde_json::from_str::<EditFileOp>("\"  OVERWRITE \\n\"").unwrap(),
            EditFileOp::Overwrite
        );
        assert_eq!(
            serde_json::from_str::<EditFileOp>("\"REPLACE\"").unwrap(),
            EditFileOp::Replace
        );
        assert_eq!(
            "  OVERWRITE \n".parse::<EditFileOp>().unwrap(),
            EditFileOp::Overwrite
        );
        assert!("unknown".parse::<EditFileOp>().is_err());
        assert_eq!(
            serde_json::from_str::<EditFileOp>("\"move_dir\"").unwrap(),
            EditFileOp::MoveDir
        );
        assert_eq!(
            "  MOVE_DIR \n".parse::<EditFileOp>().unwrap(),
            EditFileOp::MoveDir
        );
        assert_eq!(EditFileOp::Move.as_str(), "move");
        assert_eq!(EditFileOp::MoveDir.as_str(), "move_dir");
        assert_eq!(EditFileOp::Delete.to_string(), "delete");
    }

    #[test]
    fn test_edit_file_status_serde_and_parsing() {
        let status = EditFileStatus::Ok;
        assert_eq!(serde_json::to_string(&status).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::from_str::<EditFileStatus>("\"error\"").unwrap(),
            EditFileStatus::Error
        );
        assert_eq!(
            serde_json::from_str::<EditFileStatus>("\" OK \"").unwrap(),
            EditFileStatus::Ok
        );
        assert_eq!(
            serde_json::from_str::<EditFileStatus>("\"ERROR\"").unwrap(),
            EditFileStatus::Error
        );
        assert_eq!(
            " OK ".parse::<EditFileStatus>().unwrap(),
            EditFileStatus::Ok
        );
        assert!("unknown".parse::<EditFileStatus>().is_err());
        assert_eq!(EditFileStatus::Error.as_str(), "error");
        assert_eq!(EditFileStatus::Ok.to_string(), "ok");
    }

    #[test]
    fn test_edit_outcome_carries_the_verification_verdict() {
        // A commit that was never verified states that fact instead of
        // implying a success the project never claimed.
        let outcome = EditOutcome {
            applied: true,
            error: None,
            changed_files: vec!["src/lib.rs".to_string()],
            commit_hash: Some("abc1234".to_string()),
            verification: Some(VerificationVerdict::NotConfigured),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(
            json.contains("\"verification\":\"not_configured\""),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<EditOutcome>(&json).unwrap(), outcome);

        // A change set that never reached verification omits the field, so a
        // client distinguishes it from a project that declares no gate.
        let unverified = EditOutcome {
            applied: false,
            error: Some("No edits were successfully applied".to_string()),
            ..EditOutcome::default()
        };
        let json = serde_json::to_string(&unverified).unwrap();
        assert!(!json.contains("verification"), "{json}");

        // Every verdict owns one stable wire spelling.
        for (verdict, expected) in [
            (VerificationVerdict::Passed, "\"passed\""),
            (VerificationVerdict::Failed, "\"failed\""),
            (VerificationVerdict::NotConfigured, "\"not_configured\""),
            (VerificationVerdict::Cancelled, "\"cancelled\""),
        ] {
            assert_eq!(serde_json::to_string(&verdict).unwrap(), expected);
        }

        // A payload produced before the verdict existed stays decodable and
        // reports no verification rather than failing to parse.
        let legacy: EditOutcome =
            serde_json::from_str(r#"{"applied":true,"changed_files":[],"commit_hash":null}"#)
                .unwrap();
        assert!(legacy.verification.is_none());
    }
}
