use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::{ContextAccess, EditOperation, ModelResult};

use crate::context::ContextManager;
use crate::edits::stream::JsonStreamFilter;
use crate::edits::{EditProtocol, XmlStreamFilter};
use crate::model::gateway::{ChatMessage, StreamEvent};
use crate::model::openrouter::OpenRouterClient;
use crate::prompt::PromptAssembly;

pub struct ParsedPipelineOutput {
    pub assistant_text: String,
    pub parsed_result: ModelResult,
    pub completed_tool_calls: Vec<crate::model::gateway::ToolCall>,
    pub session_history_update: Option<(ChatMessage, ChatMessage)>,
    pub is_cancelled: bool,
}

/// Common execution pipeline for streaming, XML filtering, and parsing model response.
pub async fn execute_edit_pipeline(
    prompt: &str,
    client: &OpenRouterClient,
    model_name: &str,
    context_manager: &mut ContextManager,
    session_history: &[ChatMessage],
    protocol: &dyn EditProtocol,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<ParsedPipelineOutput> {
    let repo_state = crate::git::get_repository_state(None);
    let ctx_state = context_manager.get_state();
    let context_files = context_manager.read_context_files();

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
    );

    let assembled_messages = assembly.assemble_chat_messages(session_history, prompt, protocol);

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
    let forward_stream_tx = stream_tx.clone();
    let has_emitted_edits = Arc::new(AtomicBool::new(false));
    let has_emitted_edits_forward = has_emitted_edits.clone();

    let forward_task = tokio::spawn(async move {
        let mut assistant_text = String::new();
        let mut cancelled = false;

        if is_structured {
            let mut json_filter = JsonStreamFilter::new(editable_paths_for_filter, repo_root);
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
            let mut stream_filter = XmlStreamFilter::new(editable_paths_for_filter, repo_root);

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
            session_history_update: None,
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

    // 4. Build clean conversation history update as (user_msg, assistant_msg)
    let asst_msg_content = if !assistant_text.trim().is_empty() {
        if is_structured {
            if let Ok(prop) =
                serde_json::from_str::<workbench_protocol::ModelResultProposal>(&assistant_text)
            {
                if !prop.message.trim().is_empty() {
                    prop.message
                } else {
                    assistant_text.clone()
                }
            } else {
                assistant_text.clone()
            }
        } else {
            assistant_text.clone()
        }
    } else {
        match &parsed_result {
            ModelResult::Edit { summary, .. } => summary.clone(),
            _ => "Applied requested changes.".to_string(),
        }
    };

    let history_update = Some((
        ChatMessage::user(prompt),
        ChatMessage::assistant(asst_msg_content),
    ));

    // Emit final Done event for this execution
    let _ = stream_tx.send(StreamEvent::Done).await;

    Ok(ParsedPipelineOutput {
        assistant_text,
        parsed_result,
        completed_tool_calls,
        session_history_update: history_update,
        is_cancelled: false,
    })
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

    