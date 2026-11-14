use std::borrow::Cow;

use tauqe_protocol::{
    CapabilityExpr, EditCreate, EditDelete, EditFileOp, EditLineAnchor, EditMove, EditMoveDir,
    EditOperation, EditOutcome, EditOverwrite, EditReplace, LlmCapability, LlmCommand, ModelResult,
    ModelResultProposal, PatchStrategy, StructuredChangeProposal,
};

use crate::capabilities::{
    capability_expr_schema, compose_structured_protocol_instructions, decode_capability_expr,
    PromptRenderContext,
};
use crate::edits::strategy::StrategyProfile;
use crate::providers::{JsonSchemaDefinition, ResponseFormat};

use super::utils::{normalize_content, resolve_target_path_for_op};

/// Structured Output JSON-schema edit protocol
#[derive(Debug, Default, Clone)]
pub struct StructuredEditProtocol;

impl StructuredEditProtocol {
    /// Returns the edit schema of the interactive REPL capability surface.
    ///
    /// The default surface is intent-oriented, never transport-specific, so the
    /// The default surface is the shared dialogue profile, never transport-specific, so the
    pub fn schema(ctx: &PromptRenderContext<'_>) -> serde_json::Value {
        Self::schema_for(
            &crate::control::repl::dialogue_capability_expr(false, false),
            ctx,
        )
    }

    /// Builds the edit schema for exactly the enabled capabilities.
    ///
    /// The registry composes the schema from the same capability modules that
    /// render the prompt instructions and scope the decoder, so a disabled
    /// capability contributes neither an instruction nor a schema field. A
    /// property therefore cannot drift from the rules that describe it, and the
    /// strict `required` list is derived from the emitted properties, so a
    /// pruned property is never left required.
    ///
    /// The render context carries the patch strategy and the project command
    /// declaration the composed schema must observe, so a caller that renders
    /// the prompt and composes the schema hands over one identical value.
    pub fn schema_for(
        capabilities: &CapabilityExpr,
        ctx: &PromptRenderContext<'_>,
    ) -> serde_json::Value {
        capability_expr_schema(capabilities, ctx)
    }

    fn proposal_to_model_result(
        &self,
        proposal: ModelResultProposal,
        edits: Vec<EditOperation>,
    ) -> ModelResult {
        let summary = if !proposal.message.trim().is_empty() {
            let first_line = proposal
                .message
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Applied changes")
                .trim();
            if first_line.chars().count() > 100 {
                let truncated: String = first_line.chars().take(97).collect();
                format!("{}...", truncated)
            } else {
                first_line.to_string()
            }
        } else if edits.len() == 1 {
            match &edits[0] {
                EditOperation::Replace(EditReplace { path, .. }) => format!("Update {}", path),
                EditOperation::Create(EditCreate { path, .. }) => format!("Create {}", path),
                EditOperation::Delete(EditDelete { path }) => format!("Delete {}", path),
                EditOperation::Move(EditMove { from, to }) => format!("Move {} to {}", from, to),
                EditOperation::MoveDir(EditMoveDir { from, to }) => {
                    format!("Move directory {} to {}", from, to)
                }
                EditOperation::Overwrite(EditOverwrite { path, .. }) => {
                    format!("Overwrite {}", path)
                }
                EditOperation::LineAnchor(EditLineAnchor { path, .. }) => {
                    format!("Replace anchored range in {}", path)
                }
            }
        } else {
            format!("Apply {} structured edits", edits.len())
        };

        ModelResult::Edit {
            summary,
            edits,
            proposal: Some(proposal),
            outcome: EditOutcome {
                applied: false,
                error: None,
                changed_files: Vec::new(),
                commit_hash: None,
                verification: None,
            },
        }
    }

    /// Returns system prompt instructions for the structured-output protocol.
    pub fn system_instructions(
        &self,
        editable_paths: &[String],
        patch_strategy: PatchStrategy,
    ) -> String {
        self.system_instructions_for(
            &crate::control::repl::dialogue_capability_expr(false, false),
            editable_paths,
            patch_strategy,
        )
    }

    /// Returns system prompt instructions for an explicit capability set.
    ///
    /// The field reference is composed from the capability registry, so a
    /// narrowed set never advertises a field for a disabled capability.
    pub fn system_instructions_for(
        &self,
        capabilities: &CapabilityExpr,
        editable_paths: &[String],
        patch_strategy: PatchStrategy,
    ) -> String {
        let ctx =
            PromptRenderContext::new(editable_paths, None).with_patch_strategy(patch_strategy);
        let mut prompt = String::new();
        prompt.push_str("## Code Modification Protocol (Structured Output JSON)\n");
        prompt.push_str(
            "Respond with a single JSON object conforming to the structured output schema:\n",
        );
        let schema = Self::schema_for(capabilities, &ctx);
        if let Ok(schema_str) = serde_json::to_string_pretty(&schema) {
            prompt.push_str("```json\n");
            prompt.push_str(&schema_str);
            prompt.push_str("\n```\n\n");
        }
        // The schema is the single source of the response structure, so the
        // instructions carry only the operational rules a field cannot express:
        // how a change set addresses the lines it replaces.
        let operational = compose_structured_protocol_instructions(capabilities, &ctx);
        if !operational.is_empty() {
            prompt.push_str(&operational);
            prompt.push('\n');
        }
        prompt.push('\n');

        if !has_any_edit(capabilities) {
            return prompt;
        }

        // Every strategy-specific sentence of the contract comes from the
        // profile, so the instructions and the declared schema describe the same
        // fields.
        let profile = StrategyProfile::of(patch_strategy);
        prompt.push_str(profile.empty_field_guidance);

        prompt.push_str("Files inside <context> declare their status through a declarative `access` attribute: `access=\"editable\"` files may be modified directly, while a file marked `access=\"read_only\"` is provided for reference and becomes editable automatically once you propose a change to it. ");
        // The reachability of a file absent from <context> depends on whether
        // the discovery capability is enabled: only a set that advertises
        // `context_requests` in the schema may be told to use it.
        if has_any_research(capabilities) {
            prompt.push_str("A file absent from <context> is reached through `context_requests` or created with `op: 'create'`.\n\n");
        } else {
            prompt
                .push_str("A file absent from <context> must be created with `op: 'create'`.\n\n");
        }

        prompt.push_str("CRITICAL INVARIANTS:\n");
        prompt.push_str("1. Output MUST be valid JSON adhering to the specified schema.\n");
        prompt.push_str("2. For 'replace', `path` MUST match a file present in <context>.\n");
        prompt.push_str("3. ");
        prompt.push_str(profile.replace_invariant);
        let content_invariant = match patch_strategy {
            PatchStrategy::SearchReplace => {
                "4. For 'create' and 'overwrite', `content` must contain the complete file content from first to last line.\n"
            }
            PatchStrategy::LineAnchor => {
                "4. For 'create', `content` must contain the complete file content from first to last line.\n"
            }
        };
        prompt.push_str(content_invariant);
        prompt
            .push_str("5. When proposing code modifications, state the change set in `changes`.\n");
        prompt
    }

    /// Returns the JSON-schema response format used to constrain generation.
    pub fn response_format(&self, ctx: &PromptRenderContext<'_>) -> Option<ResponseFormat> {
        self.response_format_for(
            &crate::control::repl::dialogue_capability_expr(false, false),
            ctx,
        )
    }

    /// Returns the JSON-schema response format for an explicit capability set,
    /// so the wire schema matches the advertised capability surface.
    /// The render context is handed to the composed schema unchanged, so the
    /// format a caller constrains generation with is the very projection the
    /// rendered instruction described.
    pub fn response_format_for(
        &self,
        capabilities: &CapabilityExpr,
        ctx: &PromptRenderContext<'_>,
    ) -> Option<ResponseFormat> {
        Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name: "tauqe_edit_proposal".to_string(),
                description: Some("Structured code modification proposal and response".to_string()),
                schema: Cow::Owned(Self::schema_for(capabilities, ctx)),
                strict: Some(true),
            },
        })
    }

    /// Extracts ephemeral scratchpad notes from a structured response,
    /// reporting nothing while the field is absent or blank.
    pub fn parse_scratchpad(&self, raw_text: &str) -> Option<String> {
        let payload = StructuredPayload::parse(raw_text);
        let root = payload.root()?;
        let expr = CapabilityExpr::leaf(LlmCapability::Scratchpad);
        let ctx = PromptRenderContext::new(&[], None);
        decode_capability_expr(&expr, root, &ctx)
            .into_iter()
            .find_map(|command| match command {
                LlmCommand::Scratchpad(command) => Some(command.notes),
                _ => None,
            })
    }

    /// Decodes one structured round into the model result the edit contour
    /// reports.
    ///
    /// The commands are decoded by walking the very capability tree that
    /// composed the schema the model answered, so the change set the round
    /// produced is read exactly where the tree declares it and a command of
    /// a capability the tree omits never surfaces. The tree arrives from
    /// the calling protocol: it is the declaration the response was
    /// constrained against, never a reconstruction from the payload.
    pub fn parse_output(
        &self,
        expr: &CapabilityExpr,
        raw_text: &str,
        editable_paths: &[String],
    ) -> ModelResult {
        let payload = StructuredPayload::parse(raw_text);
        let Some(root) = payload.root() else {
            return ModelResult::Answer {
                text: payload.clean_text(),
            };
        };
        let mut edits: Vec<EditOperation> = Vec::new();
        let mut defect: Option<String> = None;
        let ctx = PromptRenderContext::new(editable_paths, None);
        for command in decode_capability_expr(expr, root, &ctx) {
            if let LlmCommand::ProtocolError { message } = command {
                if defect.is_none() {
                    defect = Some(message);
                }
            } else if let Some(operation) = command.into_edit_operation() {
                edits.push(operation);
            }
        }
        let proposal = payload.to_proposal().unwrap_or_default();
        if !edits.is_empty() {
            let mut resolved = Vec::with_capacity(edits.len());
            for operation in edits {
                match resolve_operation_target(operation, editable_paths) {
                    Ok(operation) => resolved.push(operation),
                    Err(message) => {
                        return ModelResult::Edit {
                            summary: format!("Invalid change: {}", message),
                            edits: Vec::new(),
                            proposal: Some(proposal),
                            outcome: EditOutcome {
                                applied: false,
                                error: Some(message),
                                changed_files: Vec::new(),
                                commit_hash: None,
                                verification: None,
                            },
                        };
                    }
                }
            }
            return self.proposal_to_model_result(proposal, resolved);
        }
        if let Some(message) = defect {
            return ModelResult::Edit {
                summary: format!("Malformed structured output: {message}"),
                edits: Vec::new(),
                proposal: None,
                outcome: EditOutcome {
                    applied: false,
                    error: Some(message),
                    changed_files: Vec::new(),
                    commit_hash: None,
                    verification: None,
                },
            };
        }
        ModelResult::Answer {
            text: proposal.message,
        }
    }
}
/// Decodes the commands of one capability through its registry module.
///
/// A response that did not parse as JSON carries no field any module could
/// read, so it yields no command instead of failing the round.
fn has_any_edit(capabilities: &CapabilityExpr) -> bool {
    capabilities.contains(LlmCapability::EditReplace)
        || capabilities.contains(LlmCapability::EditCreate)
        || capabilities.contains(LlmCapability::EditDelete)
        || capabilities.contains(LlmCapability::EditMove)
        || capabilities.contains(LlmCapability::EditMoveDir)
        || capabilities.contains(LlmCapability::EditOverwrite)
}

fn has_any_research(capabilities: &CapabilityExpr) -> bool {
    capabilities.contains(LlmCapability::ContextRequest)
        || capabilities.contains(LlmCapability::RepoSearch)
        || capabilities.contains(LlmCapability::RepoDirs)
        || capabilities.contains(LlmCapability::RepoMap)
        || capabilities.contains(LlmCapability::DirList)
}

fn is_edit_capability(capability: Option<LlmCapability>) -> bool {
    matches!(
        capability,
        Some(
            LlmCapability::EditReplace
                | LlmCapability::EditCreate
                | LlmCapability::EditDelete
                | LlmCapability::EditMove
                | LlmCapability::EditMoveDir
                | LlmCapability::EditOverwrite
        )
    )
}
#[derive(Debug, Clone, Default)]
pub struct StructuredPayload {
    parsed: Option<serde_json::Value>,
    raw_text: String,
}

impl StructuredPayload {
    pub fn parse(raw_text: &str) -> Self {
        let trimmed = raw_text.trim();
        let parsed = if let Some(obj_str) =
            crate::edits::stream::json::decode::first_json_object_str(trimmed)
        {
            serde_json::from_str::<serde_json::Value>(obj_str).ok()
        } else {
            serde_json::from_str::<serde_json::Value>(trimmed).ok()
        };
        Self {
            parsed,
            raw_text: raw_text.to_string(),
        }
    }

    pub fn field(&self, name: &str) -> Option<&serde_json::Value> {
        let val = self.parsed.as_ref()?;
        if let Some(f) = val.get(name) {
            return Some(f);
        }
        if let Some(action) = val.get("action").and_then(|a| a.as_object()) {
            if let Some(f) = action.get(name) {
                return Some(f);
            }
        }
        None
    }
    /// Returns the decoded JSON root of the response, when the text parsed as
    /// a JSON object.
    ///
    /// Module decoders receive this root, so they read the same object the
    /// strict schema constrained; a payload that is not JSON owns no field
    /// and decodes to no command.
    pub fn root(&self) -> Option<&serde_json::Value> {
        self.parsed.as_ref()
    }
    pub fn to_proposal(&self) -> Option<ModelResultProposal> {
        let val = self.parsed.as_ref()?;
        let mut proposal = ModelResultProposal::default();
        if let Some(message) = conversational_message(val) {
            proposal.message = message.to_string();
        }
        if let Some(changes_val) = self.field("changes") {
            if let Ok(changes) =
                serde_json::from_value::<Vec<StructuredChangeProposal>>(changes_val.clone())
            {
                proposal.changes = changes;
            }
        }
        Some(proposal)
    }

    pub fn clean_text(&self) -> String {
        let trimmed = self.raw_text.trim();
        let mut messages = Vec::new();
        let mut rest = trimmed;
        let mut all_json = true;
        while !rest.is_empty() {
            let rest_trimmed = rest.trim_start();
            if !rest_trimmed.starts_with('{') {
                all_json = false;
                break;
            }
            if let Some(obj_str) =
                crate::edits::stream::json::decode::first_json_object_str(rest_trimmed)
            {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(obj_str) {
                    if let Some(message) = conversational_message(&val) {
                        messages.push(message.to_string());
                    }
                    rest = &rest_trimmed[obj_str.len()..];
                } else {
                    all_json = false;
                    break;
                }
            } else {
                all_json = false;
                break;
            }
        }
        if all_json && (!messages.is_empty() || trimmed.starts_with('{')) {
            return messages.join("\n\n");
        }
        if let Some(val) = &self.parsed {
            if let Some(message) = conversational_message(val) {
                if trimmed.starts_with('{') && trimmed.ends_with('}') {
                    return message.to_string();
                }
            }
        }
        self.raw_text.clone()
    }
}

/// Reads the conversational channel a single response object declares.
///
/// The text channel is declared inside the operational branch a round selects,
/// so the branch field is read first and the response root second, which keeps
/// a payload written against the previous wire shape readable.
fn message_channel(node: &serde_json::Value) -> Option<&str> {
    node.get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|message| !message.trim().is_empty())
}

fn conversational_message(value: &serde_json::Value) -> Option<&str> {
    value
        .get("action")
        .and_then(message_channel)
        .or_else(|| message_channel(value))
}

/// Resolves one structured change into a concrete, validated edit operation.
///
/// The target path is resolved against the permitted files, the payload is
/// normalized, and a move destination is resolved exactly like its source. Both
/// wire vehicles — the structured `changes` field and the anchored change-set
/// tag of the XML contour — travel through this routine, so a change is
/// validated identically whichever encoding carried it.
///
/// A directory move is the one entry that names no file of the context block: a
/// directory is never rendered as one, so its source is only normalized here
/// and the collisions of the subtree it fans out into are decided while the
/// change set is staged.
fn change_to_operation(
    change: &StructuredChangeProposal,
    editable_paths: &[String],
) -> Result<EditOperation, String> {
    let op = change.parsed_op()?;
    let is_create = op == EditFileOp::Create;
    // Resolving a directory against the permitted files would let a suffix of
    // a context path stand in for the directory the model named, so the source
    // of a directory move is normalized instead of resolved.
    let moves_directory = op == EditFileOp::MoveDir;
    let target_path = if moves_directory {
        tauqe_protocol::edit::normalize_repo_path(&change.path)?
    } else if is_create {
        resolve_target_path_for_op(&change.path, editable_paths, true)?
    } else {
        resolve_context_target(&change.path, editable_paths)?
    };

    let mut resolved = change.clone();
    resolved.path = target_path;
    resolved.old_text = normalize_content(&change.old_text);
    resolved.new_text = normalize_content(&change.new_text);
    resolved.content = normalize_content(&change.content);

    if op == EditFileOp::Move && !change.to.is_empty() {
        resolved.to = resolve_target_path_for_op(&change.to, editable_paths, true)?;
    }

    resolved.to_edit_operation()
}

/// Resolves the edit commands of a decoded round against the working context.
///
/// The file-edits capability module decodes the change set into edit commands
/// whose paths are normalized but not yet resolved, because the module holds
/// no working context of its own. The caller that owns the context paths hands
/// the decoded commands here: every edit command receives the target
/// resolution and the context-membership check, while every other command
/// passes through unchanged, so a protocol diagnostic survives resolution.
pub fn resolve_decoded_edits(
    commands: Vec<LlmCommand>,
    editable_paths: &[String],
) -> Result<Vec<LlmCommand>, String> {
    let mut resolved = Vec::with_capacity(commands.len());
    for command in commands {
        if is_edit_capability(command.capability()) {
            if let Some(operation) = command.into_edit_operation() {
                let op = resolve_operation_target(operation, editable_paths)?;
                resolved.push(match op {
                    EditOperation::Replace(op) => LlmCommand::EditReplace(op),
                    EditOperation::Create(op) => LlmCommand::EditCreate(op),
                    EditOperation::Delete(op) => LlmCommand::EditDelete(op),
                    EditOperation::Move(op) => LlmCommand::EditMove(op),
                    EditOperation::MoveDir(op) => LlmCommand::EditMoveDir(op),
                    EditOperation::Overwrite(op) => LlmCommand::EditOverwrite(op),
                    EditOperation::LineAnchor(op) => LlmCommand::EditLineAnchor(op),
                });
            }
        } else {
            resolved.push(command);
        }
    }
    Ok(resolved)
}

/// Resolves the target paths of one edit operation against the working context.
///
/// A `create` names a file the working context does not hold yet and a
/// directory move names no file of the context block at all, so both keep the
/// normalized paths the wire boundary produced; every other operation has its
/// target resolved and checked for context membership, and a move has its
/// destination resolved exactly like its source.
fn resolve_operation_target(
    operation: EditOperation,
    editable_paths: &[String],
) -> Result<EditOperation, String> {
    match operation {
        EditOperation::Create(op) => Ok(EditOperation::Create(op)),
        EditOperation::MoveDir(op) => Ok(EditOperation::MoveDir(op)),
        EditOperation::Move(EditMove { from, to }) => Ok(EditOperation::Move(EditMove {
            from: resolve_context_target(&from, editable_paths)?,
            to: resolve_target_path_for_op(&to, editable_paths, true)?,
        })),
        EditOperation::Replace(EditReplace {
            path,
            old_text,
            new_text,
        }) => Ok(EditOperation::Replace(EditReplace {
            path: resolve_context_target(&path, editable_paths)?,
            old_text,
            new_text,
        })),
        EditOperation::Overwrite(EditOverwrite { path, content }) => {
            Ok(EditOperation::Overwrite(EditOverwrite {
                path: resolve_context_target(&path, editable_paths)?,
                content,
            }))
        }
        EditOperation::Delete(EditDelete { path }) => Ok(EditOperation::Delete(EditDelete {
            path: resolve_context_target(&path, editable_paths)?,
        })),
        EditOperation::LineAnchor(EditLineAnchor {
            path,
            start,
            end,
            content,
        }) => Ok(EditOperation::LineAnchor(EditLineAnchor {
            path: resolve_context_target(&path, editable_paths)?,
            start,
            end,
            content,
        })),
    }
}

/// Resolves one file target against the permitted files and requires the
/// working context to hold it.
fn resolve_context_target(candidate: &str, editable_paths: &[String]) -> Result<String, String> {
    let target_path = resolve_target_path_for_op(candidate, editable_paths, false)?;
    if !editable_paths.is_empty() && !editable_paths.contains(&target_path) {
        return Err(format!(
            "File '{}' is not present in context. To edit it, request it first via context_requests. Permitted files in context: {}",
            target_path,
            editable_paths.join(", ")
        ));
    }
    Ok(target_path)
}

/// Decodes an anchored change set from its JSON array.
///
/// The anchored vehicle carries the very entries the structured contour declares
/// in `changes`, so one conversion and one set of path rules serve both
/// transports: the XML contour hands in the body of its change-set tag and the
/// structured contour hands in the `changes` field. Every entry is resolved and
/// validated like a structured change, so a file outside the permitted set or an
/// operation missing its payload is reported as a diagnostic instead of reaching
/// the staging engine.
pub fn parse_anchor_edits(
    body: &str,
    editable_paths: &[String],
) -> Result<Vec<EditOperation>, String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let changes: Vec<StructuredChangeProposal> = serde_json::from_str(trimmed)
        .map_err(|error| format!("invalid anchored change set: {error}"))?;
    changes
        .iter()
        .map(|change| change_to_operation(change, editable_paths))
        .collect()
}

/// Cleans raw text or JSON into conversational text without raw schema markup.
///
/// Every control tag is removed by the single parametric sanitizer, and a
/// payload that is itself one structured response collapses to its `message`
/// channel, so neither XML control tags nor JSON scaffolding can leak into
/// conversational text, session history, or the streamed interface.
pub fn clean_raw_json_or_text(raw: &str) -> String {
    StructuredPayload::parse(raw).clean_text()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the render context a schema composition uses when the project
    /// declares no commands.
    fn render_context(patch_strategy: PatchStrategy) -> PromptRenderContext<'static> {
        PromptRenderContext::new(&[], None).with_patch_strategy(patch_strategy)
    }

    /// Builds the capability tree of the interactive REPL profile: the tree
    /// every test below decodes a response against.
    fn repl_expr() -> CapabilityExpr {
        crate::control::repl::dialogue_capability_expr(false, false)
    }

    /// Lifts the branch envelope of a composed schema into one surface.
    ///
    /// The wire keeps the operational branches mutually exclusive, so a field of
    /// a branch is addressed through the envelope that owns it. A test that
    /// asserts the shape of a field reads the surface after this lift, so it
    /// names the field rather than the branch that carries it.
    fn flattened_surface(schema: &serde_json::Value) -> serde_json::Value {
        let mut objects = vec![schema];
        if let Some(branches) = schema["properties"]["action"]["anyOf"].as_array() {
            objects.extend(branches.iter());
        }
        let mut properties = serde_json::Map::new();
        let mut required: Vec<serde_json::Value> = Vec::new();
        for object in objects {
            let Some(fields) = object["properties"].as_object() else {
                continue;
            };
            for (name, field) in fields {
                if name.as_str() == "action" || name.as_str() == "kind" {
                    continue;
                }
                // A capability two branches share — the terminal outcome, for
                // instance — is one property of the lifted surface, so its
                // required entry appears exactly once.
                if properties.contains_key(name) {
                    continue;
                }
                properties.insert(name.clone(), field.clone());
                required.push(serde_json::Value::String(name.clone()));
            }
        }
        let mut flat = serde_json::json!({
            "type": "object",
            "properties": serde_json::Value::Object(properties),
            "required": required,
            "additionalProperties": false
        });
        if let Some(definitions) = schema.get("$defs") {
            flat["$defs"] = definitions.clone();
        }
        flat
    }
    #[test]
    fn test_structured_protocol_parse_replace() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        let json = r#"{
            "message": "Update greeting function",
            "action": {
                "kind": "edit",
                "changes": [
                    {
                        "op": "replace",
                        "path": "src/lib.rs",
                        "old_text": "fn old() {}",
                        "new_text": "fn new() {}"
                    }
                ]
            }
        }"#;
        let res = proto.parse_output(&repl_expr(), json, &editable);
        match res {
            ModelResult::Edit {
                summary,
                edits,
                proposal,
                ..
            } => {
                assert_eq!(summary, "Update greeting function");
                assert_eq!(edits.len(), 1);
                assert!(proposal.is_some());
                match &edits[0] {
                    EditOperation::Replace(EditReplace {
                        path,
                        old_text,
                        new_text,
                    }) => {
                        assert_eq!(path, "src/lib.rs");
                        assert_eq!(old_text, "fn old() {}");
                        assert_eq!(new_text, "fn new() {}");
                    }
                    _ => panic!("Expected Replace operation"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_protocol_utf8_long_summary_truncation() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        // A long Cyrillic string exceeding 100 characters where byte 97 falls inside a multibyte char
        let cyrillic_msg = "Я обновлю реализацию функции валидации пути и добавлю расширенную обработку возможных ошибок для обеспечения безопасности";
        let json = serde_json::json!({
            "message": cyrillic_msg,
            "action": {
                "kind": "edit",
                "changes": [
                    {
                        "op": "replace",
                        "path": "src/lib.rs",
                        "old_text": "a",
                        "new_text": "b",
                        "content": null
                    }
                ]
            }
        })
        .to_string();
        let res = proto.parse_output(&repl_expr(), &json, &editable);
        match res {
            ModelResult::Edit { summary, .. } => {
                assert!(summary.ends_with("..."));
                assert!(summary.chars().count() <= 100);
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_schema_has_no_nullable_types() {
        let schema = flattened_surface(&StructuredEditProtocol::schema(&render_context(
            PatchStrategy::SearchReplace,
        )));
        // Only the idle representation of the plan payload is nullable; every
        // edit field stays a concrete typed value.
        let changes = &schema["properties"]["changes"]["items"];
        assert!(!changes.to_string().contains("\"null\""));
        assert_eq!(changes["additionalProperties"], serde_json::json!(false));
        let required = changes["required"].as_array().unwrap();
        assert_eq!(required.len(), 6);
    }

    #[test]
    fn test_structured_schema_and_instructions_include_overwrite() {
        let schema = flattened_surface(&StructuredEditProtocol::schema(&render_context(
            PatchStrategy::SearchReplace,
        )));
        let ops = schema["properties"]["changes"]["items"]["properties"]["op"]["enum"]
            .as_array()
            .unwrap();
        assert!(ops.iter().any(|op| op == "overwrite"));

        let instr = StructuredEditProtocol.system_instructions(&[], PatchStrategy::SearchReplace);
        assert!(instr.contains("overwrite"));
    }

    #[test]
    fn test_structured_protocol_empty_string_fields() {
        let proto = StructuredEditProtocol;
        let editable = vec![];
        let json = r#"{
            "message": "Create file",
            "action": {
                "kind": "edit",
                "changes": [
                    {"op": "create", "path": "src/new.rs", "old_text": "", "new_text": "", "content": "fn x() {}\n"}
                ]
            }
        }"#;
        let res = proto.parse_output(&repl_expr(), json, &editable);
        match res {
            ModelResult::Edit { edits, .. } => match &edits[0] {
                EditOperation::Create(EditCreate { path, content }) => {
                    assert_eq!(path, "src/new.rs");
                    assert_eq!(content, "fn x() {}\n");
                }
                _ => panic!("Expected Create"),
            },
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_protocol_changes_before_message_with_wrapper_text() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        let raw = "Here you go:\n{\"action\":{\"kind\":\"edit\",\"changes\":[{\"op\":\"replace\",\"path\":\"src/lib.rs\",\"old_text\":\"a {\",\"new_text\":\"b {\",\"content\":\"\"}]},\"message\":\"Swap a for b\"}\nThanks!";
        let res = proto.parse_output(&repl_expr(), raw, &editable);
        match res {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Swap a for b");
                assert_eq!(edits.len(), 1);
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_structured_protocol_parse_answer() {
        let proto = StructuredEditProtocol;
        let editable = vec![];
        let json = r#"{
            "message": "Hello there! How can I help you?",
            "action": {"kind": "answer"}
        }"#;
        let res = proto.parse_output(&repl_expr(), json, &editable);
        match res {
            ModelResult::Answer { text } => {
                assert_eq!(text, "Hello there! How can I help you?");
            }
            _ => panic!("Expected ModelResult::Answer"),
        }
    }

    #[test]
    fn test_structured_system_instructions_respect_the_capability_set() {
        let full = StructuredEditProtocol.system_instructions(&[], PatchStrategy::SearchReplace);
        assert!(full.contains("context_requests"));
        assert!(full.contains("overwrite"));

        let narrowed = StructuredEditProtocol.system_instructions_for(
            &CapabilityExpr::from_leaves([LlmCapability::TextMessage, LlmCapability::EditReplace]),
            &[],
            PatchStrategy::SearchReplace,
        );
        assert!(narrowed.contains("changes"));
        assert!(!narrowed.contains("context_requests"));
        assert!(!narrowed.contains("repo_searches"));
    }

    #[test]
    fn test_structured_protocol_parses_an_anchored_change_set() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];
        let json = r#"{
            "message": "Replace the guarded branch",
            "action": {
                "kind": "edit",
                "changes": [{
                    "op": "replace",
                    "path": "src/lib.rs",
                    "to": "",
                    "start": "Kf9Q",
                    "end": "Tq2W",
                    "content": "fn replacement() {}\n"
                }]
            }
        }"#;
        match proto.parse_output(&repl_expr(), json, &editable) {
            ModelResult::Edit { edits, outcome, .. } => {
                assert!(outcome.error.is_none(), "{:?}", outcome.error);
                match edits.as_slice() {
                    [EditOperation::LineAnchor(EditLineAnchor {
                        path,
                        start,
                        end,
                        content,
                    })] => {
                        assert_eq!(path, "src/lib.rs");
                        assert_eq!(start, "Kf9Q");
                        assert_eq!(end, "Tq2W");
                        assert_eq!(content, "fn replacement() {}\n");
                    }
                    other => panic!("expected one anchored replacement, got {other:?}"),
                }
            }
            other => panic!("expected anchored edits, got {other:?}"),
        }
    }
    #[test]
    fn test_structured_protocol_parses_a_directory_move() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/old".to_string()];
        let json = r#"{
            "message": "Rename the module directory",
            "action": {
                "kind": "edit",
                "changes": [{
                    "op": "move_dir",
                    "path": "src/old",
                    "to": "src/new"
                }]
            }
        }"#;
        // A directory is never a file of the context block, so the move is
        // resolved as two directory paths instead of a context membership.
        match proto.parse_output(&repl_expr(), json, &editable) {
            ModelResult::Edit { edits, outcome, .. } => {
                assert!(outcome.error.is_none(), "{:?}", outcome.error);
                assert_eq!(
                    edits,
                    vec![EditOperation::MoveDir(EditMoveDir {
                        from: "src/old".to_string(),
                        to: "src/new".to_string(),
                    })]
                );
            }
            other => panic!("expected a directory move, got {other:?}"),
        }
    }
    #[test]
    fn test_anchor_change_set_vehicle_decodes_shared_entries() {
        let editable = vec!["src/lib.rs".to_string()];
        let body = r#"[{"op":"replace","path":"src/lib.rs","to":"","start":"Kf9Q","end":"Kf9Q","content":"x\n"}]"#;
        let operations = parse_anchor_edits(body, &editable).expect("valid change set");
        assert!(matches!(
            operations.as_slice(),
            [EditOperation::LineAnchor(EditLineAnchor { start, end, .. })] if start == "Kf9Q" && end == "Kf9Q"
        ));

        // A change that addresses no file of the context is reported instead of
        // reaching the staging engine.
        assert!(
            parse_anchor_edits(r#"[{"op":"replace","path":"../escape.rs"}]"#, &editable).is_err()
        );

        // An operation missing its payload, and a body that is not JSON at all,
        // both stay diagnostics.
        assert!(parse_anchor_edits(r#"[{"op":"create","path":"src/new.rs"}]"#, &editable).is_err());
        assert!(parse_anchor_edits("not json", &editable).is_err());

        // An empty vehicle requests no change at all.
        assert!(parse_anchor_edits("   ", &editable)
            .expect("an empty vehicle")
            .is_empty());
    }

    #[test]
    fn test_anchored_schema_declares_range_fields_instead_of_echoed_text() {
        let schema = flattened_surface(&StructuredEditProtocol::schema(&render_context(
            PatchStrategy::LineAnchor,
        )));
        let items = &schema["properties"]["changes"]["items"];
        let properties = items["properties"].as_object().expect("properties object");
        for field in ["op", "path", "to", "start", "end", "content"] {
            assert!(properties.contains_key(field), "missing {field}");
        }
        for field in ["old_text", "new_text"] {
            assert!(!properties.contains_key(field), "{field} must be absent");
        }
        let mut required: Vec<&str> = items["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        required.sort();
        assert_eq!(
            required,
            vec!["content", "end", "op", "path", "start", "to"]
        );
        assert_eq!(items["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn test_structured_protocol_system_instructions() {
        let proto = StructuredEditProtocol;
        let paths = vec!["src/lib.rs".to_string()];

        let instr = proto.system_instructions(&paths, PatchStrategy::SearchReplace);
        assert!(!instr.contains("context_drops"));
        assert!(instr.contains("context_requests"));
    }

    #[test]
    fn test_structured_protocol_multibyte_utf8_slicing_safety() {
        let proto = StructuredEditProtocol;
        let editable = vec!["src/lib.rs".to_string()];

        // Surrounding text contains multi-byte Cyrillic, emoji, and escaped characters
        // directly adjacent to the JSON structure boundaries.
        let raw = "Привет, мир! 🚀 Тестируем UTF-8 границы: {\"message\":\"Всё отлично! ✨\",\"action\":{\"kind\":\"edit\",\"changes\":[{\"op\":\"replace\",\"path\":\"src/lib.rs\",\"old_text\":\"старый\",\"new_text\":\"новый\",\"to\":\"\",\"content\":\"\"}]}} Конец сообщения. 🦀";
        let res = proto.parse_output(&repl_expr(), raw, &editable);
        match res {
            ModelResult::Edit { summary, edits, .. } => {
                assert_eq!(summary, "Всё отлично! ✨");
                assert_eq!(edits.len(), 1);
                match &edits[0] {
                    EditOperation::Replace(EditReplace {
                        old_text, new_text, ..
                    }) => {
                        assert_eq!(old_text, "старый");
                        assert_eq!(new_text, "новый");
                    }
                    _ => panic!("Expected Replace operation"),
                }
            }
            _ => panic!("Expected ModelResult::Edit"),
        }
    }

    #[test]
    fn test_clean_collapses_a_silent_structured_round() {
        // A structured response whose only conversational channel is empty has
        // no prose to recover. Its JSON scaffolding is protocol chrome, never
        // an answer: collapsing it keeps raw wire markup out of session history
        // and matches what the streaming filter showed the interface (nothing).
        let silent = r#"{
            "message": "",
            "changes": [],
            "context_requests": ["src/a.rs"],
            "verify": {"target": "none", "on_success": "silent"},
            "commit_message": "",
            "reason": ""
        }"#;
        assert_eq!(clean_raw_json_or_text(silent), "");

        // Fences are not stripped in strict structured mode.
    }

    #[test]
    fn test_clean_keeps_prose_that_quotes_a_json_object() {
        // Prose that merely quotes a JSON object is conversational text: the
        // object does not span the whole payload, so the structured collapse
        // never fires and the explanation survives verbatim.
        let prose = "A silent round is {\"message\":\"\",\"changes\":[]} in the journal.";
        assert_eq!(clean_raw_json_or_text(prose), prose);
    }

    #[test]
    fn test_clean_joins_the_channels_of_concatenated_responses() {
        // A turn concatenates the rounds it received, so one payload may carry
        // several responses. Their conversational channels are joined in
        // document order instead of only the first one surviving cleaning.
        let rounds = concat!(
            "{\"message\":\"Need file a.rs\",\"changes\":[]}\n\n",
            "{\"message\":\"Updated file a.rs\",\"changes\":[]}",
        );
        assert_eq!(
            clean_raw_json_or_text(rounds),
            "Need file a.rs\n\nUpdated file a.rs"
        );

        // A silent round contributes no channel of its own and no separator.
        let silent_then_answer = concat!(
            "{\"message\":\"\",\"changes\":[]}\n\n",
            "{\"message\":\"Updated file a.rs\",\"changes\":[]}",
        );
        assert_eq!(
            clean_raw_json_or_text(silent_then_answer),
            "Updated file a.rs"
        );
    }

    #[test]
    fn test_structured_schema_declares_control_fields() {
        let schema = flattened_surface(&StructuredEditProtocol::schema(&render_context(
            PatchStrategy::SearchReplace,
        )));
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        for field in [
            "context_requests",
            "repo_searches",
            "repo_dirs",
            "repo_maps",
            "dir_lists",
            "tauqe_doc_requests",
            "outer_doc_requests",
            "scratchpad",
            "commit_message",
            "reason",
            "blocked",
            "abort",
        ] {
            assert!(
                required.contains(&field),
                "field '{field}' must be required in the schema"
            );
            assert!(
                schema["properties"].get(field).is_some(),
                "field '{field}' must be declared in properties"
            );
        }

        assert_eq!(schema["properties"]["commit_message"]["type"], "string");
        assert_eq!(schema["properties"]["message"]["type"], "string");
        assert_eq!(schema["properties"]["reason"]["type"], "string");
        assert_eq!(
            schema["properties"]["blocked"]["properties"]["is_blocked"]["type"],
            "boolean"
        );
        assert_eq!(schema["properties"]["context_requests"]["type"], "array");
        // The strict control objects carry no nullable type; the plan payload
        // is the only property whose idle form is `null`.
        assert!(!schema["properties"]["blocked"]
            .to_string()
            .contains("\"null\""));
    }

    #[test]
    fn the_composed_instructions_never_restate_the_response_fields() {
        let expr = repl_expr();
        let instructions = compose_structured_protocol_instructions(
            &expr,
            &render_context(PatchStrategy::SearchReplace),
        );

        // The schema declares every field with its type, its required-ness, and
        // its enum, so the instructions never restate one as a field reference.
        for field in [
            "message",
            "context_requests",
            "repo_searches",
            "repo_dirs",
            "repo_maps",
            "dir_lists",
            "tauqe_doc_requests",
            "outer_doc_requests",
            "scratchpad",
            "commit_message",
            "error_message",
            "reason",
            "blocked",
            "abort",
            "verify",
            "plan_create",
            "plan_update",
            "review_update",
            "custom_tools",
        ] {
            assert!(
                !instructions.contains(&format!("- `{field}`:")),
                "the instructions restate the schema field `{field}`: {instructions}"
            );
        }
        // The access enum of a discovery request belongs to the schema as well.
        assert!(!instructions.contains("'read_only'|'editable'"));

        // The operational mechanics of a change set survive the cleanup: the
        // schema cannot state how a range addresses the lines it replaces.
        assert!(instructions.contains("`changes`"));
        let anchored = compose_structured_protocol_instructions(
            &expr,
            &render_context(PatchStrategy::LineAnchor),
        );
        assert!(anchored.contains("copied character for character"));
    }

    #[test]
    fn test_structured_protocol_parse_scratchpad() {
        let proto = StructuredEditProtocol;
        let json = r#"{
            "message": "Need docs",
            "scratchpad": "Verified: parser fixed."
        }"#;
        assert_eq!(
            proto.parse_scratchpad(json).as_deref(),
            Some("Verified: parser fixed.")
        );

        let empty = r#"{"message":"Nothing","scratchpad":""}"#;
        assert_eq!(proto.parse_scratchpad(empty), None);
    }
    #[test]
    fn test_structured_schema_declares_terminal_fields() {
        let schema = flattened_surface(&StructuredEditProtocol::schema(&render_context(
            PatchStrategy::SearchReplace,
        )));
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert!(required.contains(&"commit_message"));
        assert!(required.contains(&"reason"));
        assert_eq!(schema["properties"]["commit_message"]["type"], "string");
        assert_eq!(schema["properties"]["reason"]["type"], "string");
    }

    #[test]
    fn test_structured_schema_declares_verify() {
        let schema = flattened_surface(&StructuredEditProtocol::schema(&render_context(
            PatchStrategy::SearchReplace,
        )));
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert!(required.contains(&"verify"));
    }
    #[test]
    fn test_structured_schema_reflects_the_capability_set() {
        let full = flattened_surface(&StructuredEditProtocol::schema(&render_context(
            PatchStrategy::SearchReplace,
        )));
        let full_properties = full["properties"].as_object().expect("properties object");
        for field in [
            "message",
            "changes",
            "context_requests",
            "repo_searches",
            "repo_dirs",
            "repo_maps",
            "dir_lists",
            "tauqe_doc_requests",
            "outer_doc_requests",
            "scratchpad",
            "commit_message",
            "reason",
            "blocked",
            "abort",
            "verify",
        ] {
            assert!(
                full_properties.contains_key(field),
                "the development schema must declare '{field}'"
            );
        }

        // `suggested_actions` belongs to no capability, so the registry never
        // emits it: the schema advertises only the enabled protocol surface.
        assert!(!full_properties.contains_key("suggested_actions"));
        assert_eq!(
            full["required"].as_array().map(|required| required.len()),
            Some(full_properties.len()),
            "a strict schema must require every declared property"
        );

        let narrowed = StructuredEditProtocol::schema_for(
            &CapabilityExpr::from_leaves([LlmCapability::TextMessage, LlmCapability::EditReplace]),
            &render_context(PatchStrategy::SearchReplace),
        );
        let properties = narrowed["properties"]
            .as_object()
            .expect("properties object");
        assert!(properties.contains_key("message"));
        assert!(properties.contains_key("changes"));
        for field in [
            "context_requests",
            "repo_searches",
            "repo_dirs",
            "repo_maps",
            "dir_lists",
            "tauqe_doc_requests",
            "outer_doc_requests",
            "scratchpad",
            "commit_message",
            "reason",
            "blocked",
            "abort",
            "verify",
            "plan_update",
            "custom_tools",
        ] {
            assert!(
                !properties.contains_key(field),
                "a disabled capability must not advertise '{field}'"
            );
        }

        let required: Vec<&str> = narrowed["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|field| field.as_str())
            .collect();
        assert_eq!(
            required.len(),
            properties.len(),
            "pruning a property must also prune its required entry"
        );
        assert_eq!(narrowed["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn test_structured_response_format_follows_the_capability_set() {
        let narrowed = CapabilityExpr::from_leaves([LlmCapability::TextMessage]);
        match StructuredEditProtocol
            .response_format_for(&narrowed, &render_context(PatchStrategy::SearchReplace))
        {
            Some(ResponseFormat::JsonSchema { json_schema, .. }) => {
                let properties = json_schema.schema["properties"]
                    .as_object()
                    .expect("properties object");
                assert!(properties.contains_key("message"));
                assert!(!properties.contains_key("changes"));
                assert!(!properties.contains_key("verify"));
            }
            other => panic!("expected a JSON schema response format, got {other:?}"),
        }
    }
    #[test]
    fn test_review_update_field_follows_the_review_discussion_surface() {
        let review_schema = flattened_surface(&StructuredEditProtocol::schema_for(
            &crate::control::repl::dialogue_capability_expr(false, true),
            &render_context(PatchStrategy::SearchReplace),
        ));
        assert!(review_schema["properties"].get("review_update").is_some());

        let repl_schema = flattened_surface(&StructuredEditProtocol::schema(&render_context(
            PatchStrategy::SearchReplace,
        )));
        assert!(repl_schema["properties"].get("review_update").is_none());
    }
    #[test]
    fn the_surface_declares_the_operational_branches_as_one_exclusive_envelope() {
        let schema = StructuredEditProtocol::schema(&render_context(PatchStrategy::SearchReplace));
        let branches = schema["properties"]["action"]["anyOf"]
            .as_array()
            .expect("an exclusive surface declares the branch envelope");
        assert_eq!(branches.len(), 6);

        // The branch is named by a discriminator the schema requires, so a
        // response states which one it follows before it emits a command.
        let kinds: Vec<&str> = branches
            .iter()
            .filter_map(|branch| branch["properties"]["kind"]["enum"][0].as_str())
            .collect();
        assert_eq!(
            kinds,
            vec!["research", "edit", "plan", "answer", "blocked", "abort"]
        );

        // The discovery commands, the change set, and the plan payload live in
        // different branches, so no response can carry two of them and a round
        // that patches a working tree and rewrites a plan is not expressible.
        let research = &branches[0];
        let edit = &branches[1];
        let plan = &branches[2];
        let answer = &branches[3];
        let blocked = &branches[4];
        let abort = &branches[5];
        assert!(research["properties"].get("context_requests").is_some());
        assert!(research["properties"].get("repo_searches").is_some());
        assert!(research["properties"].get("repo_dirs").is_some());
        assert!(research["properties"].get("repo_maps").is_some());
        assert!(research["properties"].get("dir_lists").is_some());
        assert!(research["properties"].get("tauqe_doc_requests").is_some());
        assert!(research["properties"].get("outer_doc_requests").is_some());
        assert!(research["properties"].get("changes").is_none());
        assert!(research["properties"].get("plan_update").is_none());
        assert!(edit["properties"].get("changes").is_some());
        assert!(edit["properties"].get("context_requests").is_none());
        assert!(edit["properties"].get("plan_update").is_none());
        assert!(plan["properties"].get("plan_create").is_some());
        assert!(plan["properties"].get("changes").is_none());
        assert!(plan["properties"].get("context_requests").is_none());

        // The answer branch carries the conversational channel and no other
        // command, so a conversational round names the branch it follows and
        // declares its message inside it.
        assert_eq!(answer["required"], serde_json::json!(["kind", "message"]));

        // The terminal signals own their branches instead of riding beside a
        // change set or a plan payload.
        assert!(blocked["properties"].get("blocked").is_some());
        assert!(blocked["properties"].get("changes").is_none());
        assert!(blocked["properties"].get("commit_message").is_none());
        assert!(blocked["properties"].get("reason").is_none());
        assert!(abort["properties"].get("abort").is_some());
        assert!(abort["properties"].get("changes").is_none());
        assert!(abort["properties"].get("commit_message").is_none());
        assert!(abort["properties"].get("reason").is_none());

        // The terminal outcome stays with the branches that act, while the
        // signals that end the turn live in branches of their own.
        for acting in [edit, plan] {
            assert!(acting["properties"].get("commit_message").is_some());
            assert!(acting["properties"].get("message").is_some());
            assert!(acting["properties"].get("reason").is_some());
            assert!(acting["properties"].get("blocked").is_none());
            assert!(acting["properties"].get("abort").is_none());
        }
        // Every object in the envelope is strict: additionalProperties is false, every
        // declared field is required, and no field the surface does not declare is accepted.
        for object in std::iter::once(&schema).chain(branches.iter()) {
            assert_eq!(object["additionalProperties"], serde_json::json!(false));
            let mut required: Vec<&str> = object["required"]
                .as_array()
                .expect("a strict object requires its fields")
                .iter()
                .filter_map(|field| field.as_str())
                .collect();
            required.sort();
            let mut declared: Vec<&str> = object["properties"]
                .as_object()
                .expect("a strict object declares its fields")
                .keys()
                .map(String::as_str)
                .collect();
            declared.sort();
            assert_eq!(required, declared);
        }
    }
}
