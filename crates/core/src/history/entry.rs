use crate::history::{HistoryFileChange, TurnRoundRecord};
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
        /// Context insertion watermark observed when the turn started.
        ///
        /// The value is the highest `ContextItem::insertion_seq` the context
        /// carried at that moment, so a chronological transcript can place a
        /// file after the turn that introduced it. Sessions recorded before the
        /// field existed decode it as `None` without a migration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_watermark: Option<u64>,
        /// Intermediate rounds the turn ran before its final answer.
        ///
        /// The rounds are nested inside the turn record instead of becoming
        /// sibling records, so a rollback or a compaction drops them together
        /// with the attempt they belong to, and the final answer stays the
        /// ordinary response record of the same turn. Sessions recorded before
        /// the field existed decode it as an empty list without a migration.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rounds: Vec<TurnRoundRecord>,
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

#[cfg(test)]
/// Tag that opens the turn-local working-memory channel inside an answer.
const SCRATCHPAD_OPEN: &str = "<scratchpad>";
#[cfg(test)]
/// Tag that closes the turn-local working-memory channel.
const SCRATCHPAD_CLOSE: &str = "</scratchpad>";

#[cfg(test)]
/// Removes the working-memory channel from text about to be recorded.
///
/// The scratchpad is turn-local by contract: it is memory, never part of what a
/// turn durably did. A round message is cleaned before it reaches
/// `history.jsonl`, so the journal and everything rebuilt from it — the
/// chronicle, the compact replay, the compaction summary — never presents an
/// intermediate hypothesis as a recorded finding (Conventions §3.5).
///
/// Everything outside the tags is preserved verbatim, markdown and fenced code
/// included, and the surrounding whitespace is trimmed so the recorded message
/// does not begin or end with an empty line. An unterminated channel belongs to
/// a truncated answer, so the text from its opening tag onward is dropped.
fn sanitize_round_message(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(open) = rest.find(SCRATCHPAD_OPEN) {
        cleaned.push_str(&rest[..open]);
        let after_open = &rest[open + SCRATCHPAD_OPEN.len()..];
        match after_open.find(SCRATCHPAD_CLOSE) {
            Some(close) => rest = &after_open[close + SCRATCHPAD_CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    cleaned.push_str(rest);
    cleaned.trim().to_string()
}

#[cfg(test)]
#[cfg(test)]
/// Builds the durable record of one intermediate round of a turn.
///
/// The constructor is the single place a round enters the journal, so the
/// cleaning of the recorded payload cannot be forgotten by a caller: the raw
/// answer is sanitized here and the caller states only the round position and
/// the compact summary of the feedback that opened the next round.
pub(crate) fn round_record(
    index: u32,
    raw_message: &str,
    feedback: impl Into<String>,
) -> TurnRoundRecord {
    TurnRoundRecord {
        index,
        message: sanitize_round_message(raw_message),
        feedback: feedback.into(),
    }
}

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

    pub(crate) fn to_jsonl_line(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize history entry to JSONL line")
    }

    pub(crate) fn from_jsonl_line(line: &str) -> Result<Self> {
        serde_json::from_str(line).context("Failed to deserialize history entry from JSONL line")
    }
}

/// Bytes one JSONL record occupies in the session journal.
///
/// The record occupies its serialized line plus the newline that terminates it,
/// so the journal size on disk and the in-memory footprint of the same record
/// share one definition instead of each path adding its own terminator.
pub(crate) fn line_bytes(line: &str) -> u64 {
    (line.len() + 1) as u64
}

/// Journal bytes one record occupies, derived from its serialized form.
///
/// A record that cannot be serialized contributes nothing, exactly as it
/// contributes no line to the journal.
pub(crate) fn record_bytes(entry: &HistoryEntry) -> u64 {
    match entry.to_jsonl_line() {
        Ok(line) => line_bytes(&line),
        Err(_) => 0,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryFileOperation;

    #[test]
    fn typed_history_files_keep_legacy_operation_decoding_and_canonical_bytes() {
        let stored = r#"{"type":"response","message":"Moved directory","files":[{"path":"src/old","op":" MOVE_DIR ","to":"src/new"},{"path":"README.md"}]}"#;
        let entry = HistoryEntry::from_jsonl_line(stored).unwrap();
        match &entry {
            HistoryEntry::Response { files, .. } => assert_eq!(
                files,
                &vec![
                    HistoryFileChange::moved_dir("src/old", "src/new"),
                    HistoryFileChange::plain("README.md"),
                ]
            ),
            _ => panic!("expected response"),
        }
        assert_eq!(
            entry.to_jsonl_line().unwrap(),
            r#"{"type":"response","message":"Moved directory","files":[{"path":"src/old","op":"move_dir","to":"src/new"},{"path":"README.md"}]}"#
        );
    }

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
            rounds: Vec::new(),
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
            rounds: Vec::new(),
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
                HistoryFileChange::operated(HistoryFileOperation::Create, "src/new.rs"),
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
                HistoryFileChange::operated(HistoryFileOperation::Create, "src/new.rs"),
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
                        HistoryFileChange::operated(HistoryFileOperation::Replace, "src/lib.rs"),
                        HistoryFileChange::operated(HistoryFileOperation::Create, "src/util.rs"),
                        HistoryFileChange::operated(HistoryFileOperation::Delete, "src/old.rs"),
                        HistoryFileChange::operated(HistoryFileOperation::Overwrite, "src/gen.rs"),
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
    #[test]
    fn test_turn_records_nested_rounds() {
        let entry = HistoryEntry::Turn {
            id: Some(6),
            prompt: "Discover the parser".to_string(),
            context: vec!["src/parser.rs".to_string()],
            context_watermark: Some(3),
            rounds: vec![
                TurnRoundRecord {
                    index: 1,
                    message: "Searching for the parser entry point.".to_string(),
                    feedback: "2 matches in src/parser.rs".to_string(),
                },
                TurnRoundRecord {
                    index: 2,
                    message: "Applying the discovered file.".to_string(),
                    feedback: String::new(),
                },
            ],
        };
        let json = entry.to_jsonl_line().unwrap();
        assert_eq!(
            json,
            r#"{"type":"turn","id":6,"prompt":"Discover the parser","context":["src/parser.rs"],"context_watermark":3,"rounds":[{"index":1,"message":"Searching for the parser entry point.","feedback":"2 matches in src/parser.rs"},{"index":2,"message":"Applying the discovered file."}]}"#
        );

        let parsed = HistoryEntry::from_jsonl_line(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_turn_without_rounds_decodes_to_an_empty_round_list() {
        // A session written before the rounds existed keeps loading, reporting
        // an empty list instead of a fabricated round.
        let legacy = r#"{"type":"turn","id":2,"prompt":"Legacy turn","context":["src/lib.rs"]}"#;
        match HistoryEntry::from_jsonl_line(legacy).unwrap() {
            HistoryEntry::Turn { rounds, .. } => assert!(rounds.is_empty()),
            other => panic!("expected a turn entry, got {:?}", other),
        }

        // A turn that recorded no round keeps the bytes the journal already
        // carries, so the field never appears as an empty array.
        let bare = HistoryEntry::Turn {
            id: Some(3),
            prompt: "Short turn".to_string(),
            context: Vec::new(),
            context_watermark: None,
            rounds: Vec::new(),
        };
        assert_eq!(
            bare.to_jsonl_line().unwrap(),
            r#"{"type":"turn","id":3,"prompt":"Short turn"}"#
        );
    }

    #[test]
    fn test_recorded_round_strips_the_scratchpad_channel() {
        let raw = "Inspecting the parser.\n\n<scratchpad>\nHypothesis: the symbol cache is stale.\n</scratchpad>\n\n```rust\nfn main() {}\n```";
        let round = round_record(2, raw, "verification failed: E0308");

        assert_eq!(round.index, 2);
        assert_eq!(round.feedback, "verification failed: E0308");
        assert!(!round.message.contains("scratchpad"));
        assert!(!round.message.contains("Hypothesis"));
        assert!(round.message.starts_with("Inspecting the parser."));
        assert!(round.message.ends_with("```"));
        assert!(round.message.contains("fn main() {}"));
    }

    #[test]
    fn test_recorded_round_drops_an_unterminated_channel() {
        // A truncated answer ends inside its own working memory, so the channel
        // and everything after it belong to the attempt and never to the record.
        let truncated = round_record(1, "Answer\n<scratchpad>\nunterminated note", "");
        assert_eq!(truncated.message, "Answer");
        assert!(truncated.feedback.is_empty());

        // An answer that carried no channel is recorded verbatim.
        let plain = round_record(3, "  No channel here.  ", "none");
        assert_eq!(plain.message, "No channel here.");
    }
}
