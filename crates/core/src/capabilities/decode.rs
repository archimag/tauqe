//! Tree-walking decoder for structured LLM responses.
//!
//! [`decode_capability_expr`] mirrors [`super::schema::capability_expr_schema`]:
//! it walks the [`CapabilityExpr`] tree synchronously with the JSON response
//! tree so every leaf reads its property from the exact object level the
//! composed schema declared.

use crate::config::project::ProjectConfig;
use crate::edits::normalize_repo_path;
use crate::edits::EditFileOp;
use crate::edits::StructuredChangeProposal;
use crate::llm::discussion::normalize_repo_map_path;
use crate::llm::discussion::DirListRequest;
use crate::llm::discussion::DiscussionContextRequest;
use crate::llm::discussion::RepoSearchRequest;
use crate::llm::discussion::ReviewUpdateProposal;
use crate::llm::discussion::{DiscussionPlanAction, DiscussionPlanUpdate};
use crate::llm::CapabilityTree;
use crate::llm::DocTopic;
use crate::llm::LlmCapability;
use crate::llm::{
    AbortCommand, BlockedCommand, CommitMessageCommand, ContextRequestCommand, CustomToolCommand,
    DirListCommand, DocRequestCommand, LlmCommand, OuterDocRequestCommand, RepoDirsCommand,
    RepoMapCommand, RepoSearchCommand, ScratchpadCommand, TextCommand, TurnErrorCommand,
    TurnSkipCommand,
};
use crate::workflow::PatchStrategy;
use serde::Deserialize;
use serde_json::Value;

use crate::capabilities::PromptRenderContext;
use crate::edits::protocol::utils::normalize_content;
use crate::edits::protocol::ContextRequest;

/// Decodes a structured JSON response by walking `expr` synchronously with
/// `node`.
pub(crate) fn decode_capability_expr(
    expr: &CapabilityTree,
    node: &Value,
    ctx: &PromptRenderContext<'_>,
) -> Vec<LlmCommand> {
    match expr {
        CapabilityTree::Leaf(cap) => decode_leaf(*cap, node, ctx),
        CapabilityTree::Set(_, items) => {
            let mut commands = Vec::new();
            for item in items {
                for cmd in decode_capability_expr(item, node, ctx) {
                    if matches!(cmd, LlmCommand::ProtocolError { .. }) && commands.contains(&cmd) {
                        continue;
                    }
                    commands.push(cmd);
                }
            }
            commands
        }
        CapabilityTree::AnyOf(label, branches) => {
            if branches.is_empty() {
                return Vec::new();
            }
            if branches.len() == 1 {
                return decode_capability_expr(&branches[0], node, ctx);
            }
            // A branch that carries no name takes part in the level that holds
            // the union, exactly as the schema places it there.
            let mut commands = Vec::new();
            for branch in branches {
                if branch.label().is_some_and(|kind| !kind.is_empty()) {
                    continue;
                }
                commands.extend(decode_capability_expr(branch, node, ctx));
            }
            let Some(branch_node) = node.get(label).filter(|value| value.is_object()) else {
                commands.push(LlmCommand::ProtocolError {
                    message: format!("missing or invalid '{label}' object for operational branch"),
                });
                return commands;
            };
            let Some(kind) = branch_node.get("kind").and_then(Value::as_str) else {
                commands.push(LlmCommand::ProtocolError {
                    message: format!("missing or non-string '{label}.kind' discriminator"),
                });
                return commands;
            };
            let selected = branches.iter().find(|branch| {
                branch
                    .label()
                    .is_some_and(|branch_kind| !branch_kind.is_empty() && branch_kind == kind)
            });
            let Some(branch) = selected else {
                let expected: Vec<&str> = branches
                    .iter()
                    .filter_map(|branch| branch.label().filter(|kind| !kind.is_empty()))
                    .collect();
                commands.push(LlmCommand::ProtocolError {
                    message: format!(
                        "unknown '{label}.kind' value '{kind}'; expected one of: {}",
                        expected.join(", ")
                    ),
                });
                return commands;
            };
            commands.extend(decode_capability_expr(branch, branch_node, ctx));
            commands
        }
    }
}

fn decode_leaf(cap: LlmCapability, node: &Value, ctx: &PromptRenderContext<'_>) -> Vec<LlmCommand> {
    match cap {
        LlmCapability::TextMessage => {
            let Some(content) = node.get("message").and_then(Value::as_str) else {
                return Vec::new();
            };
            if content.trim().is_empty() {
                return Vec::new();
            }
            vec![LlmCommand::Text(TextCommand {
                content: content.to_string(),
            })]
        }
        LlmCapability::EditReplace if ctx.patch_strategy == PatchStrategy::ApplyPatch => {
            match node.get("patch") {
                // A terminal declaration without a change set still supplies
                // the schema's patch field, but has nothing to stage.
                Some(Value::String(patch))
                    if patch.trim().is_empty()
                        && ["reason", "error_message"].into_iter().any(|field| {
                            node.get(field)
                                .and_then(Value::as_str)
                                .is_some_and(|value| !value.trim().is_empty())
                        }) =>
                {
                    Vec::new()
                }
                Some(Value::String(patch)) => vec![LlmCommand::ApplyPatch {
                    patch: patch.clone(),
                }],
                _ => vec![LlmCommand::ProtocolError {
                    message: "The edit action requires a string patch field.".into(),
                }],
            }
        }
        LlmCapability::EditCreate
        | LlmCapability::EditDelete
        | LlmCapability::EditMove
        | LlmCapability::EditMoveDir
        | LlmCapability::EditOverwrite
            if ctx.patch_strategy == PatchStrategy::ApplyPatch =>
        {
            Vec::new()
        }
        LlmCapability::EditOverwrite if ctx.patch_strategy == PatchStrategy::LineAnchor => {
            Vec::new()
        }
        LlmCapability::EditReplace => decode_changes_for_op(node, &[EditFileOp::Replace]),
        LlmCapability::EditCreate => decode_changes_for_op(node, &[EditFileOp::Create]),
        LlmCapability::EditDelete => decode_changes_for_op(node, &[EditFileOp::Delete]),
        LlmCapability::EditMove => decode_changes_for_op(node, &[EditFileOp::Move]),
        LlmCapability::EditMoveDir => decode_changes_for_op(node, &[EditFileOp::MoveDir]),
        LlmCapability::EditOverwrite => decode_changes_for_op(node, &[EditFileOp::Overwrite]),
        LlmCapability::ContextRequest => decode_context_requests(node),
        LlmCapability::RepoSearch => decode_repo_searches(node),
        LlmCapability::RepoDirs => match node.get("repo_dirs") {
            Some(Value::Bool(true)) => vec![LlmCommand::RepoDirs(RepoDirsCommand {})],
            Some(Value::Bool(false)) | None => Vec::new(),
            _ => vec![research_error("repo_dirs", "expected a boolean")],
        },
        LlmCapability::RepoMap => decode_repo_maps(node),
        LlmCapability::DirList => decode_dir_lists(node),
        LlmCapability::DocRequest => decode_doc_requests(node),
        LlmCapability::OuterDocRequest => decode_outer_doc_requests(node),
        LlmCapability::PlanCreation => {
            decode_plan_property(node, "plan_create", &[DiscussionPlanAction::Save])
        }
        LlmCapability::PlanUpdate => {
            decode_plan_property(node, "plan_update", &[DiscussionPlanAction::Update])
        }
        LlmCapability::ReviewUpdate => decode_review_update(node),
        LlmCapability::Scratchpad => {
            let Some(notes) = node
                .get("scratchpad")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Vec::new();
            };
            vec![LlmCommand::Scratchpad(ScratchpadCommand {
                notes: notes.to_string(),
            })]
        }
        LlmCapability::CommitMessage => {
            let Some(commit_message) = node
                .get("commit_message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Vec::new();
            };
            vec![LlmCommand::CommitMessage(CommitMessageCommand {
                commit_message: commit_message.to_string(),
            })]
        }
        LlmCapability::TurnError => {
            let Some(message) = node
                .get("error_message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Vec::new();
            };
            vec![LlmCommand::TurnError(TurnErrorCommand {
                error_message: message.to_string(),
            })]
        }
        LlmCapability::TurnSkip => {
            let Some(reason) = node
                .get("reason")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Vec::new();
            };
            vec![LlmCommand::TurnSkip(TurnSkipCommand {
                reason: reason.to_string(),
            })]
        }
        LlmCapability::Blocked => decode_blocked(node),
        LlmCapability::Abort => decode_abort(node),
        LlmCapability::CustomTools => {
            if ctx
                .project
                .is_some_and(|project| project.commands.is_empty())
            {
                return Vec::new();
            }
            decode_custom_tools(node, ctx.project)
        }
    }
}

fn decode_changes_for_op(node: &Value, allowed_ops: &[EditFileOp]) -> Vec<LlmCommand> {
    let Some(changes_val) = node.get("changes") else {
        return Vec::new();
    };
    let changes = match serde_json::from_value::<Vec<StructuredChangeProposal>>(changes_val.clone())
    {
        Ok(changes) => changes,
        Err(error) => {
            return vec![LlmCommand::ProtocolError {
                message: format!("changes field could not be decoded: {error}"),
            }];
        }
    };
    let mut commands = Vec::with_capacity(changes.len());
    for change in &changes {
        let mut resolved = change.clone();
        resolved.old_text = normalize_content(&change.old_text);
        resolved.new_text = normalize_content(&change.new_text);
        resolved.content = normalize_content(&change.content);
        match resolved.to_edit_operation_typed().map(LlmCommand::from) {
            Ok(command) => {
                if let Ok(op) = resolved.parsed_op() {
                    if allowed_ops.contains(&op) {
                        commands.push(command);
                    }
                }
            }
            Err(error) => {
                return vec![LlmCommand::ProtocolError {
                    message: format!("changes field could not be decoded: {error}"),
                }];
            }
        }
    }
    commands
}

fn decode_context_requests(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    let Some(reqs) = research_array(node, "context_requests") else {
        return commands;
    };
    let reqs = match reqs {
        Ok(reqs) => reqs,
        Err(error) => return vec![research_error("context_requests", error)],
    };
    for (index, item) in reqs.iter().enumerate() {
        let parsed = if let Some(spec) = item.as_str() {
            if spec.trim().is_empty() {
                continue;
            }
            DiscussionContextRequest::parse_spec(spec)
                .map(|request| (request.path, request.access))
                .ok_or_else(|| "invalid repository file path".to_string())
        } else {
            match serde_json::from_value::<ContextRequest>(item.clone()) {
                Ok(request) => {
                    normalize_repo_path(&request.path).map(|path| (path, request.access))
                }
                Err(error) => Err(error.to_string()),
            }
        };
        match parsed {
            Ok((path, access)) => {
                commands.push(LlmCommand::ContextRequest(ContextRequestCommand {
                    path,
                    access,
                }))
            }
            Err(error) => {
                return vec![research_error(&format!("context_requests[{index}]"), error)]
            }
        }
    }
    commands
}

fn research_error(field: &str, error: impl std::fmt::Display) -> LlmCommand {
    LlmCommand::ProtocolError {
        message: format!("invalid {field}: {error}"),
    }
}

fn research_array<'a>(node: &'a Value, field: &str) -> Option<Result<&'a Vec<Value>, String>> {
    node.get(field).map(|value| {
        value
            .as_array()
            .ok_or_else(|| format!("{field} must be an array"))
    })
}

fn decode_repo_searches(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    let Some(requests) = research_array(node, "repo_searches") else {
        return commands;
    };
    let requests = match requests {
        Ok(requests) => requests,
        Err(error) => return vec![research_error("repo_searches", error)],
    };
    for (index, item) in requests.iter().enumerate() {
        let request = match serde_json::from_value::<RepoSearchRequest>(item.clone()) {
            Ok(request) => request,
            Err(error) => return vec![research_error(&format!("repo_searches[{index}]"), error)],
        };
        let Some(path) = normalize_repo_map_path(&request.path) else {
            return vec![research_error(
                &format!("repo_searches[{index}].path"),
                "invalid repository path",
            )];
        };
        commands.push(LlmCommand::RepoSearch(RepoSearchCommand {
            path,
            regex: request.regex,
        }));
    }
    commands
}

fn decode_repo_maps(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    let Some(paths) = research_array(node, "repo_maps") else {
        return commands;
    };
    let paths = match paths {
        Ok(paths) => paths,
        Err(error) => return vec![research_error("repo_maps", error)],
    };
    for (index, item) in paths.iter().enumerate() {
        let Some(path) = item.as_str().and_then(normalize_repo_map_path) else {
            return vec![research_error(
                &format!("repo_maps[{index}]"),
                "expected a repository-relative path",
            )];
        };
        commands.push(LlmCommand::RepoMap(RepoMapCommand { path }));
    }
    commands
}

fn decode_dir_lists(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    let Some(requests) = research_array(node, "dir_lists") else {
        return commands;
    };
    let requests = match requests {
        Ok(requests) => requests,
        Err(error) => return vec![research_error("dir_lists", error)],
    };
    for (index, item) in requests.iter().enumerate() {
        let request = match serde_json::from_value::<DirListRequest>(item.clone()) {
            Ok(request) => request,
            Err(error) => return vec![research_error(&format!("dir_lists[{index}]"), error)],
        };
        let Some(request) = request.normalized() else {
            return vec![research_error(
                &format!("dir_lists[{index}].path"),
                "invalid repository path",
            )];
        };
        commands.push(LlmCommand::DirList(DirListCommand {
            path: request.path,
            recursive: request.recursive,
        }));
    }
    commands
}

fn decode_doc_requests(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    let val = node
        .get("tauqe_doc_requests")
        .or_else(|| node.get("doc_requests"));
    if let Some(val) = val {
        let Some(d_val) = val.as_array() else {
            return vec![research_error("tauqe_doc_requests", "expected an array")];
        };
        for (index, item) in d_val.iter().enumerate() {
            if item.is_null() {
                commands.push(LlmCommand::DocRequest(DocRequestCommand {
                    topic: DocTopic::All,
                }));
            } else if let Some(s) = item.as_str() {
                commands.push(LlmCommand::DocRequest(DocRequestCommand {
                    topic: DocTopic::from(s),
                }));
            } else {
                return vec![research_error(
                    &format!("tauqe_doc_requests[{index}]"),
                    "expected a topic string or null",
                )];
            }
        }
    }
    commands
}

fn decode_outer_doc_requests(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    let Some(ids) = research_array(node, "outer_doc_requests") else {
        return commands;
    };
    let ids = match ids {
        Ok(ids) => ids,
        Err(error) => return vec![research_error("outer_doc_requests", error)],
    };
    for (index, item) in ids.iter().enumerate() {
        let Some(id) = item.as_str() else {
            return vec![research_error(
                &format!("outer_doc_requests[{index}]"),
                "expected an identifier string",
            )];
        };
        if !id.trim().is_empty() {
            commands.push(LlmCommand::OuterDocRequest(OuterDocRequestCommand {
                id: id.trim().to_string(),
            }));
        }
    }
    commands
}

fn decode_plan_property(
    node: &Value,
    property: &str,
    owned_actions: &[DiscussionPlanAction],
) -> Vec<LlmCommand> {
    let Some(payload) = node.get(property).filter(|v| !v.is_null()) else {
        return Vec::new();
    };
    match DiscussionPlanUpdate::deserialize(payload) {
        Ok(update) if owned_actions.contains(&update.action) => vec![LlmCommand::Plan { update }],
        Ok(_) => Vec::new(),
        Err(error) => vec![LlmCommand::ProtocolError {
            message: format!("invalid {property}: {error}"),
        }],
    }
}

fn decode_review_update(node: &Value) -> Vec<LlmCommand> {
    let Some(payload) = node.get("review_update").filter(|v| !v.is_null()) else {
        return Vec::new();
    };
    match ReviewUpdateProposal::deserialize(payload) {
        Ok(update) => vec![LlmCommand::ReviewUpdate { update }],
        Err(error) => vec![LlmCommand::ProtocolError {
            message: format!("invalid review_update: {error}"),
        }],
    }
}

fn decode_blocked(node: &Value) -> Vec<LlmCommand> {
    let Some(blocked) = node.get("blocked") else {
        return Vec::new();
    };
    if !blocked
        .get("is_blocked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Vec::new();
    }
    let reason = blocked
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Execution blocked")
        .to_string();
    let details = blocked
        .get("details")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
        .to_string();
    vec![LlmCommand::Blocked(BlockedCommand { reason, details })]
}

fn decode_abort(node: &Value) -> Vec<LlmCommand> {
    let Some(abort) = node.get("abort") else {
        return Vec::new();
    };
    if !abort
        .get("is_aborted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Vec::new();
    }
    let reason = abort
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Execution aborted")
        .to_string();
    let recommendation = abort
        .get("recommendation")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("No recommendation was stated.")
        .to_string();
    vec![LlmCommand::Abort(AbortCommand {
        reason,
        recommendation,
    })]
}

fn decode_custom_tools(node: &Value, project: Option<&ProjectConfig>) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    let Some(tools) = research_array(node, "custom_tools") else {
        return commands;
    };
    let tools = match tools {
        Ok(tools) => tools,
        Err(error) => return vec![research_error("custom_tools", error)],
    };
    for (index, item) in tools.iter().enumerate() {
        let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        else {
            return vec![research_error(
                &format!("custom_tools[{index}].name"),
                "expected a command name",
            )];
        };
        let Some(args) = item.get("args").filter(|args| args.is_object()) else {
            return vec![research_error(
                &format!("custom_tools[{index}].args"),
                "expected an object",
            )];
        };
        if !item.as_object().is_some_and(|object| object.len() == 2) {
            return vec![research_error(
                &format!("custom_tools[{index}]"),
                "unexpected fields",
            )];
        }
        let mut args = args.clone();
        if let Some(resolved) = project.and_then(|project| {
            let spec = project.commands.get(name)?;
            project.resolve(&format!("commands.{name}"), spec).ok()
        }) {
            if let Some(object) = args.as_object_mut() {
                for parameter in resolved.parameters {
                    if parameter.default.is_some()
                        && object.get(&parameter.name).is_some_and(Value::is_null)
                    {
                        object.remove(&parameter.name);
                    }
                }
            }
        }
        commands.push(LlmCommand::CustomTool(CustomToolCommand {
            name: name.to_string(),
            args,
        }));
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::capability_expr_schema;
    use crate::context::ContextAccess;
    use crate::edits::EditCreate;
    use crate::edits::EditDelete;
    use crate::edits::EditLineAnchor;
    use crate::edits::EditMove;
    use crate::edits::EditMoveDir;
    use crate::edits::EditOverwrite;
    use crate::edits::EditReplace;
    use serde_json::json;

    #[test]
    fn apply_patch_terminal_declarations_do_not_propose_an_empty_patch() {
        let ctx =
            PromptRenderContext::new(&[], None).with_patch_strategy(PatchStrategy::ApplyPatch);
        let grant = crate::llm::step::turn_capability_expr();
        for (action, expected) in [
            (
                json!({"kind": "edit", "patch": "", "reason": "Already done"}),
                LlmCommand::TurnSkip(TurnSkipCommand {
                    reason: "Already done".into(),
                }),
            ),
            (
                json!({"kind": "edit", "patch": "", "error_message": "Cannot finish"}),
                LlmCommand::TurnError(TurnErrorCommand {
                    error_message: "Cannot finish".into(),
                }),
            ),
        ] {
            assert_eq!(
                decode_capability_expr(&grant, &json!({"action": action}), &ctx),
                vec![expected]
            );
        }
        for action in [
            json!({"kind": "edit", "reason": "Already done"}),
            json!({"kind": "edit", "patch": [], "reason": "Already done"}),
        ] {
            assert!(
                decode_capability_expr(&grant, &json!({"action": action}), &ctx)
                    .iter()
                    .any(|command| matches!(command, LlmCommand::ProtocolError { .. }))
            );
        }
        let action = json!({"kind": "edit", "patch": "nonempty", "reason": "Already done"});
        assert!(decode_capability_expr(&grant, &json!({"action": action}), &ctx)
            .iter()
            .any(|command| matches!(command, LlmCommand::ApplyPatch { patch } if patch == "nonempty")));
    }

    #[test]
    fn custom_tool_decoder_omits_only_nulls_for_resolved_defaults() {
        let project: ProjectConfig = serde_json::from_value(json!({
            "commands": {
                "probe": {"command": "just", "parameters": [
                    {"name": "target"}, {"name": "limit", "default": "10"}
                ]},
                "alias": {"command": "@probe"}
            }
        }))
        .unwrap();
        for (input, expected) in [
            (
                json!({"target":"src","limit":null}),
                json!({"target":"src"}),
            ),
            (
                json!({"target":"src","limit":3}),
                json!({"target":"src","limit":3}),
            ),
            (json!({"target":"src"}), json!({"target":"src"})),
            (
                json!({"target":null,"unknown":null}),
                json!({"target":null,"unknown":null}),
            ),
            (json!({}), json!({})),
        ] {
            let commands = decode_custom_tools(
                &json!({"custom_tools":[{"name":"alias","args":input}]}),
                Some(&project),
            );
            assert_eq!(
                commands,
                vec![LlmCommand::CustomTool(CustomToolCommand {
                    name: "alias".into(),
                    args: expected
                })]
            );
        }
    }

    fn project_with_command() -> ProjectConfig {
        serde_json::from_str(r#"{"commands": {"check": {"command": "just", "args": ["check"]}}}"#)
            .expect("project parses")
    }

    #[test]
    fn decodes_every_capability_consistently_with_schema() {
        let project = project_with_command();
        let search_ctx = PromptRenderContext::new(&[], None)
            .with_project(Some(&project))
            .with_patch_strategy(PatchStrategy::SearchReplace);
        let anchor_ctx = PromptRenderContext::new(&[], None)
            .with_project(Some(&project))
            .with_patch_strategy(PatchStrategy::LineAnchor);

        let flat_all = CapabilityTree::from_leaves(LlmCapability::ALL.iter().copied());
        let schema = capability_expr_schema(&flat_all, &search_ctx);
        assert!(schema["properties"].is_object());

        let payload = json!({
            "message": "Working on it",
            "changes": [
                {"op": "replace", "path": "src/a.rs", "to": "", "old_text": "old", "new_text": "new", "content": ""},
                {"op": "create", "path": "src/b.rs", "to": "", "old_text": "", "new_text": "", "content": "fn b() {}"},
                {"op": "delete", "path": "src/c.rs", "to": "", "old_text": "", "new_text": "", "content": ""},
                {"op": "move", "path": "src/d.rs", "to": "src/e.rs", "old_text": "", "new_text": "", "content": ""},
                {"op": "move_dir", "path": "src/old", "to": "src/new", "old_text": "", "new_text": "", "content": ""},
                {"op": "overwrite", "path": "src/f.rs", "to": "", "old_text": "", "new_text": "", "content": "all"}
            ],
            "context_requests": [{"path": "src/g.rs", "access": "editable"}],
            "repo_searches": [{"path": "crates", "regex": "fn main"}],
            "repo_dirs": true,
            "repo_maps": ["crates/core"],
            "dir_lists": [{"path": "crates", "recursive": true}],
            "tauqe_doc_requests": ["workflow"],
            "outer_doc_requests": ["openrouter/models"],
            "plan_create": {"action": "save", "id": "p-1", "title": "Plan"},
            "plan_update": {"action": "update", "id": "p-1"},
            "review_update": {"item_id": 3, "status": "todo", "note": "fix", "recommendations": []},
            "scratchpad": "noted",
            "commit_message": "feat: implement decoder",
            "error_message": "broken",
            "reason": "already done",
            "blocked": {"is_blocked": true, "reason": "missing dep", "details": "d"},
            "abort": {"is_aborted": true, "reason": "need auth", "recommendation": "login"},
            "custom_tools": [{"name": "check", "args": {}}]
        });

        let commands = decode_capability_expr(&flat_all, &payload, &search_ctx);
        assert!(commands.contains(&LlmCommand::Text(TextCommand {
            content: "Working on it".into()
        })));
        assert!(commands.contains(&LlmCommand::EditReplace(EditReplace {
            path: "src/a.rs".into(),
            old_text: "old".into(),
            new_text: "new".into()
        })));
        assert!(commands.contains(&LlmCommand::EditCreate(EditCreate {
            path: "src/b.rs".into(),
            content: "fn b() {}".into()
        })));
        assert!(commands.contains(&LlmCommand::EditDelete(EditDelete {
            path: "src/c.rs".into()
        })));
        assert!(commands.contains(&LlmCommand::EditMove(EditMove {
            from: "src/d.rs".into(),
            to: "src/e.rs".into()
        })));
        assert!(commands.contains(&LlmCommand::EditMoveDir(EditMoveDir {
            from: "src/old".into(),
            to: "src/new".into()
        })));
        assert!(commands.contains(&LlmCommand::EditOverwrite(EditOverwrite {
            path: "src/f.rs".into(),
            content: "all".into()
        })));
        assert!(
            commands.contains(&LlmCommand::ContextRequest(ContextRequestCommand {
                path: "src/g.rs".into(),
                access: ContextAccess::Editable
            }))
        );
        assert!(
            commands.contains(&LlmCommand::RepoSearch(RepoSearchCommand {
                path: "crates".into(),
                regex: "fn main".into()
            }))
        );
        assert!(commands.contains(&LlmCommand::RepoDirs(RepoDirsCommand {})));
        assert!(commands.contains(&LlmCommand::RepoMap(RepoMapCommand {
            path: "crates/core".into()
        })));
        assert!(commands.contains(&LlmCommand::DirList(DirListCommand {
            path: "crates".into(),
            recursive: true
        })));
        assert!(
            commands.contains(&LlmCommand::DocRequest(DocRequestCommand {
                topic: DocTopic::Workflow
            }))
        );
        assert!(
            commands.contains(&LlmCommand::OuterDocRequest(OuterDocRequestCommand {
                id: "openrouter/models".into()
            }))
        );
        assert!(commands.iter().any(|c| matches!(c, LlmCommand::Plan { update } if update.action == DiscussionPlanAction::Save)));
        assert!(commands.iter().any(|c| matches!(c, LlmCommand::Plan { update } if update.action == DiscussionPlanAction::Update)));
        assert!(commands
            .iter()
            .any(|c| matches!(c, LlmCommand::ReviewUpdate { update } if update.item_id == 3)));
        assert!(
            commands.contains(&LlmCommand::Scratchpad(ScratchpadCommand {
                notes: "noted".into()
            }))
        );
        assert!(
            commands.contains(&LlmCommand::CommitMessage(CommitMessageCommand {
                commit_message: "feat: implement decoder".into()
            }))
        );
        assert!(commands.contains(&LlmCommand::TurnError(TurnErrorCommand {
            error_message: "broken".into()
        })));
        assert!(commands.contains(&LlmCommand::TurnSkip(TurnSkipCommand {
            reason: "already done".into()
        })));
        assert!(commands.contains(&LlmCommand::Blocked(BlockedCommand {
            reason: "missing dep".into(),
            details: "d".into()
        })));
        assert!(commands.contains(&LlmCommand::Abort(AbortCommand {
            reason: "need auth".into(),
            recommendation: "login".into()
        })));
        assert!(
            commands.contains(&LlmCommand::CustomTool(CustomToolCommand {
                name: "check".into(),
                args: json!({})
            }))
        );

        // Under LineAnchor, replace projects to EditLineAnchor and EditOverwrite is inactive.
        let anchor_payload = json!({
            "changes": [
                {"op": "replace", "path": "src/a.rs", "to": "", "start": "K1", "end": "K2", "content": "replaced"},
                {"op": "overwrite", "path": "src/f.rs", "to": "", "start": "", "end": "", "content": "ignored"}
            ]
        });
        let anchor_cmds = decode_capability_expr(&flat_all, &anchor_payload, &anchor_ctx);
        assert_eq!(
            anchor_cmds,
            vec![LlmCommand::EditLineAnchor(EditLineAnchor {
                path: "src/a.rs".into(),
                start: "K1".into(),
                end: "K2".into(),
                content: "replaced".into(),
            })]
        );
    }

    #[test]
    fn decodes_any_of_branches_and_reports_kind_errors() {
        let ctx = PromptRenderContext::new(&[], None);
        let expr = crate::llm::repl::repl_capability_expr(None);
        let schema = capability_expr_schema(&expr, &ctx);
        assert!(schema["properties"]["action"]["anyOf"].is_array());

        // Selecting `edit` decodes only `edit` branch and ignores hostile top-level fields.
        let edit_payload = json!({
            "message": "Updating file",
            "scratchpad": "s",
            "context_requests": [{"path": "src/hostile.rs", "access": "read_only"}],
            "action": {
                "kind": "edit",
                "changes": [{"op": "create", "path": "src/x.rs", "to": "", "old_text": "", "new_text": "", "content": "fn x() {}"}],
                "verify": {"target": "none", "on_success": "silent"},
                "commit_message": "feat: add x",
                "reason": ""
            }
        });
        let cmds = decode_capability_expr(&expr, &edit_payload, &ctx);
        assert!(cmds.contains(&LlmCommand::Text(TextCommand {
            content: "Updating file".into()
        })));
        assert!(cmds.contains(&LlmCommand::EditCreate(EditCreate {
            path: "src/x.rs".into(),
            content: "fn x() {}".into()
        })));
        assert!(
            cmds.contains(&LlmCommand::CommitMessage(CommitMessageCommand {
                commit_message: "feat: add x".into()
            }))
        );
        assert!(!cmds
            .iter()
            .any(|c| matches!(c, LlmCommand::ContextRequest(_))));
        // The message channel is ambient: the root `message` field carries the
        // conversational text of the round, while the selected branch owns only
        // the commands of its kind.

        // Unknown kind yields ProtocolError naming the unknown kind.
        let unknown_kind = json!({
            "message": "hi",
            "action": {"kind": "teleport"}
        });
        let errs = decode_capability_expr(&expr, &unknown_kind, &ctx);
        assert!(errs.iter().any(
            |c| matches!(c, LlmCommand::ProtocolError { message } if message.contains("teleport"))
        ));

        // Missing or non-string kind yields ProtocolError.
        let missing_kind = json!({
            "message": "hi",
            "action": {}
        });
        let errs = decode_capability_expr(&expr, &missing_kind, &ctx);
        assert!(errs.iter().any(|c| matches!(c, LlmCommand::ProtocolError { message } if message.contains("action.kind"))));

        // Missing action object yields ProtocolError.
        let missing_action = json!({"message": "hi"});
        let errs = decode_capability_expr(&expr, &missing_action, &ctx);
        assert!(errs.iter().any(
            |c| matches!(c, LlmCommand::ProtocolError { message } if message.contains("action"))
        ));
    }

    #[test]
    fn decodes_single_branch_transparently_and_nested_any_of() {
        let ctx = PromptRenderContext::new(&[], None);

        // Single-branch AnyOf merges transparently at the current level.
        let single = CapabilityTree::any_of(
            "action",
            [CapabilityTree::named_set(
                "only",
                [CapabilityTree::leaf(LlmCapability::TextMessage)],
            )],
        );
        let single_schema = capability_expr_schema(&single, &ctx);
        assert!(single_schema["properties"].get("action").is_none());
        assert_eq!(
            decode_capability_expr(&single, &json!({"message": "hello"}), &ctx),
            vec![LlmCommand::Text(TextCommand {
                content: "hello".into()
            })]
        );

        // Nested AnyOf descends through outer `action` and inner `sub_action`.
        let nested = CapabilityTree::root([
            CapabilityTree::leaf(LlmCapability::TextMessage),
            CapabilityTree::any_of(
                "action",
                [
                    CapabilityTree::named_set(
                        "plan",
                        [CapabilityTree::leaf(LlmCapability::PlanCreation)],
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
                                    [CapabilityTree::leaf(LlmCapability::EditCreate)],
                                ),
                            ],
                        )],
                    ),
                ],
            ),
        ]);
        let nested_schema = capability_expr_schema(&nested, &ctx);
        assert!(
            nested_schema["properties"]["action"]["anyOf"][1]["properties"]["sub_action"]["anyOf"]
                .is_array()
        );

        let nested_payload = json!({
            "message": "Searching",
            "action": {
                "kind": "nested",
                "sub_action": {
                    "kind": "search",
                    "repo_searches": [{"path": "src", "regex": "foo"}]
                }
            }
        });
        assert_eq!(
            decode_capability_expr(&nested, &nested_payload, &ctx),
            vec![
                LlmCommand::Text(TextCommand {
                    content: "Searching".into()
                }),
                LlmCommand::RepoSearch(RepoSearchCommand {
                    path: "src".into(),
                    regex: "foo".into()
                }),
            ]
        );
    }

    #[test]
    fn deduplicates_protocol_errors_for_shared_fields_in_set() {
        let ctx = PromptRenderContext::new(&[], None);
        let edits = CapabilityTree::from_leaves([
            LlmCapability::EditReplace,
            LlmCapability::EditCreate,
            LlmCapability::EditDelete,
        ]);
        let bad_changes = json!({"changes": [{"op": "create", "path": "src/a.rs"}]});
        let cmds = decode_capability_expr(&edits, &bad_changes, &ctx);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], LlmCommand::ProtocolError { .. }));
    }

    #[test]
    fn decodes_the_plan_payload_through_the_plan_action_union() {
        let ctx = PromptRenderContext::new(&[], None);
        let surface = crate::llm::repl::repl_capability_expr(Some(
            &crate::discussion::DiscussionTarget::Plan { id: "p-1".into() },
        ));

        let create = json!({
            "action": {
                "kind": "plan",
                "plan_action": {
                    "kind": "create",
                    "plan_create": {"action": "save", "id": "p-1", "title": "Plan"}
                }
            }
        });
        let commands = decode_capability_expr(&surface, &create, &ctx);
        assert!(commands.iter().any(
            |command| matches!(command, LlmCommand::Plan { update } if update.action == DiscussionPlanAction::Save)
        ));

        let modify = json!({
            "action": {
                "kind": "plan",
                "plan_action": {
                    "kind": "modify",
                    "plan_update": {"action": "update", "id": "p-1"}
                }
            }
        });
        let commands = decode_capability_expr(&surface, &modify, &ctx);
        assert!(commands.iter().any(
            |command| matches!(command, LlmCommand::Plan { update } if update.action == DiscussionPlanAction::Update)
        ));

        // The union is a contract: a payload that names no variant, or one the
        // union does not declare, is a protocol defect instead of a silent
        // omission.
        for (payload, expected) in [
            (json!({"action": {"kind": "plan"}}), "plan_action"),
            (
                json!({"action": {"kind": "plan", "plan_action": {"kind": "teleport"}}}),
                "teleport",
            ),
        ] {
            let errors = decode_capability_expr(&surface, &payload, &ctx);
            assert!(
                errors.iter().any(
                    |command| matches!(command, LlmCommand::ProtocolError { message } if message.contains(expected))
                ),
                "the defect must name '{expected}': {errors:?}"
            );
        }
    }
}
