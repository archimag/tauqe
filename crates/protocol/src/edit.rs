use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum EditFileOp {
    Replace,
    Create,
    Delete,
    Move,
    Overwrite,
}

impl std::convert::TryFrom<String> for EditFileOp {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl EditFileOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Create => "create",
            Self::Delete => "delete",
            Self::Move => "move",
            Self::Overwrite => "overwrite",
        }
    }
}

impl fmt::Display for EditFileOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for EditFileOp {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "replace" => Ok(Self::Replace),
            "create" => Ok(Self::Create),
            "delete" => Ok(Self::Delete),
            "move" => Ok(Self::Move),
            "overwrite" => Ok(Self::Overwrite),
            other => Err(format!(
                "Unknown change operation '{}'. Expected replace, create, delete, move, or overwrite",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum EditFileStatus {
    Ok,
    Error,
}

impl std::convert::TryFrom<String> for EditFileStatus {
    type Error = String;

    fn try_from(value: String) -> Result<Self, String> {
        value.parse()
    }
}

impl EditFileStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for EditFileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for EditFileStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ok" => Ok(Self::Ok),
            "error" => Ok(Self::Error),
            other => Err(format!(
                "Unknown edit file status '{}'. Expected ok or error",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditOperation {
    Replace {
        path: String,
        old_text: String,
        new_text: String,
    },
    Create {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
    Overwrite {
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
    /// A proposal that names both anchor bounds addresses a line range instead
    /// of a text block, so it becomes [`EditOperation::LineAnchor`] and the
    /// strategy that rendered the anchors decides how the range is applied.
    ///
    /// The decision made here never differs from the one of
    /// [`Self::to_edit_operation`]: only the rejection a caller observes is
    /// typed, so the strategy-dependent one can be reworded elsewhere.
    pub fn to_edit_operation_typed(&self) -> Result<EditOperation, ChangeConversionError> {
        let path = normalize_repo_path(&self.path).map_err(ChangeConversionError::Other)?;
        if let (Some(start), Some(end)) = (self.start.as_deref(), self.end.as_deref()) {
            let start = start.trim();
            let end = end.trim();
            if !start.is_empty() && !end.is_empty() {
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

/// Normalizes and validates a repository-relative path in a single place.
///
/// Surrounding whitespace, backslashes, `.` segments, repeated slashes, and
/// leading `./` components are normalized, then the path is rejected when it is
/// empty, absolute, a Windows drive path, contains a `..` traversal component,
/// or carries a NUL byte. This is the crate's single shared path-cleaning
/// routine, so every wire boundary applies identical rules. It is a first line
/// of defence only: `core` must still canonicalize and confine paths against
/// the repository root, because the protocol cannot guarantee that every path
/// flows through this helper.
pub fn normalize_repo_path(raw: &str) -> Result<String, String> {
    if raw.contains('\0') {
        return Err("Change path must not contain NUL bytes".to_string());
    }
    let slashed = raw.trim().replace('\\', "/");
    let trimmed = slashed.trim();
    if trimmed.is_empty() {
        return Err("Change path must not be empty".to_string());
    }
    if trimmed.starts_with('/') {
        return Err(format!(
            "Change path '{}' must be repository-relative",
            trimmed
        ));
    }
    if trimmed.len() >= 2
        && trimmed.as_bytes()[1] == b':'
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
    {
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
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return Err("Change path must not be empty".to_string());
    }
    Ok(segments.join("/"))
}

// Structured Edit Events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditStartedEvent {
    pub operation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFileStartedEvent {
    pub operation_id: String,
    pub path: String,
    pub op_type: EditFileOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditHunkEvent {
    pub operation_id: String,
    pub path: String,
    pub hunk_index: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFileDoneEvent {
    pub operation_id: String,
    pub path: String,
    pub status: EditFileStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub hunks_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFileRetryingEvent {
    pub operation_id: String,
    pub path: String,
    pub attempt: usize,
    pub max_retries: usize,
    pub reason: String,
}

/// Result of applying an edit proposal.
///
/// Shared by the turn result ([`crate::ModelResult::Edit`]) and the streamed
/// completion event ([`EditFinishedEvent`]) so the applied state, diagnostics,
/// changed files, and commit hash have a single authoritative definition
/// instead of being duplicated field-by-field across two wire types.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EditOutcome {
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditFinishedEvent {
    pub operation_id: String,
    #[serde(flatten)]
    pub outcome: EditOutcome,
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
    fn test_edit_file_op_serde_and_parsing() {
        let op = EditFileOp::Replace;
        assert_eq!(serde_json::to_string(&op).unwrap(), "\"replace\"");
        assert_eq!(
            serde_json::from_str::<EditFileOp>("\"create\"").unwrap(),
            EditFileOp::Create
        );
        assert_eq!(
            serde_json::from_str::<EditFileOp>("\"  OVERWRITE \\n\"").unwrap(),
            EditFileOp::Overwrite
        );
        assert_eq!(
            serde_json::from_str::<EditFileOp>("\"REPLACE\"").unwrap(),
            EditFileOp::Replace
        );
        assert_eq!(
            "  OVERWRITE \n".parse::<EditFileOp>().unwrap(),
            EditFileOp::Overwrite
        );
        assert!("unknown".parse::<EditFileOp>().is_err());
        assert_eq!(EditFileOp::Move.as_str(), "move");
        assert_eq!(EditFileOp::Delete.to_string(), "delete");
    }

    #[test]
    fn test_edit_file_status_serde_and_parsing() {
        let status = EditFileStatus::Ok;
        assert_eq!(serde_json::to_string(&status).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::from_str::<EditFileStatus>("\"error\"").unwrap(),
            EditFileStatus::Error
        );
        assert_eq!(
            serde_json::from_str::<EditFileStatus>("\" OK \"").unwrap(),
            EditFileStatus::Ok
        );
        assert_eq!(
            serde_json::from_str::<EditFileStatus>("\"ERROR\"").unwrap(),
            EditFileStatus::Error
        );
        assert_eq!(
            " OK ".parse::<EditFileStatus>().unwrap(),
            EditFileStatus::Ok
        );
        assert!("unknown".parse::<EditFileStatus>().is_err());
        assert_eq!(EditFileStatus::Error.as_str(), "error");
        assert_eq!(EditFileStatus::Ok.to_string(), "ok");
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

        // Traversal in both separators, absolute paths, drive paths, NUL
        // bytes, and empty results are rejected.
        for raw in [
            "a/../b", "a\\..\\b", "..", ".", "/abs", "\\abs", "C:\\abs", "a\0b",
        ] {
            assert!(
                normalize_repo_path(raw).is_err(),
                "path '{raw}' must be rejected"
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
}
