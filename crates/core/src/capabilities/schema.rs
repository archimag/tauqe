//! JSON-schema fragments of the structured-output response.
//!
//! Every capability module declares the properties it populates in the
//! structured wire schema by returning the fragment defined here, so the schema
//! of a turn is a projection of the very capability set that also renders the
//! prompt instructions and scopes the decoder. A property therefore cannot
//! survive in the schema after its capability is disabled, nor appear without a
//! module that describes and decodes it.
//!
//! A fragment is an object keyed by the property names the capability owns, so
//! a capability that reports several properties — discovery carries the context
//! request together with the repository outline and listing requests — declares
//! them in one place. The registry folds the fragments in module order, so the
//! composed schema stays a deterministic projection of the enabled surface.

use serde_json::{json, Value};
use tauqe_protocol::DocTopic;

/// Schema fragment of the conversational response property.
pub(crate) fn message_fragment() -> Value {
    json!({
        "message": {
            "type": "string",
            "description": "Conversational explanation or response to the user"
        }
    })
}

/// Schema fragment of the structured change-set property.
pub(crate) fn changes_fragment() -> Value {
    json!({
        "changes": {
            "type": "array",
            "description": "List of structured code modification operations",
            "items": {
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["replace", "create", "delete", "move", "overwrite"],
                        "description": "Operation type: 'replace' for modifying existing files, 'create' for new files, 'delete' for deleting files, 'move' for moving or renaming files, 'overwrite' for replacing the entire content of an existing file"
                    },
                    "path": {
                        "type": "string",
                        "description": "Relative repository path to the target file (source file for 'move')"
                    },
                    "to": {
                        "type": "string",
                        "description": "Target repository path for 'move' operation (empty string otherwise)"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact text to replace in the file (required for 'replace', empty string otherwise)"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text (for 'replace', empty string otherwise)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Complete file content (required for 'create' and 'overwrite', empty string otherwise)"
                    }
                },
                "required": ["op", "path", "to", "old_text", "new_text", "content"],
                "additionalProperties": false
            }
        }
    })
}

/// Schema fragment of the discovery properties a single capability owns.
pub(crate) fn discovery_fragment() -> Value {
    json!({
        "context_requests": {
            "type": "array",
            "description": "Files to add to context before editing. Each entry is a repository-relative path (read-only) or a path prefixed with 'editable:' (e.g. 'editable:src/lib.rs')",
            "items": {
                "type": "string"
            }
        },
        "repo_searches": {
            "type": "array",
            "description": "Repository search requests to find symbols or text patterns across files using Rust / ripgrep regex",
            "items": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Repository-relative directory or file path to search within (use '' or '.' for repository root)"
                    },
                    "regex": {
                        "type": "string",
                        "description": "Rust / ripgrep regular expression to search for"
                    }
                },
                "required": ["path", "regex"],
                "additionalProperties": false
            }
        },
        "repo_dirs": {
            "type": "boolean",
            "description": "Whether the compact repository directory tree is requested; set true only when the repository layout is still unknown"
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
            "description": "Directory content listing requests over the tracked repository files; each entry names a repository-relative directory prefix, and an empty string or '.' requests the repository root. The listing is shallow unless 'recursive' is true",
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
        }
    })
}

/// Schema fragment of the documentation request property.
pub(crate) fn doc_requests_fragment() -> Value {
    let topics: Vec<&str> = DocTopic::ALL.iter().map(DocTopic::as_str).collect();
    json!({
        "doc_requests": {
            "type": "array",
            "description": "System documentation topics to request before answering questions about identity, capabilities, concepts, or workflows",
            "items": {
                "type": "string",
                "enum": topics
            }
        }
    })
}

/// Schema fragment of the explicit toolchain verification property.
pub(crate) fn verify_fragment() -> Value {
    json!({
        "verify": {
            "type": "object",
            "description": "Deterministic toolchain verification request",
            "properties": {
                "target": {
                    "type": "string",
                    "enum": ["none", "all", "check", "test", "clippy"],
                    "description": "'none' when no verification is requested, otherwise the verification scope"
                },
                "on_success": {
                    "type": "string",
                    "enum": ["silent", "report"],
                    "description": "'silent' reports only a concise confirmation, 'report' surfaces the full command output"
                }
            },
            "required": ["target", "on_success"],
            "additionalProperties": false
        }
    })
}

/// Schema fragment of the turn-scoped working-memory property.
pub(crate) fn scratchpad_fragment() -> Value {
    json!({
        "scratchpad": {
            "type": "string",
            "description": "Turn-local working memory notes holding hypotheses, verified findings, and the purpose of context still needed; empty string when there is nothing to record"
        }
    })
}

/// Schema fragment of the terminal outcome property.
pub(crate) fn outcome_fragment() -> Value {
    json!({
        "outcome": {
            "type": "object",
            "description": "Terminal outcome of the turn; status 'none' while the turn continues",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["none", "success", "error", "skip"],
                    "description": "'none' while the turn continues, 'success' when it succeeds, 'error' when it fails, 'skip' when the work is already satisfied and no change is required"
                },
                "summary": {
                    "type": "string",
                    "description": "Success summary (empty string otherwise)"
                },
                "message": {
                    "type": "string",
                    "description": "Error message (empty string otherwise)"
                },
                "reason": {
                    "type": "string",
                    "description": "Skip reason (empty string otherwise)"
                }
            },
            "required": ["status", "summary", "message", "reason"],
            "additionalProperties": false
        }
    })
}

/// Schema fragment of the semantic blocker property.
pub(crate) fn blocked_fragment() -> Value {
    json!({
        "blocked": {
            "type": "object",
            "description": "Semantic blocker reported when a precondition or assumption fails",
            "properties": {
                "is_blocked": {
                    "type": "boolean",
                    "description": "True only when a precondition or assumption fails"
                },
                "reason": {
                    "type": "string",
                    "description": "Violated precondition or assumption (empty string when not blocked)"
                },
                "details": {
                    "type": "string",
                    "description": "Optional expansion of the blocker (empty string otherwise)"
                }
            },
            "required": ["is_blocked", "reason", "details"],
            "additionalProperties": false
        }
    })
}

/// Schema fragment of the detected user language property.
pub(crate) fn user_language_fragment() -> Value {
    json!({
        "user_language": {
            "type": "string",
            "description": "Detected primary natural language of the user prompt; empty string when unknown"
        }
    })
}

/// Schema fragment of the structured plan payload property.
///
/// The payload is generated by `crates/protocol`, the single source of truth of
/// the plan contract, and the fragment forwards that builder verbatim instead of
/// slicing it out of the composed discussion schema. The engine therefore holds
/// no plan shape of its own: one change to the plan contract reaches both the
/// code-modification and the discussion contours at once, and no silent fallback
/// can degrade the property into an empty object. The payload admits `null`
/// because a turn that leaves the plan untouched carries none at all, which is
/// the idle representation of a property the strict schema still requires.
pub(crate) fn plan_update_fragment() -> Value {
    json!({
        "plan_update": tauqe_protocol::discussion::schema::plan_update_schema()
    })
}

/// Shared definitions referenced by the structured plan payload.
///
/// A plan is a tree: concept nodes and implementation steps nest their own
/// kind, so their schemas cannot be inlined into the fragment without the
/// fragment containing itself. The definitions come from the authoritative
/// builder of `crates/protocol` and are lifted to the root of the composed
/// schema by the registry, which is where `#/$defs/<Name>` resolves.
pub(crate) fn plan_update_definitions() -> Value {
    tauqe_protocol::discussion::schema::plan_schema_definitions()
}

/// Schema fragment of the structured abort property.
///
/// The shape follows the blocker field: a flag states whether the abort is
/// active, so the idle representation of a required strict property stays an
/// explicit `false` instead of an absent field.
pub(crate) fn abort_fragment() -> Value {
    json!({
        "abort": {
            "type": "object",
            "description": "Unconditional abort reported when the remaining work needs an action only a human can perform on the host system",
            "properties": {
                "is_aborted": {
                    "type": "boolean",
                    "description": "True only when the work cannot continue without a developer action in the host terminal"
                },
                "reason": {
                    "type": "string",
                    "description": "The concrete missing capability (empty string when no abort is reported)"
                },
                "recommendation": {
                    "type": "string",
                    "description": "The exact command or instruction the developer must run (empty string when no abort is reported)"
                }
            },
            "required": ["is_aborted", "reason", "recommendation"],
            "additionalProperties": false
        }
    })
}

/// Schema fragment of the typed project command invocation property.
///
/// An invocation addresses one command the project declares in
/// `[project.commands]`, so the array can never widen into an arbitrary shell
/// command line. An empty array is the idle representation of the property.
pub(crate) fn custom_tools_fragment() -> Value {
    json!({
        "custom_tools": {
            "type": "array",
            "description": "Typed invocations of the project commands declared in `[project.commands]`; each entry names one declared command, and an empty array requests no execution",
            "items": {
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
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_are_keyed_objects_without_nullable_types() {
        for fragment in [
            message_fragment(),
            changes_fragment(),
            discovery_fragment(),
            doc_requests_fragment(),
            verify_fragment(),
            scratchpad_fragment(),
            outcome_fragment(),
            blocked_fragment(),
            abort_fragment(),
            custom_tools_fragment(),
            user_language_fragment(),
        ] {
            let fields = fragment.as_object().expect("a fragment is an object");
            assert!(
                !fields.is_empty(),
                "a fragment declares at least one property"
            );
            // The schema stays strict: no property admits an explicit null.
            assert!(!fragment.to_string().contains("\"null\""));
        }
    }

    #[test]
    fn the_documentation_fragment_publishes_every_topic() {
        let fragment = doc_requests_fragment();
        let topics = fragment["doc_requests"]["items"]["enum"]
            .as_array()
            .expect("the topic enum is an array");
        assert_eq!(topics.len(), DocTopic::ALL.len());
        assert!(topics.iter().any(|topic| topic == "config"));
    }

    #[test]
    fn the_plan_fragment_forwards_the_authoritative_protocol_schema() {
        let fragment = plan_update_fragment();
        let plan_update = &fragment["plan_update"];

        // The engine holds no plan shape of its own: the fragment is the very
        // builder of `crates/protocol`, so the code-modification contour and the
        // discussion contour describe one identical payload.
        assert_eq!(
            plan_update,
            &tauqe_protocol::discussion::schema::plan_update_schema()
        );

        let variants = plan_update["anyOf"]
            .as_array()
            .expect("the plan payload is a nullable union");
        assert_eq!(variants.len(), 2);
        // `null` is the idle representation of the property, exactly as in the
        // discussion contract the builder feeds.
        assert_eq!(variants[1]["type"], "null");

        let object = &variants[0];
        let mut properties: Vec<&str> = object["properties"]
            .as_object()
            .expect("the plan payload declares its fields")
            .keys()
            .map(String::as_str)
            .collect();
        properties.sort();
        assert_eq!(
            properties,
            ["action", "concept", "description", "id", "items", "title"]
        );

        // A plan tree cannot inline itself, so the nested shapes travel as
        // shared definitions referenced by pointer, forwarded verbatim instead
        // of being sliced out of a sibling schema.
        let definitions = plan_update_definitions();
        assert_eq!(
            definitions,
            tauqe_protocol::discussion::schema::plan_schema_definitions()
        );
        let names = definitions
            .as_object()
            .expect("the plan definitions are an object");
        assert_eq!(names.len(), 2);
        assert!(names.contains_key("ConceptItem"));
        assert!(names.contains_key("PlanItem"));
        assert_eq!(
            object["properties"]["concept"]["items"]["$ref"],
            "#/$defs/ConceptItem"
        );
        assert_eq!(
            object["properties"]["items"]["items"]["$ref"],
            "#/$defs/PlanItem"
        );

        // Each definition requires exactly the fields it declares and nests its
        // own kind through `children`, so a tree of any depth is described by
        // the contract of its root instead of a flattened sibling shape.
        for name in ["ConceptItem", "PlanItem"] {
            let node = &definitions[name];
            assert_eq!(node["additionalProperties"], serde_json::json!(false));
            assert_eq!(node["properties"]["children"]["type"], "array", "{name}");
            assert_eq!(
                node["properties"]["children"]["items"]["$ref"],
                serde_json::json!(format!("#/$defs/{name}"))
            );

            let declared: std::collections::BTreeSet<&str> = node["properties"]
                .as_object()
                .expect("the definition declares its fields")
                .keys()
                .map(String::as_str)
                .collect();
            let required: std::collections::BTreeSet<&str> = node["required"]
                .as_array()
                .expect("the definition requires its fields")
                .iter()
                .filter_map(|field| field.as_str())
                .collect();
            assert_eq!(declared, required, "{name} requires what it declares");
        }
    }

    #[test]
    fn the_abort_and_custom_tool_fragments_declare_their_idle_representation() {
        let abort = abort_fragment();
        let fields = abort["abort"]["properties"]
            .as_object()
            .expect("the abort object declares its fields");
        assert!(fields.contains_key("is_aborted"));
        assert!(fields.contains_key("reason"));
        assert!(fields.contains_key("recommendation"));
        assert_eq!(
            abort["abort"]["properties"]["is_aborted"]["type"],
            "boolean"
        );
        assert_eq!(
            abort["abort"]["properties"]["recommendation"]["type"],
            "string"
        );

        let tools = custom_tools_fragment();
        assert_eq!(tools["custom_tools"]["type"], "array");
        let item = &tools["custom_tools"]["items"];
        assert_eq!(item["properties"]["name"]["type"], "string");
        assert_eq!(item["properties"]["args"]["type"], "object");
        assert_eq!(item["additionalProperties"], serde_json::json!(false));
    }
}
