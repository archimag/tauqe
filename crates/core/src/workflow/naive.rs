use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::{EditProposal, ModelResult};

use super::common::{
    build_verification_retry_prompt, execute_edit_pipeline_full, execute_edit_pipeline_step_opt,
    record_workflow_response, run_workflow_verification,
};
use super::{EditWorkflow, WorkflowExecutionResult};
use crate::context::ContextManager;
use crate::edits::{apply_edit_proposal, EditProtocol};
use crate::history::HistoryManager;
use crate::model::gateway::StreamEvent;
use crate::model::openrouter::OpenRouterClient;

#[derive(Debug, Clone)]
pub struct NaiveEditWorkflow {
    pub max_retries: usize,
    pub max_discovery_rounds: usize,
    pub max_auto_files_per_round: Option<usize>,
}

impl Default for NaiveEditWorkflow {
    fn default() -> Self {
        Self {
            max_retries: 3,
            max_discovery_rounds: crate::config::DEFAULT_MAX_DISCOVERY_ROUNDS,
            max_auto_files_per_round: None,
        }
    }
}

impl NaiveEditWorkflow {
    pub fn new(max_retries: Option<usize>) -> Self {
        Self {
            max_retries: max_retries.unwrap_or(3),
            max_discovery_rounds: crate::config::DEFAULT_MAX_DISCOVERY_ROUNDS,
            max_auto_files_per_round: None,
        }
    }

    pub fn with_discovery(
        mut self,
        max_discovery_rounds: Option<usize>,
        max_auto_files_per_round: Option<usize>,
    ) -> Self {
        if let Some(rounds) = max_discovery_rounds {
            self.max_discovery_rounds = rounds;
        }
        self.max_auto_files_per_round = max_auto_files_per_round;
        self
    }
}

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
        history_manager: &mut HistoryManager,
        protocol: &dyn EditProtocol,
        stream_tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<WorkflowExecutionResult> {
        let mut pipeline_out = execute_edit_pipeline_full(
            prompt,
            client,
            model_name,
            context_manager,
            history_manager,
            protocol,
            self.name(),
            stream_tx.clone(),
            cancel_rx.clone(),
            self.max_retries,
            self.max_discovery_rounds,
            self.max_auto_files_per_round,
            true,
        )
        .await?;

        if pipeline_out.is_cancelled {
            return Ok(WorkflowExecutionResult {
                result: pipeline_out.parsed_result,
                assistant_text: pipeline_out.assistant_text,
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
                        Ok(changed_files) => {
                            let mut total_changed_files = changed_files;
                            let mut verify_req = crate::edits::protocol::xml::extract_verify_request(&pipeline_out.assistant_text);
                            let mut verify_outcome = if let Some(ref req) = verify_req {
                                Some(run_workflow_verification(&repo_root, req, &stream_tx, false).await)
                            } else {
                                None
                            };

                            let mut verify_attempt = 0;
                            while let Some(ref outcome) = verify_outcome {
                                if outcome.success || verify_attempt >= self.max_retries {
                                    break;
                                }
                                verify_attempt += 1;

                                let retry_banner = format!(
                                    "\n\n---\n**[Verification Retry {}/{}]** Correcting verification failures...\n\n",
                                    verify_attempt, self.max_retries
                                );
                                let _ = stream_tx.send(StreamEvent::TextDelta(retry_banner.clone())).await;

                                let retry_prompt = build_verification_retry_prompt(
                                    &outcome.report,
                                    history_manager.detected_language(),
                                );

                                let step_out = execute_edit_pipeline_step_opt(
                                    &retry_prompt,
                                    client,
                                    model_name,
                                    context_manager,
                                    history_manager,
                                    protocol,
                                    self.name(),
                                    stream_tx.clone(),
                                    cancel_rx.clone(),
                                    false,
                                    false,
                                    None,
                                )
                                .await?;

                                if step_out.is_cancelled {
                                    return Ok(WorkflowExecutionResult {
                                        result: step_out.parsed_result,
                                        assistant_text: step_out.assistant_text,
                                    });
                                }

                                if let Some(lang) = crate::edits::protocol::xml::extract_user_language(&step_out.assistant_text) {
                                    history_manager.set_detected_language(lang);
                                }
                                let step_text = crate::edits::protocol::xml::strip_user_language_tags(&step_out.assistant_text);

                                pipeline_out.assistant_text.push_str(&retry_banner);
                                pipeline_out.assistant_text.push_str(step_text.trim_start());

                                match step_out.parsed_result {
                                    ModelResult::Edit { edits: retry_edits, summary: retry_summary, .. } if !retry_edits.is_empty() => {
                                        let retry_prop = EditProposal {
                                            summary: retry_summary,
                                            edits: retry_edits,
                                        };
                                        match apply_edit_proposal(&repo_root, context_manager, &retry_prop) {
                                            Ok(new_files) => {
                                                for f in new_files {
                                                    if !total_changed_files.contains(&f) {
                                                        total_changed_files.push(f);
                                                    }
                                                }
                                                if let Some(new_req) = crate::edits::protocol::xml::extract_verify_request(&step_out.assistant_text) {
                                                    verify_req = Some(new_req);
                                                }
                                                let req = verify_req.as_ref().unwrap();
                                                verify_outcome = Some(run_workflow_verification(&repo_root, req, &stream_tx, false).await);
                                            }
                                            Err(e) => {
                                                verify_outcome = Some(super::common::VerificationOutcome {
                                                    success: false,
                                                    report: format!("Failed to apply repair edits: {}", e),
                                                });
                                            }
                                        }
                                    }
                                    _ => {
                                        break;
                                    }
                                }
                            }

                            let mut edit_error = None;
                            if let Some(outcome) = verify_outcome {
                                if !outcome.success {
                                    edit_error = Some(format!("Code verification failed:\n{}", outcome.report));
                                    let _ = stream_tx.send(StreamEvent::TextDelta(format!("\n\n{}", outcome.report.trim()))).await;
                                    pipeline_out.assistant_text.push_str("\n\n");
                                    pipeline_out.assistant_text.push_str(outcome.report.trim());
                                } else if outcome.report.contains("✅") {
                                    let _ = stream_tx.send(StreamEvent::TextDelta(format!("\n\n{}", outcome.report.trim()))).await;
                                }
                            }

                            if edit_error.is_none() {
                                context_manager.clear_auto();
                                let ctx_state = context_manager.get_state();
                                let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
                            }

                            ModelResult::Edit {
                                summary,
                                edits,
                                proposal: None,
                                applied: edit_error.is_none(),
                                error: edit_error,
                                changed_files: total_changed_files,
                                commit_hash: None,
                            }
                        }
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
            ModelResult::Answer { mut text } => {
                let repo_root = context_manager.repo_root().to_path_buf();
                let verify_req = crate::edits::protocol::xml::extract_verify_request(&pipeline_out.assistant_text);
                if let Some(req) = verify_req {
                    let outcome = run_workflow_verification(&repo_root, &req, &stream_tx, true).await;
                    if !outcome.report.trim().is_empty() {
                        if text.trim().is_empty() {
                            text = outcome.report;
                        } else {
                            text = format!("{}\n\n{}", text.trim(), outcome.report.trim());
                        }
                        pipeline_out.assistant_text = text.clone();
                    }
                }
                ModelResult::Answer { text }
            }
        };

        record_workflow_response(history_manager, &final_result, &pipeline_out.assistant_text);

        Ok(WorkflowExecutionResult {
            result: final_result,
            assistant_text: pipeline_out.assistant_text,
        })
    }
}
