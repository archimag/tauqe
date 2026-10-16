use serde::{Deserialize, Serialize};

use crate::context::{default_context_access, ContextAccess};
use crate::edit::normalize_repo_path;
use crate::plan::{ConceptItem, PlanItem};

pub mod schema;

pub use schema::{discussion_response_schema, discussion_response_schema_definition};

/// Action to perform on a project plan as part of discussion structured output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiscussionPlanAction {
    #[default]
    Update,
    Save,
    Delete,
}

impl std::fmt::Display for DiscussionPlanAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Update => write!(f, "update"),
            Self::Save => write!(f, "save"),
            Self::Delete => write!(f, "delete"),
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
    /// whitespace, backticks, and a leading `./`. The path is normalized and
    /// validated through [`normalize_repo_path`], so an empty, absolute, or
    /// traversal path yields `None` instead of an escape attempt.
    pub fn parse_spec(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        let (access, rest) = if let Some(rest) = spec.strip_prefix("editable:") {
            (ContextAccess::Editable, rest)
        } else if let Some(rest) = spec.strip_prefix("read_only:") {
            (ContextAccess::ReadOnly, rest)
        } else {
            (ContextAccess::ReadOnly, spec)
        };
        let cleaned = rest.trim().trim_matches('`').trim();
        let path = normalize_repo_path(cleaned).ok()?;
        Some(Self { path, access })
    }
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
            DiscussionTarget::Review { id: review_id.into() },
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
    #[serde(
        default,
        deserialize_with = "deserialize_context_requests",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub context_requests: Vec<DiscussionContextRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_language: Option<String>,
}

impl DiscussionResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            plan_update: None,
            context_requests: Vec::new(),
            user_language: None,
        }
    }

    pub fn with_plan_update(mut self, plan_update: DiscussionPlanUpdate) -> Self {
        self.plan_update = Some(plan_update);
        self
    }

    pub fn with_context_requests(mut self, requests: Vec<DiscussionContextRequest>) -> Self {
        self.context_requests = requests;
        self
    }

    pub fn with_user_language(mut self, language: impl Into<String>) -> Self {
        self.user_language = Some(language.into());
        self
    }
}

fn deserialize_null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    let opt = Option::<T>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// Deserializes the `context_requests` field of [`DiscussionResponse`],
/// normalizing every path through [`normalize_repo_path`].
///
/// A `null` payload yields an empty list, and any request whose path cannot
/// identify a repository file is dropped, so a directly decoded
/// [`DiscussionResponse`] can no longer bypass path validation.
fn deserialize_context_requests<'de, D>(
    deserializer: D,
) -> Result<Vec<DiscussionContextRequest>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let requests = Option::<Vec<DiscussionContextRequest>>::deserialize(deserializer)?;
    Ok(requests
        .unwrap_or_default()
        .into_iter()
        .filter_map(DiscussionContextRequest::normalized)
        .collect())
}

fn deserialize_context_access<'de, D>(deserializer: D) -> Result<ContextAccess, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<ContextAccess>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(ContextAccess::ReadOnly))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanItemStatus;

    #[test]
    fn test_discussion_context_request_parse_spec() {
        let ro = DiscussionContextRequest::parse_spec(" ./src/a.rs ").unwrap();
        assert_eq!(ro.path, "src/a.rs");
        assert_eq!(ro.access, ContextAccess::ReadOnly);
        let ed = DiscussionContextRequest::parse_spec("editable:src/b.rs").unwrap();
        assert_eq!(ed.path, "src/b.rs");
        assert_eq!(ed.access, ContextAccess::Editable);
        assert!(DiscussionContextRequest::parse_spec("editable:").is_none());

        // Absolute, traversal, and NUL paths are rejected rather than silently
        // rewritten into a relative path.
        assert!(DiscussionContextRequest::parse_spec("/etc/passwd").is_none());
        assert!(DiscussionContextRequest::parse_spec("../escape.rs").is_none());
        assert!(DiscussionContextRequest::parse_spec("editable:a/../b").is_none());
        assert!(DiscussionContextRequest::parse_spec("a\0b").is_none());
    }

    #[test]
    fn test_discussion_response_normalizes_context_request_paths() {
        let raw = r#"{
            "message": "Need files",
            "context_requests": [
                {"path": "./src/a.rs", "access": "read_only"},
                {"path": "../escape.rs", "access": "read_only"},
                {"path": "crates/core/src/lib.rs", "access": "editable"}
            ]
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        // The `./` prefix is normalized and the traversal entry is dropped.
        assert_eq!(resp.context_requests.len(), 2);
        assert_eq!(resp.context_requests[0].path, "src/a.rs");
        assert_eq!(resp.context_requests[0].access, ContextAccess::ReadOnly);
        assert_eq!(resp.context_requests[1].path, "crates/core/src/lib.rs");
        assert_eq!(resp.context_requests[1].access, ContextAccess::Editable);

        // A `null` payload still yields an empty list.
        let null_resp: DiscussionResponse =
            serde_json::from_str(r#"{"message":"x","context_requests":null}"#).unwrap();
        assert!(null_resp.context_requests.is_empty());
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
    fn test_discussion_plan_action_display_and_serde() {
        assert_eq!(DiscussionPlanAction::Update.to_string(), "update");
        assert_eq!(DiscussionPlanAction::Save.to_string(), "save");
        assert_eq!(DiscussionPlanAction::Delete.to_string(), "delete");

        let json = serde_json::to_string(&DiscussionPlanAction::Save).unwrap();
        assert_eq!(json, "\"save\"");
        let de: DiscussionPlanAction = serde_json::from_str(&json).unwrap();
        assert_eq!(de, DiscussionPlanAction::Save);
    }

    #[test]
    fn test_discussion_response_minimal_serde() {
        let raw = r#"{"message":"Hello developer!"}"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.message, "Hello developer!");
        assert!(resp.plan_update.is_none());
        assert!(resp.context_requests.is_empty());
        assert!(resp.user_language.is_none());
    }

    #[test]
    fn test_discussion_response_with_nulls_serde() {
        let raw = r#"{
            "message": "Acknowledged",
            "plan_update": null,
            "context_requests": null,
            "user_language": null
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.message, "Acknowledged");
        assert!(resp.plan_update.is_none());
        assert!(resp.context_requests.is_empty());
        assert!(resp.user_language.is_none());
    }

    #[test]
    fn test_discussion_response_full_roundtrip() {
        let resp = DiscussionResponse::new("Let's proceed with architecture update")
            .with_user_language("Russian")
            .with_context_requests(vec![
                DiscussionContextRequest::read_only("crates/core/src/lib.rs"),
                DiscussionContextRequest::editable("crates/protocol/src/lib.rs"),
            ])
            .with_plan_update(DiscussionPlanUpdate {
                action: DiscussionPlanAction::Update,
                id: Some("structured-output".to_string()),
                title: Some("Structured Output".to_string()),
                description: Some("Implement JSON schema protocol".to_string()),
                concept: vec![],
                items: vec![PlanItem {
                    id: "1".to_string(),
                    title: "Protocol types".to_string(),
                    details: Some("Add discussion schemas".to_string()),
                    status: PlanItemStatus::Done,
                    files: vec![],
                    readonly_files: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![],
                }],
            });

        let json = serde_json::to_string_pretty(&resp).unwrap();
        let de: DiscussionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(de, resp);
    }
}
