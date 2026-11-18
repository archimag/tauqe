//! Model-output proposal retained by the engine result.
use super::discussion::{
    deserialize_null_as_default, deserialize_repo_path_list, deserialize_repo_search_requests,
    DiscussionContextRequest, RepoSearchRequest,
};
use super::DocTopic;
use crate::edits::StructuredChangeProposal;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StructuredOutcomeProposal {
    #[serde(default)]
    pub status: String,
    /// Git commit message of the change set declared by a successful outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Semantic blocker declared through a structured proposal.
///
/// Mirrors the `<blocked>` protocol tag: `is_blocked` is the explicit flag the
/// model sets when a precondition or assumption fails, `reason` names the
/// violated precondition, and `details` optionally expands it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredBlockedProposal {
    #[serde(default)]
    pub is_blocked: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Unconditional abort declared through a structured proposal.
///
/// Mirrors the `<abort>` protocol tag: `is_abort` is the explicit flag the
/// model sets when external environment or host action is required,
/// `reason` names the blocker, and `recommendation` provides actionable guidance for the developer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StructuredAbortProposal {
    #[serde(default)]
    pub is_abort: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub recommendation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelResultProposal {
    #[serde(default)]
    pub message: String,
    #[serde(default, deserialize_with = "deserialize_null_as_default")]
    pub changes: Vec<StructuredChangeProposal>,
    #[serde(
        default,
        deserialize_with = "deserialize_context_request_specs",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub context_requests: Vec<DiscussionContextRequest>,
    #[serde(
        default,
        deserialize_with = "deserialize_repo_search_requests",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub repo_searches: Vec<RepoSearchRequest>,
    /// Canonical wire property of the internal documentation request.
    ///
    /// The canonical spelling is `tauqe_doc_requests`; `doc_requests` stays the
    /// spelling an earlier revision emitted, so a stored transcript or a cached
    /// response keeps decoding identically. Serialization emits the canonical
    /// name alone, so the alias can never spread to a new response.
    ///
    /// A `null` element resolves to the tolerant `DocTopic::All` fallback
    /// instead of rejecting the proposal, matching the missing and unknown
    /// spellings, while an element of any other non-string shape still reports
    /// the precise error it produced.
    #[serde(
        default,
        rename = "tauqe_doc_requests",
        alias = "doc_requests",
        deserialize_with = "deserialize_null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub doc_requests: Vec<DocTopic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratchpad: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<StructuredOutcomeProposal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<StructuredBlockedProposal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort: Option<StructuredAbortProposal>,
    /// Repository-relative context files the turn asks to drop.
    ///
    /// Each entry names a file of the assembled context, normalized and
    /// validated at the wire boundary exactly like every sibling discovery
    /// field: a benign spelling (`./src/a.rs`, backslash separators, a trailing
    /// slash, or a backtick-wrapped path) is cleaned into the stored form, so it
    /// still matches the context entry it names, while an entry that cannot
    /// identify a repository file is dropped instead of being forwarded.
    /// Duplicate entries are preserved. An absent or explicit `null` payload
    /// decodes to an empty list.
    #[serde(default, deserialize_with = "deserialize_repo_path_list")]
    pub context_drops: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_null_as_default")]
    pub suggested_actions: Vec<String>,
}

/// Normalizes the `context_requests` field of [`ModelResultProposal`] into
/// typed, access-aware requests.
///
/// The edit-protocol schema encodes the requested access level in a compact
/// string (`"path"`, `"read_only:path"`, or `"editable:path"`), while a
/// round-tripped proposal carries explicit `{ "path", "access" }` objects. Both
/// forms are accepted so the model's requested access level is preserved in a
/// single [`DiscussionContextRequest`] representation instead of being dropped
/// into an ambiguous bare string.
///
/// A JSON `null` deserializes to an empty list, so a model that emits an
/// explicit null never aborts the proposal parse.
///
/// The element shape is dispatched with a visitor instead of an untagged enum,
/// so a malformed entry reports the precise reason it was rejected — a missing
/// `path`, an unsupported value type, or a malformed `access` level — rather
/// than the opaque "data did not match any variant of untagged enum" message,
/// which names neither the field nor the offending entry. An entry that
/// decodes but names a path that cannot identify a repository file is still
/// dropped, so one unusable path never discards the usable requests next to
/// it.
fn deserialize_context_request_specs<'de, D>(
    deserializer: D,
) -> Result<Vec<DiscussionContextRequest>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let specs = Option::<Vec<ContextRequestSpec>>::deserialize(deserializer)?;
    Ok(specs
        .unwrap_or_default()
        .into_iter()
        .filter_map(ContextRequestSpec::resolve)
        .collect())
}

/// Shared polymorphic specification of one `context_requests` entry.
///
/// A request is either the compact access-prefixed path string the
/// edit-protocol schema emits (`"path"`, `"read_only:path"`, or
/// `"editable:path"`) or the structured `{ path, access }` table a
/// round-tripped proposal carries. Both forms resolve to one
/// [`DiscussionContextRequest`], so the requested access level is preserved
/// instead of being dropped into an ambiguous bare string.
///
/// The value is dispatched on its runtime shape with a visitor rather than an
/// untagged enum. An untagged enum reports every malformed element as the
/// opaque "data did not match any variant", which discards the precise
/// diagnostic produced while parsing the structured form and cannot be
/// localized to the offending entry; the visitor forwards that diagnostic
/// verbatim.
enum ContextRequestSpec {
    /// Access-prefixed path specification emitted by the edit-protocol schema.
    Compact(String),
    /// Structured `{ path, access }` request carried by a round-tripped proposal.
    Detailed(DiscussionContextRequest),
}

impl ContextRequestSpec {
    /// Resolves the specification into a typed, access-aware request.
    ///
    /// An entry whose path cannot identify a repository file yields `None`, so
    /// it is dropped instead of discarding the usable requests around it. The
    /// requested access level is preserved.
    fn resolve(self) -> Option<DiscussionContextRequest> {
        match self {
            Self::Compact(path) => DiscussionContextRequest::parse_spec(&path),
            Self::Detailed(request) => request.normalized(),
        }
    }
}

impl<'de> Deserialize<'de> for ContextRequestSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SpecVisitor;

        impl<'de> serde::de::Visitor<'de> for SpecVisitor {
            type Value = ContextRequestSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a context request path string or a { path, access } table")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ContextRequestSpec::Compact(value.to_string()))
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                // Delegating the structured form to the request's own
                // `Deserialize` keeps its field validation identical to the
                // typed path while preserving the precise field diagnostic.
                let request = DiscussionContextRequest::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(ContextRequestSpec::Detailed(request))
            }
        }

        deserializer.deserialize_any(SpecVisitor)
    }
}
