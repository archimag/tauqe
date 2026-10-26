use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAccess {
    ReadOnly,
    Editable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextLayer {
    Pinned,
    #[default]
    User,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub path: String,
    pub access: ContextAccess,
    pub size_bytes: u64,
    pub estimated_tokens: u64,
    #[serde(default)]
    pub layer: ContextLayer,
    /// Monotonic insertion watermark of the path in the context.
    ///
    /// A payload written before the watermark existed decodes it as `0`, so
    /// stored context states keep loading without a migration.
    #[serde(default)]
    pub insertion_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContextState {
    pub revision: u64,
    pub items: Vec<ContextItem>,
    pub total_estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAddParams {
    pub path: String,
    #[serde(default = "default_context_access")]
    pub access: ContextAccess,
    #[serde(default)]
    pub layer: Option<ContextLayer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAddPatternParams {
    pub pattern: String,
    #[serde(default = "default_context_access")]
    pub access: ContextAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAddPatternResult {
    pub added_count: usize,
    pub added_tokens: u64,
    /// Set when the match set exceeded the configured batch limit and some
    /// matching files were not added, so clients can surface the cap.
    #[serde(default)]
    pub truncated: bool,
    pub state: ContextState,
}

pub(crate) fn default_context_access() -> ContextAccess {
    ContextAccess::ReadOnly
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextClearParams {
    #[serde(default)]
    pub layer: Option<ContextLayer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRemoveParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSetAccessParams {
    pub path: String,
    pub access: ContextAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextChangedEvent {
    pub state: ContextState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_item_insertion_seq_backward_compatible() {
        // A payload written before the insertion watermark existed decodes it
        // as 0 instead of rejecting the record.
        let legacy =
            r#"{"path":"src/lib.rs","access":"read_only","size_bytes":10,"estimated_tokens":3}"#;
        let item: ContextItem = serde_json::from_str(legacy).unwrap();
        assert_eq!(item.insertion_seq, 0);
        assert_eq!(item.layer, ContextLayer::User);

        // A fresh record carries the watermark through a full round-trip.
        let fresh = ContextItem {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            size_bytes: 12,
            estimated_tokens: 4,
            layer: ContextLayer::Auto,
            insertion_seq: 7,
        };
        let json = serde_json::to_string(&fresh).unwrap();
        assert_eq!(serde_json::from_str::<ContextItem>(&json).unwrap(), fresh);
    }
    #[test]
    fn test_context_access_serde() {
        let ro = ContextAccess::ReadOnly;
        let ed = ContextAccess::Editable;

        assert_eq!(serde_json::to_string(&ro).unwrap(), "\"read_only\"");
        assert_eq!(serde_json::to_string(&ed).unwrap(), "\"editable\"");

        assert_eq!(
            serde_json::from_str::<ContextAccess>("\"read_only\"").unwrap(),
            ContextAccess::ReadOnly
        );
        assert_eq!(
            serde_json::from_str::<ContextAccess>("\"editable\"").unwrap(),
            ContextAccess::Editable
        );
    }
}
