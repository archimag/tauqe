//! Normalized model-output command vocabulary.
//!
//! The protocol-specific syntax (XML tags, JSON schema) is translated into the
//! uniform [`LlmCommand`] list declared here, so the core pipeline operates
//! purely on semantics. Commands and their payloads belong to the engine; server
//! maps client-visible results into independent RPC DTOs.

use crate::context::ContextAccess;
use serde::{Deserialize, Serialize};

use crate::edits::EditCreate;
use crate::edits::EditDelete;
use crate::edits::EditLineAnchor;
use crate::edits::EditMove;
use crate::edits::EditMoveDir;
use crate::edits::EditOperation;
use crate::edits::EditOverwrite;
use crate::edits::EditReplace;
use crate::llm::discussion::DiscussionPlanUpdate;

use crate::llm::DocTopic;

/// A request to add a file to the context at a given access level.
///
/// The path follows the same wire rules as an edit path: the serde boundary
/// normalizes a benign spelling and rejects a spelling that escapes the
/// repository, because a typed value cannot silently drop itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextRequestCommand {
    #[serde(deserialize_with = "crate::edits::operation::deserialize_repo_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub(crate) path: String,
    #[schemars(schema_with = "inline_context_access_schema")]
    pub(crate) access: ContextAccess,
}

/// A search request across repository files using Rust/ripgrep regex.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoSearchCommand {
    #[serde(deserialize_with = "crate::llm::discussion::normalize::deserialize_repo_map_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub(crate) path: String,
    pub(crate) regex: String,
}

/// A request for the compact repository directory tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoDirsCommand {}

/// A request for the targeted symbol map of one subdirectory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoMapCommand {
    #[serde(deserialize_with = "crate::llm::discussion::normalize::deserialize_repo_map_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub(crate) path: String,
}

/// A request for the tracked-file listing of one directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DirListCommand {
    #[serde(deserialize_with = "crate::llm::discussion::normalize::deserialize_repo_map_path")]
    #[schemars(schema_with = "inline_string_schema")]
    pub(crate) path: String,
    pub(crate) recursive: bool,
}

/// A request for harness system documentation by topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocRequestCommand {
    #[serde(default)]
    #[schemars(schema_with = "inline_doc_topic_schema")]
    pub(crate) topic: DocTopic,
}

/// A request for one external document declared by the project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OuterDocRequestCommand {
    pub(crate) id: String,
}

/// Ephemeral working-memory notes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScratchpadCommand {
    /// Turn-local working memory: working hypotheses, verified findings, and the
    /// purpose of context still needed; empty when there is nothing to record.
    pub(crate) notes: String,
}

/// Conversational text emitted to the user interface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextCommand {
    /// Conversational answer to the developer, formulated in the language of the
    /// request as readable Markdown.
    pub content: String,
}

/// Semantic blocker emitted when a precondition or assumption fails.
///
/// `details` is an obligatory string: a blocker that carries no expansion
/// states an empty one, so the wire form never depends on the presence of the
/// key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockedCommand {
    pub(crate) reason: String,
    #[serde(default)]
    pub(crate) details: String,
}

impl BlockedCommand {
    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn details(&self) -> &str {
        &self.details
    }
}

/// Unconditional abort emitted when only the developer can act on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AbortCommand {
    pub(crate) reason: String,
    pub(crate) recommendation: String,
}

/// Terminal commit message stated by the model when the turn succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitMessageCommand {
    /// English Conventional Commits header (`type(scope): description`) in
    /// imperative mood, lowercase, without a trailing period, at most 72
    /// characters; empty when the turn did not succeed.
    pub(crate) commit_message: String,
}

/// Terminal error stated by the model when the turn failed.
///
/// The error travels in `error_message`, a channel of its own: the
/// conversational `message` field remains the user-facing answer, so a failing
/// turn can never present its error as a written reply. An absent key states
/// the empty string, so a round never fails to decode for omitting the channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnErrorCommand {
    /// Error message describing why the turn failed; empty otherwise.
    #[serde(default)]
    pub(crate) error_message: String,
}

impl TurnErrorCommand {
    pub fn error_message(&self) -> &str {
        &self.error_message
    }
}

/// Terminal skip reason stated by the model when the work is already satisfied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnSkipCommand {
    /// Reason the work is already satisfied and no change is required; empty
    /// otherwise.
    pub(crate) reason: String,
}

/// A typed invocation of one project-declared custom tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CustomToolCommand {
    pub(crate) name: String,
    #[serde(default)]
    #[schemars(schema_with = "free_form_json_schema")]
    pub(crate) args: serde_json::Value,
}

/// Normalized intent decoded from raw model output.
///
/// The protocol-specific syntax (XML tags, JSON schema) is translated into this
/// uniform command list so the core pipeline operates purely on semantics. Each
/// command corresponds to an enabled an engine capability.
///
/// `Eq` is deliberately absent: a custom tool invocation carries arbitrary JSON
/// arguments, and a JSON number may hold a float, so the command list is
/// comparable only through `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmCommand {
    /// Conversational text emitted to the user interface.
    Text(TextCommand),
    /// Replace text in an existing file.
    EditReplace(EditReplace),
    /// Atomic Codex patch staged as a single document.
    ApplyPatch { patch: String },
    /// Create a new file.
    EditCreate(EditCreate),
    /// Delete an existing file.
    EditDelete(EditDelete),
    /// Move or rename an existing file.
    EditMove(EditMove),
    /// Move or rename a directory subtree.
    EditMoveDir(EditMoveDir),
    /// Overwrite an existing file.
    EditOverwrite(EditOverwrite),
    /// Replace a line-anchor range in an existing file.
    EditLineAnchor(EditLineAnchor),
    /// A request to add a file to the context at a given access level.
    ///
    /// The path follows the same wire rules as an [`EditOperation`] path: the
    /// serde boundary normalizes a benign spelling and rejects a spelling that
    /// escapes the repository, because a typed variant cannot silently drop
    /// itself. The engine still confines the path against the repository root.
    ContextRequest(ContextRequestCommand),
    /// A search request across repository files using Rust/ripgrep regex.
    ///
    /// `path` is repository-relative and confines the search to that subtree;
    /// an empty path names the whole repository. The serde boundary applies the
    /// crate's shared repository-path rules, so a benign spelling such as
    /// `./src` normalizes to `src` and a spelling that escapes the repository
    /// surfaces as a decode failure naming it instead of reaching the engine: a
    /// typed variant cannot silently drop itself. A programmatic producer hands
    /// over an already-normalized path, and the engine still confines the path
    /// at execution, exactly as
    /// [`normalize_repo_path`](crate::edits::normalize_repo_path) documents.
    RepoSearch(RepoSearchCommand),
    /// A request for the compact repository directory tree.
    RepoDirs(RepoDirsCommand),
    /// A request for the targeted symbol map of one subdirectory.
    ///
    /// The path is a repository-relative prefix matching whole path components;
    /// an empty path names the whole repository. The serde boundary applies the
    /// same rules as the search request: a benign spelling normalizes, and an
    /// escaping one is a decode failure naming it.
    RepoMap(RepoMapCommand),
    /// A request for the tracked-file listing of one directory.
    ///
    /// The path is a repository-relative directory prefix matching whole path
    /// components; an empty path names the repository root. `recursive` widens
    /// the listing from the direct children to every tracked file underneath,
    /// keeping the default request shallow. The serde boundary applies the same
    /// rules as the search request, so a listing can never address a location
    /// outside the repository.
    DirList(DirListCommand),
    /// A request for harness system documentation by topic.
    ///
    /// The command is canonically spelled `tauqe_doc_request`, and the
    /// pre-canonical `doc_request` stays a declared alias, so a payload written
    /// before the rename keeps decoding identically.
    ///
    /// The topic may be omitted: a payload without one decodes to
    /// [`DocTopic::All`], so the documented missing-topic fallback holds on
    /// this boundary as well.
    #[serde(rename = "tauqe_doc_request", alias = "doc_request")]
    DocRequest(DocRequestCommand),
    /// A request for one external document declared by the project.
    ///
    /// The identifier names an entry of `[[outer_docs]]`, never a path: where
    /// the materialized copy is cached stays an implementation detail of the
    /// harness, so a request addresses only material the project itself
    /// declared. A document that publishes a structured index answers with the
    /// chapters it exposes, and the material it delivers enters the context as
    /// read-only reference.
    OuterDocRequest(OuterDocRequestCommand),
    /// A plan creation or modification payload.
    Plan { update: DiscussionPlanUpdate },
    /// A decision recorded for one finding of a review session.
    ///
    /// The decision is authorized by the declared regime of the turn: only a
    /// review discussion carries the capability, so the harness applies it
    /// against the session the turn names and never against whichever session
    /// happens to be active.
    ReviewUpdate {
        update: crate::llm::discussion::ReviewUpdateProposal,
    },
    /// Ephemeral working-memory notes.
    Scratchpad(ScratchpadCommand),
    /// Terminal commit message stated by the model when the turn succeeded.
    CommitMessage(CommitMessageCommand),
    /// Terminal error stated by the model when the turn failed.
    TurnError(TurnErrorCommand),
    /// Terminal skip reason stated by the model when the work is already satisfied.
    TurnSkip(TurnSkipCommand),
    /// Explicit semantic blocker emitted when plan invariants or preconditions fail.
    Blocked(BlockedCommand),
    /// Explicit unconditional abort emitted when external host environment actions or developer intervention are required.
    Abort(AbortCommand),
    /// A typed invocation of one project-declared custom tool.
    ///
    /// `name` addresses a command registered in the project configuration, and
    /// `args` carries the typed argument object the command declares. The
    /// harness validates both against the declared allowlist, so an invocation
    /// can never widen into an arbitrary shell command.
    CustomTool(CustomToolCommand),
    /// Protocol syntax or validation error encountered while parsing model output.
    ProtocolError { message: String },
}

impl LlmCommand {
    /// Extracts the underlying [`EditOperation`] if this command is a file edit.
    pub(crate) fn into_edit_operation(self) -> Option<EditOperation> {
        match self {
            Self::EditReplace(op) => Some(op.into()),
            Self::EditCreate(op) => Some(op.into()),
            Self::EditDelete(op) => Some(op.into()),
            Self::EditMove(op) => Some(op.into()),
            Self::EditMoveDir(op) => Some(op.into()),
            Self::EditOverwrite(op) => Some(op.into()),
            Self::EditLineAnchor(op) => Some(op.into()),
            _ => None,
        }
    }
}

/// Renders the schema of one plain string property in place.
///
/// A property whose wire form is produced by a serde adapter module is a string
/// on the wire, so its schema states exactly that where the property is used and
/// the derived schema of the command stays self-contained.
fn inline_string_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    <String as schemars::JsonSchema>::json_schema(gen)
}

/// Renders the derived schema of one enumeration property in place.
///
/// The strict response schema is self-contained: a property spells out its own
/// type where it is used, so a consumer resolves no external definition. Each
/// function below delegates to the derived schema of the enumeration itself, so
/// its wire names and its documented variants stay one declaration, and
/// `flat_string_enum_schema` reduces that schema to the plain enumeration a
/// strict property carries.
fn inline_context_access_schema(
    gen: &mut schemars::gen::SchemaGenerator,
) -> schemars::schema::Schema {
    flat_string_enum_schema(<ContextAccess as schemars::JsonSchema>::json_schema(gen))
}

/// Renders the documentation topic enumeration in place.
fn inline_doc_topic_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    flat_string_enum_schema(<DocTopic as schemars::JsonSchema>::json_schema(gen))
}

/// Reduces a derived enumeration schema to one flat string enumeration.
///
/// A fieldless enumeration whose variants carry documentation cannot stay a
/// plain `enum` in the derived form: a per-variant description is expressible
/// only through the `oneOf` composition keyword, which a strict flat schema
/// refuses. The variants still are the single source of truth for the member
/// list, so the derived schema is rendered once and its branches are collapsed
/// into that member list, while the description of the enumeration itself stays
/// on the property.
fn flat_string_enum_schema(schema: schemars::schema::Schema) -> schemars::schema::Schema {
    let mut object = match schema {
        schemars::schema::Schema::Object(object) => object,
        other => return other,
    };
    let mut subschemas = match object.subschemas.take() {
        Some(subschemas) => subschemas,
        None => return schemars::schema::Schema::Object(object),
    };
    let branches = match subschemas.one_of.take() {
        Some(branches) => branches,
        None => {
            object.subschemas = Some(subschemas);
            return schemars::schema::Schema::Object(object);
        }
    };
    let mut members = Vec::with_capacity(branches.len());
    for branch in branches {
        let schemars::schema::Schema::Object(branch) = branch else {
            continue;
        };
        members.extend(branch.enum_values.unwrap_or_default());
    }
    object.instance_type = Some(schemars::schema::InstanceType::String.into());
    object.enum_values = Some(members);
    schemars::schema::Schema::Object(object)
}

/// Accepts any JSON value as the arguments of a custom tool invocation.
///
/// A project command declares the shape of its own arguments, and the harness
/// validates them against that declaration, so the model-facing schema of the
/// envelope states only that the value is free-form.
fn free_form_json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    schemars::schema::Schema::Bool(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_command_text_and_edit_serde_roundtrip() {
        let text = LlmCommand::Text(TextCommand {
            content: "Refactored the module".to_string(),
        });
        let json = serde_json::to_string(&text).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), text);

        let edit = LlmCommand::EditReplace(EditReplace {
            path: "src/lib.rs".to_string(),
            old_text: "let a = 1;".to_string(),
            new_text: "let a = 2;".to_string(),
        });
        let json = serde_json::to_string(&edit).unwrap();
        assert!(json.contains("\"type\":\"edit_replace\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), edit);
    }

    #[test]
    fn test_llm_command_discovery_serde_roundtrip() {
        let request = LlmCommand::ContextRequest(ContextRequestCommand {
            path: "src/a.rs".to_string(),
            access: ContextAccess::Editable,
        });
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"context_request\""));
        assert!(json.contains("\"access\":\"editable\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), request);

        let search = LlmCommand::RepoSearch(RepoSearchCommand {
            path: "src".to_string(),
            regex: "fn main".to_string(),
        });
        let json = serde_json::to_string(&search).unwrap();
        assert!(json.contains("\"type\":\"repo_search\""));
        assert!(json.contains("\"path\":\"src\""));
        assert!(json.contains("\"regex\":\"fn main\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), search);

        let doc = LlmCommand::DocRequest(DocRequestCommand {
            topic: DocTopic::Workflow,
        });
        let json = serde_json::to_string(&doc).unwrap();
        assert!(json.contains("\"type\":\"tauqe_doc_request\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), doc);
        // The pre-canonical spelling stays a declared alias, so a payload
        // written before the rename keeps decoding identically.
        let legacy: LlmCommand =
            serde_json::from_str(r#"{"type":"doc_request","topic":"workflow"}"#)
                .expect("the legacy spelling must decode");
        assert_eq!(legacy, doc);

        let outer_doc = LlmCommand::OuterDocRequest(OuterDocRequestCommand {
            id: "mylib".to_string(),
        });
        let json = serde_json::to_string(&outer_doc).unwrap();
        assert!(json.contains("\"type\":\"outer_doc_request\""));
        assert!(json.contains("\"id\":\"mylib\""));
        assert_eq!(
            serde_json::from_str::<LlmCommand>(&json).unwrap(),
            outer_doc
        );

        let dirs = LlmCommand::RepoDirs(RepoDirsCommand {});
        let json = serde_json::to_string(&dirs).unwrap();
        assert!(json.contains("\"type\":\"repo_dirs\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), dirs);

        let map = LlmCommand::RepoMap(RepoMapCommand {
            path: "crates/core".to_string(),
        });
        let json = serde_json::to_string(&map).unwrap();
        assert!(json.contains("\"type\":\"repo_map\""));
        assert!(json.contains("\"path\":\"crates/core\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), map);

        let dir_list = LlmCommand::DirList(DirListCommand {
            path: "crates/core".to_string(),
            recursive: true,
        });
        let json = serde_json::to_string(&dir_list).unwrap();
        assert!(json.contains("\"type\":\"dir_list\""));
        assert!(json.contains("\"path\":\"crates/core\""));
        assert!(json.contains("\"recursive\":true"));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), dir_list);
    }
    /// Returns one payload per command that carries a repository path.
    ///
    /// Every entry addresses the same spelling, so one loop proves that every
    /// command applies the path rules the search command already applied.
    fn path_payloads(path: &str) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"type": "context_request", "path": path, "access": "read_only"}),
            serde_json::json!({"type": "repo_search", "path": path, "regex": "fn main"}),
            serde_json::json!({"type": "repo_map", "path": path}),
            serde_json::json!({"type": "dir_list", "path": path, "recursive": true}),
            serde_json::json!({"type": "edit_delete", "path": path}),
            serde_json::json!({"type": "edit_move", "from": path, "to": "src/b.rs"}),
            serde_json::json!({"type": "edit_move_dir", "from": "src/old", "to": path}),
        ]
    }

    /// Reproduces the spelling a repository path decodes to under the shared
    /// separator rule: a doubled backslash names one literal byte, while a
    /// single backslash is a separator.
    ///
    /// A rejection diagnostic names the decoded path, so the expectation of an
    /// escaping-spelling case is derived here instead of by a blanket
    /// replacement that would also turn the literal byte into a separator.
    fn decoded_spelling(raw: &str) -> String {
        let mut decoded = String::with_capacity(raw.len());
        let mut chars = raw.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                decoded.push(ch);
                continue;
            }
            if chars.peek() == Some(&'\\') {
                chars.next();
                decoded.push('\\');
            } else {
                decoded.push('/');
            }
        }
        decoded
    }

    #[test]
    fn test_every_command_path_is_normalized_or_rejected() {
        // A benign spelling reaches the engine already normalized, so no
        // consumer has to re-clean a path the boundary could have cleaned.
        let context: LlmCommand = serde_json::from_value(serde_json::json!({
            "type": "context_request",
            "path": " ./src/a.rs ",
            "access": "editable",
        }))
        .expect("a benign context path must decode");
        assert_eq!(
            context,
            LlmCommand::ContextRequest(ContextRequestCommand {
                path: "src/a.rs".to_string(),
                access: ContextAccess::Editable,
            })
        );

        let map: LlmCommand =
            serde_json::from_value(serde_json::json!({"type": "repo_map", "path": "./crates"}))
                .expect("a benign map path must decode");
        assert_eq!(
            map,
            LlmCommand::RepoMap(RepoMapCommand {
                path: "crates".to_string(),
            })
        );

        // A spelling made only of dot and separator segments names the whole
        // repository on every discovery variant that accepts a subtree prefix,
        // because the decision follows the decoded segment set rather than an
        // enumerated list of root spellings.
        for spelling in ["", ".", "./", "./.", "././", ".//"] {
            let listing: LlmCommand = serde_json::from_value(serde_json::json!({
                "type": "dir_list",
                "path": spelling,
                "recursive": false,
            }))
            .expect("a whole-repository listing path must decode");
            assert_eq!(
                listing,
                LlmCommand::DirList(DirListCommand {
                    path: String::new(),
                    recursive: false,
                }),
                "spelling '{spelling}' must name the whole repository"
            );

            let search: LlmCommand = serde_json::from_value(serde_json::json!({
                "type": "repo_search",
                "path": spelling,
                "regex": "fn main",
            }))
            .expect("a whole-repository search path must decode");
            assert_eq!(
                search,
                LlmCommand::RepoSearch(RepoSearchCommand {
                    path: String::new(),
                    regex: "fn main".to_string(),
                }),
                "spelling '{spelling}' must name the whole repository"
            );
            let map: LlmCommand = serde_json::from_value(serde_json::json!({
                "type": "repo_map",
                "path": spelling,
            }))
            .expect("a whole-repository map path must decode");
            assert_eq!(
                map,
                LlmCommand::RepoMap(RepoMapCommand {
                    path: String::new(),
                }),
                "spelling '{spelling}' must name the whole repository"
            );
        }

        // A file edit carries its paths through the same boundary, so a move
        // arrives with both directories normalized.
        let edit: LlmCommand = serde_json::from_value(serde_json::json!({
            "type": "edit_move",
            "from": "./src/a.rs",
            "to": " ./src/b.rs ",
        }))
        .expect("a benign move must decode");
        assert_eq!(
            edit,
            LlmCommand::EditMove(EditMove {
                from: "src/a.rs".to_string(),
                to: "src/b.rs".to_string(),
            })
        );
        // A markdown-wrapped path is stripped by the same shared cleaner, so
        // a producer habit the compact-spec parser anticipates never reaches
        // the engine as a phantom backtick directory.
        let wrapped_context: LlmCommand = serde_json::from_value(serde_json::json!({
            "type": "context_request",
            "path": "`./src/a.rs`",
            "access": "read_only",
        }))
        .expect("a backtick-wrapped context path must decode");
        assert_eq!(
            wrapped_context,
            LlmCommand::ContextRequest(ContextRequestCommand {
                path: "src/a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            })
        );

        let wrapped_edit: LlmCommand = serde_json::from_value(serde_json::json!({
            "type": "edit_move",
            "from": "`./src/a.rs`",
            "to": " `./src/b.rs` ",
        }))
        .expect("a backtick-wrapped move must decode");
        assert_eq!(
            wrapped_edit,
            LlmCommand::EditMove(EditMove {
                from: "src/a.rs".to_string(),
                to: "src/b.rs".to_string(),
            })
        );
    }

    #[test]
    fn test_every_command_path_rejects_an_escaping_spelling() {
        // One set of spellings covers every repository path the wire form
        // carries. A typed variant cannot silently drop itself, so the
        // rejection the search command applied must reach every one of them.
        // A doubled leading backslash decodes to one literal backslash byte,
        // which roots the spelling outside the repository, so this family
        // belongs to the same set.
        for spelling in [
            "../escape",
            "/etc/passwd",
            "a/../b",
            r"C:\Windows",
            r"\\server\share",
            r"\\etc\passwd",
            r"\\?\C:\x",
            r"\\",
        ] {
            let expected = decoded_spelling(spelling);
            for payload in path_payloads(spelling) {
                let error = serde_json::from_value::<LlmCommand>(payload.clone())
                    .expect_err("an escaping repository path must be rejected");
                assert!(
                    error.to_string().contains(&expected),
                    "diagnostic for {payload} must name the rejected path: {error}"
                );
            }
        }

        // A context request and a file edit address a file, so an empty path is
        // a rejection rather than the whole-repository spelling.
        for payload in [
            serde_json::json!({"type": "context_request", "path": "  ", "access": "read_only"}),
            serde_json::json!({"type": "edit_delete", "path": " "}),
        ] {
            let error = serde_json::from_value::<LlmCommand>(payload.clone())
                .expect_err("an empty file path must be rejected");
            assert!(
                error.to_string().contains("must not be empty"),
                "diagnostic for {payload} must name the empty path: {error}"
            );
        }
    }

    #[test]
    fn test_doc_request_without_topic_decodes_to_all() {
        // The documented missing-topic fallback holds at the command boundary,
        // under the canonical spelling and its declared alias alike.
        for spelling in ["tauqe_doc_request", "doc_request"] {
            let raw = format!(r#"{{"type":"{spelling}"}}"#);
            let command: LlmCommand =
                serde_json::from_str(&raw).expect("a doc request without a topic must decode");
            assert_eq!(
                command,
                LlmCommand::DocRequest(DocRequestCommand {
                    topic: DocTopic::All
                })
            );
        }
    }

    #[test]
    fn test_llm_command_plan_scratchpad_serde_roundtrip() {
        let plan = LlmCommand::Plan {
            update: DiscussionPlanUpdate::default(),
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"type\":\"plan\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), plan);

        let scratchpad = LlmCommand::Scratchpad(ScratchpadCommand {
            notes: "Verified: parser fixed.".to_string(),
        });
        let json = serde_json::to_string(&scratchpad).unwrap();
        assert!(json.contains("\"type\":\"scratchpad\""));
        assert_eq!(
            serde_json::from_str::<LlmCommand>(&json).unwrap(),
            scratchpad
        );

        let proto_err = LlmCommand::ProtocolError {
            message: "Malformed XML edit block: Unclosed tag".to_string(),
        };
        let json = serde_json::to_string(&proto_err).unwrap();
        assert!(json.contains("\"type\":\"protocol_error\""));
        assert!(json.contains("\"message\":\"Malformed XML edit block: Unclosed tag\""));
        assert_eq!(
            serde_json::from_str::<LlmCommand>(&json).unwrap(),
            proto_err
        );
    }

    #[test]
    fn test_llm_command_atomic_terminal_commands_serde_roundtrip() {
        let commit = LlmCommand::CommitMessage(CommitMessageCommand {
            commit_message: "All checks passed".to_string(),
        });
        let json = serde_json::to_string(&commit).unwrap();
        assert!(json.contains("\"type\":\"commit_message\""));
        assert!(json.contains("\"commit_message\":\"All checks passed\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), commit);

        let error = LlmCommand::TurnError(TurnErrorCommand {
            error_message: "Missing dependency".to_string(),
        });
        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("\"type\":\"turn_error\""));
        assert!(json.contains("\"error_message\":\"Missing dependency\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), error);

        let skip = LlmCommand::TurnSkip(TurnSkipCommand {
            reason: "Already implemented".to_string(),
        });
        let json = serde_json::to_string(&skip).unwrap();
        assert!(json.contains("\"type\":\"turn_skip\""));
        assert!(json.contains("\"reason\":\"Already implemented\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), skip);
    }

    #[test]
    fn test_turn_error_without_a_message_key_states_the_empty_error() {
        // The error channel is robust to an idle representation: a turn error
        // payload that carries no `error_message` key states the empty string,
        // so a round can never fail to decode for omitting the channel.
        let command: LlmCommand = serde_json::from_str(r#"{"type":"turn_error"}"#)
            .expect("a turn error without a message key must decode");
        assert_eq!(
            command,
            LlmCommand::TurnError(TurnErrorCommand {
                error_message: String::new(),
            })
        );
    }

    #[test]
    fn test_llm_command_blocked_serde_roundtrip() {
        let blocked = LlmCommand::Blocked(BlockedCommand {
            reason: "Precondition failed".to_string(),
            details: "Missing table in db".to_string(),
        });
        let json = serde_json::to_string(&blocked).unwrap();
        assert!(json.contains("\"type\":\"blocked\""));
        assert!(json.contains("\"reason\":\"Precondition failed\""));
        assert!(json.contains("\"details\":\"Missing table in db\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), blocked);
    }

    #[test]
    fn test_llm_command_abort_serde_roundtrip() {
        let abort = LlmCommand::Abort(AbortCommand {
            reason: "Approval required".to_string(),
            recommendation: "Run pnpm approve-builds".to_string(),
        });
        let json = serde_json::to_string(&abort).unwrap();
        assert!(json.contains("\"type\":\"abort\""));
        assert!(json.contains("\"reason\":\"Approval required\""));
        assert!(json.contains("\"recommendation\":\"Run pnpm approve-builds\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), abort);
    }

    #[test]
    fn test_llm_command_custom_tool_serde_roundtrip() {
        let tool = LlmCommand::CustomTool(CustomToolCommand {
            name: "migrate".to_string(),
            args: serde_json::json!({ "database": "primary", "steps": 2 }),
        });
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"type\":\"custom_tool\""));
        assert!(json.contains("\"name\":\"migrate\""));
        assert!(json.contains("\"database\":\"primary\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), tool);

        // An invocation without arguments still decodes, so a command that
        // declares none never requires the model to invent an empty object.
        let bare: LlmCommand =
            serde_json::from_str(r#"{"type":"custom_tool","name":"status"}"#).unwrap();
        assert_eq!(
            bare,
            LlmCommand::CustomTool(CustomToolCommand {
                name: "status".to_string(),
                args: serde_json::Value::Null,
            })
        );
    }
    fn assert_strict_flat_schema<T: schemars::JsonSchema>(label: &str, optional: &[&str]) {
        let root = schemars::schema_for!(T);
        let text = serde_json::to_string(&root).expect("a derived schema must serialize");
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
                "{label}: a strict flat schema must not carry {forbidden}: {text}"
            );
        }
        assert!(
            text.contains("\"additionalProperties\":false"),
            "{label}: an unknown property must be refused: {text}"
        );

        let schema = serde_json::to_value(&root.schema).expect("a derived schema must convert");
        let absent_properties = serde_json::Map::new();
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .unwrap_or(&absent_properties);
        let required = schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for name in properties.keys() {
            let expected = !optional.contains(&name.as_str());
            assert_eq!(
                required.contains(&name.as_str()),
                expected,
                "{label}: property `{name}` must {} be required: {text}",
                if expected { "" } else { "not" }
            );
        }
        assert_eq!(
            required.len(),
            properties.len() - optional.len(),
            "{label}: every declared property must be required exactly once: {text}"
        );
    }

    /// Every flat structure a round may emit derives a schema that a strict
    /// decoder accepts as it stands.
    ///
    /// The covered set is exactly the vocabulary of single-object payloads: a
    /// structure that reaches this list is checked for the whole invariant — no
    /// composition keyword, no reference, no nullable type, an explicit refusal
    /// of an unknown property, and a complete `required` list.
    #[test]
    fn test_every_flat_structure_derives_a_strict_self_contained_schema() {
        assert_strict_flat_schema::<TextCommand>("TextCommand", &[]);
        assert_strict_flat_schema::<ContextRequestCommand>("ContextRequestCommand", &[]);
        assert_strict_flat_schema::<RepoSearchCommand>("RepoSearchCommand", &[]);
        assert_strict_flat_schema::<RepoDirsCommand>("RepoDirsCommand", &[]);
        assert_strict_flat_schema::<RepoMapCommand>("RepoMapCommand", &[]);
        assert_strict_flat_schema::<DirListCommand>("DirListCommand", &[]);
        assert_strict_flat_schema::<DocRequestCommand>("DocRequestCommand", &["topic"]);
        assert_strict_flat_schema::<OuterDocRequestCommand>("OuterDocRequestCommand", &[]);
        assert_strict_flat_schema::<ScratchpadCommand>("ScratchpadCommand", &[]);
        assert_strict_flat_schema::<CommitMessageCommand>("CommitMessageCommand", &[]);
        assert_strict_flat_schema::<TurnErrorCommand>("TurnErrorCommand", &["error_message"]);
        assert_strict_flat_schema::<TurnSkipCommand>("TurnSkipCommand", &[]);
        assert_strict_flat_schema::<BlockedCommand>("BlockedCommand", &["details"]);
        assert_strict_flat_schema::<AbortCommand>("AbortCommand", &[]);
        assert_strict_flat_schema::<CustomToolCommand>("CustomToolCommand", &["args"]);
        assert_strict_flat_schema::<EditReplace>("EditReplace", &[]);
        assert_strict_flat_schema::<EditCreate>("EditCreate", &[]);
        assert_strict_flat_schema::<EditDelete>("EditDelete", &[]);
        assert_strict_flat_schema::<EditMove>("EditMove", &[]);
        assert_strict_flat_schema::<EditMoveDir>("EditMoveDir", &[]);
        assert_strict_flat_schema::<EditOverwrite>("EditOverwrite", &[]);
        assert_strict_flat_schema::<EditLineAnchor>("EditLineAnchor", &[]);
    }
}

impl From<EditOperation> for LlmCommand {
    fn from(operation: EditOperation) -> Self {
        match operation {
            EditOperation::Replace(op) => Self::EditReplace(op),
            EditOperation::Create(op) => Self::EditCreate(op),
            EditOperation::Delete(op) => Self::EditDelete(op),
            EditOperation::Move(op) => Self::EditMove(op),
            EditOperation::MoveDir(op) => Self::EditMoveDir(op),
            EditOperation::Overwrite(op) => Self::EditOverwrite(op),
            EditOperation::LineAnchor(op) => Self::EditLineAnchor(op),
        }
    }
}

#[cfg(test)]
mod topic_tests {
    use super::*;

    #[test]
    fn test_doc_topic_serde_aliases_default_and_parsing() {
        assert_eq!(serde_json::to_string(&DocTopic::Plan).unwrap(), "\"plan\"");
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"all\"").unwrap(),
            DocTopic::All
        );
        // Serde deserialization is tolerant on the wire according to Postel's law.
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"bogus\"").unwrap(),
            DocTopic::All
        );
        assert_eq!(DocTopic::default(), DocTopic::All);

        assert_eq!(DocTopic::from("bogus"), DocTopic::All);
        assert_eq!(DocTopic::from(""), DocTopic::All);
        assert_eq!(DocTopic::from("  Interface "), DocTopic::Interface);
        assert_eq!(DocTopic::Interface.to_string(), "interface");
        assert_eq!("GIT".parse::<DocTopic>().unwrap(), DocTopic::Git);

        assert_eq!(
            serde_json::to_string(&DocTopic::Config).unwrap(),
            "\"config\""
        );
        assert_eq!(
            serde_json::from_str::<DocTopic>("\"config\"").unwrap(),
            DocTopic::Config
        );
        assert_eq!(DocTopic::Config.to_string(), "config");
        assert_eq!("config".parse::<DocTopic>().unwrap(), DocTopic::Config);
        assert!("bogus".parse::<DocTopic>().is_err());
        // `plans` named the plan topic before the vocabulary was fixed, so the
        // closed topic set no longer carries it.
        assert!("plans".parse::<DocTopic>().is_err());

        // Unknown doc topic inside an LlmCommand does not fail deserialization.
        let raw = r#"{"type":"doc_request","topic":"nonexistent_topic"}"#;
        let command: LlmCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(
            command,
            LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::All
            })
        );
    }
    #[test]
    fn test_doc_request_without_topic_decodes_to_all() {
        // The documented missing-topic fallback holds at the command boundary.
        let command: LlmCommand = serde_json::from_str(r#"{"type":"doc_request"}"#).unwrap();
        assert_eq!(
            command,
            LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::All
            })
        );
    }

    /// A present-but-`null` topic is a partial emission rather than a defect, so
    /// it resolves to the tolerant fallback exactly like a missing or unknown
    /// topic and never takes the enclosing command with it.
    #[test]
    fn test_doc_topic_accepts_the_null_shape() {
        for spelling in ["tauqe_doc_request", "doc_request"] {
            let raw = format!(r#"{{"type":"{spelling}","topic":null}}"#);
            let command: LlmCommand = serde_json::from_str(&raw)
                .expect("a null topic must decode instead of rejecting the command");
            assert_eq!(
                command,
                LlmCommand::DocRequest(DocRequestCommand {
                    topic: DocTopic::All
                }),
                "{spelling}"
            );
        }

        // The topic type itself is total over the null shape.
        assert_eq!(
            serde_json::from_str::<DocTopic>("null").unwrap(),
            DocTopic::All
        );

        // A shape that is neither a string nor `null` keeps reporting its own
        // type error instead of being tolerated.
        let error = serde_json::from_str::<DocTopic>("7")
            .expect_err("an integer topic must be rejected")
            .to_string();
        assert!(error.contains("invalid type: integer"), "{error}");
    }

    #[test]
    fn test_doc_topic_all_roundtrips_through_wire_names() {
        for topic in DocTopic::ALL {
            assert_eq!(topic.as_str().parse::<DocTopic>().unwrap(), *topic);
        }
    }
}
