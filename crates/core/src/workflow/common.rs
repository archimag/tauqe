use tauqe_protocol::{EditOperation, ModelResult};
use tokio::sync::{mpsc, watch};

use tauqe_protocol::ModelRef;

use crate::history::{HistoryManager, DEFAULT_HISTORY_BUDGET_TOKENS, DEFAULT_TAIL_TURNS_COUNT};
use crate::providers::{ChatMessage, LlmProvider, StreamEvent};

pub use super::discovery::*;

/// Compacts session history if estimated tokens exceed the configured threshold using default budgets.
pub async fn compact_history_if_needed(
    history_manager: &mut HistoryManager,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    compact_history_if_needed_with_budget(
        history_manager,
        provider,
        model,
        cancel_rx,
        DEFAULT_HISTORY_BUDGET_TOKENS,
        DEFAULT_TAIL_TURNS_COUNT,
    )
    .await
}

/// Compacts session history if estimated tokens exceed a specific budget threshold.
pub async fn compact_history_if_needed_with_budget(
    history_manager: &mut HistoryManager,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    cancel_rx: watch::Receiver<bool>,
    budget_tokens: u64,
    tail_turns: usize,
) -> anyhow::Result<()> {
    if !history_manager.needs_compaction(budget_tokens)? {
        return Ok(());
    }

    let Some((head, tail)) = history_manager.prepare_compaction(tail_turns)? else {
        return Ok(());
    };

    let mut head_text = String::new();
    for entry in &head {
        if let Ok(line) = entry.to_jsonl_line() {
            head_text.push_str(&line);
            head_text.push('\n');
        }
    }

    let summarization_prompt = format!(
        "Summarize the following interaction history into a concise, structured summary capturing the completed tasks, key code changes, and context:\n\n```jsonl\n{}\n```\nRespond with a single JSON object matching the enforced schema, placing the summary text in the `summary` field. Do not wrap the JSON in markdown code fences.",
        head_text.trim()
    );

    let messages = vec![
        ChatMessage::system("You are an expert software engineering assistant. Your task is to summarize past development session history concisely and accurately."),
        ChatMessage::user(summarization_prompt),
    ];

    let (tx, mut rx) = mpsc::channel(100);
    let client_call = provider.stream_chat(
        &model.name,
        messages,
        Some(crate::prompt::history_compaction_response_format()),
        tx,
        cancel_rx,
    );

    let collect_task = tokio::spawn(async move {
        let mut text = String::new();
        let mut completed = false;
        let mut had_error = false;
        let mut was_cancelled = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::TextDelta(d) => text.push_str(&d),
                StreamEvent::Done => completed = true,
                StreamEvent::Cancelled => was_cancelled = true,
                StreamEvent::Error(err) => {
                    tracing::warn!("Stream error during history compaction: {}", err);
                    had_error = true;
                }
                _ => {}
            }
        }
        if completed && !had_error && !was_cancelled {
            Some(text)
        } else {
            None
        }
    });

    match client_call.await {
        Ok(_) => {
            let maybe_summary = collect_task.await.map_err(|join_error| {
                tracing::error!(
                    "History compaction task terminated unexpectedly: {}",
                    join_error
                );
                anyhow::anyhow!(
                    "History compaction task terminated unexpectedly: {}",
                    join_error
                )
            })?;
            if let Some(summary_text) = maybe_summary {
                match crate::prompt::parse_compaction_summary(&summary_text) {
                    Some(summary) => history_manager.apply_compaction(&summary, tail)?,
                    None => {
                        tracing::warn!(
                            "History compaction produced no recoverable summary; skipping compaction to preserve history"
                        );
                    }
                }
            } else {
                tracing::warn!(
                    "History compaction stream did not complete cleanly; skipping compaction to preserve history"
                );
            }
        }
        Err(err) => {
            tracing::warn!("Failed to summarize history during compaction: {}", err);
        }
    }

    Ok(())
}

/// Records workflow response into HistoryManager based on the final execution result.
pub fn record_workflow_response(
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
            if let Err(err) = history_manager.record_response(msg, None, summary, extra_files) {
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

            let mut seen = std::collections::HashSet::new();
            let mut files = Vec::new();
            for edit in edits {
                let entry = match edit {
                    EditOperation::Create { path, .. } => format!("[NEW] {}", path),
                    EditOperation::Delete { path } => format!("[DEL] {}", path),
                    EditOperation::Replace { path, .. } => format!("[EDIT] {}", path),
                    EditOperation::Overwrite { path, .. } => format!("[OVERWRITE] {}", path),
                    EditOperation::Move { from, to } => format!("[MOVE] {} -> {}", from, to),
                };
                if seen.insert(entry.clone()) {
                    files.push(entry);
                }
            }

            for extra in extra_files {
                if seen.insert(extra.clone()) {
                    files.push(extra);
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
    use crate::prompt::PromptAssembly;

    #[tokio::test]
    async fn test_compaction_aborted_on_stream_error_preserves_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());
        hm.record_turn("Prompt 1", vec![]).unwrap();
        hm.record_response("Resp 1".to_string(), None, None, vec![])
            .unwrap();
        hm.record_turn("Prompt 2", vec![]).unwrap();
        hm.record_response("Resp 2".to_string(), None, None, vec![])
            .unwrap();

        let provider = crate::providers::mock::MockLlmProvider::scripted([
            crate::providers::mock::MockResponse::error("Connection reset"),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let res =
            compact_history_if_needed_with_budget(&mut hm, &provider, &model, cancel_rx, 1, 1)
                .await;
        assert!(res.is_ok());

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 4);
        assert!(!matches!(
            entries[0],
            crate::history::entry::HistoryEntry::Summary { .. }
        ));
    }

    #[tokio::test]
    async fn test_compaction_aborted_on_cancellation_preserves_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());
        hm.record_turn("Prompt 1", vec![]).unwrap();
        hm.record_response("Resp 1".to_string(), None, None, vec![])
            .unwrap();
        hm.record_turn("Prompt 2", vec![]).unwrap();
        hm.record_response("Resp 2".to_string(), None, None, vec![])
            .unwrap();

        let provider = crate::providers::mock::MockLlmProvider::scripted([
            crate::providers::mock::MockResponse::text("Partial summary that was cut off"),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let (_cancel_tx, cancel_rx) = watch::channel(true);

        let res =
            compact_history_if_needed_with_budget(&mut hm, &provider, &model, cancel_rx, 1, 1)
                .await;
        assert!(res.is_ok());

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 4);
        assert!(!matches!(
            entries[0],
            crate::history::entry::HistoryEntry::Summary { .. }
        ));
    }

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
            edits: vec![EditOperation::Replace {
                path: "src/a.rs".to_string(),
                old_text: "fn old() {}".to_string(),
                new_text: "fn new() {}".to_string(),
            }],
            proposal: None,
            outcome: tauqe_protocol::EditOutcome {
                applied: true,
                error: None,
                changed_files: vec!["src/a.rs".to_string()],
                commit_hash: Some("1234567".to_string()),
            },
        };

        record_workflow_response(&mut hm, &edit_res, raw_json, vec![]);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            crate::history::entry::HistoryEntry::Response {
                message,
                commit,
                summary,
                files,
                ..
            } => {
                assert_eq!(message, "Refactored module A");
                assert_eq!(commit.as_deref(), Some("1234567"));
                assert_eq!(summary.as_deref(), Some("Refactored module A"));
                assert_eq!(files, &vec!["[EDIT] src/a.rs"]);
            }
            _ => panic!("Expected Response entry"),
        }
    }

    #[test]
    fn test_record_workflow_response_discovery_concatenation_no_raw_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());
        let raw_discovery = "{\"message\":\"Need file a.rs\",\"changes\":[],\"context_requests\":[\"editable:a.rs\"],\"suggested_actions\":[]}\n\n{\"message\":\"Updated file a.rs\",\"changes\":[{\"op\":\"replace\",\"path\":\"a.rs\",\"old_text\":\"old\",\"new_text\":\"new\",\"content\":\"\"}],\"context_requests\":[],\"suggested_actions\":[]}";

        let edit_res = ModelResult::Edit {
            summary: "Update file a.rs".to_string(),
            edits: vec![EditOperation::Replace {
                path: "a.rs".to_string(),
                old_text: "old".to_string(),
                new_text: "new".to_string(),
            }],
            proposal: None,
            outcome: tauqe_protocol::EditOutcome {
                applied: true,
                error: None,
                changed_files: vec!["a.rs".to_string()],
                commit_hash: Some("abcdef1".to_string()),
            },
        };

        record_workflow_response(&mut hm, &edit_res, raw_discovery, vec![]);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            crate::history::entry::HistoryEntry::Response { message, files, .. } => {
                assert_eq!(message, "Need file a.rs\n\nUpdated file a.rs");
                assert_eq!(files, &vec!["[EDIT] a.rs"]);
                assert!(!message.contains("changes"));
                assert!(!message.contains("old_text"));
            }
            _ => panic!("Expected Response entry"),
        }
    }

    #[test]
    fn test_context_request_preserved_before_clean() {
        let proto = crate::edits::XmlEditProtocol;
        let raw = "I need files.\n<context_request path=\"crates/tui/src/markdown.rs\" access=\"read_only\" />\n";
        let reqs = proto.parse_context_requests(raw);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].path, "crates/tui/src/markdown.rs");

        let cleaned = proto.clean_assistant_text(raw);
        assert!(!cleaned.contains("<context_request"));
        assert!(proto.parse_context_requests(&cleaned).is_empty());
    }

    #[test]
    fn test_record_workflow_response_scratchpad_isolated_from_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut hm = HistoryManager::new(dir.path().to_path_buf());

        let raw_assistant = "I found the issue and fixed it.\n<scratchpad>\nWorking hypothesis: broken parser.\nVerified: parser fixed.\n</scratchpad>";
        let edit_res = ModelResult::Edit {
            summary: "Fix parser bug".to_string(),
            edits: vec![EditOperation::Replace {
                path: "src/parser.rs".to_string(),
                old_text: "bad()".to_string(),
                new_text: "good()".to_string(),
            }],
            proposal: Some(tauqe_protocol::ModelResultProposal {
                message: "I found the issue and fixed it.".to_string(),
                ..Default::default()
            }),
            outcome: tauqe_protocol::EditOutcome {
                applied: true,
                error: None,
                changed_files: vec!["src/parser.rs".to_string()],
                commit_hash: Some("1122334".to_string()),
            },
        };

        record_workflow_response(&mut hm, &edit_res, raw_assistant, vec![]);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            crate::history::entry::HistoryEntry::Response { message, .. } => {
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
        let proto = crate::edits::XmlEditProtocol;

        // Round 1: Model emits discovery request and scratchpad note
        let round1_output = "Need to inspect config.\n<context_request path=\"src/config.rs\" access=\"read_only\" />\n<scratchpad>\n- Hypothesis: missing timeout field\n- Goal: inspect config struct\n</scratchpad>";
        let pad1 = proto.parse_scratchpad(round1_output);
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
            tauqe_protocol::EditProtocolKind::Xml,
            None,
            None,
        );
        assembly.turn_scratchpad = pad1;
        let messages =
            assembly.assemble_chat_messages("Continue", &crate::prompt::xml::XmlPromptProtocol);
        let prompt_text = &messages[0].content;
        assert!(prompt_text.contains("<scratchpad>"));
        assert!(prompt_text.contains("- Hypothesis: missing timeout field"));

        // Round 2 response: Model updates scratchpad with verified conclusion
        let round2_output = "I have updated the config.\n<tauqe_edits summary=\"Add timeout field\">\n<edit path=\"src/config.rs\">\n<search>\nstruct Config {}\n</search>\n<replace>\nstruct Config {\n    timeout: u64,\n}\n</replace>\n</edit>\n</tauqe_edits>\n<scratchpad>\n- Verified: timeout field added\n- Status: ready for test\n</scratchpad>";
        let pad2 = proto.parse_scratchpad(round2_output);
        assert_eq!(
            pad2.as_deref(),
            Some("- Verified: timeout field added\n- Status: ready for test")
        );

        // Turn completion: assistant text is cleaned and recorded in history
        let cleaned = proto.clean_assistant_text(round2_output);
        assert!(!cleaned.contains("<scratchpad"));
        assert!(!cleaned.contains("Verified: timeout field added"));
        assert!(cleaned.contains("I have updated the config."));

        let edit_res = proto.parse_output(round2_output, &["src/config.rs".to_string()]);
        record_workflow_response(&mut hm, &edit_res, &cleaned, vec![]);

        let entries = hm.get_entries().unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            crate::history::entry::HistoryEntry::Response { message, files, .. } => {
                assert!(!message.contains("scratchpad"));
                assert!(!message.contains("Verified: timeout field"));
                assert_eq!(files, &vec!["[EDIT] src/config.rs"]);
            }
            _ => panic!("Expected Response entry"),
        }
    }
}
