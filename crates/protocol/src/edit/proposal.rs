//! Client-visible edit proposal DTOs. Execution decoding and schemas belong to core.

use serde::{Deserialize, Serialize};

pub(crate) use super::operation::{decode_repo_path, strip_surrounding_markup};
pub use super::operation::{
    normalize_repo_path, EditCreate, EditDelete, EditLineAnchor, EditMove, EditMoveDir,
    EditOperation, EditOverwrite, EditReplace,
};

/// One entry of a change set: its operation, its target, and its payload.
///
/// Missing and null fields retain their historical empty-string defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredChangeProposal {
    /// Operation type: 'replace', 'create', 'delete', 'move', 'move_dir', or 'overwrite'.
    #[serde(
        default,
        deserialize_with = "crate::discussion::deserialize_null_as_default"
    )]
    pub op: String,
    /// Repository-relative path to the target file (source path for 'move' and 'move_dir').
    #[serde(
        default,
        deserialize_with = "crate::discussion::deserialize_null_as_default"
    )]
    pub path: String,
    /// Destination repository-relative path for 'move' and 'move_dir' (empty string otherwise).
    #[serde(
        default,
        deserialize_with = "crate::discussion::deserialize_null_as_default"
    )]
    pub to: String,
    /// Exact text to replace in the file (required for 'replace' under search/replace, empty string otherwise).
    #[serde(
        default,
        deserialize_with = "crate::discussion::deserialize_null_as_default"
    )]
    pub old_text: String,
    /// Replacement text (for 'replace' under search/replace, empty string otherwise).
    #[serde(
        default,
        deserialize_with = "crate::discussion::deserialize_null_as_default"
    )]
    pub new_text: String,
    /// Opaque anchor of the first replaced line, copied verbatim from <context> (required for 'replace' under line-anchor, empty string otherwise).
    #[serde(
        default,
        deserialize_with = "crate::discussion::deserialize_null_as_default"
    )]
    pub start: String,
    /// Opaque anchor of the last replaced line, copied verbatim from <context> (required for 'replace' under line-anchor, empty string otherwise).
    #[serde(
        default,
        deserialize_with = "crate::discussion::deserialize_null_as_default"
    )]
    pub end: String,
    /// Replacement range text for 'replace' (line-anchor) or complete file content for 'create' and 'overwrite' (empty string otherwise).
    #[serde(
        default,
        deserialize_with = "crate::discussion::deserialize_null_as_default"
    )]
    pub content: String,
}
