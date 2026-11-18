//! Model-facing research requests and dialogue output.
use serde::{Deserialize, Serialize};

use crate::context::ContextAccess;
fn default_context_access() -> ContextAccess {
    ContextAccess::ReadOnly
}
use crate::context::lexical_path::strip_surrounding_markup;
use crate::edits::normalize_repo_path;
pub use crate::plan::{DiscussionPlanAction, DiscussionPlanUpdate};
use crate::review::ReviewUpdateProposal;
use normalize::{
    deserialize_context_access, deserialize_context_requests, deserialize_dir_list_requests,
    deserialize_repo_map_paths,
};
pub(crate) use normalize::{
    deserialize_null_as_default, deserialize_repo_path_list, deserialize_repo_search_requests,
};

pub mod normalize;
pub mod schema;

pub use normalize::normalize_repo_map_path;
/// Declarative repository search request emitted during a research or discussion turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSearchRequest {
    pub path: String,
    pub regex: String,
}

impl RepoSearchRequest {
    pub fn new(path: impl Into<String>, regex: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            regex: regex.into(),
        }
    }

    /// Normalizes the requested search path through the shared repository path
    /// rules.
    ///
    /// A path whose decoded segments are only `.` and separator segments — the
    /// empty string, `.`, `./`, `./.`, `././`, and `.//` alike — names the whole
    /// repository and normalizes to an empty string. Any other path is
    /// validated through [`normalize_repo_map_path`], so a repository search
    /// can never address a location outside the repository; such a path yields
    /// `None` and the request is dropped instead of being allowed to escape.
    /// The search `regex` is preserved.
    fn normalized(self) -> Option<Self> {
        normalize_repo_map_path(&self.path).map(|path| Self {
            path,
            regex: self.regex,
        })
    }
}

/// Declarative directory content listing request emitted during research.
///
/// The request addresses a repository-relative directory prefix; an empty path
/// names the repository root. `recursive` widens the listing from the direct
/// children to every tracked file underneath, so the default request stays
/// shallow and never floods the context budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirListRequest {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
}

impl DirListRequest {
    pub fn new(path: impl Into<String>, recursive: bool) -> Self {
        Self {
            path: path.into(),
            recursive,
        }
    }

    /// Normalizes the requested directory path through the shared repository
    /// path rules.
    ///
    /// A path whose decoded segments are only `.` and separator segments — the
    /// empty string, `.`, `./`, `./.`, `././`, and `.//` alike — names the whole
    /// repository and normalizes to an empty string. Any other path is
    /// validated through [`normalize_repo_map_path`], so a directory listing can
    /// never address a location outside the repository; such a path yields
    /// `None` and the request is dropped instead of being allowed to escape.
    /// The `recursive` flag is preserved.
    pub fn normalized(self) -> Option<Self> {
        normalize_repo_map_path(&self.path).map(|path| Self {
            path,
            recursive: self.recursive,
        })
    }
}

/// Structured file context request emitted during discussion turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionContextRequest {
    pub path: String,
    #[serde(
        default = "default_context_access",
        deserialize_with = "deserialize_context_access"
    )]
    pub access: ContextAccess,
}

impl DiscussionContextRequest {
    /// Normalizes an already-structured request through the shared repository
    /// path rules.
    ///
    /// Returns `None` when the stored path cannot identify a repository file
    /// (empty, absolute, a Windows drive path, or containing a `..` component),
    /// so a malformed entry is dropped instead of being allowed to escape the
    /// repository. The requested access level is preserved.
    pub(crate) fn normalized(self) -> Option<Self> {
        normalize_repo_path(&self.path).ok().map(|path| Self {
            path,
            access: self.access,
        })
    }

    /// Parses a compact `context_requests` specification into an access-aware
    /// request.
    ///
    /// Accepts a bare repository-relative path (read-only), a `read_only:`
    /// prefixed path, or an `editable:` prefixed path, tolerating surrounding
    /// whitespace, backticks, and a leading `./`. Both prefixes are recognized
    /// in any letter case, so a producer that capitalizes them still reaches
    /// the access level it asked for; only the prefix is case-folded, never the
    /// path. The path is normalized and validated through
    /// [`normalize_repo_path`], so an empty, absolute, or traversal path yields
    /// `None` instead of an escape attempt.
    pub(crate) fn parse_spec(spec: &str) -> Option<Self> {
        // A producer may wrap the whole specification in backticks, so the
        // surrounding markup is removed before the access prefix is read.
        let spec = strip_surrounding_markup(spec);
        let (access, rest) = if let Some(rest) = strip_access_prefix(spec, "editable:") {
            (ContextAccess::Editable, rest)
        } else if let Some(rest) = strip_access_prefix(spec, "read_only:") {
            (ContextAccess::ReadOnly, rest)
        } else {
            (ContextAccess::ReadOnly, spec)
        };
        // The path is cleaned by the shared normalizer, which strips the
        // backticks a producer may wrap around the path alone.
        let path = normalize_repo_path(rest).ok()?;
        Some(Self { path, access })
    }
}

/// Consumes an access prefix regardless of its letter case.
///
/// Only the prefix is compared case-insensitively; the returned remainder keeps
/// its original bytes, so a path is never case-folded. A missing prefix, or a
/// specification too short to contain it, yields `None`.
fn strip_access_prefix<'a>(spec: &'a str, prefix: &str) -> Option<&'a str> {
    let head = spec.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    spec.get(prefix.len()..)
}

/// Top-level response structure for discussion and plan refinement modes.
///
/// The `message` field is placed first both in Rust definition and in generated JSON Schema
/// to ensure immediate streaming output to TUI clients while supporting strict structured validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiscussionResponse {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_update: Option<DiscussionPlanUpdate>,
    /// Decision recorded for one finding of the discussed review session.
    ///
    /// The dialogue is where an engineer actually decides what a finding is
    /// worth, so the decision travels with the dialogue payload: the turn that
    /// produces it records the status transition together with the reasoning
    /// behind it, instead of leaving the developer to re-enter the outcome by
    /// hand in the review interface afterwards.
    ///
    /// A decision is not an edit: it never carries the fix, which travels through
    /// the ordinary finding workflow and owns the transition to `fixed`. Null
    /// when the turn recorded no decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_update: Option<ReviewUpdateProposal>,
    #[serde(
        default,
        deserialize_with = "deserialize_context_requests",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub context_requests: Vec<DiscussionContextRequest>,
    #[serde(
        default,
        deserialize_with = "deserialize_repo_search_requests",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub repo_searches: Vec<RepoSearchRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_dirs: Option<bool>,
    #[serde(
        default,
        deserialize_with = "deserialize_repo_map_paths",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub repo_maps: Vec<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_dir_list_requests",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub dir_lists: Vec<DirListRequest>,
    /// Decode diagnosis of a `plan_update` payload the DTO could not honor.
    ///
    /// The structured stream filter rebuilds this response from a partially
    /// decodable payload, so a present but malformed `plan_update` is recorded
    /// here instead of disappearing: the field carries the deserialization error
    /// of the DTO boundary, and the plan-refinement contour refuses the round on
    /// it. It is a harness-side diagnostic rather than a model-authored field, so
    /// it stays out of the strict wire schema and is omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_update_error: Option<String>,
}

impl DiscussionResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            plan_update: None,
            review_update: None,
            context_requests: Vec::new(),
            repo_searches: Vec::new(),
            repo_dirs: None,
            repo_maps: Vec::new(),
            dir_lists: Vec::new(),
            plan_update_error: None,
        }
    }

    pub fn with_repo_maps(mut self, paths: Vec<String>) -> Self {
        self.repo_maps = paths
            .iter()
            .filter_map(|path| normalize_repo_map_path(path))
            .collect();
        self
    }
}

impl DiscussionResponse {
    #[cfg(test)]
    fn with_repo_dirs(mut self, request: bool) -> Self {
        self.repo_dirs = Some(request);
        self
    }
}

impl DiscussionResponse {
    #[cfg(test)]
    fn with_dir_lists(mut self, requests: Vec<DirListRequest>) -> Self {
        self.dir_lists = requests
            .into_iter()
            .filter_map(DirListRequest::normalized)
            .collect();
        self
    }
}

impl DiscussionResponse {
    #[cfg(test)]
    fn with_repo_searches(mut self, searches: Vec<RepoSearchRequest>) -> Self {
        self.repo_searches = searches
            .into_iter()
            .filter_map(RepoSearchRequest::normalized)
            .collect();
        self
    }
}

impl DiscussionResponse {
    #[cfg(test)]
    fn with_plan_update(mut self, plan_update: DiscussionPlanUpdate) -> Self {
        self.plan_update = Some(plan_update);
        self
    }
}

impl DiscussionResponse {
    #[cfg(test)]
    fn with_review_update(mut self, review_update: ReviewUpdateProposal) -> Self {
        self.review_update = Some(review_update);
        self
    }
}

impl DiscussionResponse {
    #[cfg(test)]
    fn with_context_requests(mut self, requests: Vec<DiscussionContextRequest>) -> Self {
        self.context_requests = requests
            .into_iter()
            .filter_map(DiscussionContextRequest::normalized)
            .collect();
        self
    }
}

impl DiscussionContextRequest {
    #[cfg(test)]
    fn read_only(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            access: ContextAccess::ReadOnly,
        }
    }
}

impl DiscussionContextRequest {
    #[cfg(test)]
    fn editable(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            access: ContextAccess::Editable,
        }
    }
}

#[cfg(test)]
mod compatibility_tests;
