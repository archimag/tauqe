use serde::{Deserialize, Serialize};

use crate::context::{default_context_access, ContextAccess};
use crate::plan::PlanItem;

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
        deserialize_with = "deserialize_null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub context_requests: Vec<DiscussionContextRequest>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub context_drops: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_language: Option<String>,
}

impl DiscussionResponse {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            plan_update: None,
            context_requests: Vec::new(),
            context_drops: Vec::new(),
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

    pub fn with_context_drops(mut self, drops: Vec<String>) -> Self {
        self.context_drops = drops;
        self
    }

    pub fn with_user_language(mut self, language: impl Into<String>) -> Self {
        self.user_language = Some(language.into());
        self
    }

    /// Returns the raw strict JSON Schema for `DiscussionResponse`.
    pub fn schema() -> serde_json::Value {
        discussion_response_schema()
    }

    /// Returns the complete JSON schema definition object suitable for `response_format: JsonSchema`.
    pub fn schema_definition() -> serde_json::Value {
        discussion_response_schema_definition()
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

fn deserialize_context_access<'de, D>(deserializer: D) -> Result<ContextAccess, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<ContextAccess>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(ContextAccess::ReadOnly))
}

/// Generates a strict JSON Schema representing `DiscussionResponse`.
///
/// Guarantees that the `message` string property is listed first in `properties` and `required`
/// so language models emit conversational text before plan updates or context requests.
pub fn discussion_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Conversational response, reasoning, and answers formatted in Markdown. This field is streamed immediately to the user interface."
            },
            "plan_update": {
                "anyOf": [
                    {
                        "type": "object",
                        "description": "Modifications or creation/deletion of plan items. Must be null unless the user explicitly requested to update or modify the plan.",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": ["update", "save", "delete"],
                                "description": "Action to perform: 'save' to create or completely overwrite/replace the entire plan and step tree, 'update' to modify individual existing items by ID, or 'delete' to remove the plan"
                            },
                            "id": {
                                "anyOf": [
                                    { "type": "string" },
                                    { "type": "null" }
                                ],
                                "description": "Target plan identifier"
                            },
                            "title": {
                                "anyOf": [
                                    { "type": "string" },
                                    { "type": "null" }
                                ],
                                "description": "Plan title"
                            },
                            "description": {
                                "anyOf": [
                                    { "type": "string" },
                                    { "type": "null" }
                                ],
                                "description": "Plan description and architectural rationale"
                            },
                            "items": {
                                "type": "array",
                                "description": "Hierarchical plan items or steps",
                                "items": {
                                    "$ref": "#/$defs/PlanItem"
                                }
                            }
                        },
                        "required": ["action", "id", "title", "description", "items"],
                        "additionalProperties": false
                    },
                    {
                        "type": "null"
                    }
                ],
                "description": "Optional plan update or creation to be applied at the end of the turn"
            },
            "context_requests": {
                "type": "array",
                "description": "Files to request into context for inspection or editable access (in snapshot discovery mode, specify the complete active set of auto-context files)",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Repository-relative file path"
                        },
                        "access": {
                            "type": "string",
                            "enum": ["read_only", "editable"],
                            "description": "Access level requested for the file (default: read_only)"
                        }
                    },
                    "required": ["path", "access"],
                    "additionalProperties": false
                }
            },
            "context_drops": {
                "type": "array",
                "description": "Files to drop from auto context if no longer needed (used in incremental discovery mode to release irrelevant files)",
                "items": {
                    "type": "string"
                }
            },
            "user_language": {
                "anyOf": [
                    { "type": "string" },
                    { "type": "null" }
                ],
                "description": "Detected primary natural language of user prompt (e.g. Russian, English)"
            }
        },
        "required": ["message", "plan_update", "context_requests", "context_drops", "user_language"],
        "additionalProperties": false,
        "$defs": {
            "PlanItem": {
                "type": "object",
                "description": "An item or step within a plan",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Step identifier (e.g. '1', '1.1')"
                    },
                    "title": {
                        "type": "string",
                        "description": "Short title of the step in the same language as the plan description and user request"
                    },
                    "details": {
                        "anyOf": [
                            { "type": "string" },
                            { "type": "null" }
                        ],
                        "description": "Detailed description, requirements, or execution criteria in the same language as the plan description"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["discussion", "todo", "in_progress", "done", "cancelled"],
                        "description": "Status of the plan step"
                    },
                    "children": {
                        "type": "array",
                        "description": "Sub-items or nested steps",
                        "items": {
                            "$ref": "#/$defs/PlanItem"
                        }
                    }
                },
                "required": ["id", "title", "details", "status", "children"],
                "additionalProperties": false
            }
        }
    })
}

/// Generates a complete JSON schema wrapper object for OpenAI/OpenRouter structured outputs.
pub fn discussion_response_schema_definition() -> serde_json::Value {
    serde_json::json!({
        "name": "discussion_response",
        "description": "Structured response for discussion and planning modes",
        "strict": true,
        "schema": discussion_response_schema()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanItemStatus;

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
        assert!(resp.context_drops.is_empty());
        assert!(resp.user_language.is_none());
    }

    #[test]
    fn test_discussion_response_with_nulls_serde() {
        let raw = r#"{
            "message": "Acknowledged",
            "plan_update": null,
            "context_requests": null,
            "context_drops": null,
            "user_language": null
        }"#;
        let resp: DiscussionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.message, "Acknowledged");
        assert!(resp.plan_update.is_none());
        assert!(resp.context_requests.is_empty());
        assert!(resp.context_drops.is_empty());
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
                items: vec![PlanItem {
                    id: "1".to_string(),
                    title: "Protocol types".to_string(),
                    details: Some("Add discussion schemas".to_string()),
                    status: PlanItemStatus::Done,
                    children: vec![],
                }],
            });

        let json = serde_json::to_string_pretty(&resp).unwrap();
        let de: DiscussionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(de, resp);
    }

    #[test]
    fn test_message_is_first_property_in_schema() {
        let schema = discussion_response_schema();
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("properties object must exist");

        let first_key = properties.keys().next().expect("first property must exist");
        assert_eq!(first_key, "message", "message MUST be the first property for streaming");

        let schema_str = serde_json::to_string(&schema).unwrap();
        let msg_idx = schema_str.find("\"message\"").expect("message found");
        let plan_idx = schema_str.find("\"plan_update\"").expect("plan_update found");
        let ctx_idx = schema_str.find("\"context_requests\"").expect("context_requests found");
        let drops_idx = schema_str.find("\"context_drops\"").expect("context_drops found");
        let lang_idx = schema_str.find("\"user_language\"").expect("user_language found");

        assert!(msg_idx < plan_idx);
        assert!(plan_idx < ctx_idx);
        assert!(ctx_idx < drops_idx);
        assert!(drops_idx < lang_idx);
    }

    #[test]
    fn test_schema_definition_wrapper() {
        let def = discussion_response_schema_definition();
        assert_eq!(def["name"], "discussion_response");
        assert_eq!(def["strict"], true);
        assert!(def["schema"].is_object());
    }
}
