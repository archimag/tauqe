use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::{ContextAccess, EditOperation, ModelResult};

use crate::context::{ContextFileContent, ContextManager};
use crate::edits::stream::JsonStreamFilter;
use crate::edits::{
    stage_and_validate_edits, EditError, EditProtocol, StagedEditsState, XmlStreamFilter,
};
use crate::history::{HistoryManager, DEFAULT_HISTORY_BUDGET_TOKENS, DEFAULT_TAIL_TURNS_COUNT};
use crate::model::gateway::{ChatMessage, StreamEvent};
use crate::model::openrouter::OpenRouterClient;
use crate::prompt::PromptAssembly;

pub struct ParsedPipelineOutput {
    pub assistant_text: String,
    pub parsed_result: ModelResult,
    pub completed_tool_calls: Vec<crate::model::gateway::ToolCall>,
    pub is_cancelled: bool,
}

/// Compacts session history if estimated tokens exceed the configured threshold.
pub async fn compact_history_if_needed(
    history_manager: &mut HistoryManager,
    client: &OpenRouterClient,
    model_name: &str,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    if !history_manager.needs_compaction(DEFAULT_HISTORY_BUDGET_TOKENS)? {
        return Ok(());
    }

    let Some((head, tail)) = history_manager.prepare_compaction(DEFAULT_TAIL_TURNS_COUNT)? else {
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
        "Summarize the following interaction history into a concise, structured summary capturing the completed tasks, key code changes, and context:\n\n```jsonl\n{}\n```\nProvide only the summary text without any JSON or XML wrapper.",
        head_text.trim()
    );

    let messages = vec![
        ChatMessage::system("You are an expert software engineering assistant. Your task is to summarize past development session history concisely and accurately."),
        ChatMessage::user(summarization_prompt),
    ];

    let (tx, mut rx) = mpsc::channel(100);
    let client_call = client.stream_chat_with_tools(
        model_name,
        messages,
        None,
        None,
        tx,
        cancel_rx,
    );

    let collect_task = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(ev) = rx.recv().await {
            if let StreamEvent::TextDelta(d) = ev {
                text.push_str(&d);
            }
        }
        text
    });

    match client_call.await {
        Ok(_) => {
            let summary_text = collect_task.await.unwrap_or_default();
            let trimmed = summary_text.trim();
            if !trimmed.is_empty() {
                history_manager.apply_compaction(trimmed, tail)?;
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
) {
    match result {
        ModelResult::Answer { text } => {
            let msg = if !text.trim().is_empty() {
                text.as_str()
            } else if !assistant_text.trim().is_empty() {
                assistant_text
            } else {
                "Answered."
            };
            let _ = history_manager.record_response(msg, None, None, vec![]);
        }
        ModelResult::Edit {
            summary,
            edits,
            proposal,
            applied,
            commit_hash,
            ..
        } => {
            let msg = proposal
                .as_ref()
                .map(|p| p.message.trim())
                .filter(|m| !m.is_empty())
                .map(|m| m.to_string())
                .unwrap_or_else(|| {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(assistant_text.trim()) {
                        if let Some(msg_field) = val.get("message").and_then(|m| m.as_str()) {
                            let trimmed = msg_field.trim();
                            if !trimmed.is_empty() {
                                return trimmed.to_string();
                            }
                        }
                    }
                    let extracted =
                        crate::edits::protocol::xml::extract_conversational_text(assistant_text);
                    if !extracted.trim().is_empty() {
                        extracted
                    } else {
                        summary.clone()
                    }
                });

            let mut seen = std::collections::HashSet::new();
            let mut files = Vec::new();
            for edit in edits {
                let entry = match edit {
                    EditOperation::Create { path, .. } => format!("[NEW] {}", path),
                    EditOperation::Delete { path } => format!("[DEL] {}", path),
                    EditOperation::Replace { path, .. } => format!("[EDIT] {}", path),
                };
                if seen.insert(entry.clone()) {
                    files.push(entry);
                }
            }

            let commit = if *applied {
                commit_hash.clone()
            } else {
                None
            };

            let _ = history_manager.record_response(
                &msg,
                commit,
                Some(summary.clone()),
                files,
            );
        }
    }
}

/// Formats a human-readable failure reason for `EditFileRetrying` event and prompts.
pub fn format_patch_retry_reason(err: &EditError) -> String {
    match err {
        EditError::NoMatch { .. } => {
            "Search block not found (0 matches). Check indentation and line endings.".to_string()
        }
        EditError::AmbiguousMatch { count, .. } => {
            format!(
                "Search block matches {} times. Provide more unique context.",
                count
            )
        }
        other => other.to_string(),
    }
}

/// Constructs a specialized prompt asking the model to fix failed edit blocks.
pub fn build_patch_retry_prompt(
    succeeded_files: &std::collections::HashSet<String>,
    failed_files: &std::collections::HashMap<String, EditError>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Some of the proposed edits failed to apply.\n\n");

    if !succeeded_files.is_empty() {
        let mut succ_sorted: Vec<&String> = succeeded_files.iter().collect();
        succ_sorted.sort();
        prompt.push_str("Successfully applied edits for:\n");
        for path in succ_sorted {
            prompt.push_str(&format!("- {}\n", path));
        }
        prompt.push_str("(Do NOT regenerate changes for the above files).\n\n");
    }

    prompt.push_str("Failed files and errors:\n");
    let mut failed_sorted: Vec<(&String, &EditError)> = failed_files.iter().collect();
    failed_sorted.sort_by_key(|(p, _)| *p);
    for (path, err) in failed_sorted {
        let explanation = match err {
            EditError::NoMatch { .. } => {
                "Search block not found (0 matches). Verify exact line content, indentation, and context."
            }
            EditError::AmbiguousMatch { count, .. } => {
                &format!(
                    "Search block matches {} times. Include more surrounding lines to make the match uniquely identifiable.",
                    count
                )
            }
            other => &other.to_string(),
        };
        prompt.push_str(&format!("- `{}`: {}\n", path, explanation));
    }

    prompt.push_str("\nPlease provide corrected edit blocks ONLY for the failed files listed above.");
    prompt
}

/// Common execution pipeline for streaming, XML filtering, parsing model response,
/// and automatically recovering from failed search/replace blocks using an in-memory retry loop.
pub async fn execute_edit_pipeline(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    max_retries: usize,
) -> anyhow::Result<ParsedPipelineOutput> {
    execute_edit_pipeline_with_turn(
        prompt,
        client,
        model_name,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx,
        cancel_rx,
        max_retries,
        true,
    )
    .await
}

/// Full execution pipeline with configurable turn recording and patch retry loop.
pub async fn execute_edit_pipeline_with_turn(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    max_retries: usize,
    record_turn: bool,
) -> anyhow::Result<ParsedPipelineOutput> {
    // 1. Initial attempt
    let mut pipeline_out = execute_edit_pipeline_step_opt(
        prompt,
        client,
        model_name,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx.clone(),
        cancel_rx.clone(),
        record_turn,
        false,
        None,
    )
    .await?;

    if pipeline_out.is_cancelled {
        let _ = stream_tx.send(StreamEvent::Done).await;
        return Ok(pipeline_out);
    }

    let repo_root = context_manager.repo_root().to_path_buf();

    // Check if initial output has edits to validate
    let (mut current_summary, mut current_edits, initial_proposal) = match &pipeline_out.parsed_result {
        ModelResult::Edit {
            summary,
            edits,
            proposal,
            error: None,
            ..
        } if !edits.is_empty() => (summary.clone(), edits.clone(), proposal.clone()),
        _ => {
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(pipeline_out);
        }
    };

    let mut staged_state: Option<StagedEditsState> = None;
    let mut cumulative_successful_edits: Vec<EditOperation> = Vec::new();
    let mut succeeded_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_failed_errors: std::collections::HashMap<String, EditError> =
        std::collections::HashMap::new();
    let mut attempt = 0;

    loop {
        let stage_res = stage_and_validate_edits(
            &repo_root,
            context_manager,
            &current_edits,
            staged_state.as_ref(),
        );

        staged_state = Some(stage_res.staged_state);
        succeeded_files.extend(stage_res.successful_paths);
        cumulative_successful_edits.extend(stage_res.successful_edits);

        // Remove any files that succeeded in this attempt from failed list
        for path in &succeeded_files {
            last_failed_errors.remove(path);
        }
        for failed in stage_res.failed_files {
            last_failed_errors.insert(failed.path, failed.error);
        }

        if last_failed_errors.is_empty() {
            // All files valid!
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(ParsedPipelineOutput {
                assistant_text: pipeline_out.assistant_text,
                parsed_result: ModelResult::Edit {
                    summary: current_summary,
                    edits: cumulative_successful_edits,
                    proposal: initial_proposal,
                    applied: false,
                    error: None,
                    changed_files: Vec::new(),
                    commit_hash: None,
                },
                completed_tool_calls: pipeline_out.completed_tool_calls,
                is_cancelled: false,
            });
        }

        if attempt >= max_retries {
            break;
        }

        attempt += 1;

        // Emit EditFileRetrying for each failing file
        for (path, err) in &last_failed_errors {
            let reason = format_patch_retry_reason(err);
            let _ = stream_tx
                .send(StreamEvent::EditFileRetrying {
                    path: path.clone(),
                    attempt,
                    max_retries,
                    reason,
                })
                .await;
        }

        let repair_prompt = build_patch_retry_prompt(&succeeded_files, &last_failed_errors);

        let step_out = execute_edit_pipeline_step_opt(
            &repair_prompt,
            client,
            model_name,
            context_manager,
            history_manager,
            protocol,
            workflow_name,
            stream_tx.clone(),
            cancel_rx.clone(),
            false,
            false,
            staged_state.as_ref(),
        )
        .await?;

        if step_out.is_cancelled {
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(step_out);
        }

        pipeline_out.assistant_text = step_out.assistant_text;
        pipeline_out.completed_tool_calls = step_out.completed_tool_calls;

        match step_out.parsed_result {
            ModelResult::Edit { summary, edits, .. } => {
                if !summary.is_empty() {
                    current_summary = summary;
                }
                current_edits = edits;
            }
            ModelResult::Answer { .. } => {
                current_edits = Vec::new();
            }
        }
    }

    // Exhausted retries with remaining errors
    let mut err_lines = Vec::new();
    let mut failed_sorted: Vec<(&String, &EditError)> = last_failed_errors.iter().collect();
    failed_sorted.sort_by_key(|(p, _)| *p);
    for (path, err) in failed_sorted {
        err_lines.push(format!("{}: {}", path, err));
    }
    let error_desc = format!(
        "Failed to apply edits after {} retries:\n{}",
        max_retries,
        err_lines.join("\n")
    );

    let _ = stream_tx.send(StreamEvent::Done).await;

    Ok(ParsedPipelineOutput {
        assistant_text: pipeline_out.assistant_text,
        parsed_result: ModelResult::Edit {
            summary: current_summary,
            edits: cumulative_successful_edits,
            proposal: initial_proposal,
            applied: false,
            error: Some(error_desc),
            changed_files: Vec::new(),
            commit_hash: None,
        },
        completed_tool_calls: pipeline_out.completed_tool_calls,
        is_cancelled: false,
    })
}

/// Execution pipeline step with optional user turn recording (used by auto-healing loops).
pub async fn execute_edit_pipeline_step(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    record_turn: bool,
) -> anyhow::Result<ParsedPipelineOutput> {
    execute_edit_pipeline_step_opt(
        prompt,
        client,
        model_name,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx,
        cancel_rx,
        record_turn,
        true,
        None,
    )
    .await
}

/// Internal pipeline step with control over emitting the final StreamEvent::Done and staged context overlay.
pub async fn execute_edit_pipeline_step_opt(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    record_turn: bool,
    emit_done: bool,
    staged_state: Option<&StagedEditsState>,
) -> anyhow::Result<ParsedPipelineOutput> {
    let repo_state = crate::git::get_repository_state(Some(context_manager.repo_root()));
    let ctx_state = context_manager.get_state();
    let mut context_files = context_manager.read_context_files();

    if let Some(staged) = staged_state {
        for (path, content) in &staged.staged_files {
            if let Some(f) = context_files.iter_mut().find(|cf| &cf.path == path) {
                f.content = content.clone();
            } else {
                context_files.push(ContextFileContent {
                    path: path.clone(),
                    access: ContextAccess::Editable,
                    content: content.clone(),
                });
            }
        }
        context_files.retain(|f| !staged.deleted_paths.contains(&f.path));
    }

    if record_turn {
        let _ = compact_history_if_needed(history_manager, client, model_name, cancel_rx.clone()).await;
        let context_paths: Vec<String> = ctx_state.items.iter().map(|it| it.path.clone()).collect();
        let _ = history_manager.record_turn(prompt, context_paths);
    }

    let history_tag = history_manager.format_history_tag().ok();

    let editable_paths: Vec<String> = ctx_state
        .items
        .iter()
        .filter(|it| it.access == ContextAccess::Editable)
        .map(|it| it.path.clone())
        .collect();

    let repo_root = context_manager.repo_root().to_path_buf();
    // 1. Build prompt layering
    let assembly = PromptAssembly::new(
        Some(repo_state),
        ctx_state.revision,
        context_files,
        workflow_name,
        protocol.name(),
        history_tag,
    );

    let assembled_messages = assembly.assemble_chat_messages(prompt, protocol);

    // 2. Stream response from model
    let (llm_tx, mut llm_rx) = mpsc::channel::<StreamEvent>(100);

    // Code edits are no longer delivered via native tool calls; no protocol-specific tools.
    let tools: Option<Vec<crate::model::gateway::ToolDefinition>> = None;

    let response_format = protocol.response_format(&editable_paths);
    let is_structured = protocol.name() == "structured";

    let model_str = model_name.to_string();
    let client_call = client.stream_chat_with_tools(
        &model_str,
        assembled_messages.clone(),
        tools.clone(),
        response_format,
        llm_tx,
        cancel_rx.clone(),
    );

    let editable_paths_for_filter = editable_paths.clone();
    let staged_for_filter = staged_state
        .map(|s| s.staged_files.clone())
        .unwrap_or_default();
    let forward_stream_tx = stream_tx.clone();
    let has_emitted_edits = Arc::new(AtomicBool::new(false));
    let has_emitted_edits_forward = has_emitted_edits.clone();

    let forward_task = tokio::spawn(async move {
        let mut assistant_text = String::new();
        let mut cancelled = false;

        if is_structured {
            let mut json_filter = JsonStreamFilter::new(editable_paths_for_filter, repo_root)
                .with_staged_contents(staged_for_filter);
            while let Some(event) = llm_rx.recv().await {
                match event {
                    StreamEvent::TextDelta(ref delta) => {
                        assistant_text.push_str(delta);
                        let filtered_events = json_filter.push_chunk(delta);
                        for ev in filtered_events {
                            if matches!(
                                ev,
                                StreamEvent::EditStarted
                                    | StreamEvent::EditFileStarted { .. }
                                    | StreamEvent::EditHunk { .. }
                            ) {
                                has_emitted_edits_forward.store(true, Ordering::SeqCst);
                            }
                            if forward_stream_tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                    }
                    StreamEvent::Cancelled => {
                        cancelled = true;
                        let _ = forward_stream_tx.send(StreamEvent::Cancelled).await;
                    }
                    StreamEvent::Done => {}
                    other => {
                        if matches!(
                            other,
                            StreamEvent::EditStarted
                                | StreamEvent::EditFileStarted { .. }
                                | StreamEvent::EditHunk { .. }
                        ) {
                            has_emitted_edits_forward.store(true, Ordering::SeqCst);
                        }
                        if forward_stream_tx.send(other).await.is_err() {
                            break;
                        }
                    }
                }
            }
            for ev in json_filter.finish() {
                if matches!(
                    ev,
                    StreamEvent::EditStarted
                        | StreamEvent::EditFileStarted { .. }
                        | StreamEvent::EditHunk { .. }
                ) {
                    has_emitted_edits_forward.store(true, Ordering::SeqCst);
                }
                let _ = forward_stream_tx.send(ev).await;
            }
        } else {
            let mut stream_filter = XmlStreamFilter::new(editable_paths_for_filter, repo_root)
                .with_staged_contents(staged_for_filter);

            while let Some(event) = llm_rx.recv().await {
                match event {
                    StreamEvent::TextDelta(ref delta) => {
                        assistant_text.push_str(delta);
                        let filtered_events = stream_filter.push_chunk(delta);
                        for ev in filtered_events {
                            if matches!(
                                ev,
                                StreamEvent::EditStarted
                                    | StreamEvent::EditFileStarted { .. }
                                    | StreamEvent::EditHunk { .. }
                            ) {
                                has_emitted_edits_forward.store(true, Ordering::SeqCst);
                            }
                            if forward_stream_tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                    }
                    StreamEvent::Cancelled => {
                        cancelled = true;
                        let _ = forward_stream_tx.send(StreamEvent::Cancelled).await;
                    }
                    StreamEvent::Done => {
                        // Suppress early Done from the first call so receiver does not terminate early
                    }
                    other => {
                        if matches!(
                            other,
                            StreamEvent::EditStarted
                                | StreamEvent::EditFileStarted { .. }
                                | StreamEvent::EditHunk { .. }
                        ) {
                            has_emitted_edits_forward.store(true, Ordering::SeqCst);
                        }
                        if forward_stream_tx.send(other).await.is_err() {
                            break;
                        }
                    }
                }
            }

            // Flush remaining events
            for ev in stream_filter.finish() {
                if matches!(
                    ev,
                    StreamEvent::EditStarted
                        | StreamEvent::EditFileStarted { .. }
                        | StreamEvent::EditHunk { .. }
                ) {
                    has_emitted_edits_forward.store(true, Ordering::SeqCst);
                }
                let _ = forward_stream_tx.send(ev).await;
            }
        }

        (assistant_text, cancelled)
    });

    let client_res = client_call.await;
    let (assistant_text, cancelled) = forward_task.await.unwrap_or_default();

    let completed_tool_calls = client_res?;

    if cancelled {
        return Ok(ParsedPipelineOutput {
            assistant_text: assistant_text.clone(),
            parsed_result: ModelResult::Answer {
                text: assistant_text,
            },
            completed_tool_calls,
            is_cancelled: true,
        });
    }

    // 3. Parse output according to active protocol
    let parsed_result = protocol.parse_output(&assistant_text, &editable_paths);

    // Invariant: once edit events were streamed to the client, the result must never
    // degrade to a plain Answer; surface it as a failed Edit so workflows/clients finalize it.
    let parsed_result = if has_emitted_edits.load(Ordering::SeqCst) {
        match parsed_result {
            ModelResult::Answer { .. } => ModelResult::Edit {
                summary: "Edits could not be finalized".to_string(),
                edits: Vec::new(),
                proposal: None,
                applied: false,
                error: Some(
                    "Edits were streamed but the final model response could not be parsed into a valid edit proposal"
                        .to_string(),
                ),
                changed_files: Vec::new(),
                commit_hash: None,
            },
            other => other,
        }
    } else {
        parsed_result
    };

    // If streaming parser did not emit edit events in real time, emit fallback events now
    if !has_emitted_edits.load(Ordering::SeqCst) {
        if let ModelResult::Edit { ref edits, .. } = parsed_result {
            emit_semantic_events_for_edits(edits, &stream_tx).await;
        }
    }

    // Emit final Done event for this execution if requested
    if emit_done {
        let _ = stream_tx.send(StreamEvent::Done).await;
    }

    Ok(ParsedPipelineOutput {
        assistant_text,
        parsed_result,
        completed_tool_calls,
        is_cancelled: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn test_build_patch_retry_prompt_format() {
        let mut succeeded = HashSet::new();
        succeeded.insert("crates/core/src/lib.rs".to_string());

        let mut failed = HashMap::new();
        failed.insert(
            "crates/core/src/main.rs".to_string(),
            EditError::NoMatch {
                path: "crates/core/src/main.rs".to_string(),
            },
        );
        failed.insert(
            "crates/core/src/utils.rs".to_string(),
            EditError::AmbiguousMatch {
                path: "crates/core/src/utils.rs".to_string(),
                count: 3,
            },
        );

        let prompt = build_patch_retry_prompt(&succeeded, &failed);

        assert!(prompt.contains("Successfully applied edits for:"));
        assert!(prompt.contains("- crates/core/src/lib.rs"));
        assert!(prompt.contains("(Do NOT regenerate changes for the above files)."));

        assert!(prompt.contains("Failed files and errors:"));
        assert!(prompt.contains("crates/core/src/main.rs"));
        assert!(prompt.contains("Search block not found (0 matches)"));

        assert!(prompt.contains("crates/core/src/utils.rs"));
        assert!(prompt.contains("Search block matches 3 times"));

        assert!(prompt.contains(
            "Please provide corrected edit blocks ONLY for the failed files listed above."
        ));
    }
}

async fn emit_semantic_events_for_edits(
    edits: &[EditOperation],
    stream_tx: &mpsc::Sender<StreamEvent>,
) {
    if edits.is_empty() {
        return;
    }
    let _ = stream_tx.send(StreamEvent::EditStarted).await;

    for (idx, edit) in edits.iter().enumerate() {
        match edit {
            EditOperation::Create { path, content } => {
                let _ = stream_tx
                    .send(StreamEvent::EditFileStarted {
                        path: path.clone(),
                        op_type: "create".to_string(),
                    })
                    .await;
                if !content.is_empty() {
                    let _ = stream_tx
                        .send(StreamEvent::EditHunk {
                            path: path.clone(),
                            hunk_index: idx,
                            old_text: String::new(),
                            new_text: content.clone(),
                        })
                        .await;
                }
                let _ = stream_tx
                    .send(StreamEvent::EditFileDone {
                        path: path.clone(),
                        status: "ok".to_string(),
                        error: None,
                        hunks_count: 1,
                    })
                    .await;
            }
            EditOperation::Delete { path } => {
                let _ = stream_tx
                    .send(StreamEvent::EditFileStarted {
                        path: path.clone(),
                        op_type: "delete".to_string(),
                    })
                    .await;
                let _ = stream_tx
                    .send(StreamEvent::EditFileDone {
                        path: path.clone(),
                        status: "ok".to_string(),
                        error: None,
                        hunks_count: 0,
                    })
                    .await;
            }
            EditOperation::Replace {
                path,
                old_text,
                new_text,
            } => {
                let _ = stream_tx
                    .send(StreamEvent::EditFileStarted {
                        path: path.clone(),
                        op_type: "replace".to_string(),
                    })
                    .await;
                let _ = stream_tx
                    .send(StreamEvent::EditHunk {
                        path: path.clone(),
                        hunk_index: idx,
                        old_text: old_text.clone(),
                        new_text: new_text.clone(),
                    })
                    .await;
                let _ = stream_tx
                    .send(StreamEvent::EditFileDone {
                        path: path.clone(),
                        status: "ok".to_string(),
                        error: None,
                        hunks_count: 1,
                    })
                    .await;
            }
        }
    }
}
