//! Application-owned access, layers, and detached context snapshots.
use serde::{Deserialize, Serialize};

/// Access level granted to one file of the context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextAccess {
    /// Reference material the model reads but never modifies.
    ReadOnly,
    /// Material the model may modify through an edit.
    Editable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextLayer {
    Pinned,
    /// External documentation materialized into the isolated cache.
    ///
    /// Wire aliases are interpreted by the server's protocol adapter.
    OuterDocs,
    #[default]
    User,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextItem {
    pub path: String,
    pub access: ContextAccess,
    pub size_bytes: u64,
    pub estimated_tokens: u64,
    pub layer: ContextLayer,
    /// Monotonic insertion watermark of the path in the context.
    ///
    /// Zero denotes an unrecorded arrival; positive values preserve arrival order.
    pub insertion_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextState {
    pub revision: u64,
    pub items: Vec<ContextItem>,
    pub total_estimated_tokens: u64,
}
