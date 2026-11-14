//! Edit proposals: wire-format change entries, validation, and conversion.

use std::fmt;

use serde::{Deserialize, Serialize};

pub(crate) use super::operation::{
    decode_repo_path, deserialize_repo_path, strip_surrounding_markup,
};
pub use super::operation::{
    normalize_repo_path, EditCreate, EditDelete, EditLineAnchor, EditMove, EditMoveDir,
    EditOperation, EditOverwrite, EditReplace,
};
use crate::command::LlmCommand;
use crate::edit::EditFileOp;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditProposal {
    pub summary: String,
    pub edits: Vec<EditOperation>,
}

/// One entry of a change set: its operation, its target, and its payload.
///
/// Every field is a flat `String` with `#[serde(default)]` and
/// `#[schemars(required)]`, so the derived JSON schema is strict
/// (`additionalProperties: false`, every declared property in `required`, and
/// descriptions from doc comments) while tolerant deserialization accepts
/// partial or `null`-valued entries as empty strings.
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

impl schemars::JsonSchema for StructuredChangeProposal {
    fn schema_name() -> String {
        "StructuredChangeProposal".to_string()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::{InstanceType, Metadata, ObjectValidation, Schema, SchemaObject};
        let fields = [
            (
                "op",
                "Operation type: 'replace', 'create', 'delete', 'move', 'move_dir', or 'overwrite'.",
            ),
            (
                "path",
                "Repository-relative path to the target file (source path for 'move' and 'move_dir').",
            ),
            (
                "to",
                "Destination repository-relative path for 'move' and 'move_dir' (empty string otherwise).",
            ),
            (
                "old_text",
                "Exact text to replace in the file (required for 'replace' under search/replace, empty string otherwise).",
            ),
            (
                "new_text",
                "Replacement text (for 'replace' under search/replace, empty string otherwise).",
            ),
            (
                "start",
                "Opaque anchor of the first replaced line, copied verbatim from <context> (required for 'replace' under line-anchor, empty string otherwise).",
            ),
            (
                "end",
                "Opaque anchor of the last replaced line, copied verbatim from <context> (required for 'replace' under line-anchor, empty string otherwise).",
            ),
            (
                "content",
                "Replacement range text for 'replace' (line-anchor) or complete file content for 'create' and 'overwrite' (empty string otherwise).",
            ),
        ];
        let mut properties = schemars::Map::new();
        let mut required = schemars::Set::new();
        for (name, description) in fields {
            let mut prop = <String as schemars::JsonSchema>::json_schema(gen).into_object();
            prop.metadata = Some(Box::new(Metadata {
                description: Some(description.to_string()),
                ..Default::default()
            }));
            properties.insert(name.to_string(), Schema::Object(prop));
            required.insert(name.to_string());
        }
        Schema::Object(SchemaObject {
            instance_type: Some(InstanceType::Object.into()),
            object: Some(Box::new(ObjectValidation {
                properties,
                required,
                additional_properties: Some(Box::new(Schema::Bool(false))),
                ..Default::default()
            })),
            ..Default::default()
        })
    }
}

/// Why a structured change cannot become an edit operation.
///
/// Only one rejection carries the vocabulary of a patch strategy: a `replace`
/// entry that names neither a line range nor the text it replaces. It is kept
/// apart from the rejections that use the shared dictionary, so a caller can
/// explain it in the dialect of the active strategy instead of repeating a
/// field name that strategy may never have rendered. Every variant still renders
/// the message the conversion produced before the separation, so which records
/// are accepted and which are rejected is unchanged.
#[derive(Debug, PartialEq, Eq)]
pub enum ChangeConversionError {
    /// A `replace` named neither a line range nor the text it replaces.
    ReplaceWithoutRange { path: String },
    /// Any other malformed change, rendered with the shared dictionary.
    Other(String),
}

impl ChangeConversionError {
    /// Returns the rejected repository-relative path when the rejection is
    /// specific to one file.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::ReplaceWithoutRange { path } => Some(path),
            Self::Other(_) => None,
        }
    }
}

impl fmt::Display for ChangeConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReplaceWithoutRange { .. } => {
                f.write_str("Change 'replace' requires a non-empty 'old_text' field")
            }
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl StructuredChangeProposal {
    /// Parses the declared `op` spelling into [`EditFileOp`].
    ///
    /// An empty or whitespace-only `op` defaults to [`EditFileOp::Replace`],
    /// while an unrecognized spelling is rejected with the parser's own
    /// diagnostic naming the declared spelling.
    pub fn parsed_op(&self) -> Result<EditFileOp, String> {
        match self.op.trim() {
            "" => Ok(EditFileOp::Replace),
            declared => declared.parse::<EditFileOp>(),
        }
    }

    /// Converts the proposal into a concrete edit operation.
    ///
    /// Malformed proposals are rejected with a diagnostic instead of being
    /// silently coerced into a destructive edit. The repository-relative path
    /// (and, for a move, the destination path) is normalized and validated
    /// here at the wire boundary through [`normalize_repo_path`]. This is a
    /// first line of defence only: `core` must still canonicalize and confine
    /// every path against the repository root, because the protocol cannot
    /// guarantee that all paths flow through this converter. A `create` or
    /// `overwrite` without `content`, a `move` without a valid destination,
    /// and a `replace` without a search block all yield `Err`.
    ///
    /// The rejection is rendered as text here. A caller that must explain it in
    /// the vocabulary of the active patch strategy calls
    /// [`Self::to_edit_operation_typed`] instead.
    pub fn to_edit_operation(&self) -> Result<EditOperation, String> {
        self.to_edit_operation_typed()
            .map_err(|error| error.to_string())
    }

    /// Converts the proposal into a concrete edit operation, keeping the
    /// rejection typed.
    ///
    /// A `replace` proposal that names both anchor bounds addresses a line
    /// range instead of a text block, so it becomes
    /// [`EditOperation::LineAnchor`] and the strategy that rendered the anchors
    /// decides how the range is applied. Anchors address replaced lines only,
    /// so every other operation keeps its own payload whatever bounds it
    /// carries: a `move_dir` still renames two directories, a `move` still
    /// keeps its destination, and a `create` or `overwrite` still keeps its
    /// content.
    ///
    /// The decision made here never differs from the one of
    /// [`Self::to_edit_operation`]: only the rejection a caller observes is
    /// typed, so the strategy-dependent one can be reworded elsewhere.
    ///
    /// An entry that declared an operation the shared parser rejected is
    /// rejected first, with that parser's own diagnostic naming the declared
    /// spelling.
    pub fn to_edit_operation_typed(&self) -> Result<EditOperation, ChangeConversionError> {
        let op = self.parsed_op().map_err(ChangeConversionError::Other)?;
        let path = normalize_repo_path(&self.path).map_err(ChangeConversionError::Other)?;
        let start = self.start.trim();
        let end = self.end.trim();
        // Anchors address the lines of a replaced range, so only a
        // `replace` may take this fast path. A stray bound on any other
        // operation falls through to its own branch instead of being
        // coerced into a destructive range replacement.
        if op == EditFileOp::Replace && !start.is_empty() && !end.is_empty() {
            let content = if !self.content.is_empty() {
                self.content.clone()
            } else {
                self.new_text.clone()
            };
            return Ok(EditOperation::LineAnchor(EditLineAnchor {
                path,
                start: start.to_string(),
                end: end.to_string(),
                content,
            }));
        }
        match op {
            EditFileOp::Create => {
                let content = if !self.content.is_empty() {
                    self.content.clone()
                } else if !self.new_text.is_empty() {
                    self.new_text.clone()
                } else {
                    return Err(ChangeConversionError::Other(
                        "Change 'create' requires a 'content' field".to_string(),
                    ));
                };
                Ok(EditOperation::Create(EditCreate { path, content }))
            }
            EditFileOp::Overwrite => {
                let content = if !self.content.is_empty() {
                    self.content.clone()
                } else if !self.new_text.is_empty() {
                    self.new_text.clone()
                } else {
                    return Err(ChangeConversionError::Other(
                        "Change 'overwrite' requires a 'content' field".to_string(),
                    ));
                };
                Ok(EditOperation::Overwrite(EditOverwrite { path, content }))
            }
            EditFileOp::Delete => Ok(EditOperation::Delete(EditDelete { path })),
            EditFileOp::Move => {
                if self.to.is_empty() {
                    return Err(ChangeConversionError::Other(
                        "Change 'move' requires a 'to' field".to_string(),
                    ));
                }
                let to = normalize_repo_path(&self.to).map_err(ChangeConversionError::Other)?;
                Ok(EditOperation::Move(EditMove { from: path, to }))
            }
            EditFileOp::MoveDir => {
                if self.to.is_empty() {
                    return Err(ChangeConversionError::Other(
                        "Change 'move_dir' requires a 'to' field".to_string(),
                    ));
                }
                let to = normalize_repo_path(&self.to).map_err(ChangeConversionError::Other)?;
                Ok(EditOperation::MoveDir(EditMoveDir { from: path, to }))
            }
            EditFileOp::Replace => {
                if self.old_text.is_empty() {
                    return Err(ChangeConversionError::ReplaceWithoutRange { path });
                }
                let new_text = if !self.new_text.is_empty() {
                    self.new_text.clone()
                } else {
                    self.content.clone()
                };
                Ok(EditOperation::Replace(EditReplace {
                    path,
                    old_text: self.old_text.clone(),
                    new_text,
                }))
            }
        }
    }

    /// Converts the proposal into a concrete [`LlmCommand`] edit variant,
    /// keeping the rejection typed.
    ///
    /// Routes each operation per-op with the exact same validation and
    /// anchor fast-path as [`Self::to_edit_operation_typed`].
    pub fn to_edit_command(&self) -> Result<LlmCommand, ChangeConversionError> {
        let op = self.to_edit_operation_typed()?;
        Ok(match op {
            EditOperation::Replace(op) => LlmCommand::EditReplace(op),
            EditOperation::Create(op) => LlmCommand::EditCreate(op),
            EditOperation::Delete(op) => LlmCommand::EditDelete(op),
            EditOperation::Move(op) => LlmCommand::EditMove(op),
            EditOperation::MoveDir(op) => LlmCommand::EditMoveDir(op),
            EditOperation::Overwrite(op) => LlmCommand::EditOverwrite(op),
            EditOperation::LineAnchor(op) => LlmCommand::EditLineAnchor(op),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(op: EditFileOp, path: &str) -> StructuredChangeProposal {
        StructuredChangeProposal {
            op: op.as_str().to_string(),
            path: path.to_string(),
            ..StructuredChangeProposal::default()
        }
    }

    #[test]
    fn test_structured_proposal_conversion() {
        let mut change = proposal(EditFileOp::Create, "new.txt");
        change.content = "hello".to_string();

        match change.to_edit_operation().unwrap() {
            EditOperation::Create(EditCreate { path, content }) => {
                assert_eq!(path, "new.txt");
                assert_eq!(content, "hello");
            }
            other => panic!("Expected Create operation, got {other:?}"),
        }
    }

    #[test]
    fn test_structured_proposal_rejects_malformed_input() {
        // Missing payloads that would truncate a file are rejected.
        assert!(proposal(EditFileOp::Create, "a.txt")
            .to_edit_operation()
            .is_err());
        assert!(proposal(EditFileOp::Overwrite, "a.txt")
            .to_edit_operation()
            .is_err());

        // A move requires a destination path.
        assert!(proposal(EditFileOp::Move, "a.txt")
            .to_edit_operation()
            .is_err());

        // A replace requires a non-empty search block.
        assert!(proposal(EditFileOp::Replace, "a.txt")
            .to_edit_operation()
            .is_err());
    }

    #[test]
    fn test_replace_falls_back_to_content_only_when_new_text_is_empty() {
        let mut deletion = proposal(EditFileOp::Replace, "a.txt");
        deletion.old_text = "remove me".to_string();
        match deletion.to_edit_operation().unwrap() {
            EditOperation::Replace(EditReplace { new_text, .. }) => assert!(new_text.is_empty()),
            other => panic!("Expected Replace operation, got {other:?}"),
        }

        // When `new_text` is empty, the legacy `content` field still applies.
        let mut change = proposal(EditFileOp::Replace, "a.txt");
        change.old_text = "old".to_string();
        change.content = "new".to_string();

        match change.to_edit_operation().unwrap() {
            EditOperation::Replace(EditReplace { new_text, .. }) => assert_eq!(new_text, "new"),
            other => panic!("Expected Replace operation, got {other:?}"),
        }
    }

    #[test]
    fn test_structured_change_op_deserializes_tolerantly() {
        // A known operation is parsed case-insensitively with whitespace
        // ignored.
        let known: StructuredChangeProposal =
            serde_json::from_str(r#"{"op":"  CREATE ","path":"a.txt"}"#).unwrap();
        assert_eq!(known.parsed_op(), Ok(EditFileOp::Create));

        // An unrecognized operation is preserved verbatim and rejected on conversion.
        let unknown: StructuredChangeProposal =
            serde_json::from_str(r#"{"op":"frobnicate","path":"a.txt"}"#).unwrap();
        assert_eq!(unknown.op, "frobnicate");
        assert!(unknown.parsed_op().is_err());

        // An absent operation defaults to replace.
        let absent: StructuredChangeProposal = serde_json::from_str(r#"{"path":"a.txt"}"#).unwrap();
        assert_eq!(absent.parsed_op(), Ok(EditFileOp::Replace));

        // An explicit `null` operation is tolerated exactly like an absent one.
        let null_op: StructuredChangeProposal =
            serde_json::from_str(r#"{"op":null,"path":"a.txt"}"#).unwrap();
        assert_eq!(null_op.parsed_op(), Ok(EditFileOp::Replace));
    }

    #[test]
    fn test_structured_proposal_normalizes_and_validates_paths() {
        // Surrounding whitespace and a leading `./` are stripped.
        let mut change = proposal(EditFileOp::Create, "  ./src/new.rs  ");
        change.content = "fn x() {}\n".to_string();
        match change.to_edit_operation().unwrap() {
            EditOperation::Create(EditCreate { path, .. }) => assert_eq!(path, "src/new.rs"),
            other => panic!("Expected Create operation, got {other:?}"),
        }

        // Empty, absolute, and traversal paths are rejected.
        for path in [
            "",
            "   ",
            "/etc/passwd",
            "../escape.rs",
            "a/../../b.rs",
            r"..\escape.rs",
            r"\etc\passwd",
            "C:/Windows",
            r"C:\Windows",
        ] {
            assert!(
                proposal(EditFileOp::Delete, path)
                    .to_edit_operation()
                    .is_err(),
                "path '{path}' must be rejected"
            );
        }

        // The `move` destination follows the same normalization and validation.
        let mut change = proposal(EditFileOp::Move, "src/a.rs");
        change.to = "  ./src/b.rs ".to_string();
        match change.to_edit_operation().unwrap() {
            EditOperation::Move(EditMove { from, to }) => {
                assert_eq!(from, "src/a.rs");
                assert_eq!(to, "src/b.rs");
            }
            other => panic!("Expected Move operation, got {other:?}"),
        }

        let mut change = proposal(EditFileOp::Move, "src/a.rs");
        change.to = "../escape.rs".to_string();
        assert!(change.to_edit_operation().is_err());
    }

    #[test]
    fn test_line_anchor_proposal_converts_and_normalizes() {
        // A proposal naming both bounds addresses a line range, so it carries
        // the anchors and never echoes the replaced text.
        let mut change = proposal(EditFileOp::Replace, " ./src/a.rs ");
        change.start = "Kf9Q".to_string();
        change.end = " Tq2W ".to_string();
        change.content = "fn replacement() {}\n".to_string();
        match change.to_edit_operation().unwrap() {
            EditOperation::LineAnchor(EditLineAnchor {
                path,
                start,
                end,
                content,
            }) => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(start, "Kf9Q");
                assert_eq!(end, "Tq2W");
                assert_eq!(content, "fn replacement() {}\n");
            }
            other => panic!("Expected LineAnchor operation, got {other:?}"),
        }

        // A missing or blank bound leaves the proposal a search/replace edit,
        // so a strategy that renders no anchors decodes exactly as before.
        let mut change = proposal(EditFileOp::Replace, "src/a.rs");
        change.start = "Kf9Q".to_string();
        change.end = "   ".to_string();
        change.old_text = "old();".to_string();
        change.new_text = "new();".to_string();
        assert!(matches!(
            change.to_edit_operation().unwrap(),
            EditOperation::Replace(_)
        ));
    }

    #[test]
    fn test_stray_bounds_never_coerce_other_ops_into_a_line_range() {
        // Anchors address replaced lines only, so a bound carried by any other
        // operation is ignored instead of turning that operation into a
        // destructive range replacement.
        let mut moved = proposal(EditFileOp::Move, "src/a.rs");
        moved.to = "src/b.rs".to_string();
        moved.start = "K1".to_string();
        moved.end = "K2".to_string();
        match moved.to_edit_operation_typed() {
            Ok(EditOperation::Move(EditMove { from, to })) => {
                assert_eq!(from, "src/a.rs");
                assert_eq!(to, "src/b.rs");
            }
            other => panic!("Expected a move that keeps its destination, got {other:?}"),
        }

        let mut created = proposal(EditFileOp::Create, "src/new.rs");
        created.content = "fn new() {}\n".to_string();
        created.start = "K1".to_string();
        created.end = "K2".to_string();
        match created.to_edit_operation_typed() {
            Ok(EditOperation::Create(EditCreate { path, content })) => {
                assert_eq!(path, "src/new.rs");
                assert_eq!(content, "fn new() {}\n");
            }
            other => panic!("Expected a create that keeps its content, got {other:?}"),
        }

        let mut deleted = proposal(EditFileOp::Delete, "src/gone.rs");
        deleted.start = "K1".to_string();
        deleted.end = "K2".to_string();
        match deleted.to_edit_operation_typed() {
            Ok(EditOperation::Delete(EditDelete { path })) => assert_eq!(path, "src/gone.rs"),
            other => panic!("Expected a delete that keeps its path, got {other:?}"),
        }

        let mut overwritten = proposal(EditFileOp::Overwrite, "src/a.rs");
        overwritten.content = "whole file\n".to_string();
        overwritten.start = "K1".to_string();
        overwritten.end = "K2".to_string();
        match overwritten.to_edit_operation_typed() {
            Ok(EditOperation::Overwrite(EditOverwrite { path, content })) => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(content, "whole file\n");
            }
            other => panic!("Expected an overwrite that keeps its content, got {other:?}"),
        }
    }

    #[test]
    fn test_replace_without_range_is_rejected_typed() {
        let change = proposal(EditFileOp::Replace, " ./src/a.rs ");
        let rejection = change
            .to_edit_operation_typed()
            .expect_err("a replace without a range must be rejected");
        assert_eq!(
            rejection,
            ChangeConversionError::ReplaceWithoutRange {
                path: "src/a.rs".to_string()
            }
        );
        assert_eq!(rejection.path(), Some("src/a.rs"));

        assert_eq!(
            change.to_edit_operation(),
            Err("Change 'replace' requires a non-empty 'old_text' field".to_string())
        );
    }

    #[test]
    fn test_anchored_change_keeps_the_fast_path() {
        let mut change = proposal(EditFileOp::Replace, "src/a.rs");
        change.start = "Kf9Q".to_string();
        change.end = "Tq2W".to_string();
        change.content = "fn replacement() {}\n".to_string();

        match change.to_edit_operation_typed() {
            Ok(EditOperation::LineAnchor(EditLineAnchor {
                path,
                start,
                end,
                content,
            })) => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(start, "Kf9Q");
                assert_eq!(end, "Tq2W");
                assert_eq!(content, "fn replacement() {}\n");
            }
            other => panic!("Expected an anchored replacement, got {other:?}"),
        }
    }

    #[test]
    fn test_other_conversion_rejections_keep_their_message() {
        let missing_payload = [
            (
                EditFileOp::Create,
                "Change 'create' requires a 'content' field",
            ),
            (
                EditFileOp::Overwrite,
                "Change 'overwrite' requires a 'content' field",
            ),
            (EditFileOp::Move, "Change 'move' requires a 'to' field"),
        ];
        for (op, message) in missing_payload {
            let change = proposal(op, "src/a.rs");
            assert_eq!(
                change.to_edit_operation_typed(),
                Err(ChangeConversionError::Other(message.to_string())),
                "{op}"
            );
            assert_eq!(change.to_edit_operation(), Err(message.to_string()), "{op}");
        }

        let empty_path = proposal(EditFileOp::Delete, "   ");
        let rejection = empty_path
            .to_edit_operation_typed()
            .expect_err("a blank path must be rejected");
        assert_eq!(rejection.path(), None);
        assert_eq!(
            rejection,
            ChangeConversionError::Other("Change path must not be empty".to_string())
        );

        let mut traversal = proposal(EditFileOp::Move, "src/a.rs");
        traversal.to = "../escape.rs".to_string();
        assert_eq!(
            traversal.to_edit_operation(),
            Err("Change path '../escape.rs' must not contain '..' components".to_string())
        );
    }

    #[test]
    fn test_move_dir_proposal_names_two_directories_and_no_payload() {
        let mut change = proposal(EditFileOp::MoveDir, " ./src/old ");
        change.to = "  ./src/new ".to_string();
        change.start = "Kf9Q".to_string();
        change.end = "Tq2W".to_string();
        change.content = "ignored".to_string();

        match change.to_edit_operation_typed() {
            Ok(EditOperation::MoveDir(EditMoveDir { from, to })) => {
                assert_eq!(from, "src/old");
                assert_eq!(to, "src/new");
            }
            other => panic!("Expected MoveDir operation, got {other:?}"),
        }
    }

    #[test]
    fn test_move_dir_requires_a_destination() {
        let change = proposal(EditFileOp::MoveDir, "src/old");
        assert_eq!(
            change.to_edit_operation_typed(),
            Err(ChangeConversionError::Other(
                "Change 'move_dir' requires a 'to' field".to_string()
            ))
        );
        assert_eq!(
            change.to_edit_operation(),
            Err("Change 'move_dir' requires a 'to' field".to_string())
        );

        let mut traversal = proposal(EditFileOp::MoveDir, "src/old");
        traversal.to = "../escape".to_string();
        assert_eq!(
            traversal.to_edit_operation(),
            Err("Change path '../escape' must not contain '..' components".to_string())
        );

        let mut empty = proposal(EditFileOp::MoveDir, "src/old");
        empty.to = "   ".to_string();
        assert_eq!(
            empty.to_edit_operation(),
            Err("Change path must not be empty".to_string())
        );
    }

    #[test]
    fn test_absolute_escape_spellings_fail_the_conversion_boundary() {
        for path in [r"\\server\share", r"\\etc\passwd", r"\\?\C:\x", "./C:/x"] {
            let rejection = proposal(EditFileOp::Delete, path)
                .to_edit_operation_typed()
                .expect_err("an absolute proposal path must be rejected");
            assert_eq!(rejection.path(), None);
            assert!(
                rejection.to_string().contains("repository-relative"),
                "diagnostic for '{path}' must name the absolute spelling: {rejection}"
            );
        }
    }

    #[test]
    fn test_unrecognized_operation_is_reported_instead_of_the_fallback() {
        let decoded: Vec<StructuredChangeProposal> = serde_json::from_str(
            r#"[{"op":"frobnicate","path":"src/a.rs"},
                {"op":"create","path":"src/b.rs","content":"fn b() {}\n"}]"#,
        )
        .expect("a malformed operation must not discard the change set");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].op, "frobnicate");
        assert_eq!(decoded[1].op, "create");

        let rejection = decoded[0]
            .to_edit_operation_typed()
            .expect_err("an unrecognized operation must be rejected");
        assert_eq!(
            rejection,
            ChangeConversionError::Other(
                "Unknown change operation 'frobnicate'. Expected replace, create, delete, move, move_dir, or overwrite"
                    .to_string()
            )
        );
        assert_eq!(rejection.path(), None);
        assert_eq!(decoded[0].to_edit_operation(), Err(rejection.to_string()));
    }

    #[test]
    fn test_unrecognized_operation_survives_the_wire_round_trip() {
        let decoded: StructuredChangeProposal =
            serde_json::from_str(r#"{"op":" FROBNICATE ","path":"src/a.rs"}"#)
                .expect("an unrecognized operation must decode");
        assert_eq!(decoded.op, " FROBNICATE ");
        assert!(decoded.parsed_op().is_err());

        let json = serde_json::to_string(&decoded).unwrap();
        assert!(json.contains(r#""op":" FROBNICATE ""#), "{json}");
        assert_eq!(
            serde_json::from_str::<StructuredChangeProposal>(&json).unwrap(),
            decoded
        );
    }

    #[test]
    fn test_to_edit_command_emits_concrete_variants() {
        let mut replace = proposal(EditFileOp::Replace, "src/a.rs");
        replace.old_text = "old".to_string();
        replace.new_text = "new".to_string();
        assert!(matches!(
            replace.to_edit_command(),
            Ok(LlmCommand::EditReplace(EditReplace { path, old_text, new_text }))
                if path == "src/a.rs" && old_text == "old" && new_text == "new"
        ));

        let mut anchor = proposal(EditFileOp::Replace, "src/a.rs");
        anchor.start = "Kf9Q".to_string();
        anchor.end = "Tq2W".to_string();
        anchor.content = "body".to_string();
        assert!(matches!(
            anchor.to_edit_command(),
            Ok(LlmCommand::EditLineAnchor(EditLineAnchor { path, start, end, content }))
                if path == "src/a.rs" && start == "Kf9Q" && end == "Tq2W" && content == "body"
        ));

        let mut create = proposal(EditFileOp::Create, "src/b.rs");
        create.content = "fn b() {}".to_string();
        assert!(matches!(
            create.to_edit_command(),
            Ok(LlmCommand::EditCreate(EditCreate { path, content }))
                if path == "src/b.rs" && content == "fn b() {}"
        ));

        let delete = proposal(EditFileOp::Delete, "src/c.rs");
        assert!(matches!(
            delete.to_edit_command(),
            Ok(LlmCommand::EditDelete(EditDelete { path })) if path == "src/c.rs"
        ));

        let mut mv = proposal(EditFileOp::Move, "src/d.rs");
        mv.to = "src/e.rs".to_string();
        assert!(matches!(
            mv.to_edit_command(),
            Ok(LlmCommand::EditMove(EditMove { from, to }))
                if from == "src/d.rs" && to == "src/e.rs"
        ));

        let mut mv_dir = proposal(EditFileOp::MoveDir, "src/old");
        mv_dir.to = "src/new".to_string();
        assert!(matches!(
            mv_dir.to_edit_command(),
            Ok(LlmCommand::EditMoveDir(EditMoveDir { from, to }))
                if from == "src/old" && to == "src/new"
        ));

        let mut overwrite = proposal(EditFileOp::Overwrite, "src/f.rs");
        overwrite.content = "all".to_string();
        assert!(matches!(
            overwrite.to_edit_command(),
            Ok(LlmCommand::EditOverwrite(EditOverwrite { path, content }))
                if path == "src/f.rs" && content == "all"
        ));

        let invalid = proposal(EditFileOp::Replace, "src/a.rs");
        assert_eq!(
            invalid.to_edit_command(),
            Err(ChangeConversionError::ReplaceWithoutRange {
                path: "src/a.rs".to_string(),
            })
        );
    }

    #[test]
    fn test_structured_change_proposal_derives_strict_flat_schema() {
        let root = schemars::schema_for!(StructuredChangeProposal);
        let text = serde_json::to_string(&root).expect("schema must serialize");
        for forbidden in [
            "\"oneOf\"",
            "\"allOf\"",
            "\"anyOf\"",
            "\"$ref\"",
            "\"$defs\"",
            "\"null\"",
        ] {
            assert!(
                !text.contains(forbidden),
                "schema must not carry {forbidden}: {text}"
            );
        }
        assert!(text.contains("\"additionalProperties\":false"), "{text}");

        let schema = serde_json::to_value(&root.schema).expect("schema must convert");
        let properties = schema["properties"].as_object().expect("properties object");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(
            required.len(),
            properties.len(),
            "every property must be required: {text}"
        );
        for (name, prop) in properties {
            assert!(
                required.contains(&name.as_str()),
                "missing required '{name}'"
            );
            let desc = prop
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            assert!(
                !desc.trim().is_empty(),
                "property '{name}' must have a non-empty description"
            );
        }
    }
}
