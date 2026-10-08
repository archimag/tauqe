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
