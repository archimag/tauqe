//! Edit proposals: the typed change operations and their normalization.

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::edit::EditFileOp;

/// A single typed change operation.
///
/// A decoded operation can never address a location outside the repository: the
/// serde boundary normalizes and validates every path it carries — the `path`
/// of a file operation and both directories of a move — through
/// [`normalize_repo_path`], and writes a decoded path back through the inverse
/// escape of that rule, so serializing the operation and decoding it again
/// reproduces the same target instead of retargeting it. An operation is a typed
/// enum variant rather than a
/// list entry, so a rejected path surfaces as a decode failure naming it
/// instead of being dropped silently. The absolute-path and drive-root
/// rejections are derived from the decoded segment structure rather than from
/// enumerated spellings, so a spelling that reaches the file-system root
/// through separator escapes or dot segments is refused as well. This is a
/// first line of defence only:
/// `core` still canonicalizes and confines every path against the repository
/// root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditOperation {
    Replace {
        #[serde(with = "repo_path")]
        path: String,
        old_text: String,
        new_text: String,
    },
    Create {
        #[serde(with = "repo_path")]
        path: String,
        content: String,
    },
    Delete {
        #[serde(with = "repo_path")]
        path: String,
    },
    Move {
        #[serde(with = "repo_path")]
        from: String,
        #[serde(with = "repo_path")]
        to: String,
    },
    /// Moves a directory subtree, naming the source and the destination.
    ///
    /// A directory rename is one operation rather than one entry per file: the
    /// staging engine expands it into the per-file moves of the subtree under
    /// `from`. No replaced lines, range bounds, or payload belong to it.
    MoveDir {
        #[serde(with = "repo_path")]
        from: String,
        #[serde(with = "repo_path")]
        to: String,
    },
    Overwrite {
        #[serde(with = "repo_path")]
        path: String,
        content: String,
    },
    /// Replaces a range of lines that the proposal names by opaque anchors.
    ///
    /// `start` and `end` are inclusive bounds rendered beside the context lines
    /// and decoded deterministically by the staging engine. `content` holds the
    /// complete replacement of the addressed range, so the replaced text never
    /// has to be echoed back.
    LineAnchor {
        #[serde(with = "repo_path")]
        path: String,
        start: String,
        end: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditProposal {
    pub summary: String,
    pub edits: Vec<EditOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredChangeProposal {
    #[serde(
        default = "default_change_op",
        deserialize_with = "deserialize_change_op"
    )]
    pub op: EditFileOp,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Inclusive anchor that opens the replaced line range.
    ///
    /// Both bounds are present only when the active patch strategy addresses a
    /// range by its opaque anchors; a search/replace proposal leaves them
    /// empty and keeps carrying the replaced text in `old_text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Inclusive anchor that closes the replaced line range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
}

fn default_change_op() -> EditFileOp {
    EditFileOp::Replace
}

/// Deserializes a change operation tolerantly.
///
/// A known operation is parsed case-insensitively with surrounding whitespace
/// ignored. An unrecognized operation falls back to the default `replace`
/// instead of failing the whole proposal, so a single malformed entry still
/// surfaces as a precise, recoverable conversion error in
/// [`StructuredChangeProposal::to_edit_operation`] rather than discarding the
/// entire response.
fn deserialize_change_op<'de, D>(deserializer: D) -> Result<EditFileOp, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw
        .as_deref()
        .and_then(|s| s.parse::<EditFileOp>().ok())
        .unwrap_or(EditFileOp::Replace))
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
    pub fn to_edit_operation_typed(&self) -> Result<EditOperation, ChangeConversionError> {
        let path = normalize_repo_path(&self.path).map_err(ChangeConversionError::Other)?;
        if let (Some(start), Some(end)) = (self.start.as_deref(), self.end.as_deref()) {
            let start = start.trim();
            let end = end.trim();
            // Anchors address the lines of a replaced range, so only a
            // `replace` may take this fast path. A stray bound on any other
            // operation falls through to its own branch instead of being
            // coerced into a destructive range replacement.
            if self.op == EditFileOp::Replace && !start.is_empty() && !end.is_empty() {
                return Ok(EditOperation::LineAnchor {
                    path,
                    start: start.to_string(),
                    end: end.to_string(),
                    content: self
                        .content
                        .clone()
                        .or_else(|| self.new_text.clone())
                        .unwrap_or_default(),
                });
            }
        }
        match self.op {
            EditFileOp::Create => {
                let content = self
                    .content
                    .clone()
                    .or_else(|| self.new_text.clone())
                    .ok_or_else(|| {
                        ChangeConversionError::Other(
                            "Change 'create' requires a 'content' field".to_string(),
                        )
                    })?;
                Ok(EditOperation::Create { path, content })
            }
            EditFileOp::Overwrite => {
                let content = self
                    .content
                    .clone()
                    .or_else(|| self.new_text.clone())
                    .ok_or_else(|| {
                        ChangeConversionError::Other(
                            "Change 'overwrite' requires a 'content' field".to_string(),
                        )
                    })?;
                Ok(EditOperation::Overwrite { path, content })
            }
            EditFileOp::Delete => Ok(EditOperation::Delete { path }),
            EditFileOp::Move => {
                let to = self.to.as_deref().ok_or_else(|| {
                    ChangeConversionError::Other("Change 'move' requires a 'to' field".to_string())
                })?;
                let to = normalize_repo_path(to).map_err(ChangeConversionError::Other)?;
                Ok(EditOperation::Move { from: path, to })
            }
            EditFileOp::MoveDir => {
                let to = self.to.as_deref().ok_or_else(|| {
                    ChangeConversionError::Other(
                        "Change 'move_dir' requires a 'to' field".to_string(),
                    )
                })?;
                let to = normalize_repo_path(to).map_err(ChangeConversionError::Other)?;
                Ok(EditOperation::MoveDir { from: path, to })
            }
            EditFileOp::Replace => {
                let old_text = self
                    .old_text
                    .as_deref()
                    .filter(|old| !old.is_empty())
                    .ok_or_else(|| ChangeConversionError::ReplaceWithoutRange {
                        path: path.clone(),
                    })?;
                // An explicit empty `new_text` is a deliberate deletion and
                // must be preserved; only fall back to `content` when
                // `new_text` is absent entirely.
                let new_text = self
                    .new_text
                    .clone()
                    .or_else(|| self.content.clone())
                    .unwrap_or_default();
                Ok(EditOperation::Replace {
                    path,
                    old_text: old_text.to_string(),
                    new_text,
                })
            }
        }
    }
}

/// Strips the surrounding markup a producer may wrap a value in.
///
/// The value is trimmed, a backtick run wrapped around it is dropped, and the
/// result is trimmed once more, so a markdown-wrapped spelling is cleaned
/// before it is validated. Only the surrounding markup is removed: an interior
/// backtick stays part of the value, while a value made of backticks alone
/// becomes empty, exactly as a blank value does. This is the crate's single
/// shared strip rule, so every wire boundary applies identical tolerance.
pub(crate) fn strip_surrounding_markup(raw: &str) -> &str {
    raw.trim().trim_matches('`').trim()
}

/// Decodes the separator escapes of one repository path.
///
/// Two readings of the same byte are in play, and the rules stay purely
/// syntactic because this crate performs no I/O and therefore cannot ask the
/// file system which of them a spelling means:
///
/// - a single backslash is a separator, so the spelling a Windows producer
///   emits (`src\lib.rs`) addresses `src/lib.rs`;
/// - a doubled backslash names one literal backslash byte, so a file whose own
///   name carries a backslash stays addressable on a file system that permits
///   it: `src\\new.rs` addresses the file literally named `src\new.rs`, while
///   the separator rule alone would silently retarget the identical spelling to
///   the different path `src/new.rs`.
///
/// A value carrying no backslash is returned as it is, so the common path costs
/// no allocation.
fn decode_path_separators(raw: &str) -> Cow<'_, str> {
    if !raw.contains('\\') {
        return Cow::Borrowed(raw);
    }
    let mut decoded = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(index) = rest.find('\\') {
        decoded.push_str(&rest[..index]);
        rest = &rest[index + 1..];
        match rest.strip_prefix('\\') {
            Some(tail) => {
                decoded.push('\\');
                rest = tail;
            }
            None => decoded.push('/'),
        }
    }
    decoded.push_str(rest);
    Cow::Owned(decoded)
}

/// Encodes every literal backslash byte so the decoder restores it verbatim.
///
/// Normalization leaves exactly one byte that the separator rule would otherwise
/// rewrite: the backslash of a file whose name carries one. Serializing such a
/// path raw would make this crate's own wire form unre-decodable, because the
/// reader would take that byte for a separator again. The encoder therefore
/// applies the inverse of the doubling rule and leaves every other path
/// untouched.
fn encode_path_separators(path: &str) -> Cow<'_, str> {
    if path.contains('\\') {
        Cow::Owned(path.replace('\\', "\\\\"))
    } else {
        Cow::Borrowed(path)
    }
}

/// Serde adapter of one repository-relative path field.
///
/// Both directions live here, so a field can never be normalized on the way in
/// and echoed raw on the way out: a decoded path may carry the literal backslash
/// of a file name, and the round trip must reproduce the value it started from
/// instead of reading that byte as a separator.
mod repo_path {
    use serde::{Deserializer, Serializer};

    use super::{deserialize_repo_path, encode_path_separators};

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_repo_path(deserializer)
    }

    pub(super) fn serialize<S>(path: &str, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode_path_separators(path))
    }
}

/// Reports whether a decoded segment opens with a Windows drive root.
///
/// A drive root is an ASCII letter followed by a colon at the start of the
/// first surviving segment, whether the colon closes the segment (`C:`) or
/// continues a drive-relative spelling (`C:name`): both leave the repository.
fn is_drive_root(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

/// Decodes a repository-path spelling into its normalized segment structure.
///
/// This is the crate's single shared decode-and-collapse core for repository
/// paths, so a boundary decides acceptance from this structure instead of
/// restating the rules. Surrounding whitespace and backtick markup are
/// stripped first, then the separator escapes are decoded, and then the decoded
/// spelling is split into the segments that survive the collapse of `.`
/// components and repeated slashes.
///
/// Acceptance is derived from that structure instead of from enumerated
/// spellings. A spelling that names no surviving segment yields an empty
/// string, because it names the whole repository to a research request and no
/// location at all to an edit operation, so each caller decides what it means.
/// A spelling that can physically leave the repository is rejected here,
/// because the repository confinement is never left to a caller:
///
/// - a NUL byte anywhere in the spelling;
/// - an absolute spelling, decided from the decoded path rather than from a
///   fixed prefix table: a leading forward slash is absolute, and so is a
///   leading literal backslash byte, which the separator-doubling escape can
///   place at position zero. The UNC spelling `\\server\share`, the doubled
///   leading-separator traversal `\\etc\passwd`, the Windows extended-length
///   prefix `\\?\C:\x`, and a lone doubled backslash all decode to such a
///   string;
/// - a `..` traversal component;
/// - a Windows drive root, decided from the first segment that survives the
///   collapse: a segment opening with an ASCII letter followed by a colon is a
///   drive root, so `./C:/x` is refused exactly like `C:/x`, while the same
///   bytes in a later segment stay part of a file name.
pub(crate) fn decode_repo_path(raw: &str) -> Result<String, String> {
    if raw.contains('\0') {
        return Err("Change path must not contain NUL bytes".to_string());
    }
    let slashed = decode_path_separators(strip_surrounding_markup(raw));
    let trimmed = slashed.trim();
    // An absolute spelling leaves the repository whatever separator spells
    // it: a leading forward slash names the file-system root, and a leading
    // literal backslash byte names a Windows drive root or a UNC server root.
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Err(format!(
            "Change path '{}' must be repository-relative",
            trimmed
        ));
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                return Err(format!(
                    "Change path '{}' must not contain '..' components",
                    trimmed
                ));
            }
            other => {
                // The drive root is decided from the first segment that
                // survives the collapse, so a spelling that only reaches it
                // through dot segments is refused exactly like the bare drive
                // path, while a later segment stays an ordinary file name.
                if segments.is_empty() && is_drive_root(other) {
                    return Err(format!(
                        "Change path '{}' must be repository-relative",
                        trimmed
                    ));
                }
                segments.push(other);
            }
        }
    }
    Ok(segments.join("/"))
}

/// Normalizes and validates a repository-relative path in a single place.
///
/// The shared decode-and-collapse core strips surrounding whitespace and
/// backtick markup, decodes the separator escapes, collapses `.` segments and
/// repeated slashes, and rejects every spelling that can leave the repository.
/// A backslash is read two ways, because a producer and a file name spell the
/// same byte in opposite directions:
///
/// - a single backslash is a separator, so the spelling a Windows producer
///   emits (`src\lib.rs`) addresses `src/lib.rs`;
/// - a doubled backslash names one literal backslash byte, so a file whose own
///   name carries a backslash stays addressable on a file system that permits
///   it: `src\\new.rs` addresses the file literally named `src\new.rs`, while
///   the separator rule alone would silently retarget the identical spelling to
///   the different path `src/new.rs`.
///
/// The rejection follows the decoded structure. The path is rejected when it
/// names no surviving segment, is absolute (a leading forward slash, or a
/// leading literal backslash the separator-doubling escape places at position
/// zero), carries a NUL byte, contains a `..` traversal component, or opens
/// with a Windows drive root. Interior and trailing literal-backslash names
/// keep decoding unchanged, so `src\\new.rs` addresses `src\new.rs` and
/// `src\\` addresses `src\`. This is the crate's single shared path-cleaning
/// routine, so every wire boundary applies identical rules. It is a first line
/// of defence only: `core` must still canonicalize and confine paths against
/// the repository root, because the protocol cannot guarantee that every path
/// flows through this helper.
pub fn normalize_repo_path(raw: &str) -> Result<String, String> {
    let normalized = decode_repo_path(raw)?;
    if normalized.is_empty() {
        return Err("Change path must not be empty".to_string());
    }
    Ok(normalized)
}

/// Deserializes one repository-relative path at the wire boundary.
///
/// The decoded path is normalized and validated through
/// [`normalize_repo_path`], so a field deserialized with this adapter can never
/// address a location outside the repository. A malformed path is reported as a
/// decode failure naming it, because a typed field has no representation for
/// "absent" and must not be dropped silently. This is a first line of defence
/// only: `core` still canonicalizes and confines every path against the
/// repository root.
pub(crate) fn deserialize_repo_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    normalize_repo_path(&raw).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edit_operation_serde() {
        let op = EditOperation::Replace {
            path: "foo.rs".to_string(),
            old_text: "let a = 1;".to_string(),
            new_text: "let a = 2;".to_string(),
        };

        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"replace\""));

        let de: EditOperation = serde_json::from_str(&json).unwrap();
        assert_eq!(de, op);
    }

    fn proposal(op: EditFileOp, path: &str) -> StructuredChangeProposal {
        StructuredChangeProposal {
            op,
            path: path.to_string(),
            to: None,
            old_text: None,
            new_text: None,
            content: None,
            start: None,
            end: None,
        }
    }

    #[test]
    fn test_structured_proposal_conversion() {
        let mut change = proposal(EditFileOp::Create, "new.txt");
        change.content = Some("hello".to_string());

        match change.to_edit_operation().unwrap() {
            EditOperation::Create { path, content } => {
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
    fn test_replace_falls_back_to_content_only_when_new_text_is_absent() {
        // An explicit empty `new_text` is a deletion, even when `content` is
        // populated, and must not be silently coerced into a replacement.
        let mut change = proposal(EditFileOp::Replace, "a.txt");
        change.old_text = Some("remove me".to_string());
        change.new_text = Some(String::new());
        change.content = Some("unexpected".to_string());

        match change.to_edit_operation().unwrap() {
            EditOperation::Replace { new_text, .. } => assert!(new_text.is_empty()),
            other => panic!("Expected Replace operation, got {other:?}"),
        }

        // When `new_text` is absent, the legacy `content` field still applies.
        let mut change = proposal(EditFileOp::Replace, "a.txt");
        change.old_text = Some("old".to_string());
        change.content = Some("new".to_string());

        match change.to_edit_operation().unwrap() {
            EditOperation::Replace { new_text, .. } => assert_eq!(new_text, "new"),
            other => panic!("Expected Replace operation, got {other:?}"),
        }
    }

    #[test]
    fn test_structured_change_op_deserializes_tolerantly() {
        // A known operation is parsed case-insensitively with whitespace
        // ignored, and serializes back to the canonical snake_case wire form.
        let known: StructuredChangeProposal =
            serde_json::from_str(r#"{"op":"  CREATE ","path":"a.txt"}"#).unwrap();
        assert_eq!(known.op, EditFileOp::Create);
        assert_eq!(
            serde_json::to_string(&EditFileOp::Create).unwrap(),
            "\"create\""
        );

        // An unrecognized operation falls back to the default `replace`
        // instead of failing the whole proposal.
        let unknown: StructuredChangeProposal =
            serde_json::from_str(r#"{"op":"frobnicate","path":"a.txt"}"#).unwrap();
        assert_eq!(unknown.op, EditFileOp::Replace);

        // An absent operation uses the same default.
        let absent: StructuredChangeProposal = serde_json::from_str(r#"{"path":"a.txt"}"#).unwrap();
        assert_eq!(absent.op, EditFileOp::Replace);

        // An explicit `null` operation is tolerated exactly like an absent one,
        // so a null-tolerant producer never fails the whole proposal.
        let null_op: StructuredChangeProposal =
            serde_json::from_str(r#"{"op":null,"path":"a.txt"}"#).unwrap();
        assert_eq!(null_op.op, EditFileOp::Replace);
    }

    #[test]
    fn test_normalize_repo_path_collapses_and_rejects() {
        // Dot segments, repeated slashes, and repeated `./` prefixes collapse.
        assert_eq!(normalize_repo_path("./a/./b//c/").unwrap(), "a/b/c");
        assert_eq!(normalize_repo_path(".//a").unwrap(), "a");
        // Surrounding whitespace and backtick markup are stripped before the
        // path is validated, so a markdown-wrapped spelling yields the same
        // clean result as its bare form.
        assert_eq!(normalize_repo_path(" `./src/a.rs` ").unwrap(), "src/a.rs");
        assert_eq!(
            normalize_repo_path("``crates/core``").unwrap(),
            "crates/core"
        );
        assert_eq!(normalize_repo_path("`src/`").unwrap(), "src");

        // Only the surrounding markup is stripped: an interior backtick stays
        // part of the path, and a backticks-only spelling is the empty path it
        // is rather than a literal backtick filename.
        assert_eq!(
            normalize_repo_path("src/`odd`/a.rs").unwrap(),
            "src/`odd`/a.rs"
        );
        assert!(normalize_repo_path("``").is_err());
        assert!(normalize_repo_path(" ` ` ").is_err());

        // Traversal in both separators, absolute paths, drive paths, NUL
        // bytes, and empty results are rejected. The surrounding markup never
        // hides a rejection.
        for raw in [
            "a/../b",
            "a\\..\\b",
            "..",
            ".",
            "/abs",
            "\\abs",
            "C:\\abs",
            "a\0b",
            "`../escape.rs`",
            "``/etc/passwd``",
            "`C:\\Windows`",
        ] {
            assert!(
                normalize_repo_path(raw).is_err(),
                "path '{raw}' must be rejected"
            );
        }
    }

    #[test]
    fn test_normalize_repo_path_addresses_a_literal_backslash_name() {
        // A single backslash keeps the documented Windows tolerance, so the
        // spelling a Windows producer emits still addresses the nested path.
        assert_eq!(normalize_repo_path(r"src\lib.rs").unwrap(), "src/lib.rs");

        // A doubled backslash names one literal backslash byte, so a file whose
        // own name carries one stays addressable instead of being silently
        // retargeted to the nested path the rule above resolves.
        assert_eq!(normalize_repo_path(r"src\\new.rs").unwrap(), "src\\new.rs");
        assert_ne!(
            normalize_repo_path(r"src\\new.rs").unwrap(),
            normalize_repo_path(r"src\new.rs").unwrap()
        );

        // The escape composes with the shared strip rule and with the segment
        // normalization: only the doubled byte survives, and it stays inside
        // the one segment it was written in.
        assert_eq!(
            normalize_repo_path(r" `./src\\new.rs` ").unwrap(),
            "src\\new.rs"
        );
        assert_eq!(normalize_repo_path(r"a\\b/c").unwrap(), "a\\b/c");
        assert_eq!(normalize_repo_path(r"src\\").unwrap(), "src\\");

        // The escape widens nothing: a traversal spelled with single separators
        // is still refused, and a doubled separator cannot smuggle a `..`
        // segment in, because the byte it produces stays part of one name.
        assert!(normalize_repo_path(r"a\..\b").is_err());
        assert_eq!(normalize_repo_path(r"a\\..\\b").unwrap(), "a\\..\\b");
    }

    #[test]
    fn test_a_wire_path_addresses_a_literal_backslash_name() {
        // Four backslashes in the payload decode to the two bytes the escape
        // needs, so the wire form reaches the file literally called
        // `src\new.rs` instead of the nested `src/new.rs`.
        let literal: EditOperation =
            serde_json::from_str(r#"{"type":"create","path":"src\\\\new.rs","content":"x"}"#)
                .expect("an escaped literal-backslash path must decode");
        assert_eq!(
            literal,
            EditOperation::Create {
                path: "src\\new.rs".to_string(),
                content: "x".to_string(),
            }
        );

        // The undoubled spelling keeps the Windows tolerance and addresses the
        // nested file, so the two spellings never collapse into one target.
        let nested: EditOperation =
            serde_json::from_str(r#"{"type":"create","path":"src\\new.rs","content":"x"}"#)
                .expect("a Windows separator spelling must decode");
        assert_eq!(
            nested,
            EditOperation::Create {
                path: "src/new.rs".to_string(),
                content: "x".to_string(),
            }
        );

        // Serialization applies the inverse escape, so a decoded operation
        // survives a round trip instead of being retargeted on the way back in.
        let json = serde_json::to_string(&literal).unwrap();
        assert!(json.contains(r#""path":"src\\\\new.rs""#), "{json}");
        assert_eq!(
            serde_json::from_str::<EditOperation>(&json).unwrap(),
            literal
        );
    }

    #[test]
    fn test_edit_operation_paths_strip_surrounding_backticks_on_decode() {
        // A markdown-wrapped path reaches the engine already clean, so a create
        // never turns the wrapping markup into a phantom leading directory.
        let decoded: EditOperation =
            serde_json::from_str(r#"{"type":"create","path":" `./src/new.rs` ","content":"x"}"#)
                .expect("a wrapped create path must decode");
        assert_eq!(
            decoded,
            EditOperation::Create {
                path: "src/new.rs".to_string(),
                content: "x".to_string(),
            }
        );

        // The surrounding markup never hides a rejection: an escaping or empty
        // spelling outside the repository stays a decode failure naming it.
        for payload in [
            r#"{"type":"delete","path":"`../escape.rs`"}"#,
            r#"{"type":"create","path":"``/etc/passwd``","content":""}"#,
            r#"{"type":"delete","path":"``"}"#,
        ] {
            let error = serde_json::from_str::<EditOperation>(payload)
                .expect_err("a wrapped escaping path must be rejected");
            assert!(
                error.to_string().contains("path"),
                "diagnostic for {payload} must name the rejected path: {error}"
            );
        }
    }

    #[test]
    fn test_structured_proposal_normalizes_and_validates_paths() {
        // Surrounding whitespace and a leading `./` are stripped.
        let mut change = proposal(EditFileOp::Create, "  ./src/new.rs  ");
        change.content = Some("fn x() {}\n".to_string());
        match change.to_edit_operation().unwrap() {
            EditOperation::Create { path, .. } => assert_eq!(path, "src/new.rs"),
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
        change.to = Some("  ./src/b.rs ".to_string());
        match change.to_edit_operation().unwrap() {
            EditOperation::Move { from, to } => {
                assert_eq!(from, "src/a.rs");
                assert_eq!(to, "src/b.rs");
            }
            other => panic!("Expected Move operation, got {other:?}"),
        }

        let mut change = proposal(EditFileOp::Move, "src/a.rs");
        change.to = Some("../escape.rs".to_string());
        assert!(change.to_edit_operation().is_err());
    }

    #[test]
    fn test_line_anchor_proposal_converts_and_normalizes() {
        // A proposal naming both bounds addresses a line range, so it carries
        // the anchors and never echoes the replaced text.
        let mut change = proposal(EditFileOp::Replace, " ./src/a.rs ");
        change.start = Some("Kf9Q".to_string());
        change.end = Some(" Tq2W ".to_string());
        change.content = Some("fn replacement() {}\n".to_string());
        match change.to_edit_operation().unwrap() {
            EditOperation::LineAnchor {
                path,
                start,
                end,
                content,
            } => {
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
        change.start = Some("Kf9Q".to_string());
        change.end = Some("   ".to_string());
        change.old_text = Some("old();".to_string());
        change.new_text = Some("new();".to_string());
        assert!(matches!(
            change.to_edit_operation().unwrap(),
            EditOperation::Replace { .. }
        ));
    }
    #[test]
    fn test_stray_bounds_never_coerce_other_ops_into_a_line_range() {
        // Anchors address replaced lines only, so a bound carried by any other
        // operation is ignored instead of turning that operation into a
        // destructive range replacement.
        let mut moved = proposal(EditFileOp::Move, "src/a.rs");
        moved.to = Some("src/b.rs".to_string());
        moved.start = Some("K1".to_string());
        moved.end = Some("K2".to_string());
        match moved.to_edit_operation_typed() {
            Ok(EditOperation::Move { from, to }) => {
                assert_eq!(from, "src/a.rs");
                assert_eq!(to, "src/b.rs");
            }
            other => panic!("Expected a move that keeps its destination, got {other:?}"),
        }

        let mut created = proposal(EditFileOp::Create, "src/new.rs");
        created.content = Some("fn new() {}\n".to_string());
        created.start = Some("K1".to_string());
        created.end = Some("K2".to_string());
        match created.to_edit_operation_typed() {
            Ok(EditOperation::Create { path, content }) => {
                assert_eq!(path, "src/new.rs");
                assert_eq!(content, "fn new() {}\n");
            }
            other => panic!("Expected a create that keeps its content, got {other:?}"),
        }

        let mut deleted = proposal(EditFileOp::Delete, "src/gone.rs");
        deleted.start = Some("K1".to_string());
        deleted.end = Some("K2".to_string());
        match deleted.to_edit_operation_typed() {
            Ok(EditOperation::Delete { path }) => assert_eq!(path, "src/gone.rs"),
            other => panic!("Expected a delete that keeps its path, got {other:?}"),
        }

        let mut overwritten = proposal(EditFileOp::Overwrite, "src/a.rs");
        overwritten.content = Some("whole file\n".to_string());
        overwritten.start = Some("K1".to_string());
        overwritten.end = Some("K2".to_string());
        match overwritten.to_edit_operation_typed() {
            Ok(EditOperation::Overwrite { path, content }) => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(content, "whole file\n");
            }
            other => panic!("Expected an overwrite that keeps its content, got {other:?}"),
        }
    }

    #[test]
    fn test_replace_without_range_is_rejected_typed() {
        // A replace that names neither anchors nor the replaced text is the one
        // rejection whose wording depends on the active patch strategy, so it
        // carries its own variant together with the rejected path.
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

        // An explicit empty `old_text` is the very same rejection.
        let mut empty_needle = proposal(EditFileOp::Replace, "src/a.rs");
        empty_needle.old_text = Some(String::new());
        assert_eq!(
            empty_needle.to_edit_operation_typed(),
            Err(ChangeConversionError::ReplaceWithoutRange {
                path: "src/a.rs".to_string()
            })
        );

        // The textual adapter renders exactly the message the conversion
        // produced before the rejection became typed.
        assert_eq!(
            change.to_edit_operation(),
            Err("Change 'replace' requires a non-empty 'old_text' field".to_string())
        );
    }

    #[test]
    fn test_anchored_change_keeps_the_fast_path() {
        let mut change = proposal(EditFileOp::Replace, "src/a.rs");
        change.start = Some("Kf9Q".to_string());
        change.end = Some("Tq2W".to_string());
        change.content = Some("fn replacement() {}\n".to_string());

        match change.to_edit_operation_typed() {
            Ok(EditOperation::LineAnchor {
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
            other => panic!("Expected an anchored replacement, got {other:?}"),
        }
    }

    #[test]
    fn test_other_conversion_rejections_keep_their_message() {
        // A rejection that names no strategy vocabulary stays in the shared
        // dictionary and renders verbatim through both entry points.
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

        // A blank path and a traversal destination are shared rejections too.
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
        traversal.to = Some("../escape.rs".to_string());
        assert_eq!(
            traversal.to_edit_operation(),
            Err("Change path '../escape.rs' must not contain '..' components".to_string())
        );
    }

    #[test]
    fn test_move_dir_proposal_names_two_directories_and_no_payload() {
        let mut change = proposal(EditFileOp::MoveDir, " ./src/old ");
        change.to = Some("  ./src/new ".to_string());
        // A directory move names two directories. A stray bound or payload
        // must be ignored instead of turning the move into a range
        // replacement, because a directory never addresses a line.
        change.start = Some("Kf9Q".to_string());
        change.end = Some("Tq2W".to_string());
        change.content = Some("ignored".to_string());

        match change.to_edit_operation_typed() {
            Ok(EditOperation::MoveDir { from, to }) => {
                assert_eq!(from, "src/old");
                assert_eq!(to, "src/new");
            }
            other => panic!("Expected a directory move, got {other:?}"),
        }
    }

    #[test]
    fn test_move_dir_requires_a_destination() {
        // The source and the destination are the whole payload of a directory
        // move, so an absent destination is rejected instead of degrading into
        // a rename of the directory onto itself.
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

        // The destination obeys the same repository-relative rules as the
        // source, and an empty destination is rejected as well.
        let mut traversal = proposal(EditFileOp::MoveDir, "src/old");
        traversal.to = Some("../escape".to_string());
        assert_eq!(
            traversal.to_edit_operation(),
            Err("Change path '../escape' must not contain '..' components".to_string())
        );

        let mut empty = proposal(EditFileOp::MoveDir, "src/old");
        empty.to = Some("   ".to_string());
        assert_eq!(
            empty.to_edit_operation(),
            Err("Change path must not be empty".to_string())
        );
    }

    #[test]
    fn test_move_dir_operation_serde() {
        let op = EditOperation::MoveDir {
            from: "src/old".to_string(),
            to: "src/new".to_string(),
        };
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(
            json,
            r#"{"type":"move_dir","from":"src/old","to":"src/new"}"#
        );

        let decoded: EditOperation = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, op);
    }
    #[test]
    fn test_edit_operation_paths_are_confined_at_the_wire_boundary() {
        // A benign spelling is normalized on decode, so the engine never has to
        // re-clean a path the boundary could have cleaned.
        let decoded: EditOperation =
            serde_json::from_str(r#"{"type":"move","from":"./src/a.rs","to":" ./src/b.rs "}"#)
                .expect("a benign move must decode");
        assert_eq!(
            decoded,
            EditOperation::Move {
                from: "src/a.rs".to_string(),
                to: "src/b.rs".to_string(),
            }
        );

        // An escaping path is a decode failure naming it, because a typed enum
        // variant cannot silently drop itself.
        for payload in [
            r#"{"type":"delete","path":"../escape.rs"}"#,
            r#"{"type":"create","path":"/etc/passwd","content":""}"#,
            r#"{"type":"replace","path":"a/../b.rs","old_text":"a","new_text":"b"}"#,
            r#"{"type":"overwrite","path":"C:\\Windows","content":""}"#,
            r#"{"type":"line_anchor","path":"../escape.rs","start":"K1","end":"K2","content":"x"}"#,
            r#"{"type":"move_dir","from":"../escape","to":"src/new"}"#,
        ] {
            let error = serde_json::from_str::<EditOperation>(payload)
                .expect_err("an escaping edit path must be rejected");
            assert!(
                error.to_string().contains("path"),
                "diagnostic for {payload} must name the rejected path: {error}"
            );
        }
    }
    #[test]
    fn test_absolute_rejection_is_derived_from_the_decoded_structure() {
        // A doubled leading backslash decodes to a literal backslash byte at
        // position zero, which names a Windows drive root or a UNC server
        // root. Every spelling that produces it is absolute and leaves the
        // repository, although none of them opens with a forward slash.
        for raw in [
            r"\\server\share",
            r"\\etc\passwd",
            r"\\?\C:\x",
            r"\\",
            r"\abs",
            "/abs",
        ] {
            let rejection = normalize_repo_path(raw).expect_err("absolute path must be rejected");
            assert!(
                rejection.contains("repository-relative"),
                "diagnostic for '{raw}' must name the absolute spelling: {rejection}"
            );
        }

        // The drive root is decided from the first segment that survives the
        // dot collapse, so a spelling that only reaches it through dot
        // segments is refused exactly like the bare drive path.
        for raw in ["C:/Windows", "./C:/Windows", "././C:name", r"C:\Windows"] {
            assert!(normalize_repo_path(raw).is_err(), "drive path '{raw}'");
        }

        // The same bytes in a later segment stay part of a file name, and
        // interior or trailing literal backslashes keep decoding unchanged.
        assert_eq!(normalize_repo_path("src/C:/x").unwrap(), "src/C:/x");
        assert_eq!(normalize_repo_path("src/C:name").unwrap(), "src/C:name");
        assert_eq!(normalize_repo_path(r"src\\new.rs").unwrap(), "src\\new.rs");
        assert_eq!(normalize_repo_path(r"src\\").unwrap(), "src\\");
        assert_eq!(normalize_repo_path(r"a\\..\\b").unwrap(), "a\\..\\b");
    }

    #[test]
    fn test_absolute_escape_spellings_fail_the_wire_and_conversion_boundaries() {
        // The same absolute spellings are refused when they reach the edit
        // boundary through serde, so no typed operation ever carries a target
        // rooted outside the repository.
        for payload in [
            r#"{"type":"delete","path":"\\\\server\\share"}"#,
            r#"{"type":"create","path":"\\\\etc\\passwd","content":""}"#,
            r#"{"type":"overwrite","path":"\\\\?\\C:\\x","content":"x"}"#,
            r#"{"type":"delete","path":"./C:/x"}"#,
        ] {
            let error = serde_json::from_str::<EditOperation>(payload)
                .expect_err("an absolute edit path must be rejected");
            assert!(
                error.to_string().contains("repository-relative"),
                "diagnostic for {payload} must name the absolute spelling: {error}"
            );
        }

        // The proposal conversion applies the very same derived rules.
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
    fn test_decode_repo_path_reports_the_root_and_refuses_absolute_spellings() {
        // The shared core returns the collapsed segment structure, so a
        // spelling that leaves no surviving segment yields the empty path.
        // Every whole-repository caller derives its decision from that empty
        // result instead of from an enumerated spelling list.
        for raw in ["", ".", "./", "./.", "././", ".//", r".\."] {
            assert_eq!(decode_repo_path(raw), Ok(String::new()));
        }

        // A leading separator is absolute whether a forward slash spells it or
        // the separator-doubling escape places a literal backslash byte at
        // position zero, so the core never reports such a spelling as a root.
        for raw in ["/", "///", r"\\server\share", r"\\?\C:\x", r"\\"] {
            assert!(decode_repo_path(raw).is_err());
        }

        // The traversal and drive-root rules keep their behaviour, so the
        // derived root never widens acceptance beyond the decoded structure.
        assert!(decode_repo_path("../escape.rs").is_err());
        assert!(decode_repo_path("C:/Windows").is_err());
    }
}
