use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiHistoryKind {
    User,
    Assistant,
    System,
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

    #[test]
    fn test_history_compacted_event_serde_roundtrip() {
        let event = HistoryCompactedEvent {
            summary: UiHistoryItem {
                id: 5,
                kind: UiHistoryKind::System,
                text: "Session compacted".to_string(),
                summary: None,
                commit_hash: None,
                files: Vec::new(),
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
                id: 1,
                kind: UiHistoryKind::System,
                text: "Compacted".to_string(),
                summary: None,
                commit_hash: None,
                files: Vec::new(),
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
