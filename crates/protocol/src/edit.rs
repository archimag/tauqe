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
            "overwrite" => Ok(Self::Overwrite),
            other => Err(format!(
                "Unknown change operation '{}'. Expected replace, create, delete, move, or overwrite",
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
/// changed files, and commit hash have a single authoritative definition
/// instead of being duplicated field-by-field across two wire types.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EditOutcome {
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
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
        assert_eq!(EditFileOp::Move.as_str(), "move");
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
}
