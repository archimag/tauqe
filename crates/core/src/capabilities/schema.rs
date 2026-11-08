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
//! a capability that reports several properties — research carries the context
//! request together with the repository outline and listing requests — declares
//! them in one place. The registry folds the fragments in module order, so the
//! composed schema stays a deterministic projection of the enabled surface.

use serde_json::{json, Value};
use tauqe_protocol::{DocTopic, PatchStrategy};

use crate::edits::strategy::StrategyProfile;
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
///
/// The patch strategy decides which fields address the lines a change replaces:
/// a search-replace change echoes the replaced text, while an anchored change
/// names the replaced range through the opaque anchors rendered beside every
/// context line. The replaced-line vocabulary lives in the [`StrategyProfile`]
/// of the strategy, so the wire schema and the prompt instructions of one
/// strategy can never disagree about the fields a change carries.
pub(crate) fn changes_fragment(patch_strategy: PatchStrategy) -> Value {
    json!({
        "changes": {
            "type": "array",
            "description": "List of structured code modification operations",
            "items": change_entry(patch_strategy)
        }
    })
}

/// Schema of one change entry for the active patch strategy.
fn change_entry(patch_strategy: PatchStrategy) -> Value {
    let profile = StrategyProfile::of(patch_strategy);

    // The target and the move destination address every strategy identically,
    // so they are declared once, ahead of the fields the profile owns. The
    // operation selector follows the strategy, because the set of operations a
    // strategy offers is strategy vocabulary rather than a shared shape.
    let mut properties = serde_json::Map::new();
    properties.insert("op".to_string(), op_property(patch_strategy));
    properties.insert("path".to_string(), path_property());
    properties.insert("to".to_string(), destination_property());
    let mut required: Vec<Value> = vec![json!("op"), json!("path"), json!("to")];

    // The replaced lines are addressed by the vocabulary of the strategy: the
    // echoed text of a search-replace change or the opaque anchors of an
    // anchored one. The property and its required entry come from the same
    // profile field, so neither can survive without the other.
    for field in profile.replaced_line_fields {
        properties.insert(field.name.to_string(), string_property(field.description));
        required.push(Value::String(field.name.to_string()));
    }

    properties.insert(
        "content".to_string(),
        string_property(profile.content_description),
    );
    required.push(json!("content"));

    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false
    })
}

/// Schema of one string property carrying the given description.
fn string_property(description: &str) -> Value {
    json!({
        "type": "string",
        "description": description
    })
}

/// Schema of the declared operation selector for a patch strategy.
///
/// The line-anchor strategy addresses the replaced lines by a range, so a full
/// rewrite is a range spanning the whole file and the separate `overwrite`
/// operation has no meaning there. Advertising it would offer the model two
/// spellings of one change and a way around the minimal-diff guarantee the
/// range carries, so the strict enum leaves it out. The search/replace
/// strategy keeps the operation, because echoing the whole file as `old_text`
/// is how a full rewrite is expressed there. The selector therefore follows
/// the same patch strategy that already decides the replaced-line fields, and
/// the schema never advertises an operation the strategy does not offer.
///
/// A directory move names two directories and no lines, so it addresses no
/// replaced-lines vocabulary at all and both strategies offer it: the strict
/// enum declares it beside the operations the strategy itself defines.
fn op_property(patch_strategy: PatchStrategy) -> Value {
    let (operations, description): (&[&str], &str) = match patch_strategy {
        PatchStrategy::SearchReplace => (
            &["replace", "create", "delete", "move", "move_dir", "overwrite"],
            "Operation type: 'replace' for modifying existing files, 'create' for new files, 'delete' for deleting files, 'move' for moving or renaming files, 'move_dir' for moving or renaming a directory subtree, 'overwrite' for replacing the entire content of an existing file",
        ),
        PatchStrategy::LineAnchor => (
            &["replace", "create", "delete", "move", "move_dir"],
            "Operation type: 'replace' for modifying existing files, 'create' for new files, 'delete' for deleting files, 'move' for moving or renaming files, 'move_dir' for moving or renaming a directory subtree",
        ),
    };
    json!({
        "type": "string",
        "enum": operations,
        "description": description
    })
}

/// Schema of the target path shared by both change shapes.
fn path_property() -> Value {
    json!({
        "type": "string",
        "description": "Relative repository path to the target file (the source file for 'move', the source directory for 'move_dir')"
    })
}

/// Schema of the move destination shared by both change shapes.
fn destination_property() -> Value {
    json!({
        "type": "string",
        "description": "Target repository path for 'move' and 'move_dir' operations (empty string otherwise)"
    })
}

/// Schema fragment of the research properties a single capability owns.
pub(crate) fn research_fragment() -> Value {
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

/// Schema fragment of the canonical documentation request property.
///
/// The strict schema advertises one spelling per command, so the harness
/// publishes `tauqe_doc_requests` alone. The pre-canonical `doc_requests`
/// spelling stays a permanent alias in the decoder, which keeps a cached
/// response and a stored transcript decodable without widening the wire
/// contract the model is constrained by.
pub(crate) fn doc_requests_fragment() -> Value {
    let topics: Vec<&str> = DocTopic::ALL.iter().map(DocTopic::as_str).collect();
    json!({
        "tauqe_doc_requests": {
            "type": "array",
            "description": "System documentation topics to request before answering questions about identity, capabilities, concepts, or workflows",
            "items": {
                "type": "string",
                "enum": topics
            }
        }
    })
}

/// Schema fragment of the external project documentation request property.
///
/// A request names one entry the project declares in `[[outer_docs]]`, never a
/// cache path: where the materialized copy lives is decided by the harness, so
/// only material the project itself declared is reachable. An empty array is the
/// idle representation of the property, exactly as it is for every other
/// request array.
pub(crate) fn outer_doc_requests_fragment() -> Value {
    json!({
        "outer_doc_requests": {
            "type": "array",
            "description": "External project documents to materialize before answering, each named by the identifier of an entry declared in the project `[[outer_docs]]` configuration; a materialized document enters the context as read-only reference",
            "items": {
                "type": "string"
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
///
/// A terminal status is a claim that something happened, so the fragment states
/// the payload contract in its descriptions: `success` carries a non-empty
/// `commit_message`, `error` carries its `message`, and `skip` carries its
/// `reason`.
/// JSON Schema cannot express non-emptiness — `required` already demands every
/// field and an empty string stays valid — so the descriptions formulate the
/// contract while the harness enforces it after decoding.
pub(crate) fn outcome_fragment() -> Value {
    json!({
        "outcome": {
            "type": "object",
            "description": "Terminal outcome of the turn; status 'none' while the turn continues, while a terminal status carries its own payload",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["none", "success", "error", "skip"],
                    "description": "'none' while the turn continues, 'success' when it succeeds, 'error' when it fails, 'skip' when the work is already satisfied and no change is required"
                },
                "commit_message": {
                    "type": "string",
                    "description": "Git commit message of the change set declared by status 'success': an English Conventional Commits header (`type(scope): description`) in imperative mood, lowercase, without a trailing period, at most 72 characters. It becomes the literal commit message of the turn, so the conversational language never applies to it and it stays English whatever the dialogue language is. A success without it is a protocol defect rather than a result (empty string otherwise)"
                },
                "message": {
                    "type": "string",
                    "description": "Error message carried by status 'error' (empty string otherwise)"
                },
                "reason": {
                    "type": "string",
                    "description": "Skip reason carried by status 'skip' (empty string otherwise)"
                }
            },
            "required": ["status", "commit_message", "message", "reason"],
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
/// the idle representation of the property the strict schema requires.
/// The builder serves its cached schema by reference, so the fragment takes the
/// single owned copy the composed edit schema needs. That copy is unavoidable:
/// the registry composes a fresh schema on every composition, and the fragment
/// becomes part of it.
pub(crate) fn plan_update_fragment() -> Value {
    json!({
        "plan_update": tauqe_protocol::discussion::schema::plan_update_schema().clone()
    })
}

/// Shared definitions referenced by the structured plan payload.
///
/// A plan is a tree: concept nodes and implementation steps nest their own
/// kind, but strict Structured Output mode rejects a definition that
/// references itself. The authoritative builder of `crates/protocol` therefore
/// unrolls the tree into one acyclic definition per depth level, and a
/// consumer lifts those definitions to the root of its composed schema, which
/// is where their `#/$defs/<Name>` references resolve. The builder serves its
/// cached definitions by reference, so lifting them into the composed schema
/// takes the one owned copy composition needs.
pub(crate) fn plan_update_definitions() -> Value {
    tauqe_protocol::discussion::schema::plan_schema_definitions().clone()
}

/// Schema fragment of the structured review decision property.
///
/// The decision shape is generated by `crates/protocol`, the single source of
/// truth of the review contract, and the fragment forwards that builder verbatim
/// instead of restating it. The engine therefore holds no decision shape of its
/// own: one change to the review contract reaches the discussion contract and
/// the code-modification schema at once.
///
/// The payload admits `null` because a turn that records no decision carries
/// none at all, which is the idle representation of the property the strict
/// schema requires. The builder serves its cached schema by reference, so the
/// fragment takes the single owned copy the composed edit schema needs.
pub(crate) fn review_update_fragment() -> Value {
    json!({
        "review_update": tauqe_protocol::discussion::schema::review_update_schema().clone()
    })
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
            changes_fragment(PatchStrategy::SearchReplace),
            research_fragment(),
            doc_requests_fragment(),
            outer_doc_requests_fragment(),
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
    fn the_change_set_fragment_follows_the_patch_strategy() {
        let search_replace = changes_fragment(PatchStrategy::SearchReplace);
        let search_fields = search_replace["changes"]["items"]["properties"]
            .as_object()
            .expect("the change entry declares its fields");
        assert!(search_fields.contains_key("old_text"));
        assert!(search_fields.contains_key("new_text"));
        assert!(!search_fields.contains_key("start"));

        // An anchored change names the replaced range instead of echoing it.
        let anchored = changes_fragment(PatchStrategy::LineAnchor);
        let anchor_fields = anchored["changes"]["items"]["properties"]
            .as_object()
            .expect("the change entry declares its fields");
        assert!(anchor_fields.contains_key("start"));
        assert!(anchor_fields.contains_key("end"));
        assert!(anchor_fields.contains_key("content"));
        assert!(!anchor_fields.contains_key("old_text"));
        assert!(!anchor_fields.contains_key("new_text"));

        // Both shapes describe the very same operation vocabulary and stay
        // strict: a declared field is always required and no unknown field is
        // accepted.
        for fragment in [search_replace, anchored] {
            let entry = &fragment["changes"]["items"];
            assert_eq!(entry["additionalProperties"], serde_json::json!(false));
            assert_eq!(
                entry["required"].as_array().map(|required| required.len()),
                entry["properties"]
                    .as_object()
                    .map(|properties| properties.len())
            );
            assert!(entry["properties"].get("op").is_some());
            assert!(entry["properties"].get("path").is_some());
            assert!(entry["properties"].get("to").is_some());
        }
    }

    #[test]
    fn the_documentation_fragment_publishes_every_topic() {
        let fragment = doc_requests_fragment();
        let topics = fragment["tauqe_doc_requests"]["items"]["enum"]
            .as_array()
            .expect("the topic enum is an array");
        assert_eq!(topics.len(), DocTopic::ALL.len());
        assert!(topics.iter().any(|topic| topic == "config"));
    }

    #[test]
    fn the_plan_fragment_forwards_the_authoritative_protocol_schema() {
        use tauqe_protocol::discussion::schema::PLAN_TREE_DEPTH;

        let fragment = plan_update_fragment();
        let plan_update = &fragment["plan_update"];

        // The engine holds no plan shape of its own: the fragment is the very
        // builder of `crates/protocol`, so the code-modification contour and the
        // discussion contour describe one identical payload.
        assert_eq!(
            plan_update,
            tauqe_protocol::discussion::schema::plan_update_schema()
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

        // The plan tree cannot reference itself, so the nested shapes travel as
        // unrolled shared definitions referenced by pointer, forwarded verbatim
        // instead of being sliced out of a sibling schema.
        let definitions = plan_update_definitions();
        assert_eq!(
            &definitions,
            tauqe_protocol::discussion::schema::plan_schema_definitions()
        );
        let map = definitions
            .as_object()
            .expect("the plan definitions are an object");
        assert_eq!(map.len(), 2 * PLAN_TREE_DEPTH);
        assert_eq!(
            object["properties"]["concept"]["items"]["$ref"],
            "#/$defs/ConceptItemL1"
        );
        assert_eq!(
            object["properties"]["items"]["items"]["$ref"],
            "#/$defs/PlanItemL1"
        );

        // Each unrolled level requires exactly the fields it declares, chains
        // `children` to the next level of its own kind, and seals the array at
        // the deepest level, so the tree is described by an acyclic chain
        // instead of a self-referencing definition.
        for kind in ["ConceptItem", "PlanItem"] {
            for level in 1..=PLAN_TREE_DEPTH {
                let name = format!("{kind}L{level}");
                let node = map
                    .get(name.as_str())
                    .unwrap_or_else(|| panic!("the unrolled tree declares {name}"));
                assert_eq!(node["additionalProperties"], serde_json::json!(false));
                assert_eq!(node["properties"]["children"]["type"], "array", "{name}");
                if level < PLAN_TREE_DEPTH {
                    assert_eq!(
                        node["properties"]["children"]["items"]["$ref"],
                        format!("#/$defs/{kind}L{}", level + 1),
                        "{name} chains to the next level of its own kind"
                    );
                } else {
                    // The deepest level seals the tree, so the chain ends where
                    // the schema says it ends.
                    assert_eq!(
                        node["properties"]["children"]["maxItems"],
                        serde_json::json!(0)
                    );
                }

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
    #[test]
    fn the_outcome_fragment_states_the_payload_contract() {
        let fragment = outcome_fragment();
        let outcome = fragment["outcome"]
            .as_object()
            .expect("the outcome is an object");
        let properties = outcome["properties"]
            .as_object()
            .expect("the outcome declares its fields");

        // A terminal status is a claim that something happened, so the schema
        // states which payload each status carries, and it names the consumer of
        // the success payload: the Git commit of the change set.
        let commit_message = properties["commit_message"]["description"]
            .as_str()
            .expect("the commit message declares its purpose");
        assert!(
            commit_message.contains("Conventional Commits"),
            "{commit_message}"
        );
        assert!(commit_message.contains("'success'"), "{commit_message}");
        assert!(
            commit_message.contains("protocol defect"),
            "{commit_message}"
        );
        assert!(commit_message.contains("72"), "{commit_message}");
        assert!(!properties.contains_key("summary"));
        assert!(properties["message"]["description"]
            .as_str()
            .expect("the message declares its purpose")
            .contains("'error'"));
        assert!(properties["reason"]["description"]
            .as_str()
            .expect("the reason declares its purpose")
            .contains("'skip'"));
        assert!(outcome["description"]
            .as_str()
            .expect("the outcome declares its purpose")
            .contains("carries its own payload"));

        // Non-emptiness stays a description rather than a shape: the strict
        // object still requires exactly the fields it declares.
        assert_eq!(
            outcome["required"].as_array().map(Vec::len),
            Some(properties.len())
        );
    }

    /// Reads the operation selector the change-set schema declares.
    ///
    /// The selector is the surface that decides which operations the active
    /// patch strategy offers, so a test reads it instead of restating the enum
    /// of the strategy it checks.
    fn operation_selector(patch_strategy: PatchStrategy) -> Value {
        let fragment = changes_fragment(patch_strategy);
        fragment["changes"]["items"]["properties"]["op"].clone()
    }

    #[test]
    fn the_operation_selector_follows_the_patch_strategy() {
        // A full rewrite under the line-anchor strategy is a range spanning the
        // whole file, so the separate operation is absent from the strict enum
        // and from the description that explains it.
        let anchored = operation_selector(PatchStrategy::LineAnchor);
        let anchored_operations = json!(["replace", "create", "delete", "move", "move_dir"]);
        assert_eq!(anchored["enum"], anchored_operations);
        let description = anchored["description"]
            .as_str()
            .expect("the operation selector states its values");
        assert!(!description.contains("overwrite"), "{description}");
        // A directory move addresses no replaced lines, so it addresses no
        // strategy vocabulary either and both strategies offer it.
        assert!(description.contains("'move_dir'"), "{description}");

        // The search/replace strategy keeps the operation, because echoing the
        // whole file as `old_text` is how a full rewrite is expressed there.
        let echoed = operation_selector(PatchStrategy::SearchReplace);
        let echoed_operations = json!([
            "replace",
            "create",
            "delete",
            "move",
            "move_dir",
            "overwrite"
        ]);
        assert_eq!(echoed["enum"], echoed_operations);
        let description = echoed["description"]
            .as_str()
            .expect("the operation selector states its values");
        assert!(description.contains("'overwrite'"), "{description}");
        assert!(description.contains("'move_dir'"), "{description}");
    }
    /// Asserts that the fragment forwards the authoritative protocol builder.
    #[test]
    fn the_review_fragment_forwards_the_authoritative_protocol_schema() {
        use tauqe_protocol::review::ReviewDecisionStatus;

        let fragment = review_update_fragment();
        let review_update = &fragment["review_update"];

        // The engine holds no decision shape of its own: the fragment is the
        // very builder of `crates/protocol`, so the discussion contract and the
        // code-modification contour describe one identical payload.
        assert_eq!(
            review_update,
            tauqe_protocol::discussion::schema::review_update_schema()
        );

        let variants = review_update["anyOf"]
            .as_array()
            .expect("the review payload is a nullable union");
        assert_eq!(variants.len(), 2);
        // `null` is the idle representation of the property, exactly as in the
        // discussion contract the builder feeds.
        assert_eq!(variants[1]["type"], "null");

        let object = &variants[0];
        let mut properties: Vec<&str> = object["properties"]
            .as_object()
            .expect("the review payload declares its fields")
            .keys()
            .map(String::as_str)
            .collect();
        properties.sort();
        assert_eq!(
            properties,
            ["group_id", "item_id", "note", "recommendations", "status"]
        );

        // The vocabulary comes from the typed model, so every decision a
        // discussion may record is expressible and no status owned by execution
        // is.
        let decisions: Vec<&str> = object["properties"]["status"]["enum"]
            .as_array()
            .expect("the status enum is an array")
            .iter()
            .filter_map(|status| status.as_str())
            .collect();
        for decision in [
            ReviewDecisionStatus::Todo,
            ReviewDecisionStatus::Skipped,
            ReviewDecisionStatus::Rejected,
        ] {
            assert!(
                decisions.contains(&decision.as_str()),
                "the status enum is missing {decision:?}"
            );
        }
        assert_eq!(decisions.len(), 3);
    }
}
