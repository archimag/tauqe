use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauqe_protocol::HistoryFileChange;

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
        /// Context insertion watermark observed when the turn started.
        ///
        /// The value is the highest `ContextItem::insertion_seq` the context
        /// carried at that moment, so a chronological transcript can place a
        /// file after the turn that introduced it. Sessions recorded before the
        /// field existed decode it as `None` without a migration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_watermark: Option<u64>,
    },
    Response {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Vec::is_empty",
            deserialize_with = "deserialize_file_changes"
        )]
        files: Vec<HistoryFileChange>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
}

/// One persisted file entry of a [`HistoryEntry::Response`].
///
/// The typed form is the only form the engine writes: the turn knows
/// deterministically which operation it performed on a path, so the operation
/// is authored once by the producer instead of being flattened into display
/// text that every client would have to re-parse (Conventions §1.5).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PersistedFileChange {
    Typed(HistoryFileChange),
    LegacyDisplay(String),
}

/// Decodes the persisted file entries, migrating the retired display form.
///
/// Sessions written before the typed record existed store each file as a
/// prefixed display string. Reading such a journal migrates every entry through
/// the one named legacy parser (`HistoryFileChange::from_legacy_display`), so a
/// pre-typed session loads instead of being skipped as corrupt. This is the
/// announced backward-compatible auto-migration of the on-disk layout
/// (`crates/core` Conventions §3.2). An array item that is neither a typed
/// record nor a legacy string fails the whole entry, so the storage reader
/// skips it exactly as it skips any other corrupt line.
fn deserialize_file_changes<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<HistoryFileChange>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let persisted = Vec::<PersistedFileChange>::deserialize(deserializer)?;
    Ok(persisted
        .into_iter()
        .map(|entry| match entry {
            PersistedFileChange::Typed(change) => change,
            PersistedFileChange::LegacyDisplay(raw) => HistoryFileChange::from_legacy_display(&raw),
        })
        .collect())
}

impl HistoryEntry {
    pub fn id(&self) -> Option<u64> {
        match self {
            HistoryEntry::Summary { id, .. }
            | HistoryEntry::Turn { id, .. }
            | HistoryEntry::Response { id, .. }
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
    use tauqe_protocol::EditFileOp;

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
            context_watermark: None,
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
    fn test_turn_records_context_watermark() {
        let entry = HistoryEntry::Turn {
            id: Some(4),
            prompt: "Add the parser".to_string(),
            context: vec!["src/parser.rs".to_string()],
            context_watermark: Some(12),
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"turn","id":4,"prompt":"Add the parser","context":["src/parser.rs"],"context_watermark":12}"#
        );
        assert_eq!(HistoryEntry::from_jsonl_line(&json).unwrap(), entry);

        // A record written before the watermark existed stays readable and
        // reports no watermark instead of a fabricated one.
        let legacy = r#"{"type":"turn","id":5,"prompt":"Legacy turn","context":["src/lib.rs"]}"#;
        match HistoryEntry::from_jsonl_line(legacy).unwrap() {
            HistoryEntry::Turn {
                context_watermark, ..
            } => assert_eq!(context_watermark, None),
            other => panic!("expected a turn entry, got {:?}", other),
        }
    }
    #[test]
    fn test_response_serialization() {
        let entry = HistoryEntry::Response {
            id: None,
            message: "Applied edits".to_string(),
            commit: Some("a1b2c3d".to_string()),
            summary: Some("Initial refactor".to_string()),
            files: vec![
                HistoryFileChange::operated(EditFileOp::Create, "src/new.rs"),
                HistoryFileChange::plain("src/history.rs"),
            ],
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"response","message":"Applied edits","commit":"a1b2c3d","summary":"Initial refactor","files":[{"path":"src/new.rs","op":"create"},{"path":"src/history.rs"}]}"#
        );

        let parsed = HistoryEntry::from_jsonl_line(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_response_typed_files_roundtrip() {
        let entry = HistoryEntry::Response {
            id: Some(3),
            message: "Refactored".to_string(),
            commit: None,
            summary: None,
            files: vec![
                HistoryFileChange::operated(EditFileOp::Create, "src/new.rs"),
                HistoryFileChange::moved("src/a.rs", "src/b.rs"),
                HistoryFileChange::plain("src/lib.rs"),
            ],
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"response","id":3,"message":"Refactored","files":[{"path":"src/new.rs","op":"create"},{"path":"src/a.rs","op":"move","to":"src/b.rs"},{"path":"src/lib.rs"}]}"#
        );
        assert_eq!(HistoryEntry::from_jsonl_line(&json).unwrap(), entry);
    }

    #[test]
    fn test_response_migrates_legacy_display_files() {
        // A record written before the typed change stored each file as a
        // display string. Reading it migrates every entry through the one named
        // legacy parser instead of rejecting the line as corrupt.
        let line = r#"{"type":"response","id":7,"message":"Legacy","files":["[EDIT] src/lib.rs","[NEW] src/util.rs","[DEL] src/old.rs","[OVERWRITE] src/gen.rs","[MOVE] src/a.rs -> src/b.rs","src/plain.rs"]}"#;
        match HistoryEntry::from_jsonl_line(line).unwrap() {
            HistoryEntry::Response { id, files, .. } => {
                assert_eq!(id, Some(7));
                assert_eq!(
                    files,
                    vec![
                        HistoryFileChange::operated(EditFileOp::Replace, "src/lib.rs"),
                        HistoryFileChange::operated(EditFileOp::Create, "src/util.rs"),
                        HistoryFileChange::operated(EditFileOp::Delete, "src/old.rs"),
                        HistoryFileChange::operated(EditFileOp::Overwrite, "src/gen.rs"),
                        HistoryFileChange::moved("src/a.rs", "src/b.rs"),
                        HistoryFileChange::plain("src/plain.rs"),
                    ]
                );
            }
            other => panic!("expected a response entry, got {:?}", other),
        }

        // Re-serializing the migrated record writes only the typed form, so the
        // retired prefixes never reach the journal again.
        let migrated = HistoryEntry::from_jsonl_line(line).unwrap();
        let json = migrated.to_jsonl_line().unwrap();
        assert!(json.contains(r#"{"path":"src/lib.rs","op":"replace"}"#));
        assert!(!json.contains("[EDIT] "));
    }

    #[test]
    fn test_response_rejects_malformed_file_item() {
        // An array item that is neither a typed record nor a legacy string
        // fails the whole entry, so storage skips it like any corrupt line.
        let line = r#"{"type":"response","id":1,"message":"Broken","files":[42]}"#;
        assert!(HistoryEntry::from_jsonl_line(line).is_err());
    }

    #[test]
    fn test_review_serialization() {
        let entry = HistoryEntry::Review {
            id: Some(5),
            model: "anthropic/claude-3.7-sonnet".to_string(),
            user_prompt: Some("Focus on security".to_string()),
            files: vec!["crates/server/src/main.rs".to_string()],
            findings_count: 3,
            summary: Some("The design is sound but locking is racy.".to_string()),
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"review","id":5,"model":"anthropic/claude-3.7-sonnet","user_prompt":"Focus on security","files":["crates/server/src/main.rs"],"findings_count":3,"summary":"The design is sound but locking is racy."}"#
        );

        let parsed = HistoryEntry::from_jsonl_line(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_review_serialization_omits_absent_summary() {
        // Legacy records written before the summary field existed must still
        // deserialize, and a review without a summary must not emit the key.
        let entry = HistoryEntry::Review {
            id: Some(1),
            model: "test/model".to_string(),
            user_prompt: None,
            files: Vec::new(),
            findings_count: 0,
            summary: None,
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"review","id":1,"model":"test/model","findings_count":0}"#
        );

        let parsed = HistoryEntry::from_jsonl_line(&json).unwrap();
        assert_eq!(parsed, entry);
    }
}
