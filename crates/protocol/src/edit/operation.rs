//! Typed edit operation structs, the substrate wrapper enum, and repository path confinement.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// Replaces a matched text block in an existing file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditReplace {
    #[serde(with = "repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub path: String,
    pub old_text: String,
    pub new_text: String,
}

/// Creates a new file with the given full content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditCreate {
    #[serde(with = "repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub path: String,
    pub content: String,
}

/// Deletes an existing file from the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditDelete {
    #[serde(with = "repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub path: String,
}

/// Moves or renames a single file within the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditMove {
    #[serde(with = "repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub from: String,
    #[serde(with = "repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub to: String,
}

/// Moves or renames a directory subtree within the repository.
///
/// A directory rename is one operation rather than one entry per file: the
/// staging engine expands it into the per-file moves of the subtree under
/// `from`. No replaced lines, range bounds, or payload belong to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditMoveDir {
    #[serde(with = "repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub from: String,
    #[serde(with = "repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub to: String,
}

/// Overwrites an existing file with the given full content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditOverwrite {
    #[serde(with = "repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub path: String,
    pub content: String,
}

/// Replaces a range of lines that the proposal names by opaque anchors.
///
/// `start` and `end` are inclusive bounds rendered beside the context lines
/// and decoded deterministically by the staging engine. `content` holds the
/// complete replacement of the addressed range, so the replaced text never
/// has to be echoed back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditLineAnchor {
    #[serde(with = "repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub path: String,
    pub start: String,
    pub end: String,
    pub content: String,
}

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
    Replace(EditReplace),
    Create(EditCreate),
    Delete(EditDelete),
    Move(EditMove),
    /// Moves a directory subtree, naming the source and the destination.
    ///
    /// A directory rename is one operation rather than one entry per file: the
    /// staging engine expands it into the per-file moves of the subtree under
    /// `from`. No replaced lines, range bounds, or payload belong to it.
    MoveDir(EditMoveDir),
    Overwrite(EditOverwrite),
    /// Replaces a range of lines that the proposal names by opaque anchors.
    ///
    /// `start` and `end` are inclusive bounds rendered beside the context lines
    /// and decoded deterministically by the staging engine. `content` holds the
    /// complete replacement of the addressed range, so the replaced text never
    /// has to be echoed back.
    LineAnchor(EditLineAnchor),
}

impl From<EditReplace> for EditOperation {
    fn from(op: EditReplace) -> Self {
        Self::Replace(op)
    }
}

impl From<EditCreate> for EditOperation {
    fn from(op: EditCreate) -> Self {
        Self::Create(op)
    }
}

impl From<EditDelete> for EditOperation {
    fn from(op: EditDelete) -> Self {
        Self::Delete(op)
    }
}

impl From<EditMove> for EditOperation {
    fn from(op: EditMove) -> Self {
        Self::Move(op)
    }
}

impl From<EditMoveDir> for EditOperation {
    fn from(op: EditMoveDir) -> Self {
        Self::MoveDir(op)
    }
}

impl From<EditOverwrite> for EditOperation {
    fn from(op: EditOverwrite) -> Self {
        Self::Overwrite(op)
    }
}

impl From<EditLineAnchor> for EditOperation {
    fn from(op: EditLineAnchor) -> Self {
        Self::LineAnchor(op)
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

/// Renders the schema of one plain path property in place.
///
/// The wire form of a path is normalized and confined by `repo_path`, which is a
/// serde adapter module rather than a type, so a derived schema must not be
/// built from it. A path is a string on the wire, and its schema states exactly
/// that where the property is used.
fn inline_string_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    <String as schemars::JsonSchema>::json_schema(gen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edit_operation_serde() {
        let op = EditOperation::Replace(EditReplace {
            path: "foo.rs".to_string(),
            old_text: "let a = 1;".to_string(),
            new_text: "let a = 2;".to_string(),
        });

        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"type\":\"replace\""));

        let de: EditOperation = serde_json::from_str(&json).unwrap();
        assert_eq!(de, op);
    }

    #[test]
    fn test_named_operation_structs_serde_and_path_confinement() {
        let replace: EditReplace =
            serde_json::from_str(r#"{"path":" ./src/a.rs ","old_text":"a","new_text":"b"}"#)
                .unwrap();
        assert_eq!(replace.path, "src/a.rs");
        assert_eq!(
            serde_json::from_str::<EditReplace>(&serde_json::to_string(&replace).unwrap()).unwrap(),
            replace
        );

        let create: EditCreate =
            serde_json::from_str(r#"{"path":"`src/b.rs`","content":"x"}"#).unwrap();
        assert_eq!(create.path, "src/b.rs");

        let delete: EditDelete = serde_json::from_str(r#"{"path":"./src/c.rs"}"#).unwrap();
        assert_eq!(delete.path, "src/c.rs");

        let moved: EditMove =
            serde_json::from_str(r#"{"from":"./src/a.rs","to":"./src/b.rs"}"#).unwrap();
        assert_eq!(moved.from, "src/a.rs");
        assert_eq!(moved.to, "src/b.rs");

        let move_dir: EditMoveDir =
            serde_json::from_str(r#"{"from":"./old","to":"./new"}"#).unwrap();
        assert_eq!(move_dir.from, "old");
        assert_eq!(move_dir.to, "new");

        let overwrite: EditOverwrite =
            serde_json::from_str(r#"{"path":"./src/d.rs","content":"y"}"#).unwrap();
        assert_eq!(overwrite.path, "src/d.rs");

        let anchor: EditLineAnchor =
            serde_json::from_str(r#"{"path":"./src/e.rs","start":"K1","end":"K2","content":"z"}"#)
                .unwrap();
        assert_eq!(anchor.path, "src/e.rs");

        assert!(serde_json::from_str::<EditDelete>(r#"{"path":"../escape.rs"}"#).is_err());
        assert!(serde_json::from_str::<EditMove>(r#"{"from":"a.rs","to":"/abs"}"#).is_err());
        assert!(serde_json::from_str::<EditMoveDir>(r#"{"from":"../old","to":"new"}"#).is_err());
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
            EditOperation::Create(EditCreate {
                path: "src\\new.rs".to_string(),
                content: "x".to_string(),
            })
        );

        // The undoubled spelling keeps the Windows tolerance and addresses the
        // nested file, so the two spellings never collapse into one target.
        let nested: EditOperation =
            serde_json::from_str(r#"{"type":"create","path":"src\\new.rs","content":"x"}"#)
                .expect("a Windows separator spelling must decode");
        assert_eq!(
            nested,
            EditOperation::Create(EditCreate {
                path: "src/new.rs".to_string(),
                content: "x".to_string(),
            })
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
            EditOperation::Create(EditCreate {
                path: "src/new.rs".to_string(),
                content: "x".to_string(),
            })
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
    fn test_move_dir_operation_serde() {
        let op = EditOperation::MoveDir(EditMoveDir {
            from: "src/old".to_string(),
            to: "src/new".to_string(),
        });
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
            EditOperation::Move(EditMove {
                from: "src/a.rs".to_string(),
                to: "src/b.rs".to_string(),
            })
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
    fn test_absolute_escape_spellings_fail_the_edit_operation_wire_boundary() {
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
