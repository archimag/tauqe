use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A semantic record in the interaction history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryEntry {
    Summary {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        text: String,
    },
    Turn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        prompt: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        context: Vec<String>,
    },
    Response {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
    },
    Undo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        commit: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        restored_checkpoint: bool,
    },
    Review {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
        findings_count: usize,
    },
}

impl HistoryEntry {
    pub fn id(&self) -> Option<u64> {
        match self {
            HistoryEntry::Summary { id, .. }
            | HistoryEntry::Turn { id, .. }
            | HistoryEntry::Response { id, .. }
            | HistoryEntry::Undo { id, .. }
            | HistoryEntry::Review { id, .. } => *id,
        }
    }

    pub fn to_jsonl_line(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize history entry to JSONL line")
    }

    pub fn from_jsonl_line(line: &str) -> Result<Self> {
        serde_json::from_str(line).context("Failed to deserialize history entry from JSONL line")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summary_serialization() {
        let entry = HistoryEntry::Summary {
            id: None,
            text: "Base summary".to_string(),
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(json, r#"{"type":"summary","text":"Base summary"}"#);

        let parsed = HistoryEntry::from_jsonl_line(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_turn_serialization() {
        let entry = HistoryEntry::Turn {
            id: None,
            prompt: "Refactor storage".to_string(),
            context: vec!["src/history.rs".to_string()],
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"turn","prompt":"Refactor storage","context":["src/history.rs"]}"#
        );

        let parsed = HistoryEntry::from_jsonl_line(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_response_serialization() {
        let entry = HistoryEntry::Response {
            id: None,
            message: "Applied edits".to_string(),
            commit: Some("a1b2c3d".to_string()),
            summary: Some("Initial refactor".to_string()),
            files: vec!["src/history.rs".to_string()],
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"response","message":"Applied edits","commit":"a1b2c3d","summary":"Initial refactor","files":["src/history.rs"]}"#
        );

        let parsed = HistoryEntry::from_jsonl_line(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_undo_serialization() {
        let entry = HistoryEntry::Undo {
            id: None,
            commit: "a1b2c3d".to_string(),
            restored_checkpoint: true,
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"undo","commit":"a1b2c3d","restored_checkpoint":true}"#
        );

        let parsed = HistoryEntry::from_jsonl_line(&json).unwrap();
        assert_eq!(parsed, entry);

        // Default restored_checkpoint omitted when false
        let entry_false = HistoryEntry::Undo {
            id: None,
            commit: "deadbeef".to_string(),
            restored_checkpoint: false,
        };
        let json_false = entry_false.to_jsonl_line().unwrap();
        assert_eq!(json_false, r#"{"type":"undo","commit":"deadbeef"}"#);

        let parsed_false = HistoryEntry::from_jsonl_line(&json_false).unwrap();
        assert_eq!(parsed_false, entry_false);
    }

    #[test]
    fn test_review_serialization() {
        let entry = HistoryEntry::Review {
            id: Some(5),
            model: "anthropic/claude-3.7-sonnet".to_string(),
            user_prompt: Some("Focus on security".to_string()),
            files: vec!["crates/server/src/main.rs".to_string()],
            findings_count: 3,
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"review","id":5,"model":"anthropic/claude-3.7-sonnet","user_prompt":"Focus on security","files":["crates/server/src/main.rs"],"findings_count":3}"#
        );

        let parsed = HistoryEntry::from_jsonl_line(&json).unwrap();
        assert_eq!(parsed, entry);
    }
}
