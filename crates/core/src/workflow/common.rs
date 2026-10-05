use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::{ContextAccess, ModelResult};

use crate::context::ContextManager;
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

    let tools = protocol.tools(&editable_paths);
    let model_str = model_name.to_string();
    let client_call = client.stream_chat_with_tools(&model_str, assembled_messages, tools, llm_tx, cancel_rx);

    let editable_paths_for_filter = editable_paths.clone();
    let forward_stream_tx = stream_tx.clone();
    let forward_task = tokio::spawn(async move {
        let mut assistant_text = String::new();
        let mut cancelled = false;
        let mut stream_filter = XmlStreamFilter::new(editable_paths_for_filter, repo_root);

        while let Some(event) = llm_rx.recv().await {
            match event {
                StreamEvent::TextDelta(ref delta) => {
                    assistant_text.push_str(delta);
                    let filtered_events = stream_filter.push_chunk(delta);
                    for ev in filtered_events {
                        if forward_stream_tx.send(ev).await.is_err() {
                            break;
                        }
                    }
                }
                StreamEvent::Cancelled => {
                    cancelled = true;
                    let _ = forward_stream_tx.send(StreamEvent::Cancelled).await;
                }
                other => {
                    if forward_stream_tx.send(other).await.is_err() {
                        break;
                    }
                }
            }
        }

        // Flush remaining events
        for ev in stream_filter.finish() {
            let _ = forward_stream_tx.send(ev).await;
        }

        (assistant_text, cancelled)
    });

    let client_res = client_call.await;
    let (assistant_text, cancelled) = forward_task.await.unwrap_or_default();

    let completed_tool_calls = client_res?;

    if !completed_tool_calls.is_empty() {
        let _ = stream_tx.send(StreamEvent::EditStarted).await;
        for call in &completed_tool_calls {
            emit_semantic_events_for_tool_call(call, &stream_tx).await;
        }
    }

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
    let parsed_result = if !completed_tool_calls.is_empty() {
        if let Some(res) = protocol.parse_tool_calls(&completed_tool_calls, &editable_paths) {
            res
        } else {
            protocol.parse_output(&assistant_text, &editable_paths)
        }
    } else {
        protocol.parse_output(&assistant_text, &editable_paths)
    };

    // 4. Build clean conversation history update as (user_msg, assistant_msg)
    let asst_msg_content = if !assistant_text.trim().is_empty() {
        assistant_text.clone()
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

    Ok(ParsedPipelineOutput {
        assistant_text,
        parsed_result,
        completed_tool_calls,
        session_history_update: history_update,
        is_cancelled: false,
    })
}

async fn emit_semantic_events_for_tool_call(
    call: &crate::model::gateway::ToolCall,
    stream_tx: &mpsc::Sender<StreamEvent>,
) {
    let fn_name = call.function.name.as_str();
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.function.arguments) else {
        return;
    };

    let path = args
        .get("path")
        .or_else(|| args.get("file"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if path.is_empty() {
        return;
    }

    match fn_name {
        "edit_file" | "replace_in_file" | "str_replace" | "replace" => {
            let old_text = args
                .get("old_text")
                .or_else(|| args.get("search"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let new_text = args
                .get("new_text")
                .or_else(|| args.get("replace"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let _ = stream_tx
                .send(StreamEvent::EditFileStarted {
                    path: path.clone(),
                    op_type: "replace".to_string(),
                })
                .await;
            let _ = stream_tx
                .send(StreamEvent::EditHunk {
                    path: path.clone(),
                    hunk_index: 0,
                    old_text,
                    new_text,
                })
                .await;
            let _ = stream_tx
                .send(StreamEvent::EditFileDone {
                    path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: 1,
                })
                .await;
        }
        "create_file" | "write_file" | "new_file" => {
            let _ = stream_tx
                .send(StreamEvent::EditFileStarted {
                    path: path.clone(),
                    op_type: "create".to_string(),
                })
                .await;
            let _ = stream_tx
                .send(StreamEvent::EditFileDone {
                    path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: 1,
                })
                .await;
        }
        "delete_file" | "remove_file" => {
            let _ = stream_tx
                .send(StreamEvent::EditFileStarted {
                    path: path.clone(),
                    op_type: "delete".to_string(),
                })
                .await;
            let _ = stream_tx
                .send(StreamEvent::EditFileDone {
                    path,
                    status: "ok".to_string(),
                    error: None,
                    hunks_count: 0,
                })
                .await;
        }
        _ => {}
    }
}
