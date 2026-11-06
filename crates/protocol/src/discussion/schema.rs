//! Strict JSON Schema for the discussion structured-output contract.
//!
//! The schema is written by hand instead of being derived, because
//! `crates/protocol` is a dependency-free leaf crate: admitting a schema
//! derivation dependency would violate its mandate (Conventions §2). To keep a
//! single source of truth (Conventions §1.3), the schema is pinned to the
//! authoritative Rust data model by `test_schema_matches_data_model`, which
//! fails the build whenever a field, required key, enum variant, or numeric
//! domain drifts away from `DiscussionResponse` and its nested types.
//!
//! Every schema is assembled once into a process-wide cache and served by
//! shared reference, so a caller composes or serializes through the value the
//! cache holds instead of paying a per-call deep copy of the tree. The
//! accessors therefore return `&'static serde_json::Value`: a consumer that
//! needs an owned tree clones the reference explicitly, and embedding one
//! schema into another does exactly that once, while the cache is initialized.
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
use crate::review::{ReviewDecisionStatus, ReviewUpdateProposal};

/// Process-wide cache of the assembled discussion schema.
///
/// The schema is immutable and depends only on compile-time structure, so it is
/// assembled once and served by reference for the rest of the process.
static DISCUSSION_RESPONSE_SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();

/// Process-wide cache of the structured plan payload property.
static PLAN_UPDATE_SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();

/// Process-wide cache of the shared definitions the plan payload references.
static PLAN_SCHEMA_DEFINITIONS: OnceLock<serde_json::Value> = OnceLock::new();

/// Process-wide cache of the structured review decision property.
static REVIEW_UPDATE_SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();

/// Process-wide cache of the assembled structured-output wrapper.
static DISCUSSION_RESPONSE_SCHEMA_DEFINITION: OnceLock<serde_json::Value> = OnceLock::new();

/// Process-wide cache of the project custom tool invocation contract.
static CUSTOM_TOOL_CALL_SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();

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
/// carries no payload at all, which is the idle representation of a property
/// the strict schema requires.
///
/// The payload is assembled once and served by a shared reference, so a caller
/// that composes or serializes it pays no per-call deep copy of the tree.
/// Embedding it elsewhere clones the reference explicitly, which is why the
/// accessor hands workspace consumers a `&'static serde_json::Value` instead of
/// an owned tree.
pub fn plan_update_schema() -> &'static serde_json::Value {
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
/// a variant the model gains or loses cannot drift away from the schema the
/// model reads: the builder matches the enum exhaustively, and a test pins
/// every generated spelling to the serde representation that travels on the
/// wire.
///
/// The definitions are assembled once and served by a shared reference, so a
/// consumer that embeds them clones the reference once instead of paying a
/// per-call deep copy of both trees.
pub fn plan_schema_definitions() -> &'static serde_json::Value {
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
                            "description": "Status of the plan step. A step that a plan mutation creates or amends may carry only 'todo' or 'discussion': 'in_progress', 'blocked', and 'cancelled' belong to the execution pipeline and to the developer's manual control, while 'done' is reachable only through a verified step turn. An assignment of any other status to a created step is repaired to 'todo' and reported instead of being persisted."
                        },
                        "commit": {
                            "anyOf": [
                                { "type": "string" },
                                { "type": "null" }
                            ],
                            "description": "Identifier of the Git commit that completed this step. The harness records it when the step reaches 'done', and a mutation that reproduces a finished step reproduces its commit verbatim."
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
                        "outer_docs": {
                            "type": "array",
                            "description": "Identifiers of the project-declared `[[outer_docs]]` entries preloaded as read-only reference context when this step executes",
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
                    "required": ["id", "title", "details", "status", "commit", "files", "readonly_files", "outer_docs", "concept_refs", "depends_on", "children"],
                    "additionalProperties": false
                }
            })
        })
}

/// Generates the structured review decision property of the review contract.
///
/// The decision shape is declared here once, beside the Rust review model it
/// mirrors, so both contours describe one identical payload: the discussion
/// contract embeds it as its `review_update` property and the
/// code-modification schema forwards the very same builder through its own
/// capability module.
///
/// The property admits `null` because a turn that records no decision carries no
/// payload at all, which is the idle representation of a property the strict
/// schema requires.
///
/// The payload is assembled once and served by a shared reference, so a caller
/// that composes or serializes it pays no per-call deep copy of the tree.
pub fn review_update_schema() -> &'static serde_json::Value {
    REVIEW_UPDATE_SCHEMA.get_or_init(|| {
        serde_json::json!({
            "anyOf": [
                {
                    "type": "object",
                    "description": "Decision recorded for one finding of the discussed review session. Null unless the discussion is anchored on a review session and the engineer has reached a final decision.",
                    "properties": {
                        "item_id": {
                            "type": "integer",
                            "minimum": ReviewUpdateProposal::ITEM_ID_MINIMUM,
                            "maximum": ReviewUpdateProposal::ITEM_ID_MAXIMUM,
                            "description": "Identifier of the finding inside the discussed review session, an unsigned 32-bit integer"
                        },
                        "group_id": {
                            "anyOf": [
                                { "type": "string" },
                                { "type": "null" }
                            ],
                            "description": "Identifier of the review group the decision is recorded for; null when the decision addresses the finding alone"
                        },
                        "status": {
                            "type": "string",
                            "enum": review_decision_status_enum(),
                            "description": "Status the finding moves to: 'todo' queues it for execution, 'skipped' records that the finding is not reproducible, and 'rejected' records a false positive or an accepted trade-off"
                        },
                        "note": {
                            "type": "string",
                            "description": "The decision itself, in the engineer's words: the recorded resolution of the finding"
                        },
                        "recommendations": {
                            "type": "array",
                            "description": "Additional remediation recommendations agreed in the discussion; empty when the decision adds none",
                            "items": {
                                "type": "string"
                            }
                        }
                    },
                    "required": ["item_id", "group_id", "status", "note", "recommendations"],
                    "additionalProperties": false
                },
                {
                    "type": "null"
                }
            ],
            "description": "Optional decision recorded for one review finding at the end of the turn"
        })
    })
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

/// Builds the `status` enum of the review update from [`ReviewDecisionStatus`].
///
/// The vocabulary is exactly the decision a discussion may record, so the enum is
/// generated from the typed model instead of being copied here: a decision that
/// gains or loses a variant cannot drift away from the schema the model reads.
fn review_decision_status_enum() -> serde_json::Value {
    const DECISIONS: &[ReviewDecisionStatus] = &[
        ReviewDecisionStatus::Todo,
        ReviewDecisionStatus::Skipped,
        ReviewDecisionStatus::Rejected,
    ];
    serde_json::Value::Array(
        DECISIONS
            .iter()
            .map(|decision| serde_json::Value::String(decision.as_str().to_string()))
            .collect(),
    )
}

/// Generates a strict JSON Schema representing `DiscussionResponse`.
///
/// Guarantees that the `message` string property is listed first in `properties`
/// and `required` so language models emit conversational text before plan
/// updates or context requests. That order survives only because the protocol
/// crate enables `serde_json`'s `preserve_order` feature (see
/// `crates/protocol/Cargo.toml`), which switches JSON objects from a
/// `BTreeMap` to an insertion-ordered `IndexMap`. Dropping that feature would
/// silently re-sort `properties` alphabetically and push `message` out of
/// first position.
///
/// The schema is assembled once and served by a shared reference, so the
/// discussion request path composes its payload by borrowing this value instead
/// of deep-copying the whole tree, `$defs` included.
pub fn discussion_response_schema() -> &'static serde_json::Value {
    DISCUSSION_RESPONSE_SCHEMA
        .get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Conversational response, reasoning, and answers formatted in Markdown. This field is streamed immediately to the user interface."
                    },
                    "plan_update": plan_update_schema().clone(),
                    "review_update": review_update_schema().clone(),
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
                "required": ["message", "plan_update", "review_update", "context_requests", "repo_searches", "repo_dirs", "repo_maps", "dir_lists", "user_language"],
                "additionalProperties": false,
                "$defs": plan_schema_definitions().clone()
            })
        })
}

/// Generates the complete JSON schema wrapper object for OpenAI/OpenRouter
/// structured outputs.
///
/// The wrapper embeds the whole discussion schema, so it is assembled once into
/// its own process-wide cache and served by a shared reference: composing a
/// request from it borrows the cached wrapper instead of rebuilding it and
/// deep-copying the schema tree it carries.
pub fn discussion_response_schema_definition() -> &'static serde_json::Value {
    DISCUSSION_RESPONSE_SCHEMA_DEFINITION.get_or_init(|| {
        serde_json::json!({
            "name": "discussion_response",
            "description": "Structured response for discussion and planning modes",
            "strict": true,
            "schema": discussion_response_schema().clone()
        })
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
///
/// The schema is assembled once and served by a shared reference, so a caller
/// that composes or serializes it pays no rebuild and no deep copy of the tree.
pub fn custom_tool_call_schema() -> &'static serde_json::Value {
    CUSTOM_TOOL_CALL_SCHEMA.get_or_init(|| {
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
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        discussion_response_schema, discussion_response_schema_definition, plan_item_status_wire,
        plan_schema_definitions, plan_update_schema,
    };
    use crate::context::ContextAccess;
    use crate::discussion::{DiscussionPlanAction, DiscussionResponse};
    use crate::plan::PlanItemStatus;
    use crate::review::{IssueStatus, ReviewDecisionStatus, ReviewUpdateProposal};

    #[test]
    fn test_custom_tool_call_schema_pins_the_invocation_contract() {
        let schema = super::custom_tool_call_schema();
        // The contract is assembled once and served by a shared reference, so a
        // repeated call observes the same allocation instead of a rebuilt tree.
        assert!(std::ptr::eq(schema, super::custom_tool_call_schema()));
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
        let review_idx = schema_str
            .find("\"review_update\"")
            .expect("review_update found");
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
        assert!(plan_idx < review_idx);
        assert!(review_idx < ctx_idx);
        assert!(ctx_idx < search_idx);
        assert!(search_idx < dirs_idx);
        assert!(dirs_idx < maps_idx);
        assert!(maps_idx < lists_idx);
        assert!(lists_idx < lang_idx);
    }

    /// Collects the property names a schema object declares.
    fn declared_fields(node: &serde_json::Value) -> BTreeSet<String> {
        node["properties"]
            .as_object()
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Collects the property names a schema object requires.
    fn required_fields(node: &serde_json::Value) -> BTreeSet<String> {
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

    /// Declaring source of the plan model, embedded at compile time.
    ///
    /// The parity suite reads the declarations themselves instead of a
    /// serialized projection of them, so a field hidden from a fixture by
    /// `skip_serializing_if` stays visible to the comparison.
    const PLAN_ITEM_SOURCE: &str = include_str!("../plan/item.rs");

    /// Declaring source of the discussion wire model.
    const DISCUSSION_SOURCE: &str = include_str!("../discussion.rs");

    /// Declaring source of the review wire model.
    const REVIEW_ITEM_SOURCE: &str = include_str!("../review/item.rs");

    /// Every struct whose declared fields the discussion schema mirrors.
    const SCHEMA_MIRRORED_STRUCTS: &[&str] = &[
        "DiscussionResponse",
        "DiscussionPlanUpdate",
        "ReviewUpdateProposal",
        "DiscussionContextRequest",
        "RepoSearchRequest",
        "DirListRequest",
        "ConceptItem",
        "PlanItem",
    ];

    /// A deliberate divergence between a declared field and the schema.
    struct SchemaFieldExemption {
        /// Struct whose declaration carries the field.
        struct_name: &'static str,
        /// Field declared on the struct and withheld from the schema.
        field: &'static str,
        /// Why the divergence is deliberate rather than drift.
        reason: &'static str,
    }

    /// Every divergence the model-facing structured-output contract may carry.
    ///
    /// The schema declares `additionalProperties: false` under `strict: true`,
    /// so a property it omits can never be emitted by constrained decoding. A
    /// field is therefore withheld only when the model must not author it, and
    /// recording that intent here is what tells a maintainer apart from drift.
    /// `test_schema_field_exemption_table_stays_honest` checks every entry
    /// against both the declaration and the schema.
    const SCHEMA_FIELD_EXEMPTIONS: &[SchemaFieldExemption] = &[SchemaFieldExemption {
        struct_name: "PlanItem",
        field: "blocked_reason",
        reason: "harness-authored diagnostic: the bounded turn records why the step could not proceed, so the model never authors the field",
    },
    SchemaFieldExemption {
        struct_name: "DiscussionResponse",
        field: "plan_update_error",
        reason: "decode-boundary diagnostic: the structured stream records why a present plan_update could not be honored, so the model never authors the field",
    },
];

    /// Net count of square brackets a line opens.
    fn bracket_balance(line: &str) -> i32 {
        line.chars().fold(0, |balance, ch| match ch {
            '[' => balance + 1,
            ']' => balance - 1,
            _ => balance,
        })
    }

    /// Extracts the body of a `pub struct <name> { ... }` declaration.
    fn struct_body<'a>(source: &'a str, struct_name: &str) -> &'a str {
        let header = format!("pub struct {struct_name} {{");
        let start = source
            .find(&header)
            .unwrap_or_else(|| panic!("{struct_name} must be declared as `{header}`"))
            + header.len();
        let mut depth = 1usize;
        let mut in_string = false;
        let mut escaped = false;
        for (offset, ch) in source[start..].char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[start..start + offset];
                    }
                }
                _ => {}
            }
        }
        panic!("the declaration of {struct_name} must carry a complete struct body");
    }

    /// Declared `pub` fields of a struct, read from its source declaration.
    ///
    /// Doc comments, plain comments, and serde attribute blocks — including
    /// multi-line ones — are skipped, so the enumeration reports the fields the
    /// struct declares rather than the keys a serialized value happens to
    /// carry.
    fn declared_struct_fields(source: &str, struct_name: &str) -> BTreeSet<String> {
        let body = struct_body(source, struct_name);
        let mut fields = BTreeSet::new();
        let mut open_attributes = 0i32;
        for raw in body.lines() {
            let line = raw.trim();
            if open_attributes > 0 {
                open_attributes += bracket_balance(line);
                continue;
            }
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            if line.starts_with("#[") || line.starts_with("#![") {
                open_attributes += bracket_balance(line);
                continue;
            }
            let declaration = line.strip_prefix("pub ").unwrap_or_else(|| {
                panic!("every field of {struct_name} is declared `pub`, found: {line}")
            });
            let (name, _) = declaration
                .split_once(':')
                .unwrap_or_else(|| panic!("a field of {struct_name} carries no name: {line}"));
            fields.insert(name.trim().to_string());
        }
        fields
    }

    /// Embedded source that declares a struct the schema mirrors.
    fn source_for_struct(struct_name: &str) -> &'static str {
        match struct_name {
            "DiscussionResponse"
            | "DiscussionPlanUpdate"
            | "DiscussionContextRequest"
            | "RepoSearchRequest"
            | "DirListRequest" => DISCUSSION_SOURCE,
            "ConceptItem" | "PlanItem" => PLAN_ITEM_SOURCE,
            "ReviewUpdateProposal" => REVIEW_ITEM_SOURCE,
            other => panic!("no embedded source declares {other}"),
        }
    }

    /// Schema object that mirrors one struct declaration.
    fn schema_object_for<'a>(
        schema: &'a serde_json::Value,
        struct_name: &str,
    ) -> &'a serde_json::Value {
        match struct_name {
            "DiscussionResponse" => schema,
            "DiscussionPlanUpdate" => &schema["properties"]["plan_update"]["anyOf"][0],
            "ReviewUpdateProposal" => &schema["properties"]["review_update"]["anyOf"][0],
            "DiscussionContextRequest" => &schema["properties"]["context_requests"]["items"],
            "RepoSearchRequest" => &schema["properties"]["repo_searches"]["items"],
            "DirListRequest" => &schema["properties"]["dir_lists"]["items"],
            "ConceptItem" => &schema["$defs"]["ConceptItem"],
            "PlanItem" => &schema["$defs"]["PlanItem"],
            other => panic!("no schema object mirrors {other}"),
        }
    }

    /// Declared fields of a struct minus the recorded exemptions.
    fn expected_schema_fields(source: &str, struct_name: &str) -> BTreeSet<String> {
        let declared = declared_struct_fields(source, struct_name);
        let exempted: BTreeSet<&str> = SCHEMA_FIELD_EXEMPTIONS
            .iter()
            .filter(|exemption| exemption.struct_name == struct_name)
            .map(|exemption| exemption.field)
            .collect();
        for field in &exempted {
            assert!(
                declared.contains(*field),
                "the exemption for {struct_name}.{field} no longer names a declared field"
            );
        }
        declared
            .into_iter()
            .filter(|field| !exempted.contains(field.as_str()))
            .collect()
    }

    /// Asserts a schema object declares exactly `expected` and requires all of
    /// it, which is what a strict structured-output contract means.
    fn assert_strict_object(label: &str, node: &serde_json::Value, expected: &BTreeSet<String>) {
        let (properties, required) = (declared_fields(node), required_fields(node));
        assert_eq!(
            &properties, expected,
            "{label} must declare exactly the fields of its Rust struct minus the recorded exemptions"
        );
        assert_eq!(
            required, properties,
            "{label} is strict, so it must require every field it declares"
        );
    }

    /// A plan is authored by two contours, so its payload is generated once.
    ///
    /// The discussion contract embeds the authoritative builders verbatim, and
    /// the code-modification contract re-exports them, which is what makes one
    /// definition of the plan shape serve both.
    #[test]
    fn test_plan_payload_is_generated_once() {
        let schema = discussion_response_schema();
        assert_eq!(&schema["properties"]["plan_update"], plan_update_schema());
        assert_eq!(&schema["$defs"], plan_schema_definitions());

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

    /// The status property states the lifecycle barrier the storage enforces.
    ///
    /// The enum admits every state a step can reach, while a mutation may
    /// introduce a step only as `todo` or `discussion`. The description carries
    /// that narrowing to the model, so a payload never asserts a status the
    /// status barrier would repair.
    #[test]
    fn test_plan_item_status_description_states_the_creation_barrier() {
        let definitions = plan_schema_definitions();
        let description = definitions["PlanItem"]["properties"]["status"]["description"]
            .as_str()
            .expect("the status property describes the lifecycle of a step");
        assert!(
            description.contains("'todo' or 'discussion'"),
            "{description}"
        );
        assert!(description.contains("'done'"), "{description}");
        assert!(description.contains("repaired"), "{description}");
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
    /// Every schema object must declare exactly the fields its Rust struct
    /// declares, minus the divergences recorded in `SCHEMA_FIELD_EXEMPTIONS`,
    /// and must require every field it declares. The declared fields are
    /// enumerated from their sources instead of from a serialized fixture:
    /// `skip_serializing_if` hides an absent `Option` from a fixture, which
    /// would let the schema omit a declared field while the comparison stayed
    /// green.
    #[test]
    fn test_schema_matches_data_model() {
        let schema = discussion_response_schema();

        // 1. Every mirrored struct is compared against its own declaration, so
        //    a field the schema withholds is either recorded as a deliberate
        //    exemption or reported as drift.
        for struct_name in SCHEMA_MIRRORED_STRUCTS {
            let expected = expected_schema_fields(source_for_struct(struct_name), struct_name);
            assert_strict_object(
                struct_name,
                schema_object_for(schema, struct_name),
                &expected,
            );
        }

        // 2. The fields without an object shape carry their own contracts:
        //    repo_dirs is a nullable boolean and repo_maps is a path array.
        let dir_variants = schema["properties"]["repo_dirs"]["anyOf"]
            .as_array()
            .expect("repo_dirs anyOf array");
        assert!(dir_variants.iter().any(|v| v["type"] == "boolean"));
        assert!(dir_variants.iter().any(|v| v["type"] == "null"));
        assert_eq!(schema["properties"]["repo_maps"]["type"], "array");
        assert_eq!(schema["properties"]["repo_maps"]["items"]["type"], "string");

        // 3. Every PlanItemStatus variant must be expressible in the status enum.
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

        // 4. Every DiscussionPlanAction variant must be expressible in the
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

        // 5. Every decision a discussion may record is expressible in the
        //    review_update.status enum, and no status owned by verification or
        //    by execution is.
        let decision_enum: BTreeSet<String> = schema["properties"]["review_update"]["anyOf"][0]
            ["properties"]["status"]["enum"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        for decision in [
            ReviewDecisionStatus::Todo,
            ReviewDecisionStatus::Skipped,
            ReviewDecisionStatus::Rejected,
        ] {
            assert!(
                decision_enum.contains(decision.as_str()),
                "review_update.status enum is missing variant {decision:?}"
            );
        }
        for status in [
            IssueStatus::Discussion,
            IssueStatus::InProgress,
            IssueStatus::Fixed,
        ] {
            assert!(
                !decision_enum.contains(status.as_str()),
                "review_update.status enum must not admit {status:?}"
            );
        }

        // 6. Every ContextAccess variant must be expressible in the
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

    /// A recorded divergence must still be a divergence, and still declared.
    ///
    /// An exemption is only meaningful while the field it names exists in the
    /// declaration and is genuinely absent from the schema. Once the field
    /// disappears or the schema starts publishing it, this test fails and the
    /// entry must be retired instead of silently widening the blind spot it
    /// was meant to record.
    #[test]
    fn test_schema_field_exemption_table_stays_honest() {
        let schema = discussion_response_schema();
        for exemption in SCHEMA_FIELD_EXEMPTIONS {
            assert!(
                !exemption.reason.trim().is_empty(),
                "{}.{} is exempted without a reason, so its omission cannot be told apart from drift",
                exemption.struct_name,
                exemption.field
            );
            let declared = declared_struct_fields(
                source_for_struct(exemption.struct_name),
                exemption.struct_name,
            );
            assert!(
                declared.contains(exemption.field),
                "{}.{} is exempted but no longer declared",
                exemption.struct_name,
                exemption.field
            );
            let properties = declared_fields(schema_object_for(schema, exemption.struct_name));
            assert!(
                !properties.contains(exemption.field),
                "{}.{} is published by the schema again, so its exemption must be retired",
                exemption.struct_name,
                exemption.field
            );
        }
    }

    /// The decision payload is authored by two contours, so it is declared once.
    ///
    /// The discussion contract embeds the authoritative builder verbatim, and
    /// the code-modification schema forwards the same builder, which is what
    /// makes one definition of the decision serve both.
    #[test]
    fn test_review_payload_is_generated_once() {
        let schema = discussion_response_schema();
        assert_eq!(
            &schema["properties"]["review_update"],
            super::review_update_schema()
        );

        let payload = super::review_update_schema();
        let variants = payload["anyOf"]
            .as_array()
            .expect("the review payload is a nullable union");
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

        // A decision addresses one finding, so remediation recommendations stay
        // plain strings instead of nested objects.
        assert_eq!(
            object["properties"]["recommendations"]["items"]["type"],
            "string"
        );

        // The vocabulary is generated from the typed model, so a status owned by
        // verification or by execution can never appear here.
        let decisions: Vec<&str> = object["properties"]["status"]["enum"]
            .as_array()
            .expect("the status enum is an array")
            .iter()
            .filter_map(|status| status.as_str())
            .collect();
        assert_eq!(decisions, vec!["todo", "skipped", "rejected"]);
    }

    /// Builds a minimal response that records a decision for one finding.
    fn decision_payload(item_id: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "message": "",
            "review_update": {
                "item_id": item_id,
                "group_id": null,
                "status": "todo",
                "note": "",
                "recommendations": []
            }
        })
    }

    /// Decodes the decision a response carries through the discussion DTO.
    fn decode_decision(item_id: &serde_json::Value) -> ReviewUpdateProposal {
        let payload = decision_payload(item_id);
        let response: DiscussionResponse =
            serde_json::from_value(payload).expect("the decision decodes");
        response.review_update.expect("the decision travels")
    }

    /// The finding identifier is authored by the model, so the domain the strict
    /// schema publishes must be the domain of the Rust field it mirrors.
    ///
    /// A bare `integer` admits values the typed decoder rejects, and because the
    /// whole response is decoded as one unit, one such value discards the entire
    /// turn output: the message, the plan update, and every discovery request
    /// travel together with the offending field. The bounds are therefore read
    /// from the field they describe instead of being restated here.
    #[test]
    fn test_review_item_id_is_bounded_by_the_domain_of_its_field() {
        let schema = discussion_response_schema();
        let review_update = &schema["properties"]["review_update"]["anyOf"][0];
        let item_id = &review_update["properties"]["item_id"];

        assert_eq!(item_id["type"], "integer");
        let minimum = item_id["minimum"]
            .as_u64()
            .expect("the lower bound is an integer");
        let maximum = item_id["maximum"]
            .as_u64()
            .expect("the upper bound is an integer");
        assert_eq!(minimum, u64::from(ReviewUpdateProposal::ITEM_ID_MINIMUM));
        assert_eq!(maximum, u64::from(ReviewUpdateProposal::ITEM_ID_MAXIMUM));

        // The published constants are the domain of the field itself, so the
        // contract and the decoder accept exactly the same values.
        assert_eq!(ReviewUpdateProposal::ITEM_ID_MINIMUM, u32::MIN);
        assert_eq!(ReviewUpdateProposal::ITEM_ID_MAXIMUM, u32::MAX);

        // Both endpoints are admissible: the contract states them and the typed
        // decoder accepts them.
        for identifier in [
            ReviewUpdateProposal::ITEM_ID_MINIMUM,
            ReviewUpdateProposal::ITEM_ID_MAXIMUM,
        ] {
            let decision = decode_decision(&serde_json::json!(identifier));
            assert_eq!(decision.item_id, identifier);
        }

        // One step outside the domain is refused by the decoder, which is what
        // makes an unbounded property lossy rather than merely imprecise.
        for identifier in [
            serde_json::json!(-1),
            serde_json::json!(u64::from(ReviewUpdateProposal::ITEM_ID_MAXIMUM) + 1),
        ] {
            let payload = decision_payload(&identifier);
            assert!(
                serde_json::from_value::<DiscussionResponse>(payload).is_err(),
                "{identifier} lies outside the domain and must be refused"
            );
        }
    }
}
