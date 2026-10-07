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
    pub state: ContextState,
}

pub(crate) fn default_context_access() -> ContextAccess {
    ContextAccess::ReadOnly
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
