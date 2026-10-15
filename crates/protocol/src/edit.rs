use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditOperation {
    Replace {
        path: String,
        old_text: String,
        new_text: String,
    },
    Create {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
    Overwrite {
        path: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditProposal {
    pub summary: String,
    pub edits: Vec<EditOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredChangeProposal {
    #[serde(default = "default_change_op")]
    pub op: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

fn default_change_op() -> String {
    "replace".to_string()
}

impl StructuredChangeProposal {
    /// Converts the proposal into a concrete edit operation.
    ///
    /// Malformed proposals are rejected with a diagnostic instead of being
    /// silently coerced into a destructive edit. An unknown `op`, a `create`
    /// or `overwrite` without `content`, a `move` without a destination, and a
    /// `replace` without a search block all yield `Err`.
    pub fn to_edit_operation(&self) -> Result<EditOperation, String> {
        let op = EditFileOp::from_str(&self.op)?;
        match op {
            EditFileOp::Create => {
                let content = self.content.clone().ok_or_else(|| {
                    "Change 'create' requires a 'content' field".to_string()
                })?;
                Ok(EditOperation::Create {
                    path: self.path.clone(),
                    content,
                })
            }
            EditFileOp::Overwrite => {
                let content = self.content.clone().ok_or_else(|| {
                    "Change 'overwrite' requires a 'content' field".to_string()
                })?;
                Ok(EditOperation::Overwrite {
                    path: self.path.clone(),
                    content,
                })
            }
            EditFileOp::Delete => Ok(EditOperation::Delete {
                path: self.path.clone(),
            }),
            EditFileOp::Move => {
                let to = self
                    .to
                    .as_deref()
                    .map(str::trim)
                    .filter(|to| !to.is_empty())
                    .ok_or_else(|| {
                        "Change 'move' requires a non-empty 'to' field".to_string()
                    })?;
                Ok(EditOperation::Move {
                    from: self.path.clone(),
                    to: to.to_string(),
                })
            }
            EditFileOp::Replace => {
                let old_text = self
                    .old_text
                    .as_deref()
                    .filter(|old| !old.is_empty())
                    .ok_or_else(|| {
                        "Change 'replace' requires a non-empty 'old_text' field".to_string()
                    })?;
                // An explicit empty `new_text` is a deliberate deletion and
                // must be preserved; only fall back to `content` when
                // `new_text` is absent entirely.
                let new_text = self
                    .new_text
                    .clone()
                    .or_else(|| self.content.clone())
                    .unwrap_or_default();
                Ok(EditOperation::Replace {
                    path: self.path.clone(),
                    old_text: old_text.to_string(),
                    new_text,
                })
            }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFinishedEvent {
    pub operation_id: String,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edit_operation_serde() {
        let op = EditOperation::Replace {
            path: "foo.rs".to_string(),
            old_text: "let a = 1;".to_string(),
            new_text: "let a = 2;".to_string(),
        };

        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"replace\""));

        let de: EditOperation = serde_json::from_str(&json).unwrap();
        assert_eq!(de, op);
    }

    fn proposal(op: &str, path: &str) -> StructuredChangeProposal {
        StructuredChangeProposal {
            op: op.to_string(),
            path: path.to_string(),
            to: None,
            old_text: None,
            new_text: None,
            content: None,
        }
    }

    #[test]
    fn test_structured_proposal_conversion() {
        let mut change = proposal("create", "new.txt");
        change.content = Some("hello".to_string());

        match change.to_edit_operation().unwrap() {
            EditOperation::Create { path, content } => {
                assert_eq!(path, "new.txt");
                assert_eq!(content, "hello");
            }
            other => panic!("Expected Create operation, got {other:?}"),
        }
    }

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

    #[test]
    fn test_structured_proposal_rejects_malformed_input() {
        // An unknown operation is never coerced into a replacement.
        assert!(proposal("frobnicate", "a.txt").to_edit_operation().is_err());

        // Missing payloads that would truncate a file are rejected.
        assert!(proposal("create", "a.txt").to_edit_operation().is_err());
        assert!(proposal("overwrite", "a.txt").to_edit_operation().is_err());

        // A move requires a non-empty destination path.
        assert!(proposal("move", "a.txt").to_edit_operation().is_err());

        // A replace requires a non-empty search block.
        assert!(proposal("replace", "a.txt").to_edit_operation().is_err());
    }

    #[test]
    fn test_replace_falls_back_to_content_only_when_new_text_is_absent() {
        // An explicit empty `new_text` is a deletion, even when `content` is
        // populated, and must not be silently coerced into a replacement.
        let mut change = proposal("replace", "a.txt");
        change.old_text = Some("remove me".to_string());
        change.new_text = Some(String::new());
        change.content = Some("unexpected".to_string());

        match change.to_edit_operation().unwrap() {
            EditOperation::Replace { new_text, .. } => assert!(new_text.is_empty()),
            other => panic!("Expected Replace operation, got {other:?}"),
        }

        // When `new_text` is absent, the legacy `content` field still applies.
        let mut change = proposal("replace", "a.txt");
        change.old_text = Some("old".to_string());
        change.content = Some("new".to_string());

        match change.to_edit_operation().unwrap() {
            EditOperation::Replace { new_text, .. } => assert_eq!(new_text, "new"),
            other => panic!("Expected Replace operation, got {other:?}"),
        }
    }
}
