use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::{EditProposal, ModelResult};

use super::common::execute_edit_pipeline;
use super::{EditWorkflow, WorkflowExecutionResult};
use crate::context::ContextManager;
use crate::edits::{apply_edit_proposal, EditProtocol};
use crate::git::{create_ai_commit, create_checkpoint, restore_checkpoint};
use crate::model::gateway::{ChatMessage, StreamEvent};
use crate::model::openrouter::OpenRouterClient;

/// Git-native workflow providing pre-edit checkpoints and atomic AI commits.
#[derive(Debug, Default, Clone)]
pub struct GitEditWorkflow;

#[async_trait]
impl EditWorkflow for GitEditWorkflow {
    fn name(&self) -> &'static str {
        "git"
    }

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
        let pipeline_out = execute_edit_pipeline(
            prompt,
            client,
            model_name,
            context_manager,
            session_history,
            protocol,
            self.name(),
            stream_tx,
            cancel_rx,
        )
        .await?;

        if pipeline_out.is_cancelled {
            return Ok(WorkflowExecutionResult {
                result: pipeline_out.parsed_result,
                assistant_text: pipeline_out.assistant_text,
                session_history_update: None,
            });
        }

        let repo_root = context_manager.repo_root().to_path_buf();

        let final_result = match pipeline_out.parsed_result {
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
                        commit_hash: None,
                    }
                } else if edits.is_empty() {
                    ModelResult::Edit {
                        summary,
                        edits: Vec::new(),
                        applied: false,
                        error: Some("No valid edit operations found in model output".to_string()),
                        changed_files: Vec::new(),
                        commit_hash: None,
                    }
                } else {
                    let proposal = EditProposal {
                        summary: summary.clone(),
                        edits: edits.clone(),
                    };

                    // 1. Create pre-edit checkpoint to preserve developer changes
                    let checkpoint = create_checkpoint(&repo_root).ok();

                    // 2. Apply edits atomically
                    match apply_edit_proposal(&repo_root, context_manager, &proposal) {
                        Ok(changed_files) => {
                            // 3. Create isolated AI commit with author metadata and model's summary
                            let commit_hash = match create_ai_commit(&repo_root, &changed_files, &summary) {
                                Ok(hash) => Some(hash),
                                Err(err) => {
                                    // Non-fatal if git commit fails (e.g. not a git repo), keep edits
                                    tracing::warn!("Failed to create AI commit: {}", err);
                                    None
                                }
                            };

                            ModelResult::Edit {
                                summary,
                                edits,
                                applied: true,
                                error: None,
                                changed_files,
                                commit_hash,
                            }
                        }
                        Err(err) => {
                            // Restore checkpoint on failure
                            if let Some(cp) = &checkpoint {
                                let _ = restore_checkpoint(&repo_root, cp);
                            }

                            ModelResult::Edit {
                                summary,
                                edits,
                                applied: false,
                                error: Some(err.to_string()),
                                changed_files: Vec::new(),
                                commit_hash: None,
                            }
                        }
                    }
                }
            }
            ModelResult::Answer { text } => ModelResult::Answer { text },
        };

        Ok(WorkflowExecutionResult {
            result: final_result,
            assistant_text: pipeline_out.assistant_text,
            session_history_update: pipeline_out.session_history_update,
        })
    }
}
