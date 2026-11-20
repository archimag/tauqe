//! Test-only support for recording cleaned turn responses.

use crate::history::HistoryFileChange;
use crate::history::HistoryFileOperation;

use crate::edits::EditCreate;
use crate::edits::EditDelete;
use crate::edits::EditLineAnchor;
use crate::edits::EditMove;
use crate::edits::EditMoveDir;
use crate::edits::EditOperation;
use crate::edits::EditOverwrite;
use crate::edits::EditReplace;
use crate::llm::ModelResult;

use crate::history::HistoryManager;

/// Records a finished turn into the session journal according to its result.
fn record_workflow_response(
    history_manager: &mut HistoryManager,
    result: &ModelResult,
    assistant_text: &str,
    extra_files: Vec<String>,
) {
    match result {
        ModelResult::Answer { text } => {
            let candidate = if !text.trim().is_empty() {
                text.clone()
            } else if !assistant_text.trim().is_empty() {
                assistant_text.to_string()
            } else {
                "Answered.".to_string()
            };
            let msg = crate::edits::protocol::structured::clean_raw_json_or_text(&candidate);
            let msg = if msg.trim().is_empty() {
                "Answered.".to_string()
            } else {
                msg
            };
            let summary = if !extra_files.is_empty() {
                Some(extra_files.join(", "))
            } else {
                None
            };
            let files: Vec<HistoryFileChange> = extra_files
                .into_iter()
                .map(HistoryFileChange::plain)
                .collect();
            if let Err(err) = history_manager.record_response(msg, None, summary, files) {
                tracing::warn!(
                    "Failed to record the assistant answer in session history: {}",
                    err
                );
            }
        }
        ModelResult::Edit {
            summary,
            edits,
            proposal,
            outcome,
            ..
        } => {
            let msg = if let Some(m) = proposal
                .as_ref()
                .map(|p| p.message.trim())
                .filter(|m| !m.is_empty())
            {
                m.to_string()
            } else if !assistant_text.trim().is_empty() {
                crate::edits::protocol::structured::clean_raw_json_or_text(assistant_text)
            } else {
                summary.clone()
            };

            // The operation is typed knowledge the turn already holds, so the
            // producer authors one record per touched path instead of flattening
            // it into display text that every client would have to re-parse
            // (Conventions §1.5).
            let mut seen = std::collections::HashSet::new();
            let mut files = Vec::new();
            for edit in edits {
                let entry = match edit {
                    EditOperation::Create(EditCreate { path, .. }) => {
                        HistoryFileChange::operated(HistoryFileOperation::Create, path.clone())
                    }
                    EditOperation::Delete(EditDelete { path }) => {
                        HistoryFileChange::operated(HistoryFileOperation::Delete, path.clone())
                    }
                    EditOperation::Replace(EditReplace { path, .. }) => {
                        HistoryFileChange::operated(HistoryFileOperation::Replace, path.clone())
                    }
                    EditOperation::LineAnchor(EditLineAnchor { path, .. }) => {
                        HistoryFileChange::operated(HistoryFileOperation::Replace, path.clone())
                    }
                    EditOperation::Overwrite(EditOverwrite { path, .. }) => {
                        HistoryFileChange::operated(HistoryFileOperation::Overwrite, path.clone())
                    }
                    EditOperation::Move(EditMove { from, to }) => {
                        HistoryFileChange::moved(from.clone(), to.clone())
                    }
                    EditOperation::MoveDir(EditMoveDir { from, to }) => {
                        HistoryFileChange::moved_dir(from.clone(), to.clone())
                    }
                };
                if seen.insert(entry.clone()) {
                    files.push(entry);
                }
            }

            // Post-hook artifacts name a path but carry no operation.
            for extra in extra_files {
                let entry = HistoryFileChange::plain(extra);
                if seen.insert(entry.clone()) {
                    files.push(entry);
                }
            }

            let commit = if outcome.applied {
                outcome.commit_hash.clone()
            } else {
                None
            };

            if let Err(err) =
                history_manager.record_response(&msg, commit, Some(summary.clone()), files)
            {
                tracing::warn!(
                    "Failed to record the assistant response in session history: {}",
                    err
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(test)]
    use crate::edits::EditOperation;
    use crate::edits::EditOutcome;
    #[cfg(test)]
    use crate::edits::EditReplace;
    use crate::history::HistoryEntry;
    use crate::prompt::PromptAssembly;
    use crate::workflow::EditProtocolKind;

    #[test]
    fn test_record_workflow_response_structured_strips_edits() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());
        let raw_json = r#"{
            "message": "Refactored module A",
            "changes": [
                {
                    "op": "replace",
                    "path": "src/a.rs",
                    "old_text": "fn old() {}",
                    "new_text": "fn new() {}",
                    "content": ""
                }
            ],
            "context_requests": [],
            "suggested_actions": []
        }"#;

        let edit_res = ModelResult::Edit {
            summary: "Refactored module A".to_string(),
            edits: vec![EditOperation::Replace(EditReplace {
                path: "src/a.rs".to_string(),
                old_text: "fn old() {}".to_string(),
                new_text: "fn new() {}".to_string(),
            })],
            proposal: None,
            outcome: EditOutcome {
                applied: true,
                error: None,
                changed_files: vec!["src/a.rs".to_string()],
                commit_hash: Some("1234567".to_string()),
                verification: None,
            },
        };

        record_workflow_response(&mut hm, &edit_res, raw_json, vec![]);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            HistoryEntry::Response {
                message,
                commit,
                summary,
                files,
                ..
            } => {
                assert_eq!(message, "Refactored module A");
                assert_eq!(commit.as_deref(), Some("1234567"));
                assert_eq!(summary.as_deref(), Some("Refactored module A"));
                assert_eq!(
                    files,
                    &vec![HistoryFileChange::operated(
                        HistoryFileOperation::Replace,
                        "src/a.rs"
                    )]
                );
            }
            _ => panic!("Expected Response entry"),
        }
    }

    #[test]
    fn test_record_workflow_response_research_concatenation_no_raw_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());
        let raw_research = "{\"message\":\"Need file a.rs\",\"changes\":[],\"context_requests\":[\"editable:a.rs\"],\"suggested_actions\":[]}\n\n{\"message\":\"Updated file a.rs\",\"changes\":[{\"op\":\"replace\",\"path\":\"a.rs\",\"old_text\":\"old\",\"new_text\":\"new\",\"content\":\"\"}],\"context_requests\":[],\"suggested_actions\":[]}";

        let edit_res = ModelResult::Edit {
            summary: "Update file a.rs".to_string(),
            edits: vec![EditOperation::Replace(EditReplace {
                path: "a.rs".to_string(),
                old_text: "old".to_string(),
                new_text: "new".to_string(),
            })],
            proposal: None,
            outcome: EditOutcome {
                applied: true,
                error: None,
                changed_files: vec!["a.rs".to_string()],
                commit_hash: Some("abcdef1".to_string()),
                verification: None,
            },
        };

        record_workflow_response(&mut hm, &edit_res, raw_research, vec![]);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            HistoryEntry::Response { message, files, .. } => {
                assert_eq!(message, "Need file a.rs\n\nUpdated file a.rs");
                assert_eq!(
                    files,
                    &vec![HistoryFileChange::operated(
                        HistoryFileOperation::Replace,
                        "a.rs"
                    )]
                );
                assert!(!message.contains("changes"));
                assert!(!message.contains("old_text"));
            }
            _ => panic!("Expected Response entry"),
        }
    }

    #[test]
    fn test_record_workflow_response_scratchpad_isolated_from_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());

        let raw_assistant = "I found the issue and fixed it.\n<scratchpad>\nWorking hypothesis: broken parser.\nVerified: parser fixed.\n</scratchpad>";
        let edit_res = ModelResult::Edit {
            summary: "Fix parser bug".to_string(),
            edits: vec![EditOperation::Replace(EditReplace {
                path: "src/parser.rs".to_string(),
                old_text: "bad()".to_string(),
                new_text: "good()".to_string(),
            })],
            proposal: Some(crate::llm::ModelResultProposal {
                message: "I found the issue and fixed it.".to_string(),
                ..Default::default()
            }),
            outcome: EditOutcome {
                applied: true,
                error: None,
                changed_files: vec!["src/parser.rs".to_string()],
                commit_hash: Some("1122334".to_string()),
                verification: None,
            },
        };

        record_workflow_response(&mut hm, &edit_res, raw_assistant, vec![]);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            HistoryEntry::Response { message, .. } => {
                assert_eq!(message, "I found the issue and fixed it.");
                assert!(!message.contains("scratchpad"));
                assert!(!message.contains("Working hypothesis"));
            }
            _ => panic!("Expected Response entry"),
        }
    }

    #[test]
    fn test_scratchpad_round_accumulation_and_turn_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());
        let proto = crate::edits::protocol::structured::StructuredEditProtocol;

        // Round 1: Model emits research request and scratchpad note
        let round1_output = serde_json::json!({
            "message": "Need to inspect config.",
            "scratchpad": "- Hypothesis: missing timeout field\n- Goal: inspect config struct",
            "action": {
                "kind": "research",
                "research_requests": [{
                    "kind": "file",
                    "path": "src/config.rs",
                    "access": "read_only",
                    "regex": "",
                    "recursive": false,
                    "topic": "",
                    "id": ""
                }],
                "custom_tools": [],
                "scratchpad": "- Hypothesis: missing timeout field\n- Goal: inspect config struct"
            },
        })
        .to_string();
        let pad1 = proto.parse_scratchpad(&round1_output);
        assert_eq!(
            pad1.as_deref(),
            Some("- Hypothesis: missing timeout field\n- Goal: inspect config struct")
        );

        // Round 2: Prompt assembly injects scratchpad into next round
        let mut assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            EditProtocolKind::Structured,
            None,
            None,
        );
        assembly.turn_scratchpad = pad1;
        let protocol = crate::prompt::json::JsonPromptProtocol::new(
            crate::control::repl::dialogue_capability_expr(false, false),
        );
        let messages = assembly.assemble_chat_messages("Continue", &protocol);
        // The scratchpad is rewritten on every round, so it travels in the
        // volatile tail of the transcript instead of the byte-stable head.
        let head = &messages[0].content;
        assert!(!head.contains("<scratchpad>"));
        let tail = &messages[messages.len() - 2].content;
        assert!(tail.contains("<scratchpad>"));
        assert!(tail.contains("- Hypothesis: missing timeout field"));

        // Round 2 response: Model updates scratchpad with verified conclusion
        let round2_output = serde_json::json!({
            "message": "I have updated the config.",
            "scratchpad": "- Verified: timeout field added\n- Status: ready for test",
            "action": {
                "kind": "edit",
                "changes": [{
                    "op": "replace",
                    "path": "src/config.rs",
                    "to": "",
                    "start": "AAAA",
                    "end": "BBBB",
                    "content": "struct Config {\n    timeout: u64,\n}\n"
                }],
                "outcome": {
                    "status": "success",
                    "commit_message": "feat: add timeout",
                    "message": "",
                    "reason": ""
                }
            },
        })
        .to_string();
        let pad2 = proto.parse_scratchpad(&round2_output);
        assert_eq!(
            pad2.as_deref(),
            Some("- Verified: timeout field added\n- Status: ready for test")
        );

        // Turn completion: assistant text is cleaned and recorded in history
        let cleaned = crate::edits::protocol::structured::clean_raw_json_or_text(&round2_output);
        assert_eq!(cleaned, "I have updated the config.");

        let edit_res = proto.parse_output(
            &crate::control::repl::dialogue_capability_expr(false, false),
            &round2_output,
            &["src/config.rs".to_string()],
            None,
        );
        record_workflow_response(&mut hm, &edit_res, &cleaned, vec![]);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            HistoryEntry::Response { message, files, .. } => {
                assert_eq!(message, "I have updated the config.");
                assert!(!message.contains("scratchpad"));
                assert!(!message.contains("Verified: timeout field"));
                assert_eq!(
                    files,
                    &vec![HistoryFileChange::operated(
                        HistoryFileOperation::Replace,
                        "src/config.rs"
                    )]
                );
            }
            _ => panic!("Expected Response entry"),
        }
    }
}
