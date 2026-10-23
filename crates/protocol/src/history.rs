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
    pub files: Vec<String>,
    /// Lifecycle state of the record, see [`HistoryEntryStatus`].
    #[serde(default, skip_serializing_if = "HistoryEntryStatus::is_committed")]
    pub status: HistoryEntryStatus,
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
