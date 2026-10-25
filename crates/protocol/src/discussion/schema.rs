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

use crate::plan::PlanItemStatus;

/// Process-wide cache of the assembled discussion schema.
///
/// The schema is immutable and depends only on compile-time structure, so it is
/// assembled once and cloned per call rather than rebuilt on every request.
static DISCUSSION_RESPONSE_SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();

/// Process-wide cache of the structured plan payload property.
static PLAN_UPDATE_SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();

/// Process-wide cache of the shared definitions the plan payload references.
static PLAN_SCHEMA_DEFINITIONS: OnceLock<serde_json::Value> = OnceLock::new();

/// Generates the structured plan payload property of the plan contract.
///
/// The plan shape is declared here once, beside the Rust plan model it mirrors,
/// and every contour that can author a plan composes this builder: the
/// discussion contract embeds it as its `plan_update` property and the
/// code-modification contract re-exports it verbatim. A code-modification turn
/// and a discussion turn therefore describe one identical plan, and no consumer
/// re-declares the shape.
///
/// The property admits `null` because a turn that leaves the plan untouched
/// carries no payload at all, which is the idle representation of a property a
/// strict schema still requires.
pub fn plan_update_schema() -> serde_json::Value {
    PLAN_UPDATE_SCHEMA
        .get_or_init(|| {
            serde_json::json!({
                "anyOf": [
                    {
                        "type": "object",
                        "description": "Modifications or creation/deletion of plan items. Must be null unless the user explicitly requested to update or modify the plan.",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": ["update", "save", "delete"],
                                "description": "Action to perform: 'save' to create a plan or to replace its entire implementation tree (every step already in 'done' status must be reproduced verbatim, otherwise the completed-step immutability check rejects the mutation); 'update' to patch existing steps and concept nodes by ID and to append new leaf steps to the plan root; 'delete' to remove the plan"
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
                                "description": "Hierarchical implementation plan items or steps; an 'update' action may add leaf steps only",
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
            })
        })
        .clone()
}

/// Generates the shared definitions [`plan_update_schema`] references.
///
/// A plan is a tree: concept nodes and implementation steps nest their own kind,
/// so neither shape can be inlined into the payload without the payload
/// containing itself. The definitions travel beside the payload, and a consumer
/// lifts them to the root of its composed schema, which is where the
/// `#/$defs/<Name>` references resolve.
///
/// `children` is declared on both definitions and points back to the definition
/// that owns it, so a nested tree of any depth validates against the very same
/// contract as its root. Every field of `ConceptItem` and `PlanItem` appears
/// exactly once, and the `status` enum is generated from [`PlanItemStatus`], so
/// a new variant cannot be forgotten here.
pub fn plan_schema_definitions() -> serde_json::Value {
    PLAN_SCHEMA_DEFINITIONS
        .get_or_init(|| {
            serde_json::json!({
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
                            "enum": plan_item_status_enum(),
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
            })
        })
        .clone()
}

/// Builds the `status` enum of the plan item definition from [`PlanItemStatus`].
fn plan_item_status_enum() -> serde_json::Value {
    const STATUSES: &[PlanItemStatus] = &[
        PlanItemStatus::Discussion,
        PlanItemStatus::Todo,
        PlanItemStatus::InProgress,
        PlanItemStatus::Done,
        PlanItemStatus::Blocked,
        PlanItemStatus::Cancelled,
    ];
    serde_json::Value::Array(
        STATUSES
            .iter()
            .map(|status| serde_json::Value::String(plan_item_status_wire(*status).to_string()))
            .collect(),
    )
}

/// Wire spelling of every [`PlanItemStatus`] variant.
///
/// The match is exhaustive, so adding a variant fails compilation until its
/// spelling is declared here; that is what keeps the schema enum in lockstep
/// with the Rust model instead of a hand-copied list. The spelling itself is
/// pinned to the serde representation by a test, because only serde decides what
/// actually travels on the wire.
fn plan_item_status_wire(status: PlanItemStatus) -> &'static str {
    match status {
        PlanItemStatus::Discussion => "discussion",
        PlanItemStatus::Todo => "todo",
        PlanItemStatus::InProgress => "in_progress",
        PlanItemStatus::Done => "done",
        PlanItemStatus::Blocked => "blocked",
        PlanItemStatus::Cancelled => "cancelled",
    }
}

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
                    "plan_update": plan_update_schema(),
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
                    "repo_searches": {
                        "type": "array",
                        "description": "Repository search requests to find symbols or text patterns across files",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Repository-relative directory or file path to search within"
                                },
                                "regex": {
                                    "type": "string",
                                    "description": "Regular expression pattern to search for"
                                }
                            },
                            "required": ["path", "regex"],
                            "additionalProperties": false
                        }
                    },
                    "repo_dirs": {
                        "anyOf": [
                            { "type": "boolean" },
                            { "type": "null" }
                        ],
                        "description": "Request the compact repository directory tree when the repository layout is still unknown; use null or false when the provided contours are sufficient"
                    },
                    "repo_maps": {
                        "type": "array",
                        "description": "Targeted symbol map requests for specific subsystems; each entry is a repository-relative directory or file prefix, and an empty string or '.' requests the whole repository",
                        "items": {
                            "type": "string"
                        }
                    },
                    "dir_lists": {
                        "type": "array",
                        "description": "Directory content listing requests over the tracked repository files; each entry names a repository-relative directory prefix, and an empty string or '.' requests the repository root. The listing is shallow unless 'recursive' is true.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Repository-relative directory path, or an empty string for the repository root"
                                },
                                "recursive": {
                                    "type": "boolean",
                                    "description": "List every tracked file underneath the directory instead of only its direct children"
                                }
                            },
                            "required": ["path", "recursive"],
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
                "required": ["message", "plan_update", "context_requests", "repo_searches", "repo_dirs", "repo_maps", "dir_lists", "user_language"],
                "additionalProperties": false,
                "$defs": plan_schema_definitions()
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

/// Generates the JSON Schema of a single project custom tool invocation.
///
/// A custom tool is a command the project declares in `tauqe.toml`. The harness
/// publishes exactly the declared commands as typed tools, so an invocation
/// carries the name of one declared command together with its arguments. The
/// `name` and the argument container are fixed by the wire contract here, while
/// the concrete argument properties belong to the command's own declaration: the
/// harness validates `args` against that declaration instead of accepting an
/// arbitrary command line, which is what keeps a custom tool from widening into
/// an unbounded shell.
pub fn custom_tool_call_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Name of one project-declared command from the custom tool allowlist"
            },
            "args": {
                "type": "object",
                "description": "Typed argument object validated against the argument schema declared for this command"
            }
        },
        "required": ["name", "args"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::{
        discussion_response_schema, discussion_response_schema_definition, plan_item_status_wire,
        plan_schema_definitions, plan_update_schema,
    };
    use crate::context::ContextAccess;
    use crate::discussion::{
        DirListRequest, DiscussionContextRequest, DiscussionPlanAction, DiscussionPlanUpdate,
        DiscussionResponse, RepoSearchRequest,
    };
    use crate::plan::{ConceptItem, PlanItem, PlanItemStatus};

    #[test]
    fn test_custom_tool_call_schema_pins_the_invocation_contract() {
        let schema = super::custom_tool_call_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["name"]["type"], "string");
        // The argument container is an object whose properties are declared by
        // the project, so a custom tool can never carry a free-form command line.
        assert_eq!(schema["properties"]["args"]["type"], "object");
        let properties = schema["properties"]
            .as_object()
            .expect("properties object must exist");
        assert_eq!(properties.len(), 2);

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required array must exist")
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert_eq!(required, ["name", "args"]);
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn test_message_is_first_property_in_schema() {
        let schema = discussion_response_schema();
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("properties object must exist");

        let first_key = properties.keys().next().expect("first property must exist");
        assert_eq!(
            first_key, "message",
            "message MUST be the first property for streaming"
        );

        let schema_str = serde_json::to_string(&schema).unwrap();
        let msg_idx = schema_str.find("\"message\"").expect("message found");
        let plan_idx = schema_str
            .find("\"plan_update\"")
            .expect("plan_update found");
        let ctx_idx = schema_str
            .find("\"context_requests\"")
            .expect("context_requests found");
        let search_idx = schema_str
            .find("\"repo_searches\"")
            .expect("repo_searches found");
        let dirs_idx = schema_str.find("\"repo_dirs\"").expect("repo_dirs found");
        let maps_idx = schema_str.find("\"repo_maps\"").expect("repo_maps found");
        let lists_idx = schema_str.find("\"dir_lists\"").expect("dir_lists found");
        let lang_idx = schema_str
            .find("\"user_language\"")
            .expect("user_language found");

        assert!(msg_idx < plan_idx);
        assert!(plan_idx < ctx_idx);
        assert!(ctx_idx < search_idx);
        assert!(search_idx < dirs_idx);
        assert!(dirs_idx < maps_idx);
        assert!(maps_idx < lists_idx);
        assert!(lists_idx < lang_idx);
    }

    /// Collects the property names a schema object declares.
    fn declared_fields(node: &serde_json::Value) -> std::collections::BTreeSet<String> {
        node["properties"]
            .as_object()
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Collects the property names a schema object requires.
    fn required_fields(node: &serde_json::Value) -> std::collections::BTreeSet<String> {
        node["required"]
            .as_array()
            .map(|required| {
                required
                    .iter()
                    .filter_map(|field| field.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A plan is authored by two contours, so its payload is generated once.
    ///
    /// The discussion contract embeds the authoritative builders verbatim, and
    /// the code-modification contract re-exports them, which is what makes one
    /// definition of the plan shape serve both.
    #[test]
    fn test_plan_payload_is_generated_once() {
        let schema = discussion_response_schema();
        assert_eq!(schema["properties"]["plan_update"], plan_update_schema());
        assert_eq!(schema["$defs"], plan_schema_definitions());

        let payload = plan_update_schema();
        let variants = payload["anyOf"]
            .as_array()
            .expect("the plan payload is a nullable union");
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[1]["type"], "null");

        let object = &variants[0];
        assert_eq!(object["type"], "object");
        assert_eq!(object["additionalProperties"], serde_json::json!(false));
        assert_eq!(
            declared_fields(object),
            required_fields(object),
            "the payload requires exactly the fields it declares"
        );
        assert_eq!(
            object["properties"]["concept"]["items"]["$ref"],
            "#/$defs/ConceptItem"
        );
        assert_eq!(
            object["properties"]["items"]["items"]["$ref"],
            "#/$defs/PlanItem"
        );
    }

    /// The definitions mirror `ConceptItem` and `PlanItem`, including `children`.
    ///
    /// Each definition requires exactly the fields it declares and nests its own
    /// kind through `children`, so a tree of any depth validates against the
    /// contract of its root. The `status` enum is generated from the Rust model,
    /// and its spellings are the spellings serde actually emits.
    #[test]
    fn test_plan_definitions_mirror_the_plan_model() {
        let definitions = plan_schema_definitions();
        let names: Vec<String> = definitions
            .as_object()
            .map(|definitions| definitions.keys().cloned().collect())
            .unwrap_or_default();
        assert_eq!(
            names,
            vec!["ConceptItem".to_string(), "PlanItem".to_string()]
        );

        for name in ["ConceptItem", "PlanItem"] {
            let node = &definitions[name];
            assert_eq!(node["type"], "object", "{name}");
            assert_eq!(node["additionalProperties"], serde_json::json!(false));
            assert_eq!(
                declared_fields(node),
                required_fields(node),
                "{name} requires exactly the fields it declares"
            );
            assert_eq!(node["properties"]["children"]["type"], "array", "{name}");
            assert_eq!(
                node["properties"]["children"]["items"]["$ref"],
                serde_json::json!(format!("#/$defs/{name}"))
            );
        }

        let statuses = [
            PlanItemStatus::Discussion,
            PlanItemStatus::Todo,
            PlanItemStatus::InProgress,
            PlanItemStatus::Done,
            PlanItemStatus::Blocked,
            PlanItemStatus::Cancelled,
        ];
        let published: Vec<&str> = definitions["PlanItem"]["properties"]["status"]["enum"]
            .as_array()
            .expect("the status enum is an array")
            .iter()
            .filter_map(|status| status.as_str())
            .collect();
        let expected: Vec<&str> = statuses.into_iter().map(plan_item_status_wire).collect();
        assert_eq!(published, expected);
        for status in statuses {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(plan_item_status_wire(status).to_string())
            );
        }
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
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
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
            .with_repo_searches(vec![RepoSearchRequest::new("crates/core", "regex")])
            .with_repo_dirs(true)
            .with_repo_maps(vec!["crates/core".to_string()])
            .with_dir_lists(vec![DirListRequest::new("crates/core", true)])
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

        // 3a. repo_searches item object.
        let (props, required) = field_sets(&schema["properties"]["repo_searches"]["items"]);
        assert_eq!(
            props,
            keys(&serde_json::to_value(RepoSearchRequest::new("a", "b")).unwrap())
        );
        assert_eq!(required, props);

        // 3b. repo_dirs is a nullable boolean and repo_maps is a path array.
        let dir_variants = schema["properties"]["repo_dirs"]["anyOf"]
            .as_array()
            .expect("repo_dirs anyOf array");
        assert!(dir_variants.iter().any(|v| v["type"] == "boolean"));
        assert!(dir_variants.iter().any(|v| v["type"] == "null"));
        assert_eq!(schema["properties"]["repo_maps"]["type"], "array");
        assert_eq!(schema["properties"]["repo_maps"]["items"]["type"], "string");

        // 3c. dir_lists item object.
        let (props, required) = field_sets(&schema["properties"]["dir_lists"]["items"]);
        assert_eq!(
            props,
            keys(&serde_json::to_value(DirListRequest::new("crates/core", true)).unwrap())
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
        let status_enum: BTreeSet<String> = schema["$defs"]["PlanItem"]["properties"]["status"]
            ["enum"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
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
        let action_enum: BTreeSet<String> = schema["properties"]["plan_update"]["anyOf"][0]
            ["properties"]["action"]["enum"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
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
        let access_enum: BTreeSet<String> = schema["properties"]["context_requests"]["items"]
            ["properties"]["access"]["enum"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        for access in [ContextAccess::ReadOnly, ContextAccess::Editable] {
            let wire = serde_json::to_value(access).unwrap();
            let wire = wire.as_str().unwrap();
            assert!(
                access_enum.contains(&*wire.to_string()),
                "context_requests.access enum is missing variant {wire}"
            );
        }
    }
}
