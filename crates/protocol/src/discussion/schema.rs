//! Strict JSON Schema for the discussion structured-output contract.
//!
//! The schema is written by hand instead of being derived, because
//! `crates/protocol` is a dependency-free leaf crate: admitting a schema
//! derivation dependency would violate its mandate (Conventions §2). To keep a
//! single source of truth (Conventions §1.3), the schema is pinned to the
//! authoritative Rust data model by `test_schema_matches_data_model`, which
//! fails the build whenever a field, required key, or enum variant drifts away
//! from `DiscussionResponse` and its nested types.
//!
//! Property order is part of the wire contract: `message` must remain the first
//! property so a language model emits the conversational answer before the
//! heavier structural fields, letting the interface stream text immediately.
//! That order survives only because the protocol crate enables `serde_json`'s
//! `preserve_order` feature (see `crates/protocol/Cargo.toml`), which switches
//! JSON objects from a `BTreeMap` to an insertion-ordered `IndexMap`. Dropping
//! that feature would silently re-sort `properties` alphabetically and push
//! `message` out of first position.

use std::sync::OnceLock;

/// Process-wide cache of the assembled discussion schema.
///
/// The schema is immutable and depends only on compile-time structure, so it is
/// assembled once and cloned per call rather than rebuilt on every request.
static DISCUSSION_RESPONSE_SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();

/// Generates a strict JSON Schema representing `DiscussionResponse`.
///
/// Guarantees that the `message` string property is listed first in `properties`
/// and `required` so language models emit conversational text before plan
/// updates or context requests. The ordering relies on `serde_json`'s
/// `preserve_order` feature; see the module documentation.
pub fn discussion_response_schema() -> serde_json::Value {
    DISCUSSION_RESPONSE_SCHEMA
        .get_or_init(|| {
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
                                    "concept": {
                                        "type": "array",
                                        "description": "Hierarchical architectural concept items (requirements, invariants, decisions). Mandatory when action is 'save' to establish the conceptual foundation.",
                                        "items": {
                                            "$ref": "#/$defs/ConceptItem"
                                        }
                                    },
                                    "items": {
                                        "type": "array",
                                        "description": "Hierarchical implementation plan items or steps",
                                        "items": {
                                            "$ref": "#/$defs/PlanItem"
                                        }
                                    }
                                },
                                "required": ["action", "id", "title", "description", "concept", "items"],
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
                        "description": "Files to request into context for inspection or editable access",
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
                    "user_language": {
                        "anyOf": [
                            { "type": "string" },
                            { "type": "null" }
                        ],
                        "description": "Detected primary natural language of the user prompt"
                    }
                },
                "required": ["message", "plan_update", "context_requests", "user_language"],
                "additionalProperties": false,
                "$defs": {
                    "ConceptItem": {
                        "type": "object",
                        "description": "An architectural concept item or requirement",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Concept item identifier (e.g. 'c-1', '1.1')"
                            },
                            "title": {
                                "type": "string",
                                "description": "Short title or thesis of the concept item"
                            },
                            "details": {
                                "anyOf": [
                                    { "type": "string" },
                                    { "type": "null" }
                                ],
                                "description": "Detailed explanation, rationale, or invariant"
                            },
                            "children": {
                                "type": "array",
                                "description": "Sub-items or nested concept requirements",
                                "items": {
                                    "$ref": "#/$defs/ConceptItem"
                                }
                            }
                        },
                        "required": ["id", "title", "details", "children"],
                        "additionalProperties": false
                    },
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
                                "enum": ["discussion", "todo", "in_progress", "done", "blocked", "cancelled"],
                                "description": "Status of the plan step"
                            },
                            "files": {
                                "type": "array",
                                "description": "Target repository-relative file paths for this implementation step",
                                "items": {
                                    "type": "string"
                                }
                            },
                            "readonly_files": {
                                "type": "array",
                                "description": "Repository-relative file paths for read-only reference context (subsystem conventions, interface contracts, protocol types)",
                                "items": {
                                    "type": "string"
                                }
                            },
                            "concept_refs": {
                                "type": "array",
                                "description": "IDs of referenced concept items",
                                "items": {
                                    "type": "string"
                                }
                            },
                            "depends_on": {
                                "type": "array",
                                "description": "IDs of prerequisite steps that must reach DONE status before this step executes",
                                "items": {
                                    "type": "string"
                                }
                            },
                            "children": {
                                "type": "array",
                                "description": "Sub-items or nested steps",
                                "items": {
                                    "$ref": "#/$defs/PlanItem"
                                }
                            }
                        },
                        "required": ["id", "title", "details", "status", "files", "readonly_files", "concept_refs", "depends_on", "children"],
                        "additionalProperties": false
                    }
                }
            })
        })
        .clone()
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
    use super::{discussion_response_schema, discussion_response_schema_definition};
    use crate::context::ContextAccess;
    use crate::discussion::{
        DiscussionContextRequest, DiscussionPlanAction, DiscussionPlanUpdate, DiscussionResponse,
    };
    use crate::plan::{ConceptItem, PlanItem, PlanItemStatus};

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
        let lang_idx = schema_str.find("\"user_language\"").expect("user_language found");

        assert!(msg_idx < plan_idx);
        assert!(plan_idx < ctx_idx);
        assert!(ctx_idx < lang_idx);
    }

    #[test]
    fn test_schema_definition_wrapper() {
        let def = discussion_response_schema_definition();
        assert_eq!(def["name"], "discussion_response");
        assert_eq!(def["strict"], true);
        assert!(def["schema"].is_object());
    }

    /// Pins the strict JSON Schema to the authoritative Rust data model.
    ///
    /// Every property of `DiscussionResponse` and its nested types must appear in the
    /// schema (and vice versa), and every required field must be listed in `required`.
    /// This prevents silent drift between the strict schema and the types, which would
    /// otherwise forbid the model from expressing fields that exist in the model.
    #[test]
    fn test_schema_matches_data_model() {
        use std::collections::BTreeSet;

        fn keys(value: &serde_json::Value) -> BTreeSet<String> {
            value
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        }

        fn field_sets(node: &serde_json::Value) -> (BTreeSet<String>, BTreeSet<String>) {
            let props = keys(&node["properties"]);
            let required: BTreeSet<String> = node["required"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            (props, required)
        }

        let schema = discussion_response_schema();

        // 1. Top-level DiscussionResponse fields must match the schema exactly.
        let response = DiscussionResponse::new("hello")
            .with_plan_update(DiscussionPlanUpdate {
                action: DiscussionPlanAction::Save,
                id: Some("p1".to_string()),
                title: Some("Plan".to_string()),
                description: Some("desc".to_string()),
                concept: vec![ConceptItem::default()],
                items: vec![PlanItem::default()],
            })
            .with_context_requests(vec![DiscussionContextRequest::read_only("a.rs")])
            .with_user_language("English");
        let (props, required) = field_sets(&schema);
        assert_eq!(props, keys(&serde_json::to_value(&response).unwrap()));
        assert_eq!(
            required, props,
            "strict schema must require every DiscussionResponse field"
        );

        // 2. Nested plan_update object.
        let (props, required) = field_sets(&schema["properties"]["plan_update"]["anyOf"][0]);
        assert_eq!(
            props,
            keys(&serde_json::to_value(response.plan_update.clone().unwrap()).unwrap())
        );
        assert_eq!(required, props);

        // 3. context_requests item object.
        let (props, required) = field_sets(&schema["properties"]["context_requests"]["items"]);
        assert_eq!(
            props,
            keys(&serde_json::to_value(DiscussionContextRequest::read_only("a.rs")).unwrap())
        );
        assert_eq!(required, props);

        // 4. ConceptItem definition.
        let concept = ConceptItem {
            id: "c-1".to_string(),
            title: "Concept".to_string(),
            details: Some("details".to_string()),
            children: vec![ConceptItem::default()],
        };
        let (props, required) = field_sets(&schema["$defs"]["ConceptItem"]);
        assert_eq!(props, keys(&serde_json::to_value(&concept).unwrap()));
        assert_eq!(required, props);

        // 5. PlanItem definition (includes readonly_files, depends_on, status).
        let item = PlanItem {
            id: "1".to_string(),
            title: "Step".to_string(),
            details: Some("details".to_string()),
            status: PlanItemStatus::Todo,
            files: vec!["src/a.rs".to_string()],
            readonly_files: vec!["src/b.rs".to_string()],
            concept_refs: vec!["c-1".to_string()],
            depends_on: vec!["0".to_string()],
            children: vec![PlanItem::default()],
        };
        let (props, required) = field_sets(&schema["$defs"]["PlanItem"]);
        assert_eq!(props, keys(&serde_json::to_value(&item).unwrap()));
        assert_eq!(required, props);

        // 6. Every PlanItemStatus variant must be expressible in the status enum.
        let status_enum: BTreeSet<String> =
            schema["$defs"]["PlanItem"]["properties"]["status"]["enum"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
        for status in [
            PlanItemStatus::Discussion,
            PlanItemStatus::Todo,
            PlanItemStatus::InProgress,
            PlanItemStatus::Done,
            PlanItemStatus::Blocked,
            PlanItemStatus::Cancelled,
        ] {
            let wire = serde_json::to_value(status).unwrap();
            let wire = wire.as_str().unwrap();
            assert!(
                status_enum.contains(wire),
                "status enum is missing variant {wire}"
            );
        }

        // 7. Every DiscussionPlanAction variant must be expressible in the
        //    plan_update.action enum.
        let action_enum: BTreeSet<String> =
            schema["properties"]["plan_update"]["anyOf"][0]["properties"]["action"]["enum"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
        for action in [
            DiscussionPlanAction::Update,
            DiscussionPlanAction::Save,
            DiscussionPlanAction::Delete,
        ] {
            let wire = serde_json::to_value(action).unwrap();
            let wire = wire.as_str().unwrap();
            assert!(
                action_enum.contains(wire),
                "plan_update.action enum is missing variant {wire}"
            );
        }

        // 8. Every ContextAccess variant must be expressible in the
        //    context_requests.access enum.
        let access_enum: BTreeSet<String> =
            schema["properties"]["context_requests"]["items"]["properties"]["access"]["enum"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
        for access in [ContextAccess::ReadOnly, ContextAccess::Editable] {
            let wire = serde_json::to_value(access).unwrap();
            let wire = wire.as_str().unwrap();
            assert!(
                access_enum.contains(wire),
                "context_requests.access enum is missing variant {wire}"
            );
        }
    }
}
