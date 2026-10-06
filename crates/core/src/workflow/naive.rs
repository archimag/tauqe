use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::{EditProposal, ModelResult};

use super::common::execute_edit_pipeline;
use super::{EditWorkflow, WorkflowExecutionResult};
use crate::context::ContextManager;
use crate::edits::{apply_edit_proposal, EditProtocol};
use crate::model::gateway::{ChatMessage, StreamEvent};
use crate::model::openrouter::OpenRouterClient;

#[derive(Debug, Default, Clone)]
pub struct NaiveEditWorkflow;

#[async_trait]
impl EditWorkflow for NaiveEditWorkflow {
    fn name(&self) -> &'static str {
        "naive"
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

        // Apply edits atomically without Git versioning
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
                        proposal: None,
                        applied: false,
                        error: Some(err_msg),
                        changed_files: Vec::new(),
                        commit_hash: None,
                    }
                } else if edits.is_empty() {
                    ModelResult::Edit {
                        summary,
                        edits: Vec::new(),
                        proposal: None,
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

                    let repo_root = context_manager.repo_root().to_path_buf();
                    match apply_edit_proposal(&repo_root, context_manager, &proposal) {
                        Ok(changed_files) => ModelResult::Edit {
                            summary,
                            edits,
                            proposal: None,
                            applied: true,
                            error: None,
                            changed_files,
                            commit_hash: None,
                        },
                        Err(err) => ModelResult::Edit {
                            summary,
                            edits,
                            proposal: None,
                            applied: false,
                            error: Some(err.to_string()),
                            changed_files: Vec::new(),
                            commit_hash: None,
                        },
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
