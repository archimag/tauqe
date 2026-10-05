use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::{ContextAccess, EditProposal, ModelResult};

use super::{EditWorkflow, WorkflowExecutionResult};
use crate::context::ContextManager;
use crate::edits::{apply_edit_proposal, EditProtocol, XmlStreamFilter};
use crate::model::gateway::{ChatMessage, StreamEvent};
use crate::model::openrouter::OpenRouterClient;
use crate::prompt::PromptAssembly;

#[derive(Debug, Default, Clone)]
pub struct NaiveEditWorkflow;

#[async_trait]
impl EditWorkflow for NaiveEditWorkflow {
    async fn execute(
        &self,
        prompt: &str,
        client: &OpenRouterClient,
        model_name: &str,
        context_manager: &mut ContextManager,
        session_history: &[ChatMessage],
        protocol: &dyn EditProtocol,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<WorkflowExecutionResult> {
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

        // 1. Build prompt layering including protocol-specific instructions
        let assembly = PromptAssembly::new(Some(repo_state), ctx_state.revision, context_files);
        let assembled_messages = assembly.assemble_chat_messages(session_history, prompt, protocol);

        // 2. Stream response from model
        let (llm_tx, mut llm_rx) = mpsc::channel::<StreamEvent>(100);

        let model_str = model_name.to_string();
        let client_call = client.stream_chat(&model_str, assembled_messages, llm_tx, cancel_rx);

        let editable_paths_for_filter = editable_paths.clone();
        let forward_task = tokio::spawn(async move {
            let mut assistant_text = String::new();
            let mut cancelled = false;
            let mut stream_filter = XmlStreamFilter::new(editable_paths_for_filter, repo_root);

            while let Some(event) = llm_rx.recv().await {
                match event {
                    StreamEvent::TextDelta(delta) => {
                        assistant_text.push_str(&delta);
                        let filtered_events = stream_filter.push_chunk(&delta);
                        for ev in filtered_events {
                            if stream_tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                    }
                    StreamEvent::Cancelled => {
                        cancelled = true;
                        let _ = stream_tx.send(StreamEvent::Cancelled).await;
                    }
                    other => {
                        if stream_tx.send(other).await.is_err() {
                            break;
                        }
                    }
                }
            }

            // Flush remaining stream filter events
            for ev in stream_filter.finish() {
                let _ = stream_tx.send(ev).await;
            }

            (assistant_text, cancelled)
        });

        // Run client chat request
        let client_res = client_call.await;
        let (assistant_text, cancelled) = forward_task.await.unwrap_or_default();

        if let Err(err) = client_res {
            return Err(err);
        }

        if cancelled {
            return Ok(WorkflowExecutionResult {
                result: ModelResult::Answer {
                    text: assistant_text,
                },
                assistant_text: String::new(),
                session_history_update: None,
            });
        }

        // 3. Parse model output according to current EditProtocol
        let parsed_result = protocol.parse_output(&assistant_text, &editable_paths);

        // 4. Atomically apply edits if present
        let final_result = match parsed_result {
            ModelResult::Edit {
                summary,
                edits,
                error,
                ..
            } => {
                if let Some(err_msg) = error {
                    ModelResult::Edit {
                        summary,
                        edits: Vec::new(),
                        applied: false,
                        error: Some(err_msg),
                        changed_files: Vec::new(),
                    }
                } else if edits.is_empty() {
                    ModelResult::Edit {
                        summary,
                        edits: Vec::new(),
                        applied: false,
                        error: Some("No valid edit operations found in model output".to_string()),
                        changed_files: Vec::new(),
                    }
                } else {
                    let proposal = EditProposal {
                        summary: summary.clone(),
                        edits: edits.clone(),
                    };

                    let repo_root = context_manager.repo_root().to_path_buf();
                    match apply_edit_proposal(&repo_root, context_manager, &proposal) {
                        Ok(changed_files) => ModelResult::Edit {
                            summary,
                            edits,
                            applied: true,
                            error: None,
                            changed_files,
                        },
                        Err(err) => ModelResult::Edit {
                            summary,
                            edits,
                            applied: false,
                            error: Some(err.to_string()),
                            changed_files: Vec::new(),
                        },
                    }
                }
            }
            ModelResult::Answer { text } => ModelResult::Answer { text },
        };

        let history_update = Some((
            ChatMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: assistant_text.clone(),
            },
        ));

        Ok(WorkflowExecutionResult {
            result: final_result,
            assistant_text,
            session_history_update: history_update,
        })
    }
}
