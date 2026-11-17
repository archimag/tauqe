//! Session journal maintenance of the control contour.
//!
//! The journal is the durable record a bounded turn reads and every step of a
//! batch extends. Two operations belong to that lifetime: keeping the session
//! inside the configured token budget by folding its summarized prefix into one
//! record, and recording the outcome of a finished turn.
//!
//! Both live in the control contour because the loops are what produce the
//! material they record, and the budget they apply is the project's, never a
//! compile-time constant (`crates/core/Conventions.md` §3.5).

use tokio::sync::{mpsc, watch};

use tauqe_protocol::ModelRef;
use tauqe_protocol::{
    EditCreate, EditDelete, EditFileOp, EditLineAnchor, EditMove, EditMoveDir, EditOperation,
    EditOverwrite, EditReplace, HistoryFileChange, ModelResult,
};

use crate::history::{HistoryManager, DEFAULT_HISTORY_BUDGET_TOKENS, DEFAULT_TAIL_TURNS_COUNT};
use crate::providers::{ChatMessage, LlmProvider, StreamEvent};

/// Compacts session history if estimated tokens exceed the configured threshold using
/// the default budgets.
///
/// A caller that owns the project configuration applies the budget the project
/// declares through [`compact_history_if_needed_with_budget`] instead; this entry
/// point exists for a caller that carries no configuration of its own.
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
///
/// The Scribe summarizes the prefix the partition cuts off and the retained tail
/// is preserved whole, so a card a turn recorded on both sides of its attempts —
/// the turn with the rounds it ran and the answer that closed it — survives the
/// compaction. A stream that fails or is cancelled leaves the journal untouched
/// instead of replacing it with an empty summary.
pub async fn compact_history_if_needed_with_budget(
    history_manager: &mut HistoryManager,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    cancel_rx: watch::Receiver<bool>,
    budget_tokens: u64,
    tail_turns: usize,
) -> anyhow::Result<()> {
    if !history_manager.needs_compaction(budget_tokens) {
        return Ok(());
    }

    let Some((head, tail)) = history_manager.prepare_compaction(tail_turns)? else {
        return Ok(());
    };

    let summarization_prompt = crate::history::compaction::build_compaction_request(&head);

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

/// Records a finished turn into the session journal according to its result.
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
                        HistoryFileChange::operated(EditFileOp::Create, path.clone())
                    }
                    EditOperation::Delete(EditDelete { path }) => {
                        HistoryFileChange::operated(EditFileOp::Delete, path.clone())
                    }
                    EditOperation::Replace(EditReplace { path, .. }) => {
                        HistoryFileChange::operated(EditFileOp::Replace, path.clone())
                    }
                    EditOperation::LineAnchor(EditLineAnchor { path, .. }) => {
                        HistoryFileChange::operated(EditFileOp::Replace, path.clone())
                    }
                    EditOperation::Overwrite(EditOverwrite { path, .. }) => {
                        HistoryFileChange::operated(EditFileOp::Overwrite, path.clone())
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
    use crate::history::HistoryEntry;
    use crate::prompt::PromptAssembly;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_protocol::{
        EditFileOp, EditOperation, EditOutcome, EditProtocolKind, EditReplace, HistoryFileChange,
        ModelRef,
    };
    use tokio::sync::watch;

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

    /// A dialogue reads the journal it is about to extend, so a session that
    /// outgrew the configured budget is folded into one summary record ahead of
    /// the turn while the retained tail keeps the latest exchange.
    #[tokio::test]
    async fn an_over_budget_dialogue_journal_is_compacted_before_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = HistoryManager::new(dir.path().to_path_buf());
        for index in 1..=4 {
            history
                .record_turn(format!("Task {index}"), vec!["src/lib.rs".to_string()])
                .unwrap();
            history
                .record_response(format!("Done {index}"), None, None, vec![])
                .unwrap();
        }
        let before = history.estimated_tokens();
        let budget = before / 2;
        assert!(history.needs_compaction(budget));

        let provider = MockLlmProvider::scripted([MockResponse::text(
            r#"{"summary":"Tasks 1 through 3 completed and verified."}"#,
        )]);
        let model = ModelRef::openrouter("mock/scribe");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        compact_history_if_needed_with_budget(
            &mut history,
            &provider,
            &model,
            cancel_rx,
            budget,
            1,
        )
        .await
        .expect("compaction runs");

        let entries = history.get_entries().unwrap();
        assert_eq!(entries.len(), 3, "one summary and the retained exchange");
        match &entries[0] {
            HistoryEntry::Summary { text, .. } => {
                assert_eq!(text, "Tasks 1 through 3 completed and verified.")
            }
            other => panic!("the summarized prefix folds into a summary, got {other:?}"),
        }
        assert!(matches!(entries[1], HistoryEntry::Turn { .. }));
        assert!(matches!(entries[2], HistoryEntry::Response { .. }));
        assert!(
            !history.needs_compaction(budget),
            "the compacted journal must land inside the budget"
        );
    }

    /// A batch executes many steps against one journal, so the run compacts it
    /// between steps. The two-sided card a step records — the turn with the
    /// rounds it ran and the answer that closed it — is durable evidence, so a
    /// retained card survives whole and the journal lands inside the budget.
    #[tokio::test]
    async fn a_budgeted_step_batch_keeps_both_sided_cards() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::history::SessionStorage::new(dir.path().to_path_buf());
        for (card_id, prompt) in [(1u64, "Implement step one"), (3u64, "Implement step two")] {
            storage
                .append_entry(&HistoryEntry::Turn {
                    id: Some(card_id),
                    prompt: prompt.to_string(),
                    context: vec!["src/lib.rs".to_string()],
                    context_watermark: Some(1),
                    rounds: vec![crate::history::entry::round_record(
                        1,
                        "Inspected the module and staged the change.",
                        "verification passed",
                    )],
                })
                .unwrap();
            storage
                .append_entry(&HistoryEntry::Response {
                    id: Some(card_id + 1),
                    message: format!("{prompt} closed."),
                    commit: Some("abc1234".to_string()),
                    summary: Some(prompt.to_string()),
                    files: vec![HistoryFileChange::operated(
                        EditFileOp::Replace,
                        "src/lib.rs",
                    )],
                })
                .unwrap();
        }
        let mut history = HistoryManager::new(dir.path().to_path_buf());
        let before = history.estimated_tokens();
        let budget = before - before / 4;
        assert!(history.needs_compaction(budget));

        let provider = MockLlmProvider::scripted([MockResponse::text(
            r#"{"summary":"Step one completed and verified."}"#,
        )]);
        let model = ModelRef::openrouter("mock/scribe");
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        compact_history_if_needed_with_budget(
            &mut history,
            &provider,
            &model,
            cancel_rx,
            budget,
            1,
        )
        .await
        .expect("compaction runs");

        assert!(
            !history.needs_compaction(budget),
            "the step journal must land inside the budget"
        );
        let entries = history.get_entries().unwrap();
        assert_eq!(entries.len(), 3, "one summary and the retained card");
        assert!(matches!(entries[0], HistoryEntry::Summary { .. }));
        match &entries[1] {
            HistoryEntry::Turn { prompt, rounds, .. } => {
                assert_eq!(prompt, "Implement step two");
                assert_eq!(rounds.len(), 1, "the retained card keeps its rounds");
                assert_eq!(rounds[0].feedback, "verification passed");
            }
            other => panic!("expected the retained turn card, got {other:?}"),
        }
        match &entries[2] {
            HistoryEntry::Response {
                message, commit, ..
            } => {
                assert_eq!(message, "Implement step two closed.");
                assert_eq!(commit.as_deref(), Some("abc1234"));
            }
            other => panic!("expected the retained answer card, got {other:?}"),
        }
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
                    &vec![HistoryFileChange::operated(EditFileOp::Replace, "src/a.rs")]
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
                    &vec![HistoryFileChange::operated(EditFileOp::Replace, "a.rs")]
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
            proposal: Some(tauqe_protocol::ModelResultProposal {
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
        let messages = assembly.assemble_chat_messages(
            "Continue",
            &crate::prompt::json::JsonPromptProtocol::default(),
        );
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
                        EditFileOp::Replace,
                        "src/config.rs"
                    )]
                );
            }
            _ => panic!("Expected Response entry"),
        }
    }
}
