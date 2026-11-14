use serde::{Deserialize, Serialize};

use crate::context::{default_context_access, ContextAccess};
use crate::edit::normalize_repo_path;
use crate::edit::proposal::strip_surrounding_markup;
use crate::plan::{ConceptItem, PlanItem};
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
pub use schema::custom_tool_call_schema;

/// Action to perform on a project plan as part of discussion structured output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiscussionPlanAction {
    #[default]
    Update,
    Save,
}

impl std::fmt::Display for DiscussionPlanAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Update => write!(f, "update"),
            Self::Save => write!(f, "save"),
        }
    }
}

/// Structured plan modification payload produced in discussion mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiscussionPlanUpdate {
    #[serde(default)]
    pub action: DiscussionPlanAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub concept: Vec<ConceptItem>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub items: Vec<PlanItem>,
}

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
    pub fn normalized(self) -> Option<Self> {
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
    pub fn read_only(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            access: ContextAccess::ReadOnly,
        }
    }

    pub fn editable(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            access: ContextAccess::Editable,
        }
    }

    /// Normalizes an already-structured request through the shared repository
    /// path rules.
    ///
    /// Returns `None` when the stored path cannot identify a repository file
    /// (empty, absolute, a Windows drive path, or containing a `..` component),
    /// so a malformed entry is dropped instead of being allowed to escape the
    /// repository. The requested access level is preserved.
    pub fn normalized(self) -> Option<Self> {
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
    pub fn parse_spec(spec: &str) -> Option<Self> {
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

/// Active focus anchor for continuous multi-turn discussion in REPL.
///
/// A focus is either detached (`None`) or anchored on a specific entity
/// (`Focused`). The anchored entity is identified by the same
/// [`DiscussionTarget`] used by [`DiscussionRequest`], so plan and review
/// anchors share one identification vocabulary instead of duplicating their
/// variants. `focused_items` narrows the anchor to individual items: plan step
/// and concept ids for a plan, and the decimal string form of finding ids for a
/// review session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiscussionFocus {
    #[default]
    None,
    Focused {
        target: DiscussionTarget,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        focused_items: Vec<String>,
    },
}

impl DiscussionFocus {
    pub fn plan(plan_id: impl Into<String>, focused_items: Vec<String>) -> Self {
        Self::from_target(DiscussionTarget::Plan { id: plan_id.into() }, focused_items)
    }

    pub fn review(review_id: impl Into<String>, focused_items: Vec<String>) -> Self {
        Self::from_target(
            DiscussionTarget::Review {
                id: review_id.into(),
            },
            focused_items,
        )
    }

    /// Anchors a focus on the entity addressed by a [`DiscussionTarget`].
    pub fn from_target(target: DiscussionTarget, focused_items: Vec<String>) -> Self {
        Self::Focused {
            target,
            focused_items,
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub fn is_active(&self) -> bool {
        !self.is_none()
    }

    /// Returns the anchored target, or `None` for a detached focus.
    pub fn target(&self) -> Option<&DiscussionTarget> {
        match self {
            Self::None => None,
            Self::Focused { target, .. } => Some(target),
        }
    }

    /// Returns the focused item ids as stringified identifiers, or an empty
    /// slice for a detached focus.
    pub fn focused_items(&self) -> &[String] {
        match self {
            Self::None => &[],
            Self::Focused { focused_items, .. } => focused_items,
        }
    }
}

/// Parameters for setting the active discussion focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionSetFocusParams {
    pub focus: DiscussionFocus,
}

/// Result of setting the active discussion focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionSetFocusResult {
    pub focus: DiscussionFocus,
}

/// Parameters for getting the active discussion focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiscussionGetFocusParams {}

/// Result of querying the active discussion focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionGetFocusResult {
    pub focus: DiscussionFocus,
}

/// Parameters for clearing the active discussion focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiscussionClearFocusParams {}

/// Result of clearing the active discussion focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionClearFocusResult {
    pub focus: DiscussionFocus,
}

/// Notification emitted when discussion focus state changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionFocusChangedEvent {
    pub focus: DiscussionFocus,
}

/// Explicit target of a discussion turn.
///
/// The discussion mode and its target are carried as typed request fields rather
/// than inferred from a magic prefix inside user-controlled prompt text, so a
/// message whose body merely resembles a discussion heading can never enter
/// discussion mode, and identifiers are transported verbatim.
///
/// The same type also identifies the entity anchored by [`DiscussionFocus`], so
/// plan and review targets have a single canonical representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiscussionTarget {
    /// Discussion focused on a plan, addressed by plan identifier.
    Plan { id: String },
    /// Discussion focused on a review session, addressed by session identifier.
    Review { id: String },
    /// Discussion focused on one group of a review session.
    ///
    /// An architectural group owns the root cause that blocks it, so the
    /// dialogue addresses the group by the pair of its session and its own
    /// identifier: a group identifier is unique only inside one session.
    ReviewGroup {
        session_id: String,
        group_id: String,
    },
}

/// Structured input of a discussion turn.
///
/// The server generates the discussion prompt from these typed fields, so the
/// wire request never relies on parsing free-form prompt text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscussionRequest {
    pub target: DiscussionTarget,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub focused_items: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
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

    pub fn with_plan_update(mut self, plan_update: DiscussionPlanUpdate) -> Self {
        self.plan_update = Some(plan_update);
        self
    }

    pub fn with_review_update(mut self, review_update: ReviewUpdateProposal) -> Self {
        self.review_update = Some(review_update);
        self
    }

    pub fn with_context_requests(mut self, requests: Vec<DiscussionContextRequest>) -> Self {
        self.context_requests = requests
            .into_iter()
            .filter_map(DiscussionContextRequest::normalized)
            .collect();
        self
    }

    pub fn with_repo_searches(mut self, searches: Vec<RepoSearchRequest>) -> Self {
        self.repo_searches = searches
            .into_iter()
            .filter_map(RepoSearchRequest::normalized)
            .collect();
        self
    }

    pub fn with_repo_dirs(mut self, request: bool) -> Self {
        self.repo_dirs = Some(request);
        self
    }

    pub fn with_repo_maps(mut self, paths: Vec<String>) -> Self {
        self.repo_maps = paths
            .iter()
            .filter_map(|path| normalize_repo_map_path(path))
            .collect();
        self
    }

    pub fn with_dir_lists(mut self, requests: Vec<DirListRequest>) -> Self {
        self.dir_lists = requests
            .into_iter()
            .filter_map(DirListRequest::normalized)
            .collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanItemStatus;
    use crate::review::ReviewDecisionStatus;

    #[test]
    fn test_discussion_context_request_parse_spec() {
        let ro = DiscussionContextRequest::parse_spec(" ./src/a.rs ").unwrap();
        assert_eq!(ro.path, "src/a.rs");
        assert_eq!(ro.access, ContextAccess::ReadOnly);
        let ed = DiscussionContextRequest::parse_spec("editable:src/b.rs").unwrap();
        assert_eq!(ed.path, "src/b.rs");
        assert_eq!(ed.access, ContextAccess::Editable);
        assert!(DiscussionContextRequest::parse_spec("editable:").is_none());
        // The shared normalizer owns the strip rule, so a backticks-only
        // specification is the empty path it is.
        assert!(DiscussionContextRequest::parse_spec("``").is_none());

        // Absolute, traversal, and NUL paths are rejected rather than silently
        // rewritten into a relative path.
        assert!(DiscussionContextRequest::parse_spec("/etc/passwd").is_none());
        assert!(DiscussionContextRequest::parse_spec("../escape.rs").is_none());
        assert!(DiscussionContextRequest::parse_spec("editable:a/../b").is_none());
        assert!(DiscussionContextRequest::parse_spec("a\0b").is_none());

        // A doubled leading backslash decodes to one literal backslash byte,
        // which roots the spelling outside the repository, so it yields no
        // request at all.
        for spelling in [r"\\server\share", r"\\etc\passwd", r"\\?\C:\x", r"\\"] {
            assert!(
                DiscussionContextRequest::parse_spec(spelling).is_none(),
                "spelling '{spelling}' must yield no request"
            );
        }

        // A request addresses a file rather than the repository root, so a
        // spelling made only of dot and separator segments yields none either,
        // exactly like the empty path it decodes to.
        assert!(DiscussionContextRequest::parse_spec("./.").is_none());
        assert!(DiscussionContextRequest::parse_spec("././").is_none());
        assert!(DiscussionContextRequest::parse_spec(".//").is_none());
    }

    #[test]
    fn test_discussion_context_request_parse_spec_folds_prefix_case() {
        // A capitalized access prefix selects the access level it names instead
        // of falling through to the bare read-only branch with the prefix still
        // attached to the decoded path.
        let editable = DiscussionContextRequest::parse_spec("Editable:src/b.rs").unwrap();
        assert_eq!(editable.access, ContextAccess::Editable);
        assert_eq!(editable.path, "src/b.rs");

        let read_only = DiscussionContextRequest::parse_spec("READ_ONLY: src/a.rs").unwrap();
        assert_eq!(read_only.access, ContextAccess::ReadOnly);
        assert_eq!(read_only.path, "src/a.rs");

        // Backtick markup around the whole specification, or around the path
        // alone, never hides the prefix or leaks into the decoded path.
        let wrapped = DiscussionContextRequest::parse_spec("`Editable:src/c.rs`").unwrap();
        assert_eq!(wrapped.access, ContextAccess::Editable);
        assert_eq!(wrapped.path, "src/c.rs");
        let inner = DiscussionContextRequest::parse_spec("editable:`src/d.rs`").unwrap();
        assert_eq!(inner.access, ContextAccess::Editable);
        assert_eq!(inner.path, "src/d.rs");

        // Only the prefix is case-folded; the path keeps its exact case.
        let mixed = DiscussionContextRequest::parse_spec("Editable:Src/CaseSensitive.rs").unwrap();
        assert_eq!(mixed.access, ContextAccess::Editable);
        assert_eq!(mixed.path, "Src/CaseSensitive.rs");

        // An unprefixed path still defaults to read-only.
        let bare = DiscussionContextRequest::parse_spec("./src/e.rs").unwrap();
        assert_eq!(bare.access, ContextAccess::ReadOnly);
        assert_eq!(bare.path, "src/e.rs");
    }

    #[test]
    fn test_discussion_focus_lifecycle_and_serde() {
        let none_focus = DiscussionFocus::None;
        assert!(none_focus.is_none());
        assert!(!none_focus.is_active());
        let none_json = serde_json::to_string(&none_focus).unwrap();
        assert_eq!(none_json, "{\"kind\":\"none\"}");
        let de_none: DiscussionFocus = serde_json::from_str(&none_json).unwrap();
        assert_eq!(de_none, DiscussionFocus::None);

        let plan_focus = DiscussionFocus::plan("plan-1", vec!["1.1".to_string()]);
        assert!(!plan_focus.is_none());
        assert!(plan_focus.is_active());
        assert_eq!(
            plan_focus.target(),
            Some(&DiscussionTarget::Plan {
                id: "plan-1".to_string()
            })
        );
        assert_eq!(plan_focus.focused_items(), ["1.1".to_string()].as_slice());
        let plan_json = serde_json::to_string(&plan_focus).unwrap();
        assert!(plan_json.contains("\"kind\":\"focused\""));
        assert!(plan_json.contains("\"target\":{\"kind\":\"plan\",\"id\":\"plan-1\"}"));
        assert!(plan_json.contains("\"focused_items\":[\"1.1\"]"));
        let de_plan: DiscussionFocus = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(de_plan, plan_focus);

        let review_focus = DiscussionFocus::review("rev-1", vec!["1".to_string(), "2".to_string()]);
        assert!(!review_focus.is_none());
        assert!(review_focus.is_active());
        assert_eq!(
            review_focus.target(),
            Some(&DiscussionTarget::Review {
                id: "rev-1".to_string()
            })
        );
        assert_eq!(
            review_focus.focused_items(),
            ["1".to_string(), "2".to_string()].as_slice()
        );
        let review_json = serde_json::to_string(&review_focus).unwrap();
        assert!(review_json.contains("\"kind\":\"focused\""));
        assert!(review_json.contains("\"target\":{\"kind\":\"review\",\"id\":\"rev-1\"}"));
        let de_review: DiscussionFocus = serde_json::from_str(&review_json).unwrap();
        assert_eq!(de_review, review_focus);

        // A detached focus exposes no target and no focused items.
        assert_eq!(DiscussionFocus::None.target(), None);
        assert!(DiscussionFocus::None.focused_items().is_empty());
    }

    #[test]
    fn test_discussion_focus_rpc_types_serde() {
        let set_params = DiscussionSetFocusParams {
            focus: DiscussionFocus::plan("p-1", vec![]),
        };
        let set_json = serde_json::to_string(&set_params).unwrap();
        let de_set: DiscussionSetFocusParams = serde_json::from_str(&set_json).unwrap();
        assert_eq!(de_set, set_params);

        let set_res = DiscussionSetFocusResult {
            focus: DiscussionFocus::None,
        };
        let res_json = serde_json::to_string(&set_res).unwrap();
        let de_res: DiscussionSetFocusResult = serde_json::from_str(&res_json).unwrap();
        assert_eq!(de_res, set_res);

        let event = DiscussionFocusChangedEvent {
            focus: DiscussionFocus::review("rev-2", vec!["3".to_string()]),
        };
        let event_json = serde_json::to_string(&event).unwrap();
        let de_event: DiscussionFocusChangedEvent = serde_json::from_str(&event_json).unwrap();
        assert_eq!(de_event, event);
    }

    #[test]
    fn test_discussion_request_serde() {
        let request = DiscussionRequest {
            target: DiscussionTarget::Plan {
                id: "p-1".to_string(),
            },
            focused_items: vec!["1".to_string()],
            comment: "Review the concept".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"kind\":\"plan\""));
        assert!(json.contains("\"id\":\"p-1\""));
        assert!(json.contains("\"comment\":\"Review the concept\""));
        let de: DiscussionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(de, request);

        let review_json = r#"{"target":{"kind":"review","id":"rev-9"}}"#;
        let de_review: DiscussionRequest = serde_json::from_str(review_json).unwrap();
        assert_eq!(
            de_review.target,
            DiscussionTarget::Review {
                id: "rev-9".to_string()
            }
        );
        assert!(de_review.focused_items.is_empty());
        assert!(de_review.comment.is_empty());
    }

    #[test]
    fn test_discussion_target_review_group_serde() {
        let target = DiscussionTarget::ReviewGroup {
            session_id: "rev-1".to_string(),
            group_id: "arch-group".to_string(),
        };
        let json = serde_json::to_string(&target).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"review_group","session_id":"rev-1","group_id":"arch-group"}"#
        );
        let de: DiscussionTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(de, target);

        // A focus anchors the same pair, so the group is addressed inside the
        // one session whose group list defines it.
        let focus = DiscussionFocus::from_target(target.clone(), vec!["7".to_string()]);
        assert_eq!(focus.target(), Some(&target));
        assert_eq!(focus.focused_items(), ["7".to_string()].as_slice());
    }

    #[test]
    fn test_discussion_plan_action_display_and_serde() {
        assert_eq!(DiscussionPlanAction::Update.to_string(), "update");
        assert_eq!(DiscussionPlanAction::Save.to_string(), "save");

        let json = serde_json::to_string(&DiscussionPlanAction::Save).unwrap();
        assert_eq!(json, "\"save\"");
        let de: DiscussionPlanAction = serde_json::from_str(&json).unwrap();
        assert_eq!(de, DiscussionPlanAction::Save);
    }

    #[test]
    fn test_repo_search_request_serde() {
        let req = RepoSearchRequest::new("crates/protocol", r"pub fn \w+");
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"path":"crates/protocol","regex":"pub fn \\w+"}"#);
        let de: RepoSearchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(de, req);
    }

    #[test]
    fn test_discussion_response_minimal_serde() {
        let raw = r#"{"message":"Hello developer!"}"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.message, "Hello developer!");
        assert!(resp.plan_update.is_none());
        assert!(resp.context_requests.is_empty());
        assert!(resp.repo_searches.is_empty());
        assert!(resp.repo_dirs.is_none());
        assert!(resp.repo_maps.is_empty());
        assert!(resp.dir_lists.is_empty());
        assert!(resp.review_update.is_none());
    }

    #[test]
    fn test_discussion_response_review_update_roundtrip() {
        let response =
            DiscussionResponse::new("Decision recorded").with_review_update(ReviewUpdateProposal {
                item_id: 7,
                group_id: Some("arch-group".to_string()),
                status: ReviewDecisionStatus::Rejected,
                note: "The reported race cannot happen: the guard is released after the await"
                    .to_string(),
                recommendations: Vec::new(),
            });

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"review_update\":{"), "got: {json}");
        assert!(json.contains("\"status\":\"rejected\""), "got: {json}");
        assert!(json.contains("\"item_id\":7"), "got: {json}");
        assert!(json.contains("\"group_id\":\"arch-group\""), "got: {json}");
        let de: DiscussionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(de, response);

        // A turn that records no decision states it explicitly and keeps the
        // field out of the wire form, exactly like an absent plan update.
        let idle = serde_json::to_string(&DiscussionResponse::new("Hello")).unwrap();
        assert!(!idle.contains("review_update"), "got: {idle}");
    }
    #[test]
    fn test_discussion_response_plan_update_error_is_a_decode_diagnostic() {
        // The field records why a present `plan_update` could not be honored, so
        // the diagnosis travels with the response instead of disappearing.
        let mut response = DiscussionResponse::new("Partial decode");
        response.plan_update_error =
            Some("invalid plan_update: missing field `action`".to_string());

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"plan_update_error\""), "got: {json}");
        let de: DiscussionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(de, response);

        // An absent diagnosis declares nothing and stays out of the wire form.
        let idle = serde_json::to_string(&DiscussionResponse::new("Hello")).unwrap();
        assert!(!idle.contains("plan_update_error"), "got: {idle}");
        let de: DiscussionResponse = serde_json::from_str(&idle).unwrap();
        assert!(de.plan_update_error.is_none());
    }

    #[test]
    fn test_discussion_response_with_nulls_serde() {
        let raw = r#"{
            "message": "Acknowledged",
            "plan_update": null,
            "review_update": null,
            "context_requests": null,
            "repo_searches": null,
            "repo_dirs": null,
            "repo_maps": null,
            "dir_lists": null
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.message, "Acknowledged");
        assert!(resp.plan_update.is_none());
        assert!(resp.review_update.is_none());
        assert!(resp.context_requests.is_empty());
        assert!(resp.repo_searches.is_empty());
        assert!(resp.repo_dirs.is_none());
        assert!(resp.repo_maps.is_empty());
        assert!(resp.dir_lists.is_empty());
    }

    #[test]
    fn test_discussion_response_full_roundtrip() {
        let resp = DiscussionResponse::new("Proceed with the protocol update")
            .with_repo_dirs(true)
            .with_repo_maps(vec!["./crates/core/".to_string()])
            .with_dir_lists(vec![DirListRequest::new("./crates/core/", true)])
            .with_context_requests(vec![
                DiscussionContextRequest::read_only("crates/core/src/lib.rs"),
                DiscussionContextRequest::editable("crates/protocol/src/lib.rs"),
            ])
            .with_repo_searches(vec![RepoSearchRequest::new(
                "crates/core",
                r"pub fn run_\w+",
            )])
            .with_plan_update(DiscussionPlanUpdate {
                action: DiscussionPlanAction::Update,
                id: Some("structured-output".to_string()),
                title: Some("Structured Output".to_string()),
                description: Some("Implement JSON schema protocol".to_string()),
                concept: vec![ConceptItem {
                    id: "c-1".to_string(),
                    title: "One wire contract".to_string(),
                    details: Some("Schemas mirror the Rust data model".to_string()),
                    children: vec![],
                }],
                items: vec![PlanItem {
                    id: "1".to_string(),
                    title: "Protocol types".to_string(),
                    details: Some("Add discussion schemas".to_string()),
                    status: PlanItemStatus::Done,
                    blocked_reason: None,
                    commit: None,
                    files: vec!["crates/protocol/src/discussion.rs".to_string()],
                    readonly_files: vec![],
                    outer_docs: vec![],
                    concept_refs: vec!["c-1".to_string()],
                    depends_on: vec![],
                    children: vec![],
                }],
            });

        let json = serde_json::to_string_pretty(&resp).unwrap();
        let de: DiscussionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(de, resp);
    }
    #[test]
    fn test_context_requests_builder_applies_repository_path_rules() {
        // The builder shares the boundary of its sibling builders: an entry that
        // the decode path drops never reaches the response through this
        // construction path either, so a directly built response carries only
        // repository-relative paths.
        let response = DiscussionResponse::new("Hello").with_context_requests(vec![
            DiscussionContextRequest::read_only("./crates/core/src/lib.rs"),
            DiscussionContextRequest::editable("/etc/passwd"),
            DiscussionContextRequest::read_only("../escape.rs"),
            DiscussionContextRequest::editable("   "),
            DiscussionContextRequest::editable("crates/core/src/lib.rs"),
        ]);

        // The two reachable requests keep their order, normalized path, and
        // requested access level.
        assert_eq!(response.context_requests.len(), 2);
        assert_eq!(response.context_requests[0].path, "crates/core/src/lib.rs");
        assert_eq!(response.context_requests[0].access, ContextAccess::ReadOnly);
        assert_eq!(response.context_requests[1].path, "crates/core/src/lib.rs");
        assert_eq!(response.context_requests[1].access, ContextAccess::Editable);

        // An empty request list stays empty instead of defaulting to anything.
        assert!(DiscussionResponse::new("Hello")
            .with_context_requests(Vec::new())
            .context_requests
            .is_empty());
    }
    #[test]
    fn test_discovery_builders_keep_dot_compositions_as_the_repository_root() {
        // The whole-repository decision follows the decoded segment set, so a
        // composition of dot and separator segments reaches the same root as
        // the empty path instead of being dropped by the builder.
        let response = DiscussionResponse::new("Root")
            .with_repo_maps(vec![
                "./.".to_string(),
                "././".to_string(),
                ".//".to_string(),
                "./crates/core/".to_string(),
            ])
            .with_dir_lists(vec![
                DirListRequest::new("./.", true),
                DirListRequest::new(".//", false),
                DirListRequest::new("./crates/protocol/", true),
            ])
            .with_repo_searches(vec![
                RepoSearchRequest::new("././", "pub fn"),
                RepoSearchRequest::new("./crates/core/", "fn run"),
            ]);

        // Every dot composition survives as the whole-repository root in the
        // order it was declared, beside a real subtree that keeps its path.
        assert_eq!(response.repo_maps.len(), 4);
        assert!(response.repo_maps[..3].iter().all(|path| path.is_empty()));
        assert_eq!(response.repo_maps[3], "crates/core");

        assert_eq!(response.dir_lists.len(), 3);
        assert!(response.dir_lists[0].path.is_empty());
        assert!(response.dir_lists[0].recursive);
        assert!(response.dir_lists[1].path.is_empty());
        assert!(!response.dir_lists[1].recursive);
        assert_eq!(response.dir_lists[2].path, "crates/protocol");
        assert!(response.dir_lists[2].recursive);

        assert_eq!(response.repo_searches.len(), 2);
        assert!(response.repo_searches[0].path.is_empty());
        assert_eq!(response.repo_searches[0].regex, "pub fn");
        assert_eq!(response.repo_searches[1].path, "crates/core");
        assert_eq!(response.repo_searches[1].regex, "fn run");
    }
}
