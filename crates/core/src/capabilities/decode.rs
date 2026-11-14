//! Tree-walking decoder for structured LLM responses.
//!
//! [`decode_capability_expr`] mirrors [`super::schema::capability_expr_schema`]:
//! it walks the [`CapabilityExpr`] tree synchronously with the JSON response
//! tree so every leaf reads its property from the exact object level the
//! composed schema declared.

use serde::Deserialize;
use serde_json::Value;
use tauqe_protocol::discussion::normalize_repo_map_path;
use tauqe_protocol::review::ReviewUpdateProposal;
use tauqe_protocol::{
    AbortCommand, BlockedCommand, CapabilityExpr, CommitMessageCommand, ContextAccess,
    ContextRequestCommand, CustomToolCommand, DirListCommand, DirListRequest, DiscussionPlanAction,
    DiscussionPlanUpdate, DocRequestCommand, DocTopic, EditFileOp, LlmCapability, LlmCommand,
    OuterDocRequestCommand, PatchStrategy, RepoDirsCommand, RepoMapCommand, RepoSearchCommand,
    RepoSearchRequest, ScratchpadCommand, ShortDescriptionCommand, StructuredChangeProposal,
    TextCommand, TurnErrorCommand, TurnSkipCommand, VerifyCommand, VerifyOnSuccess, VerifyTarget,
};

use crate::capabilities::PromptRenderContext;
use crate::edits::protocol::utils::normalize_content;
use crate::edits::protocol::ContextRequest;

/// Decodes a structured JSON response by walking `expr` synchronously with
/// `node`.
pub(crate) fn decode_capability_expr(
    expr: &CapabilityExpr,
    node: &Value,
    ctx: &PromptRenderContext<'_>,
) -> Vec<LlmCommand> {
    match expr {
        CapabilityExpr::Leaf(cap) => decode_leaf(*cap, node, ctx),
        CapabilityExpr::Set(items) => {
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
        CapabilityExpr::AnyOf(label, branches) => {
            if branches.is_empty() {
                return Vec::new();
            }
            if branches.len() == 1 {
                return decode_capability_expr(&branches[0].expr, node, ctx);
            }
            let Some(branch_node) = node.get(label).filter(|v| v.is_object()) else {
                return vec![LlmCommand::ProtocolError {
                    message: format!("missing or invalid '{label}' object for operational branch"),
                }];
            };
            let Some(kind) = branch_node.get("kind").and_then(Value::as_str) else {
                return vec![LlmCommand::ProtocolError {
                    message: format!("missing or non-string '{label}.kind' discriminator"),
                }];
            };
            let Some(branch) = branches.iter().find(|b| b.kind == kind) else {
                let expected: Vec<&str> = branches.iter().map(|b| b.kind.as_str()).collect();
                return vec![LlmCommand::ProtocolError {
                    message: format!(
                        "unknown '{label}.kind' value '{kind}'; expected one of: {}",
                        expected.join(", ")
                    ),
                }];
            };
            decode_capability_expr(&branch.expr, branch_node, ctx)
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
        LlmCapability::ShortDescription => {
            let Some(content) = node.get("message").and_then(Value::as_str) else {
                return Vec::new();
            };
            if content.trim().is_empty() {
                return Vec::new();
            }
            vec![LlmCommand::ShortDescription(ShortDescriptionCommand {
                content: content.to_string(),
            })]
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
        LlmCapability::RepoDirs => {
            if node.get("repo_dirs").and_then(Value::as_bool) == Some(true) {
                vec![LlmCommand::RepoDirs(RepoDirsCommand {})]
            } else {
                Vec::new()
            }
        }
        LlmCapability::RepoMap => decode_repo_maps(node),
        LlmCapability::DirList => decode_dir_lists(node),
        LlmCapability::DocRequest => decode_doc_requests(node),
        LlmCapability::OuterDocRequest => decode_outer_doc_requests(node),
        LlmCapability::ToolchainVerify => decode_verify(node),
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
            decode_custom_tools(node)
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
        match resolved.to_edit_command() {
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
    if let Some(reqs) = node.get("context_requests").and_then(Value::as_array) {
        for item in reqs {
            if let Some(spec) = item.as_str() {
                let access = if spec.starts_with("editable:") {
                    ContextAccess::Editable
                } else {
                    ContextAccess::ReadOnly
                };
                let path = spec.strip_prefix("editable:").unwrap_or(spec).trim();
                if !path.is_empty() {
                    commands.push(LlmCommand::ContextRequest(ContextRequestCommand {
                        path: path.to_string(),
                        access,
                    }));
                }
            } else if let Ok(parsed) = serde_json::from_value::<ContextRequest>(item.clone()) {
                commands.push(LlmCommand::ContextRequest(ContextRequestCommand {
                    path: parsed.path,
                    access: parsed.access,
                }));
            }
        }
    }
    commands
}

fn decode_repo_searches(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    if let Some(s_val) = node.get("repo_searches").and_then(Value::as_array) {
        if let Ok(parsed) =
            serde_json::from_value::<Vec<RepoSearchRequest>>(Value::Array(s_val.clone()))
        {
            for request in parsed {
                commands.push(LlmCommand::RepoSearch(RepoSearchCommand {
                    path: request.path,
                    regex: request.regex,
                }));
            }
        }
    }
    commands
}

fn decode_repo_maps(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    if let Some(m_val) = node.get("repo_maps").and_then(Value::as_array) {
        for item in m_val {
            if let Some(p) = item.as_str() {
                if !p.contains("..") {
                    if let Some(normalized) = normalize_repo_map_path(p) {
                        commands.push(LlmCommand::RepoMap(RepoMapCommand { path: normalized }));
                    }
                }
            }
        }
    }
    commands
}

fn decode_dir_lists(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    if let Some(d_val) = node.get("dir_lists").and_then(Value::as_array) {
        for item in d_val {
            if let Ok(req) = serde_json::from_value::<DirListRequest>(item.clone()) {
                if !req.path.contains("..") {
                    if let Some(normalized) = req.normalized() {
                        commands.push(LlmCommand::DirList(DirListCommand {
                            path: normalized.path,
                            recursive: normalized.recursive,
                        }));
                    }
                }
            }
        }
    }
    commands
}

fn decode_doc_requests(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    let val = node
        .get("tauqe_doc_requests")
        .or_else(|| node.get("doc_requests"));
    if let Some(d_val) = val.and_then(Value::as_array) {
        for item in d_val {
            if item.is_null() {
                commands.push(LlmCommand::DocRequest(DocRequestCommand {
                    topic: DocTopic::All,
                }));
            } else if let Some(s) = item.as_str() {
                if let Ok(topic) = s.parse::<DocTopic>() {
                    commands.push(LlmCommand::DocRequest(DocRequestCommand { topic }));
                }
            }
        }
    }
    commands
}

fn decode_outer_doc_requests(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    if let Some(o_val) = node.get("outer_doc_requests").and_then(Value::as_array) {
        for item in o_val {
            if let Some(id) = item.as_str() {
                let trimmed = id.trim();
                if !trimmed.is_empty() {
                    commands.push(LlmCommand::OuterDocRequest(OuterDocRequestCommand {
                        id: trimmed.to_string(),
                    }));
                }
            }
        }
    }
    commands
}

fn decode_verify(node: &Value) -> Vec<LlmCommand> {
    let Some(v_val) = node.get("verify") else {
        return Vec::new();
    };
    if let Ok(cmd) = serde_json::from_value::<VerifyCommand>(v_val.clone()) {
        return vec![LlmCommand::Verify(cmd)];
    }
    let Some(target_str) = v_val.get("target").and_then(Value::as_str) else {
        return Vec::new();
    };
    let target = match target_str {
        "all" => VerifyTarget::All,
        "check" => VerifyTarget::Check,
        "test" => VerifyTarget::Test,
        "clippy" => VerifyTarget::Clippy,
        _ => return Vec::new(),
    };
    let on_success = match v_val.get("on_success").and_then(Value::as_str) {
        Some("report") => VerifyOnSuccess::Report,
        _ => VerifyOnSuccess::Silent,
    };
    vec![LlmCommand::Verify(VerifyCommand { target, on_success })]
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

fn decode_custom_tools(node: &Value) -> Vec<LlmCommand> {
    let mut commands = Vec::new();
    if let Some(t_val) = node.get("custom_tools").and_then(Value::as_array) {
        for item in t_val {
            if let (Some(name), Some(args)) =
                (item.get("name").and_then(Value::as_str), item.get("args"))
            {
                commands.push(LlmCommand::CustomTool(CustomToolCommand {
                    name: name.to_string(),
                    args: args.clone(),
                }));
            }
        }
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::capability_expr_schema;
    use serde_json::json;
    use tauqe_protocol::{
        CapabilityBranch, EditCreate, EditDelete, EditLineAnchor, EditMove, EditMoveDir,
        EditOverwrite, EditReplace, ProjectConfig,
    };

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

        let flat_all = CapabilityExpr::from_leaves(LlmCapability::ALL.iter().copied());
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
            "verify": {"target": "clippy", "on_success": "report"},
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
        assert!(
            commands.contains(&LlmCommand::ShortDescription(ShortDescriptionCommand {
                content: "Working on it".into()
            }))
        );
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
        assert!(commands.contains(&LlmCommand::Verify(VerifyCommand {
            target: VerifyTarget::Clippy,
            on_success: VerifyOnSuccess::Report
        })));
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
        let expr = crate::control::repl::repl_capability_expr(None);
        let schema = capability_expr_schema(&expr, &ctx);
        assert!(schema["properties"]["action"]["anyOf"].is_array());

        // Selecting `edit` decodes only `edit` branch and ignores hostile top-level fields.
        let edit_payload = json!({
            "message": "",
            "scratchpad": "s",
            "context_requests": [{"path": "src/hostile.rs", "access": "read_only"}],
            "action": {
                "kind": "edit",
                "changes": [{"op": "create", "path": "src/x.rs", "to": "", "old_text": "", "new_text": "", "content": "fn x() {}"}],
                "verify": {"target": "none", "on_success": "silent"},
                "commit_message": "feat: add x",
                "message": "Updating file",
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
        // The branch declares the `message` field itself, so the terminal
        // channel that reads the same field carries the same text.

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
        let single = CapabilityExpr::any_of(
            "action",
            [CapabilityBranch::new(
                "only",
                CapabilityExpr::leaf(LlmCapability::TextMessage),
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
        let nested = CapabilityExpr::set([
            CapabilityExpr::leaf(LlmCapability::TextMessage),
            CapabilityExpr::any_of(
                "action",
                [
                    CapabilityBranch::new(
                        "plan",
                        CapabilityExpr::leaf(LlmCapability::PlanCreation),
                    ),
                    CapabilityBranch::new(
                        "nested",
                        CapabilityExpr::any_of(
                            "sub_action",
                            [
                                CapabilityBranch::new(
                                    "search",
                                    CapabilityExpr::leaf(LlmCapability::RepoSearch),
                                ),
                                CapabilityBranch::new(
                                    "edit",
                                    CapabilityExpr::leaf(LlmCapability::EditCreate),
                                ),
                            ],
                        ),
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
        let edits = CapabilityExpr::from_leaves([
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
        let surface = crate::control::repl::repl_capability_expr(Some(
            &tauqe_protocol::DiscussionTarget::Plan { id: "p-1".into() },
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
