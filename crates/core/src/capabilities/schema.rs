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
//!
//! The composed schema is self-contained: a shared definition a plan payload
//! references is inlined at its use site and no `$defs` section survives,
//! because strict Structured Output mode is not guaranteed to resolve a JSON
//! Pointer of a shared definition, least of all inside a discriminated union.

#[cfg(test)]
use crate::config::project::ProjectConfig;
use crate::edits::StructuredChangeProposal;
use crate::llm::CapabilityTree;
#[cfg(test)]
use crate::llm::DocTopic;
use crate::llm::LlmCapability;
use crate::llm::{
    AbortCommand, BlockedCommand, CommitMessageCommand, ContextRequestCommand, DirListCommand,
    DocRequestCommand, OuterDocRequestCommand, RepoDirsCommand, RepoMapCommand, RepoSearchCommand,
    ScratchpadCommand, TextCommand, TurnErrorCommand, TurnSkipCommand,
};
use crate::workflow::PatchStrategy;
use serde_json::{json, Map, Value};

use crate::capabilities::PromptRenderContext;
use crate::edits::strategy::StrategyProfile;

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
        PatchStrategy::SearchReplace | PatchStrategy::ApplyPatch => (
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
/// Schema of the discriminator property that names one operational branch.
///
/// The property admits exactly one constant, so a response cannot claim two
/// branches at once, and its description restates the exclusion because the
/// discriminator is the only place the model reads what the branch means.
fn branch_kind_property(label: &str) -> Value {
    let description = if label == "answer" {
        "Operational branch this response follows: 'answer'. Select this when your response in 'message' is complete and final, requiring no further research rounds or code edits.".to_string()
    } else {
        format!(
            "Operational branch this response follows: '{label}'. A response follows exactly one branch, so the commands of another branch are never stated in the same round."
        )
    };
    json!({
        "type": "string",
        "enum": [label],
        "description": description
    })
}

/// Schema of one variant of a discriminated union of operational branches.
///
/// The variant is a strict object: its `kind` discriminator selects the branch and
/// the branch's own fields follow it, so constrained decoding can emit this branch's
/// commands and physically omit the properties of every other branch. `required`
/// mirrors the emitted properties, because a strict schema must require exactly what
/// it declares.
fn branch_variant(label: &str, fields: Map<String, Value>) -> Value {
    let mut properties = Map::new();
    properties.insert("kind".to_string(), branch_kind_property(label));
    for (name, schema) in fields {
        properties.entry(name).or_insert(schema);
    }
    let required: Vec<Value> = properties
        .keys()
        .map(|name| Value::String(name.clone()))
        .collect();
    let description = if label == "answer" {
        "Variant of the 'answer' branch: terminal marker indicating that your conversational text in 'message' is the complete final answer to the user and no further research or code edits are required".to_string()
    } else {
        format!(
            "Variant of the '{label}' branch: it carries the branch discriminator together with the commands of that branch, and nothing of any other branch"
        )
    };
    json!({
        "type": "object",
        "description": description,
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false
    })
}

/// Schema of the property that carries a discriminated union of branches.
///
/// The union is what makes the exclusion grammatical: a strict schema cannot forbid
/// a field, but it can refuse to declare two shapes at once, so the fields of the
/// branch a response did not select are not part of the object it produces.
fn action_union(label: String, variants: Vec<Value>) -> (String, Value) {
    (
        label,
        json!({
            "type": "object",
            "description": "Single operational branch of this response: the branches are mutually exclusive, so a response states exactly one of them and never combines their commands.",
            "anyOf": variants
        }),
    )
}

/// Converts a `schemars`-derived schema for `T` into a plain JSON object,
/// stripping top-level `$schema` and `title` metadata.
fn schemars_object<T: schemars::JsonSchema>() -> Map<String, Value> {
    let root = schemars::schema_for!(T);
    let mut value = serde_json::to_value(&root.schema).unwrap_or_else(|_| json!({}));
    if let Some(map) = value.as_object_mut() {
        map.remove("$schema");
        map.remove("title");
        return map.clone();
    }
    Map::new()
}

/// Ensures every property in `object["properties"]` has a non-empty `description`
/// and that `required` lists every declared property.
fn finalize_strict_object(
    mut object: Map<String, Value>,
    description: &str,
    field_descriptions: &[(&str, &str)],
) -> Value {
    object.insert("type".to_string(), Value::String("object".to_string()));
    object.insert(
        "description".to_string(),
        Value::String(description.to_string()),
    );
    object.insert("additionalProperties".to_string(), Value::Bool(false));

    let mut required = Vec::new();
    if let Some(props) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for (name, prop) in props.iter_mut() {
            required.push(Value::String(name.clone()));
            if let Some(prop_map) = prop.as_object_mut() {
                let has_desc = prop_map
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|d| !d.trim().is_empty());
                if !has_desc {
                    if let Some((_, fallback)) = field_descriptions.iter().find(|(k, _)| *k == name)
                    {
                        prop_map.insert(
                            "description".to_string(),
                            Value::String((*fallback).to_string()),
                        );
                    }
                }
            }
        }
    }
    object.insert("required".to_string(), Value::Array(required));
    Value::Object(object)
}

/// Builds the `changes` array schema by projecting `StructuredChangeProposal`
/// according to `patch_strategy`.
fn projected_changes_schema(patch_strategy: PatchStrategy) -> Value {
    let profile = StrategyProfile::of(patch_strategy);
    let mut entry = schemars_object::<StructuredChangeProposal>();
    entry.insert(
        "description".to_string(),
        Value::String(
            "One structured code modification operation: the operation selector, the target path, the move destination, and the payload the selected operation carries"
                .to_string(),
        ),
    );

    let hidden_fields: &[&str] = match patch_strategy {
        PatchStrategy::SearchReplace | PatchStrategy::ApplyPatch => &["start", "end"],
        PatchStrategy::LineAnchor => &["old_text", "new_text"],
    };

    if let Some(props) = entry.get_mut("properties").and_then(Value::as_object_mut) {
        for field in hidden_fields {
            props.remove(*field);
        }
        props.insert("op".to_string(), op_property(patch_strategy));
        props.insert(
            "content".to_string(),
            string_property(profile.content_description),
        );
        for field in profile.replaced_line_fields {
            props.insert(field.name.to_string(), string_property(field.description));
        }
    }
    let required: Vec<Value> = entry
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| props.keys().map(|k| Value::String(k.clone())).collect())
        .unwrap_or_default();
    entry.insert("required".to_string(), Value::Array(required));

    json!({
        "type": "array",
        "description": "List of structured code modification operations",
        "minItems": 1,
        "items": Value::Object(entry)
    })
}

/// Returns the JSON schema properties owned by one atomic [`LlmCapability`]
/// under `ctx`, or `None` when the capability is inactive in `ctx`.
fn capability_schema(
    cap: LlmCapability,
    ctx: &PromptRenderContext<'_>,
) -> Option<Map<String, Value>> {
    let mut map = Map::new();
    match cap {
        LlmCapability::TextMessage => {
            let _ = schemars_object::<TextCommand>();
            map.insert(
                "message".to_string(),
                string_property(
                    "Conversational explanation or response to the user, present at every stage of the turn whatever branch the action selects",
                ),
            );
        }
        LlmCapability::EditOverwrite if ctx.patch_strategy == PatchStrategy::LineAnchor => {
            return None;
        }
        LlmCapability::EditReplace
        | LlmCapability::EditCreate
        | LlmCapability::EditDelete
        | LlmCapability::EditMove
        | LlmCapability::EditMoveDir
        | LlmCapability::EditOverwrite => {
            if ctx.patch_strategy == PatchStrategy::ApplyPatch {
                map.insert(
                    "patch".to_string(),
                    string_property("Complete Codex patch document"),
                );
            } else {
                map.insert(
                    "changes".to_string(),
                    projected_changes_schema(ctx.patch_strategy),
                );
            }
        }
        LlmCapability::ContextRequest => {
            let item = finalize_strict_object(
                schemars_object::<ContextRequestCommand>(),
                "One file to add to the working context",
                &[
                    ("path", "Repository-relative path to the file"),
                    ("access", "Access level: 'read_only' or 'editable'"),
                ],
            );
            map.insert(
                "context_requests".to_string(),
                json!({
                    "type": "array",
                    "description": "Files to add to context before editing",
                    "items": item
                }),
            );
        }
        LlmCapability::RepoSearch => {
            let item = finalize_strict_object(
                schemars_object::<RepoSearchCommand>(),
                "One Rust / ripgrep regex search query across repository files",
                &[
                    ("path", "Repository-relative directory or file path to search within (empty string for repository root)"),
                    ("regex", "Rust / ripgrep regular expression pattern"),
                ],
            );
            map.insert(
                "repo_searches".to_string(),
                json!({
                    "type": "array",
                    "description": "Repository search requests to find symbols or text patterns across files using Rust / ripgrep regex",
                    "items": item
                }),
            );
        }
        LlmCapability::RepoDirs => {
            let _ = schemars_object::<RepoDirsCommand>();
            map.insert(
                "repo_dirs".to_string(),
                json!({
                    "type": "boolean",
                    "description": "Whether the compact repository directory tree is requested; set true only when the repository layout is still unknown"
                }),
            );
        }
        LlmCapability::RepoMap => {
            let _ = schemars_object::<RepoMapCommand>();
            map.insert(
                "repo_maps".to_string(),
                json!({
                    "type": "array",
                    "description": "Targeted symbol map requests for specific subsystems; each entry is a repository-relative directory or file prefix, and an empty string or '.' requests the whole repository",
                    "items": {
                        "type": "string"
                    }
                }),
            );
        }
        LlmCapability::DirList => {
            let item = finalize_strict_object(
                schemars_object::<DirListCommand>(),
                "One tracked-file directory listing request",
                &[
                    ("path", "Repository-relative directory path, or an empty string for the repository root"),
                    ("recursive", "List every tracked file underneath the directory instead of only its direct children"),
                ],
            );
            map.insert(
                "dir_lists".to_string(),
                json!({
                    "type": "array",
                    "description": "Directory content listing requests over the tracked repository files",
                    "items": item
                }),
            );
        }
        LlmCapability::DocRequest => {
            let doc = schemars_object::<DocRequestCommand>();
            let topic_schema = doc
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|p| p.get("topic"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "string"}));
            map.insert(
                "tauqe_doc_requests".to_string(),
                json!({
                    "type": "array",
                    "description": "System documentation topics to request before answering questions about identity, capabilities, concepts, or workflows",
                    "items": topic_schema
                }),
            );
        }
        LlmCapability::OuterDocRequest => {
            let _ = schemars_object::<OuterDocRequestCommand>();
            map.insert(
                "outer_doc_requests".to_string(),
                json!({
                    "type": "array",
                    "description": "External project documents to materialize before answering, each named by the identifier of an entry declared in the project `[[outer_docs]]` configuration; a materialized document enters the context as read-only reference",
                    "items": {
                        "type": "string"
                    }
                }),
            );
        }
        LlmCapability::PlanCreation => {
            map.insert(
                "plan_create".to_string(),
                crate::llm::discussion::schema::plan_create_schema().clone(),
            );
        }
        LlmCapability::PlanUpdate => {
            map.insert(
                "plan_update".to_string(),
                crate::llm::discussion::schema::plan_update_schema().clone(),
            );
        }
        LlmCapability::ReviewUpdate => {
            map.insert(
                "review_update".to_string(),
                crate::llm::discussion::schema::review_update_schema().clone(),
            );
        }
        LlmCapability::Scratchpad => {
            let _ = schemars_object::<ScratchpadCommand>();
            map.insert(
                "scratchpad".to_string(),
                string_property("Turn-local working memory notes holding hypotheses, verified findings, and the purpose of context still needed; empty string when there is nothing to record"),
            );
        }
        LlmCapability::CommitMessage => {
            let _ = schemars_object::<CommitMessageCommand>();
            map.insert(
                "commit_message".to_string(),
                string_property("Git commit message of the change set when the turn succeeds: an English Conventional Commits header (`type(scope): description`) in imperative mood, lowercase, without a trailing period, at most 72 characters (empty string otherwise)"),
            );
        }
        LlmCapability::TurnError => {
            let _ = schemars_object::<TurnErrorCommand>();
            map.insert(
                "error_message".to_string(),
                string_property("Error message when the turn fails (empty string otherwise)"),
            );
        }
        LlmCapability::TurnSkip => {
            let _ = schemars_object::<TurnSkipCommand>();
            map.insert(
                "reason".to_string(),
                string_property("Skip reason when the work is already satisfied and no change is required (empty string otherwise)"),
            );
        }
        LlmCapability::Blocked => {
            let mut blocked = schemars_object::<BlockedCommand>();
            if let Some(props) = blocked.get_mut("properties").and_then(Value::as_object_mut) {
                props.insert(
                    "is_blocked".to_string(),
                    json!({
                        "type": "boolean",
                        "description": "True only when a precondition or assumption fails"
                    }),
                );
            }
            let schema = finalize_strict_object(
                blocked,
                "Semantic blocker reported when a precondition or assumption fails",
                &[
                    (
                        "reason",
                        "Violated precondition or assumption (empty string when not blocked)",
                    ),
                    (
                        "details",
                        "Optional expansion of the blocker (empty string otherwise)",
                    ),
                ],
            );
            map.insert("blocked".to_string(), schema);
        }
        LlmCapability::Abort => {
            let mut abort = schemars_object::<AbortCommand>();
            if let Some(props) = abort.get_mut("properties").and_then(Value::as_object_mut) {
                props.insert(
                    "is_aborted".to_string(),
                    json!({
                        "type": "boolean",
                        "description": "True only when the work cannot continue without a developer action in the host terminal"
                    }),
                );
            }
            let schema = finalize_strict_object(
                abort,
                "Unconditional abort reported when the remaining work needs an action only a human can perform on the host system",
                &[
                    ("reason", "The concrete missing capability (empty string when no abort is reported)"),
                    ("recommendation", "The exact command or instruction the developer must run (empty string when no abort is reported)"),
                ],
            );
            map.insert("abort".to_string(), schema);
        }
        LlmCapability::CustomTools => {
            let project = ctx.project?;
            if project.commands.is_empty() {
                return None;
            }
            let variants: Vec<_> = project
                .commands
                .iter()
                .filter_map(|(name, spec)| {
                    let command = project.resolve(&format!("commands.{name}"), spec).ok()?;
                    let mut properties = Map::new();
                    let mut required = Vec::new();
                    for parameter in command.parameters {
                        let mut types = vec!["string", "number", "boolean"];
                        let mut description = parameter
                            .doc
                            .filter(|doc| !doc.trim().is_empty())
                            .unwrap_or_else(|| format!("Literal value for {}", parameter.name));
                        if parameter.default.is_some() {
                            types.push("null");
                            description.push_str(". Use null to keep the declared default.");
                        }
                        required.push(parameter.name.clone());
                        properties.insert(
                            parameter.name,
                            json!({"type": types, "description": description}),
                        );
                    }
                    Some(json!({
                        "type": "object",
                        "description": format!("Run the project command '{name}' with its declared arguments."),
                        "properties": {
                            "name": {"type": "string", "enum": [name], "description": "Name of the project command to run."},
                            "args": {
                                "type": "object",
                                "description": "Values for the parameters declared by this command.",
                                "properties": properties,
                                "required": required,
                                "additionalProperties": false
                            }
                        },
                        "required": ["name", "args"],
                        "additionalProperties": false
                    }))
                })
                .collect();
            if variants.is_empty() {
                return None;
            }
            map.insert(
                "custom_tools".to_string(),
                json!({
                    "type": "array",
                    "description": "Typed invocations of the project commands declared in `[project.commands]`; each entry names one declared command, and an empty array requests no execution",
                    "items": {"anyOf": variants}
                }),
            );
        }
    }
    Some(map)
}

/// Recursively collects properties and shared `$defs` from a [`CapabilityTree`].
fn collect_expr_schema(
    expr: &CapabilityTree,
    ctx: &PromptRenderContext<'_>,
    defs: &mut Map<String, Value>,
) -> Map<String, Value> {
    let mut properties = Map::new();
    match expr {
        CapabilityTree::Leaf(cap) => {
            if matches!(cap, LlmCapability::PlanCreation | LlmCapability::PlanUpdate) {
                if let Some(entries) =
                    crate::llm::discussion::schema::plan_schema_definitions().as_object()
                {
                    for (name, schema) in entries {
                        defs.entry(name.clone()).or_insert_with(|| schema.clone());
                    }
                }
            }
            if let Some(fields) = capability_schema(*cap, ctx) {
                for (name, schema) in fields {
                    properties.entry(name).or_insert(schema);
                }
            }
        }
        CapabilityTree::Set(_, items) => {
            for item in items {
                for (name, schema) in collect_expr_schema(item, ctx, defs) {
                    properties.entry(name).or_insert(schema);
                }
            }
        }
        CapabilityTree::AnyOf(label, branches) => {
            let mut variants: Vec<(&str, Map<String, Value>)> = Vec::new();
            for branch in branches {
                match branch.label().filter(|kind| !kind.is_empty()) {
                    Some(kind) => variants.push((kind, collect_expr_schema(branch, ctx, defs))),
                    // A branch that carries no name takes part in the level that
                    // holds the union, exactly as a transparent block does.
                    None => {
                        for (name, schema) in collect_expr_schema(branch, ctx, defs) {
                            properties.entry(name).or_insert(schema);
                        }
                    }
                }
            }
            if variants.len() < 2 {
                // A union of fewer than two named branches excludes nothing, so
                // its properties return to the level that holds it.
                for (_, fields) in variants {
                    for (name, schema) in fields {
                        properties.entry(name).or_insert(schema);
                    }
                }
            } else {
                let tagged: Vec<Value> = variants
                    .into_iter()
                    .map(|(kind, fields)| branch_variant(kind, fields))
                    .collect();
                let (name, union) = action_union(label.clone(), tagged);
                properties.insert(name, union);
            }
        }
    }
    properties
}

/// Prefix of the JSON Pointer that addresses one shared definition.
const DEFINITION_POINTER_PREFIX: &str = "#/$defs/";

/// Resolves every shared-definition reference of `node` in place.
///
/// The plan tree is unrolled into one definition per depth level and a payload
/// references them by pointer, because a definition that references itself is
/// not expressible at all. A shared definition is still a second place a schema
/// can break: a strict Structured Output mode is not guaranteed to resolve a
/// JSON Pointer, and a pointer nested inside a discriminated union is the least
/// reliable of them. The composed surface is therefore self-contained — every
/// reference is replaced by the body of the definition it names, inlined once
/// per use site.
///
/// `visited` carries the definitions of the current walk, so a definition that
/// reached itself keeps its pointer instead of recursing forever; the unrolled
/// plan tree is an acyclic chain, so that guard never fires for it.
fn inline_definitions(
    node: &Value,
    definitions: &Map<String, Value>,
    visited: &mut Vec<String>,
) -> Value {
    match node {
        Value::Object(object) => {
            let reference = object
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|target| target.strip_prefix(DEFINITION_POINTER_PREFIX))
                .filter(|name| !visited.iter().any(|seen| seen.as_str() == *name))
                .and_then(|name| definitions.get(name).map(|body| (name, body)));
            if let Some((name, definition)) = reference {
                visited.push(name.to_string());
                let body = inline_definitions(definition, definitions, visited);
                visited.pop();
                let siblings: Map<String, Value> = object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "$ref")
                    .map(|(key, value)| {
                        (key.clone(), inline_definitions(value, definitions, visited))
                    })
                    .collect();
                return match body {
                    Value::Object(mut resolved) => {
                        resolved.extend(siblings);
                        Value::Object(resolved)
                    }
                    other => other,
                };
            }
            let mut inlined = Map::with_capacity(object.len());
            for (key, value) in object {
                inlined.insert(key.clone(), inline_definitions(value, definitions, visited));
            }
            Value::Object(inlined)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| inline_definitions(item, definitions, visited))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Composes the full strict JSON schema for a [`CapabilityExpr`] under `ctx`.
///
/// The composed schema is the whole wire contract, so it carries the plan tree
/// a payload uses in place and never a definitions section: every reference the
/// capability modules emit is resolved here, and one projection therefore
/// serves the schema, the decoder, and the rendered instructions unchanged.
pub(crate) fn capability_expr_schema(
    expr: &CapabilityTree,
    ctx: &PromptRenderContext<'_>,
) -> Value {
    let mut definitions = Map::new();
    let collected = collect_expr_schema(expr, ctx, &mut definitions);
    let mut visited: Vec<String> = Vec::new();
    let properties: Map<String, Value> = collected
        .into_iter()
        .map(|(name, schema)| {
            (
                name,
                inline_definitions(&schema, &definitions, &mut visited),
            )
        })
        .collect();
    let required: Vec<Value> = properties
        .keys()
        .map(|name| Value::String(name.clone()))
        .collect();
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::PromptRenderContext;
    use crate::llm::LlmCapability;

    fn project_with_command() -> ProjectConfig {
        serde_json::from_str(r#"{"commands": {"check": {"command": "just", "args": ["check"]}}}"#)
            .expect("project parses")
    }

    /// Returns the inlined plan tree node a payload declares at `level`.
    ///
    /// The composed schema substitutes every shared definition at its use site,
    /// so a deeper level is reached through the `children` chain of the level
    /// above it instead of through a reference to a definition.
    fn inlined_plan_node<'a>(
        composed: &'a Value,
        payload: &str,
        collection: &str,
        level: usize,
    ) -> &'a Value {
        let mut node = &composed["properties"][payload]["properties"][collection]["items"];
        for _ in 1..level {
            node = &node["properties"]["children"]["items"];
        }
        node
    }

    #[test]
    fn capability_schemas_have_no_nullable_types_except_plan_and_review() {
        let project = project_with_command();
        let ctx = PromptRenderContext::new(&[], None)
            .with_project(Some(&project))
            .with_patch_strategy(PatchStrategy::SearchReplace);
        for capability in LlmCapability::ALL {
            let Some(fields) = capability_schema(*capability, &ctx) else {
                continue;
            };
            assert!(
                !fields.is_empty(),
                "{capability} declares at least one property"
            );
            if matches!(
                capability,
                LlmCapability::PlanCreation
                    | LlmCapability::PlanUpdate
                    | LlmCapability::ReviewUpdate
            ) {
                continue;
            }
            let serialized = Value::Object(fields).to_string();
            assert!(
                !serialized.contains("\"null\""),
                "{capability}: {serialized}"
            );
        }
    }

    #[test]
    fn the_change_set_schema_follows_the_patch_strategy() {
        let sr_ctx =
            PromptRenderContext::new(&[], None).with_patch_strategy(PatchStrategy::SearchReplace);
        let search_replace =
            Value::Object(capability_schema(LlmCapability::EditReplace, &sr_ctx).unwrap());
        let search_fields = search_replace["changes"]["items"]["properties"]
            .as_object()
            .expect("the change entry declares its fields");
        assert!(search_fields.contains_key("old_text"));
        assert!(search_fields.contains_key("new_text"));
        assert!(!search_fields.contains_key("start"));

        let la_ctx =
            PromptRenderContext::new(&[], None).with_patch_strategy(PatchStrategy::LineAnchor);
        let anchored =
            Value::Object(capability_schema(LlmCapability::EditReplace, &la_ctx).unwrap());
        let anchor_fields = anchored["changes"]["items"]["properties"]
            .as_object()
            .expect("the change entry declares its fields");
        assert!(anchor_fields.contains_key("start"));
        assert!(anchor_fields.contains_key("end"));
        assert!(anchor_fields.contains_key("content"));
        assert!(!anchor_fields.contains_key("old_text"));
        assert!(!anchor_fields.contains_key("new_text"));

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
    fn the_documentation_schema_publishes_every_topic() {
        let ctx = PromptRenderContext::new(&[], None);
        let fragment = Value::Object(capability_schema(LlmCapability::DocRequest, &ctx).unwrap());
        let topics = fragment["tauqe_doc_requests"]["items"]["enum"]
            .as_array()
            .expect("the topic enum is an array");
        assert_eq!(topics.len(), DocTopic::ALL.len());
        assert!(topics.iter().any(|topic| topic == "config"));
    }

    #[test]
    fn the_plan_payloads_are_split_and_forwarded_authoritatively() {
        use crate::llm::discussion::schema::{
            plan_create_schema, plan_update_schema, PLAN_TREE_DEPTH,
        };

        let ctx = PromptRenderContext::new(&[], None);
        let create = Value::Object(capability_schema(LlmCapability::PlanCreation, &ctx).unwrap());
        let update = Value::Object(capability_schema(LlmCapability::PlanUpdate, &ctx).unwrap());

        // Each capability owns exactly one property, and the names differ, so no
        // wire declares both plan payloads at the same level.
        assert_eq!(create.as_object().map(|fields| fields.len()), Some(1));
        assert_eq!(update.as_object().map(|fields| fields.len()), Some(1));
        assert_eq!(&create["plan_create"], plan_create_schema());
        assert_eq!(&update["plan_update"], plan_update_schema());
        assert!(create.get("plan_update").is_none());
        assert!(update.get("plan_create").is_none());

        for (label, payload, action) in [
            ("plan_create", plan_create_schema(), "save"),
            ("plan_update", plan_update_schema(), "update"),
        ] {
            assert_eq!(payload["type"], "object", "{label}");
            assert_eq!(
                payload["additionalProperties"],
                serde_json::json!(false),
                "{label}"
            );
            assert_eq!(
                payload["properties"]["action"]["enum"],
                serde_json::json!([action]),
                "{label} owns its own action"
            );
            let mut properties: Vec<&str> = payload["properties"]
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
            assert_eq!(
                payload["properties"]["concept"]["items"]["$ref"],
                "#/$defs/ConceptItemL1"
            );
            assert_eq!(
                payload["properties"]["items"]["items"]["$ref"],
                "#/$defs/PlanItemL1"
            );
        }

        // The composed surface is self-contained: the shared definitions are
        // inlined where a payload uses them, so neither a definitions section
        // nor a reference reaches the wire, whichever plan capability composes
        // it.
        for (capability, payload) in [
            (LlmCapability::PlanCreation, "plan_create"),
            (LlmCapability::PlanUpdate, "plan_update"),
        ] {
            let composed = capability_expr_schema(&CapabilityTree::leaf(capability), &ctx);
            assert!(composed.get("$defs").is_none(), "{capability}");
            let serialized = composed.to_string();
            assert!(!serialized.contains("$ref"), "{capability}");
            assert!(!serialized.contains("$defs"), "{capability}");

            for (collection, kind) in [("concept", "ConceptItem"), ("items", "PlanItem")] {
                for level in 1..=PLAN_TREE_DEPTH {
                    let node = inlined_plan_node(&composed, payload, collection, level);
                    assert_eq!(node["type"], "object", "{kind} level {level}");
                    assert_eq!(
                        node["additionalProperties"],
                        serde_json::json!(false),
                        "{kind} level {level}"
                    );
                    let children = &node["properties"]["children"];
                    assert_eq!(children["type"], "array", "{kind} level {level}");
                    if level < PLAN_TREE_DEPTH {
                        assert_eq!(
                            children["items"]["type"], "object",
                            "{kind} level {level} chains to the next level of its own kind"
                        );
                    } else {
                        assert_eq!(
                            children["maxItems"],
                            serde_json::json!(0),
                            "{kind} level {level} seals the tree"
                        );
                    }

                    let declared: std::collections::BTreeSet<&str> = node["properties"]
                        .as_object()
                        .expect("the inlined level declares its fields")
                        .keys()
                        .map(String::as_str)
                        .collect();
                    let required: std::collections::BTreeSet<&str> = node["required"]
                        .as_array()
                        .expect("the inlined level requires its fields")
                        .iter()
                        .filter_map(|field| field.as_str())
                        .collect();
                    assert_eq!(
                        declared, required,
                        "{kind} level {level} requires what it declares"
                    );
                }
            }
        }
    }

    #[test]
    fn the_abort_and_custom_tool_schemas_declare_their_idle_representation() {
        let project = project_with_command();
        let ctx = PromptRenderContext::new(&[], None).with_project(Some(&project));
        let abort = Value::Object(capability_schema(LlmCapability::Abort, &ctx).unwrap());
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

        let tools = Value::Object(capability_schema(LlmCapability::CustomTools, &ctx).unwrap());
        assert_eq!(tools["custom_tools"]["type"], "array");
        let item = &tools["custom_tools"]["items"]["anyOf"][0];
        assert_eq!(item["properties"]["name"]["type"], "string");
        assert_eq!(item["properties"]["args"]["type"], "object");
        assert_eq!(item["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn the_outcome_schemas_state_the_payload_contract() {
        let ctx = PromptRenderContext::new(&[], None);
        let commit_fragment =
            Value::Object(capability_schema(LlmCapability::CommitMessage, &ctx).unwrap());
        let commit_message = commit_fragment["commit_message"]["description"]
            .as_str()
            .expect("the commit message declares its purpose");
        assert!(
            commit_message.contains("Conventional Commits"),
            "{commit_message}"
        );
        assert!(commit_message.contains("succeeds"), "{commit_message}");
        assert!(commit_message.contains("72"), "{commit_message}");
        assert_eq!(commit_fragment["commit_message"]["type"], "string");

        let error_fragment =
            Value::Object(capability_schema(LlmCapability::TurnError, &ctx).unwrap());
        assert_eq!(error_fragment["error_message"]["type"], "string");
        assert!(error_fragment["error_message"]["description"]
            .as_str()
            .expect("the error message declares its purpose")
            .contains("fails"));

        let skip_fragment =
            Value::Object(capability_schema(LlmCapability::TurnSkip, &ctx).unwrap());
        assert_eq!(skip_fragment["reason"]["type"], "string");
        assert!(skip_fragment["reason"]["description"]
            .as_str()
            .expect("the skip reason declares its purpose")
            .contains("satisfied"));
    }

    fn operation_selector(patch_strategy: PatchStrategy) -> Value {
        let ctx = PromptRenderContext::new(&[], None).with_patch_strategy(patch_strategy);
        let fragment = Value::Object(capability_schema(LlmCapability::EditReplace, &ctx).unwrap());
        fragment["changes"]["items"]["properties"]["op"].clone()
    }

    #[test]
    fn the_operation_selector_follows_the_patch_strategy() {
        let anchored = operation_selector(PatchStrategy::LineAnchor);
        let anchored_operations = json!(["replace", "create", "delete", "move", "move_dir"]);
        assert_eq!(anchored["enum"], anchored_operations);
        let description = anchored["description"]
            .as_str()
            .expect("the operation selector states its values");
        assert!(!description.contains("overwrite"), "{description}");
        assert!(description.contains("'move_dir'"), "{description}");

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

    #[test]
    fn the_review_schema_forwards_the_authoritative_protocol_schema() {
        use crate::llm::discussion::ReviewDecisionStatus;

        let ctx = PromptRenderContext::new(&[], None);
        let fragment = Value::Object(capability_schema(LlmCapability::ReviewUpdate, &ctx).unwrap());
        let review_update = &fragment["review_update"];

        assert_eq!(
            review_update,
            crate::llm::discussion::schema::review_update_schema()
        );

        // The nullable shape is the canonical strict form: one object schema
        // whose `type` array admits `null` beside `object`, so no combinator
        // stands between the payload and the fields it declares.
        assert_eq!(review_update["type"], serde_json::json!(["object", "null"]));

        let object = review_update;
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

    #[test]
    fn the_branch_variant_admits_exactly_one_discriminator() {
        let mut fields = Map::new();
        fields.insert("changes".to_string(), json!({"type": "array"}));

        let variant = branch_variant("edit", fields);
        assert_eq!(variant["properties"]["kind"]["enum"], json!(["edit"]));
        assert_eq!(variant["properties"]["changes"]["type"], "array");
        assert_eq!(variant["additionalProperties"], json!(false));
        assert_eq!(
            variant["required"]
                .as_array()
                .map(|required| required.len()),
            variant["properties"].as_object().map(|fields| fields.len())
        );

        let (label, union) = action_union("action".to_string(), vec![variant]);
        assert_eq!(label, "action");
        assert_eq!(
            union["anyOf"].as_array().map(|variants| variants.len()),
            Some(1)
        );
        assert!(union["description"]
            .as_str()
            .is_some_and(|description| description.contains("mutually exclusive")));
    }

    #[test]
    fn the_answer_branch_states_that_it_is_a_terminal_marker() {
        let variant = branch_variant("answer", Map::new());
        assert_eq!(variant["properties"]["kind"]["enum"], json!(["answer"]));
        assert_eq!(variant["additionalProperties"], json!(false));

        // The branch that ends a dialogue carries nothing but its discriminator,
        // so both descriptions say that the answer itself lives in the root
        // `message` and that selecting this branch is what closes the turn.
        let kind = variant["properties"]["kind"]["description"]
            .as_str()
            .expect("the discriminator states its meaning");
        assert!(kind.contains("complete and final"));
        assert!(kind.contains("no further research rounds or code edits"));

        let description = variant["description"]
            .as_str()
            .expect("the variant states its meaning");
        assert!(description.contains("terminal marker"));
        assert!(description.contains("'message'"));
        assert!(description.contains("complete final answer"));
    }

    fn assert_descriptions(node: &serde_json::Value, path: &str, is_root: bool) {
        let Some(map) = node.as_object() else {
            return;
        };
        if let Some(properties) = map.get("properties").and_then(|value| value.as_object()) {
            if !is_root && !properties.is_empty() {
                let described = map
                    .get("description")
                    .and_then(|value| value.as_str())
                    .is_some_and(|description| !description.trim().is_empty());
                assert!(
                    described,
                    "{path} declares properties without a description"
                );
            }
            for (name, schema) in properties {
                let described = schema
                    .get("description")
                    .and_then(|value| value.as_str())
                    .is_some_and(|description| !description.trim().is_empty());
                assert!(
                    described,
                    "{path}.{name} is a schema property without a description"
                );
                assert_descriptions(schema, &format!("{path}.{name}"), false);
            }
        }
        if let Some(items) = map.get("items") {
            assert_descriptions(items, &format!("{path}[]"), false);
        }
        if let Some(variants) = map.get("anyOf").and_then(|value| value.as_array()) {
            for (index, variant) in variants.iter().enumerate() {
                assert_descriptions(variant, &format!("{path}|{index}"), false);
            }
        }
        if let Some(definitions) = map.get("$defs").and_then(|value| value.as_object()) {
            for (name, definition) in definitions {
                assert_descriptions(definition, &format!("$defs.{name}"), false);
            }
        }
    }

    fn property_map(fragment: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "properties": fragment.clone() })
    }

    #[test]
    fn every_schema_property_states_a_description() {
        let project = project_with_command();
        for strategy in [PatchStrategy::SearchReplace, PatchStrategy::LineAnchor] {
            let ctx = PromptRenderContext::new(&[], None)
                .with_project(Some(&project))
                .with_patch_strategy(strategy);
            for capability in LlmCapability::ALL {
                if let Some(fields) = capability_schema(*capability, &ctx) {
                    assert_descriptions(
                        &property_map(&Value::Object(fields)),
                        &format!("{capability}/{strategy}"),
                        true,
                    );
                }
            }
            let surface = crate::control::repl::repl_capability_expr(None);
            let composed = capability_expr_schema(&surface, &ctx);
            assert_descriptions(&composed, &format!("interactive_repl/{strategy}"), true);
        }
    }

    #[test]
    fn capability_schema_projects_strategy_and_project_commands() {
        let search_ctx =
            PromptRenderContext::new(&[], None).with_patch_strategy(PatchStrategy::SearchReplace);
        let anchor_ctx =
            PromptRenderContext::new(&[], None).with_patch_strategy(PatchStrategy::LineAnchor);

        // EditOverwrite is active under SearchReplace and excluded under LineAnchor.
        assert!(capability_schema(LlmCapability::EditOverwrite, &search_ctx).is_some());
        assert!(capability_schema(LlmCapability::EditOverwrite, &anchor_ctx).is_none());

        // Projected `changes` fields follow the active PatchStrategy.
        let sr_changes = capability_schema(LlmCapability::EditReplace, &search_ctx)
            .expect("SearchReplace changes");
        let sr_props = sr_changes["changes"]["items"]["properties"]
            .as_object()
            .expect("SearchReplace properties");
        assert!(sr_props.contains_key("old_text"));
        assert!(sr_props.contains_key("new_text"));
        assert!(!sr_props.contains_key("start"));
        assert!(!sr_props.contains_key("end"));

        let la_changes =
            capability_schema(LlmCapability::EditReplace, &anchor_ctx).expect("LineAnchor changes");
        let la_props = la_changes["changes"]["items"]["properties"]
            .as_object()
            .expect("LineAnchor properties");
        assert!(la_props.contains_key("start"));
        assert!(la_props.contains_key("end"));
        assert!(!la_props.contains_key("old_text"));
        assert!(!la_props.contains_key("new_text"));

        // CustomTools is active only when the project declares commands.
        assert!(capability_schema(LlmCapability::CustomTools, &search_ctx).is_none());
        let project: crate::config::project::ProjectConfig = serde_json::from_str(
            r#"{"commands": {"check": {"command": "just", "args": ["check"]}}}"#,
        )
        .expect("project parses");
        let with_project = PromptRenderContext::new(&[], None).with_project(Some(&project));
        assert!(capability_schema(LlmCapability::CustomTools, &with_project).is_some());
    }

    #[test]
    fn capability_expr_schema_handles_single_and_nested_any_of_and_defs() {
        let ctx = PromptRenderContext::new(&[], None);

        // Single-branch AnyOf merges transparently into the enclosing level.
        let single = CapabilityTree::any_of(
            "action",
            [CapabilityTree::named_set(
                "only",
                [CapabilityTree::leaf(LlmCapability::TextMessage)],
            )],
        );
        let single_schema = capability_expr_schema(&single, &ctx);
        assert!(single_schema["properties"].get("action").is_none());
        assert!(single_schema["properties"].get("message").is_some());
        assert_eq!(single_schema["additionalProperties"], json!(false));
        assert_descriptions(&single_schema, "single", true);

        // Nested AnyOf and PlanCreation lift `$defs` to the root and keep strict invariants.
        let nested = CapabilityTree::root([
            CapabilityTree::leaf(LlmCapability::TextMessage),
            CapabilityTree::any_of(
                "action",
                [
                    CapabilityTree::named_set(
                        "plan",
                        [
                            CapabilityTree::leaf(LlmCapability::PlanCreation),
                            CapabilityTree::leaf(LlmCapability::CommitMessage),
                        ],
                    ),
                    CapabilityTree::named_set(
                        "nested",
                        [CapabilityTree::any_of(
                            "sub_action",
                            [
                                CapabilityTree::named_set(
                                    "search",
                                    [CapabilityTree::leaf(LlmCapability::RepoSearch)],
                                ),
                                CapabilityTree::named_set(
                                    "edit",
                                    [CapabilityTree::leaf(LlmCapability::EditReplace)],
                                ),
                            ],
                        )],
                    ),
                ],
            ),
        ]);
        let nested_schema = capability_expr_schema(&nested, &ctx);
        // The plan branch of the union carries its unrolled tree inlined in
        // place, so the exclusion of the branches costs no shared definition and
        // no reference a strict mode would have to resolve.
        assert!(nested_schema.get("$defs").is_none());
        assert!(!nested_schema.to_string().contains("$ref"));
        let variants = nested_schema["properties"]["action"]["anyOf"]
            .as_array()
            .expect("outer anyOf");
        assert_eq!(variants.len(), 2);
        assert!(variants[1]["properties"]["sub_action"]["anyOf"].is_array());
        assert_eq!(
            variants[0]["properties"]["plan_create"]["properties"]["concept"]["items"]["type"],
            "object"
        );
        assert_descriptions(&nested_schema, "nested", true);

        // Full interactive_repl surface composes cleanly with all descriptions.
        let repl_schema =
            capability_expr_schema(&crate::control::repl::repl_capability_expr(None), &ctx);
        assert_descriptions(&repl_schema, "interactive_repl_expr", true);
    }

    #[test]
    fn a_plan_focused_surface_wraps_the_plan_payloads_in_one_union() {
        use crate::discussion::DiscussionTarget;

        let ctx = PromptRenderContext::new(&[], None);
        let focused = crate::control::repl::repl_capability_expr(Some(&DiscussionTarget::Plan {
            id: "p-1".into(),
        }));
        let composed = capability_expr_schema(&focused, &ctx);
        assert_descriptions(&composed, "plan_focused", true);

        let variants = composed["properties"]["action"]["anyOf"]
            .as_array()
            .expect("the branches form a discriminated union");
        let plan = variants
            .iter()
            .find(|variant| variant["properties"]["kind"]["enum"][0] == "plan")
            .expect("the plan branch is declared");
        assert!(
            plan["properties"].get("plan_update").is_none(),
            "the plan payloads never share the level of the branch"
        );

        let union = &plan["properties"]["plan_action"];
        let variants = union["anyOf"]
            .as_array()
            .expect("the plan payloads form a union");
        let kinds: Vec<&str> = variants
            .iter()
            .map(|variant| {
                variant["properties"]["kind"]["enum"][0]
                    .as_str()
                    .expect("the variant names its kind")
            })
            .collect();
        assert_eq!(kinds, vec!["create", "modify"]);
        for (variant, property) in variants.iter().zip(["plan_create", "plan_update"]) {
            assert_eq!(variant["additionalProperties"], json!(false));
            assert_eq!(
                variant["required"]
                    .as_array()
                    .map(|required| required.len()),
                variant["properties"]
                    .as_object()
                    .map(|properties| properties.len())
            );
            assert!(
                variant["properties"].get(property).is_some(),
                "the variant carries the payload its kind selects"
            );
            let other = if property == "plan_create" {
                "plan_update"
            } else {
                "plan_create"
            };
            assert!(
                variant["properties"].get(other).is_none(),
                "the two plan payloads never share one level"
            );
            assert_eq!(variant["properties"][property]["type"], "object");
            assert!(
                variant["properties"][property].get("anyOf").is_none(),
                "a plan payload carries no nullable wrapper"
            );
        }
    }
}
