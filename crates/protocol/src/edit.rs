use serde::{Deserialize, Serialize};

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
    pub fn to_edit_operation(&self) -> EditOperation {
        match self.op.as_str() {
            "create" => EditOperation::Create {
                path: self.path.clone(),
                content: self.content.clone().unwrap_or_default(),
            },
            "overwrite" => EditOperation::Overwrite {
                path: self.path.clone(),
                content: self.content.clone().unwrap_or_default(),
            },
            "delete" => EditOperation::Delete {
                path: self.path.clone(),
            },
            "move" => EditOperation::Move {
                from: self.path.clone(),
                to: self.to.clone().unwrap_or_default(),
            },
            _ => EditOperation::Replace {
                path: self.path.clone(),
                old_text: self.old_text.clone().unwrap_or_default(),
                new_text: self
                    .new_text
                    .clone()
                    .or_else(|| self.content.clone())
                    .unwrap_or_default(),
            },
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
    pub op_type: String, // "replace", "create", "delete"
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
    pub status: String, // "ok", "error"
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

    #[test]
    fn test_structured_proposal_conversion() {
        let proposal = StructuredChangeProposal {
            op: "create".to_string(),
            path: "new.txt".to_string(),
            to: None,
            old_text: None,
            new_text: None,
            content: Some("hello".to_string()),
        };

        let edit = proposal.to_edit_operation();
        match edit {
            EditOperation::Create { path, content } => {
                assert_eq!(path, "new.txt");
                assert_eq!(content, "hello");
            }
            _ => panic!("Expected Create operation"),
        }
    }
}
