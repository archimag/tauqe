//! Capability classification of normalized model commands.

use super::LlmCapability;
use crate::llm::discussion::DiscussionPlanAction;
use crate::llm::LlmCommand;

/// Capability that this command realizes, or `None` when the command is a
/// protocol diagnostic rather than a realized capability.
pub fn command_capability(command: &LlmCommand) -> Option<LlmCapability> {
    match command {
        LlmCommand::Text(_) => Some(LlmCapability::TextMessage),
        LlmCommand::EditReplace(_)
        | LlmCommand::EditLineAnchor(_)
        | LlmCommand::ApplyPatch { .. } => Some(LlmCapability::EditReplace),
        LlmCommand::EditCreate(_) => Some(LlmCapability::EditCreate),
        LlmCommand::EditDelete(_) => Some(LlmCapability::EditDelete),
        LlmCommand::EditMove(_) => Some(LlmCapability::EditMove),
        LlmCommand::EditMoveDir(_) => Some(LlmCapability::EditMoveDir),
        LlmCommand::EditOverwrite(_) => Some(LlmCapability::EditOverwrite),
        LlmCommand::ContextRequest(_) => Some(LlmCapability::ContextRequest),
        LlmCommand::RepoSearch(_) => Some(LlmCapability::RepoSearch),
        LlmCommand::RepoDirs(_) => Some(LlmCapability::RepoDirs),
        LlmCommand::RepoMap(_) => Some(LlmCapability::RepoMap),
        LlmCommand::DirList(_) => Some(LlmCapability::DirList),
        LlmCommand::DocRequest(_) => Some(LlmCapability::DocRequest),
        LlmCommand::OuterDocRequest(_) => Some(LlmCapability::OuterDocRequest),
        LlmCommand::Plan { update } => Some(match update.action {
            DiscussionPlanAction::Save => LlmCapability::PlanCreation,
            DiscussionPlanAction::Update => LlmCapability::PlanUpdate,
        }),
        LlmCommand::Scratchpad(_) => Some(LlmCapability::Scratchpad),
        LlmCommand::CommitMessage(_) => Some(LlmCapability::CommitMessage),
        LlmCommand::TurnError(_) => Some(LlmCapability::TurnError),
        LlmCommand::TurnSkip(_) => Some(LlmCapability::TurnSkip),
        LlmCommand::Blocked(_) => Some(LlmCapability::Blocked),
        LlmCommand::Abort(_) => Some(LlmCapability::Abort),
        LlmCommand::CustomTool(_) => Some(LlmCapability::CustomTools),
        LlmCommand::ReviewUpdate { .. } => Some(LlmCapability::ReviewUpdate),
        LlmCommand::ProtocolError { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;
    use crate::edits::*;
    use crate::llm::command::*;
    use crate::llm::discussion::{DiscussionPlanAction, DiscussionPlanUpdate};
    use crate::llm::*;
    #[test]
    fn test_llm_command_capability_classification() {
        let text = LlmCommand::Text(TextCommand {
            content: "hello".to_string(),
        });
        assert_eq!(command_capability(&text), Some(LlmCapability::TextMessage));

        let edit = LlmCommand::EditReplace(EditReplace {
            path: "src/lib.rs".to_string(),
            old_text: "a".to_string(),
            new_text: "b".to_string(),
        });
        assert_eq!(command_capability(&edit), Some(LlmCapability::EditReplace));

        let context = LlmCommand::ContextRequest(ContextRequestCommand {
            path: "src/a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        });
        assert_eq!(
            command_capability(&context),
            Some(LlmCapability::ContextRequest)
        );

        let search = LlmCommand::RepoSearch(RepoSearchCommand {
            path: "src".to_string(),
            regex: "fn main".to_string(),
        });
        assert_eq!(command_capability(&search), Some(LlmCapability::RepoSearch));

        let dirs = LlmCommand::RepoDirs(RepoDirsCommand {});
        assert_eq!(command_capability(&dirs), Some(LlmCapability::RepoDirs));

        let map = LlmCommand::RepoMap(RepoMapCommand {
            path: "crates/core".to_string(),
        });
        assert_eq!(command_capability(&map), Some(LlmCapability::RepoMap));

        let dir_list = LlmCommand::DirList(DirListCommand {
            path: "crates/core".to_string(),
            recursive: false,
        });
        assert_eq!(command_capability(&dir_list), Some(LlmCapability::DirList));

        let doc = LlmCommand::DocRequest(DocRequestCommand {
            topic: DocTopic::Workflow,
        });
        assert_eq!(command_capability(&doc), Some(LlmCapability::DocRequest));

        let outer_doc = LlmCommand::OuterDocRequest(OuterDocRequestCommand {
            id: "mylib".to_string(),
        });
        assert_eq!(
            command_capability(&outer_doc),
            Some(LlmCapability::OuterDocRequest)
        );

        let scratchpad = LlmCommand::Scratchpad(ScratchpadCommand {
            notes: "notes".to_string(),
        });
        assert_eq!(
            command_capability(&scratchpad),
            Some(LlmCapability::Scratchpad)
        );

        let commit = LlmCommand::CommitMessage(CommitMessageCommand {
            commit_message: "fix(core): resolve null pointer".to_string(),
        });
        assert_eq!(
            command_capability(&commit),
            Some(LlmCapability::CommitMessage)
        );

        let blocked = LlmCommand::Blocked(BlockedCommand {
            reason: "blocked".to_string(),
            details: String::new(),
        });
        assert_eq!(command_capability(&blocked), Some(LlmCapability::Blocked));

        let abort = LlmCommand::Abort(AbortCommand {
            reason: "blocked by host".to_string(),
            recommendation: "run script".to_string(),
        });
        assert_eq!(command_capability(&abort), Some(LlmCapability::Abort));

        let custom_tool = LlmCommand::CustomTool(CustomToolCommand {
            name: "migrate".to_string(),
            args: serde_json::json!({ "database": "primary" }),
        });
        assert_eq!(
            command_capability(&custom_tool),
            Some(LlmCapability::CustomTools)
        );

        let error = LlmCommand::ProtocolError {
            message: "malformed".to_string(),
        };
        assert_eq!(command_capability(&error), None);

        let plan_save = LlmCommand::Plan {
            update: DiscussionPlanUpdate {
                action: DiscussionPlanAction::Save,
                ..Default::default()
            },
        };
        let save_capability = command_capability(&plan_save);
        assert_eq!(save_capability, Some(LlmCapability::PlanCreation));

        let plan_update = LlmCommand::Plan {
            update: DiscussionPlanUpdate {
                action: DiscussionPlanAction::Update,
                ..Default::default()
            },
        };
        assert_eq!(
            command_capability(&plan_update),
            Some(LlmCapability::PlanUpdate)
        );
    }

    #[test]
    fn test_llm_command_review_update_serde_roundtrip() {
        let command = LlmCommand::ReviewUpdate {
            update: crate::llm::discussion::ReviewUpdateProposal {
                item_id: 7,
                group_id: None,
                status: crate::llm::discussion::ReviewDecisionStatus::Rejected,
                note: "The reported race cannot happen".to_string(),
                recommendations: vec!["Document the ordering".to_string()],
            },
        };
        let json = serde_json::to_string(&command).unwrap();
        assert!(json.contains("\"type\":\"review_update\""));
        assert!(json.contains("\"status\":\"rejected\""));
        assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), command);

        // A decision is a capability of the review discussion regime, so the
        // command carries that capability.
        assert_eq!(
            command_capability(&command),
            Some(LlmCapability::ReviewUpdate)
        );
    }

    #[test]
    fn test_llm_command_edit_variants_serde_roundtrip() {
        let cases = [
            (
                LlmCommand::EditReplace(EditReplace {
                    path: "src/lib.rs".to_string(),
                    old_text: "let a = 1;".to_string(),
                    new_text: "let a = 2;".to_string(),
                }),
                "\"type\":\"edit_replace\"",
                EditOperation::Replace(EditReplace {
                    path: "src/lib.rs".to_string(),
                    old_text: "let a = 1;".to_string(),
                    new_text: "let a = 2;".to_string(),
                }),
            ),
            (
                LlmCommand::EditCreate(EditCreate {
                    path: "src/new.rs".to_string(),
                    content: "pub fn f() {}".to_string(),
                }),
                "\"type\":\"edit_create\"",
                EditOperation::Create(EditCreate {
                    path: "src/new.rs".to_string(),
                    content: "pub fn f() {}".to_string(),
                }),
            ),
            (
                LlmCommand::EditDelete(EditDelete {
                    path: "src/old.rs".to_string(),
                }),
                "\"type\":\"edit_delete\"",
                EditOperation::Delete(EditDelete {
                    path: "src/old.rs".to_string(),
                }),
            ),
            (
                LlmCommand::EditMove(EditMove {
                    from: "src/a.rs".to_string(),
                    to: "src/b.rs".to_string(),
                }),
                "\"type\":\"edit_move\"",
                EditOperation::Move(EditMove {
                    from: "src/a.rs".to_string(),
                    to: "src/b.rs".to_string(),
                }),
            ),
            (
                LlmCommand::EditMoveDir(EditMoveDir {
                    from: "src/old".to_string(),
                    to: "src/new".to_string(),
                }),
                "\"type\":\"edit_move_dir\"",
                EditOperation::MoveDir(EditMoveDir {
                    from: "src/old".to_string(),
                    to: "src/new".to_string(),
                }),
            ),
            (
                LlmCommand::EditOverwrite(EditOverwrite {
                    path: "src/lib.rs".to_string(),
                    content: "pub fn g() {}".to_string(),
                }),
                "\"type\":\"edit_overwrite\"",
                EditOperation::Overwrite(EditOverwrite {
                    path: "src/lib.rs".to_string(),
                    content: "pub fn g() {}".to_string(),
                }),
            ),
            (
                LlmCommand::EditLineAnchor(EditLineAnchor {
                    path: "src/lib.rs".to_string(),
                    start: "Raaa".to_string(),
                    end: "kbbb".to_string(),
                    content: "let b = 3;".to_string(),
                }),
                "\"type\":\"edit_line_anchor\"",
                EditOperation::LineAnchor(EditLineAnchor {
                    path: "src/lib.rs".to_string(),
                    start: "Raaa".to_string(),
                    end: "kbbb".to_string(),
                    content: "let b = 3;".to_string(),
                }),
            ),
        ];

        for (cmd, expected_type, expected_op) in cases {
            assert!(matches!(
                command_capability(&cmd),
                Some(
                    LlmCapability::EditReplace
                        | LlmCapability::EditCreate
                        | LlmCapability::EditDelete
                        | LlmCapability::EditMove
                        | LlmCapability::EditMoveDir
                        | LlmCapability::EditOverwrite
                )
            ));
            let json = serde_json::to_string(&cmd).unwrap();
            assert!(
                json.contains(expected_type),
                "missing {expected_type} in {json}"
            );
            assert_eq!(serde_json::from_str::<LlmCommand>(&json).unwrap(), cmd);
            assert_eq!(cmd.into_edit_operation(), Some(expected_op));
        }

        let non_edit = LlmCommand::RepoDirs(RepoDirsCommand {});
        assert_eq!(non_edit.into_edit_operation(), None);
    }
}
