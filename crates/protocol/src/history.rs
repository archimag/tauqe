use crate::edit::EditFileOp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiHistoryKind {
    User,
    Assistant,
    System,
}

/// Lifecycle state of a single [`UiHistoryItem`] record.
///
/// The model's transaction history and the developer's interactive chronicle
/// are deliberately different views of one session. A prompt becomes a record
/// as soon as the developer sends it, so the interface can always show what the
/// harness is running, while the model receives only the records whose turn
/// committed. This enum is the typed contract that keeps the two apart: the
/// state is never inferred from surrounding events or from the text of a
/// message, and it never travels as a free-form string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryEntryStatus {
    /// The turn is still running: the record is visible to the developer but
    /// has not entered the model context yet.
    Active,
    /// The turn committed its result: the record is authoritative and belongs
    /// to the model context.
    #[default]
    Committed,
    /// The turn rolled back: the record stays in the chronicle as a failed
    /// attempt but never reached the model context.
    RolledBack,
}

impl HistoryEntryStatus {
    /// Whether the record carries a committed result.
    ///
    /// The predicate backs the wire encoding: the ordinary committed state adds
    /// no field, and every payload written before the status existed
    /// deserializes into it.
    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiHistoryItem {
    pub id: u64,
    pub kind: UiHistoryKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<HistoryFileChange>,
    /// Lifecycle state of the record, see [`HistoryEntryStatus`].
    #[serde(default, skip_serializing_if = "HistoryEntryStatus::is_committed")]
    pub status: HistoryEntryStatus,
}

/// One file a history entry touched, as a typed record rather than display text.
///
/// The turn knows deterministically which operation it performed on a path, so
/// the wire carries the path together with the operation and lets every client
/// derive its own badge. Flattening the operation into a display prefix would
/// force the presentation layer to re-infer typed knowledge from text, where a
/// wording change or a path that itself begins with a prefix silently
/// misclassifies the entry without any compiler noticing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HistoryFileChange {
    /// Repository-relative path the entry touched; the move source for a move.
    pub path: String,
    /// Operation the turn performed, absent for a plain path listing such as a
    /// context file or a post-hook artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<EditFileOp>,
    /// Move destination, present only when `op` is [`EditFileOp::Move`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// Retired display prefix of a move record, whose remainder is `from -> to`.
const LEGACY_MOVE_PREFIX: &str = "[MOVE] ";

/// Separator between the move source and its destination in the retired format.
const LEGACY_MOVE_ARROW: &str = " -> ";

/// Retired display prefixes and the operations they denoted.
///
/// The prefixes are disjoint, so the table is a direct decoding of the format
/// that existing sessions already contain on disk.
const LEGACY_DISPLAY_PREFIXES: &[(&str, EditFileOp)] = &[
    ("[NEW] ", EditFileOp::Create),
    ("[DEL] ", EditFileOp::Delete),
    ("[OVERWRITE] ", EditFileOp::Overwrite),
    ("[EDIT] ", EditFileOp::Replace),
];

impl HistoryFileChange {
    /// Builds a record that only names a path.
    pub fn plain(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            op: None,
            to: None,
        }
    }

    /// Builds a record for a path the turn modified with `op`.
    pub fn operated(op: EditFileOp, path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            op: Some(op),
            to: None,
        }
    }

    /// Builds a record for a move from `from` to `to`.
    pub fn moved(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            path: from.into(),
            op: Some(EditFileOp::Move),
            to: Some(to.into()),
        }
    }

    /// Decodes one file entry written by the retired display-prefix format.
    ///
    /// Compatibility note (Conventions §1.5): history records persisted before
    /// this type existed flattened the file operation into display prefixes
    /// (`[NEW] `, `[DEL] `, `[OVERWRITE] `, `[MOVE] from -> to`, `[EDIT] `).
    /// This parser exists solely to migrate those already-stored strings into
    /// typed records on read. It never participates in writing or rendering new
    /// data: the operation is authored once by the producer and travels typed.
    ///
    /// A legacy string that genuinely begins with a prefix is indistinguishable
    /// from an annotated one, which is exactly why the format is retired; such a
    /// value decodes into the operation its prefix names. Anything unrecognized
    /// falls back to a plain record so a foreign entry never fails to load.
    pub fn from_legacy_display(raw: &str) -> Self {
        if let Some(rest) = raw.strip_prefix(LEGACY_MOVE_PREFIX) {
            if let Some((from, to)) = rest.split_once(LEGACY_MOVE_ARROW) {
                return Self::moved(from, to);
            }
        }
        for (prefix, op) in LEGACY_DISPLAY_PREFIXES {
            if let Some(path) = raw.strip_prefix(prefix) {
                return Self::operated(*op, path);
            }
        }
        Self::plain(raw)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HistoryGetParams {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub before_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryGetResult {
    pub items: Vec<UiHistoryItem>,
    pub has_more: bool,
    pub total_count: usize,
    #[serde(default)]
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntryAddedEvent {
    pub item: UiHistoryItem,
    #[serde(default)]
    pub estimated_tokens: Option<u64>,
}

/// Emitted when a history record is rolled back.
///
/// A rollback is neither an append nor a deletion. The turn failed or was
/// cancelled before its result reached the model context, so the transactional
/// journal drops the record, while the developer's chronicle keeps it visible
/// with [`HistoryEntryStatus::RolledBack`]. The event carries the authoritative
/// record so a client updates exactly that entry instead of guessing which
/// entry a later failure refers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntryRolledBackEvent {
    pub item: UiHistoryItem,
}

/// Emitted when session history is compacted.
///
/// Compaction is not an append: it structurally replaces the summarized prefix
/// of the session with a single [`UiHistoryItem`] summary while preserving the
/// retained tail. The event carries the generated summary, the ids of the
/// preserved tail entries in order, the resulting total entry count, and the
/// estimated token footprint, so a client can rebuild its view from the
/// authoritative result instead of reconstructing it from append events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCompactedEvent {
    pub summary: UiHistoryItem,
    #[serde(default)]
    pub tail_ids: Vec<u64>,
    pub total_count: usize,
    #[serde(default)]
    pub estimated_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed_item(id: u64, kind: UiHistoryKind, text: &str) -> UiHistoryItem {
        UiHistoryItem {
            id,
            kind,
            text: text.to_string(),
            summary: None,
            commit_hash: None,
            files: Vec::new(),
            status: HistoryEntryStatus::Committed,
        }
    }

    #[test]
    fn test_history_entry_status_wire_names() {
        let json = serde_json::to_value(HistoryEntryStatus::Active).unwrap();
        assert_eq!(json, serde_json::json!("active"));

        let json = serde_json::to_value(HistoryEntryStatus::Committed).unwrap();
        assert_eq!(json, serde_json::json!("committed"));

        let json = serde_json::to_value(HistoryEntryStatus::RolledBack).unwrap();
        assert_eq!(json, serde_json::json!("rolledBack"));

        let parsed: HistoryEntryStatus =
            serde_json::from_value(serde_json::json!("rolledBack")).unwrap();
        assert_eq!(parsed, HistoryEntryStatus::RolledBack);
    }

    #[test]
    fn test_history_entry_status_default_is_committed() {
        assert_eq!(HistoryEntryStatus::default(), HistoryEntryStatus::Committed);
        assert!(HistoryEntryStatus::Committed.is_committed());
        assert!(!HistoryEntryStatus::Active.is_committed());
        assert!(!HistoryEntryStatus::RolledBack.is_committed());
    }

    #[test]
    fn test_ui_history_item_omits_committed_status() {
        let json = serde_json::to_value(committed_item(1, UiHistoryKind::User, "Task")).unwrap();
        assert!(
            json.get("status").is_none(),
            "the committed state is implicit"
        );

        // A payload written before the status field existed must still land on
        // the committed state instead of failing to deserialize.
        let legacy: UiHistoryItem = serde_json::from_value(serde_json::json!({
            "id": 1,
            "kind": "user",
            "text": "Task",
        }))
        .unwrap();
        assert_eq!(legacy.status, HistoryEntryStatus::Committed);
        assert_eq!(legacy, committed_item(1, UiHistoryKind::User, "Task"));
    }

    #[test]
    fn test_ui_history_item_serializes_active_and_rolled_back_status() {
        let mut item = committed_item(2, UiHistoryKind::User, "Running task");

        item.status = HistoryEntryStatus::Active;
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["status"], serde_json::json!("active"));

        item.status = HistoryEntryStatus::RolledBack;
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["status"], serde_json::json!("rolledBack"));

        let parsed: UiHistoryItem = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, item);
    }

    #[test]
    fn test_history_entry_rolled_back_event_roundtrip() {
        let item = UiHistoryItem {
            status: HistoryEntryStatus::RolledBack,
            ..committed_item(7, UiHistoryKind::User, "Rejected prompt")
        };
        let event = HistoryEntryRolledBackEvent { item };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["item"]["id"], 7);
        assert_eq!(json["item"]["status"], serde_json::json!("rolledBack"));

        let parsed: HistoryEntryRolledBackEvent = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn test_history_file_change_serde_roundtrip() {
        // A plain record carries the path alone: `op` and `to` are omitted from
        // the wire instead of travelling as nulls.
        let plain = HistoryFileChange::plain("src/main.rs");
        let json = serde_json::to_value(&plain).unwrap();
        assert_eq!(json, serde_json::json!({ "path": "src/main.rs" }));

        // An operated record serializes the snake_case operation.
        let operated = HistoryFileChange::operated(EditFileOp::Create, "src/new.rs");
        let json = serde_json::to_value(&operated).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "path": "src/new.rs", "op": "create" })
        );

        // A move names its source in `path` and its destination in `to`.
        let moved = HistoryFileChange::moved("src/a.rs", "src/b.rs");
        let json = serde_json::to_value(&moved).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "path": "src/a.rs", "op": "move", "to": "src/b.rs" })
        );

        for change in [plain, operated, moved] {
            let json = serde_json::to_string(&change).unwrap();
            let parsed: HistoryFileChange = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, change);
        }
    }

    #[test]
    fn test_history_file_change_absent_operation_fields_deserialize_as_plain() {
        let parsed: HistoryFileChange =
            serde_json::from_value(serde_json::json!({ "path": "src/lib.rs" })).unwrap();
        assert_eq!(parsed, HistoryFileChange::plain("src/lib.rs"));
    }

    #[test]
    fn test_history_file_change_from_legacy_display_prefixes() {
        assert_eq!(
            HistoryFileChange::from_legacy_display("[NEW] src/new.rs"),
            HistoryFileChange::operated(EditFileOp::Create, "src/new.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[DEL] src/old.rs"),
            HistoryFileChange::operated(EditFileOp::Delete, "src/old.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[OVERWRITE] src/gen.rs"),
            HistoryFileChange::operated(EditFileOp::Overwrite, "src/gen.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[EDIT] src/lib.rs"),
            HistoryFileChange::operated(EditFileOp::Replace, "src/lib.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[MOVE] src/a.rs -> src/b.rs"),
            HistoryFileChange::moved("src/a.rs", "src/b.rs")
        );
    }

    #[test]
    fn test_history_file_change_from_legacy_display_move_arrow_split() {
        // The split happens on the first arrow, so an arrow inside the source or
        // destination path does not shift where the destination begins.
        assert_eq!(
            HistoryFileChange::from_legacy_display("[MOVE] a -> b -> c"),
            HistoryFileChange::moved("a", "b -> c")
        );

        // A move marker without an arrow is not a move and stays a plain record.
        assert_eq!(
            HistoryFileChange::from_legacy_display("[MOVE] src/a.rs"),
            HistoryFileChange::plain("[MOVE] src/a.rs")
        );
    }

    #[test]
    fn test_history_file_change_from_legacy_display_falls_back_to_plain() {
        assert_eq!(
            HistoryFileChange::from_legacy_display("src/plain.rs"),
            HistoryFileChange::plain("src/plain.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[EDIT]src/no-space.rs"),
            HistoryFileChange::plain("[EDIT]src/no-space.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display(""),
            HistoryFileChange::plain("")
        );
    }

    #[test]
    fn test_ui_history_item_carries_typed_file_changes() {
        let mut item = committed_item(3, UiHistoryKind::Assistant, "Applied the change");
        item.files = vec![
            HistoryFileChange::operated(EditFileOp::Create, "src/new.rs"),
            HistoryFileChange::moved("src/a.rs", "src/b.rs"),
            HistoryFileChange::plain("src/lib.rs"),
        ];

        // The operation travels typed on the wire, so a client matches it
        // instead of re-inferring it from display text.
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(
            json["files"],
            serde_json::json!([
                { "path": "src/new.rs", "op": "create" },
                { "path": "src/a.rs", "op": "move", "to": "src/b.rs" },
                { "path": "src/lib.rs" }
            ])
        );

        let parsed: UiHistoryItem = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, item);
    }

    #[test]
    fn test_ui_history_item_without_files_deserializes_to_empty_records() {
        let parsed: UiHistoryItem = serde_json::from_value(serde_json::json!({
            "id": 9,
            "kind": "assistant",
            "text": "No files touched",
        }))
        .unwrap();
        assert!(parsed.files.is_empty());
    }

    #[test]
    fn test_history_compacted_event_serde_roundtrip() {
        let event = HistoryCompactedEvent {
            summary: UiHistoryItem {
                status: HistoryEntryStatus::Committed,
                ..committed_item(5, UiHistoryKind::System, "Session compacted")
            },
            tail_ids: vec![6, 7, 8],
            total_count: 4,
            estimated_tokens: 128,
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["summary"]["id"], 5);
        assert_eq!(json["tail_ids"], serde_json::json!([6, 7, 8]));
        assert_eq!(json["total_count"], 4);
        assert_eq!(json["estimated_tokens"], 128);

        let parsed: HistoryCompactedEvent = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn test_history_compacted_event_omits_optional_fields_when_absent() {
        let event = HistoryCompactedEvent {
            summary: UiHistoryItem {
                status: HistoryEntryStatus::Committed,
                ..committed_item(1, UiHistoryKind::System, "Compacted")
            },
            tail_ids: Vec::new(),
            total_count: 1,
            estimated_tokens: 0,
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["tail_ids"], serde_json::json!([]));
        assert_eq!(json["total_count"], 1);
        assert_eq!(json["estimated_tokens"], 0);
    }
}
