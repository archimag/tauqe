use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::{EditOperation, EditProposal, ModelResult};

use super::common::execute_edit_pipeline;
use super::{EditWorkflow, WorkflowExecutionResult};
use crate::context::ContextManager;
use crate::edits::{apply_edit_proposal, EditProtocol};
use crate::git::{create_ai_commit, create_checkpoint, restore_checkpoint, CheckpointInfo};
use crate::model::gateway::{ChatMessage, StreamEvent};
use crate::model::openrouter::OpenRouterClient;
use crate::toolchain::{detect_toolchain, run_toolchain_check};

/// Toolchain-guided edit workflow.
///
/// Features:
/// 1. Pre-edit Git checkpoints to protect developer's uncommitted changes.
/// 2. Atomic disk application of model edit proposals.
/// 3. Deterministic toolchain verification (e.g. `cargo check`, test, linter).
/// 4. Auto-healing feedback loop: if the toolchain reports errors, sends diagnostics back to LLM to self-correct.
/// 5. On success: isolated AI Git commit.
/// 6. On failure after retries: safe rollback to checkpoint so working tree is never broken.
#[derive(Debug, Clone, Default)]
pub struct ToolchainEditWorkflow {
    pub custom_check_command: Option<String>,
    pub max_retries: usize,
    pub auto_heal: bool,
}

impl ToolchainEditWorkflow {
    pub fn new(
        custom_check_command: Option<String>,
        max_retries: Option<usize>,
        auto_heal: Option<bool>,
    ) -> Self {
        Self {
            custom_check_command,
            max_retries: max_retries.unwrap_or(2),
            auto_heal: auto_heal.unwrap_or(true),
        }
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let prefix: String = s.chars().take(max_chars).collect();
        format!("{}...", prefix)
    } else {
        s.to_string()
    }
}

#[async_trait]
impl EditWorkflow for ToolchainEditWorkflow {
    fn name(&self) -> &'static str {
        "toolchain"
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
        let repo_root = context_manager.repo_root().to_path_buf();
        let mut accumulated_history = session_history.to_vec();
        let mut current_prompt = prompt.to_string();
        let mut attempt = 0;
        let max_heal_attempts = if self.auto_heal { self.max_retries } else { 0 };

        let mut last_assistant_text;
        let mut checkpoint: Option<CheckpointInfo> = None;
        let mut all_changed_files: Vec<String> = Vec::new();
        let mut final_summary = String::new();
        let mut final_edits: Vec<EditOperation> = Vec::new();

        loop {
            let pipeline_out = execute_edit_pipeline(
                &current_prompt,
                client,
                model_name,
                context_manager,
                &accumulated_history,
                protocol,
                self.name(),
                stream_tx.clone(),
                cancel_rx.clone(),
            )
            .await?;

            if pipeline_out.is_cancelled {
                if let Some(cp) = &checkpoint {
                    let _ = restore_checkpoint(&repo_root, cp);
                }
                return Ok(WorkflowExecutionResult {
                    result: pipeline_out.parsed_result,
                    assistant_text: pipeline_out.assistant_text,
                    session_history_update: None,
                });
            }

            last_assistant_text = pipeline_out.assistant_text.clone();

            match pipeline_out.parsed_result {
                ModelResult::Answer { text } => {
                    if attempt == 0 {
                        return Ok(WorkflowExecutionResult {
                            result: ModelResult::Answer { text },
                            assistant_text: last_assistant_text,
                            session_history_update: pipeline_out.session_history_update,
                        });
                    } else {
                        // In repair loop, conversational answer without edits implies aborting repairs
                        break;
                    }
                }
                ModelResult::Edit {
                    summary,
                    edits,
                    error,
                    ..
                } => {
                    if let Some(err_msg) = error {
                        if attempt == 0 {
                            return Ok(WorkflowExecutionResult {
                                result: ModelResult::Edit {
                                    summary,
                                    edits: Vec::new(),
                                    proposal: None,
                                    applied: false,
                                    error: Some(err_msg),
                                    changed_files: Vec::new(),
                                    commit_hash: None,
                                },
                                assistant_text: last_assistant_text,
                                session_history_update: pipeline_out.session_history_update,
                            });
                        } else {
                            break;
                        }
                    } else if edits.is_empty() {
                        if attempt == 0 {
                            return Ok(WorkflowExecutionResult {
                                result: ModelResult::Edit {
                                    summary,
                                    edits: Vec::new(),
                                    proposal: None,
                                    applied: false,
                                    error: Some("No valid edit operations found in model output".to_string()),
                                    changed_files: Vec::new(),
                                    commit_hash: None,
                                },
                                assistant_text: last_assistant_text,
                                session_history_update: pipeline_out.session_history_update,
                            });
                        } else {
                            break;
                        }
                    }

                    if final_summary.is_empty() {
                        final_summary = summary.clone();
                    }
                    final_edits = edits.clone();

                    // Create checkpoint on the first edit batch
                    if checkpoint.is_none() {
                        checkpoint = create_checkpoint(&repo_root).ok();
                    }

                    let proposal = EditProposal {
                        summary: summary.clone(),
                        edits: edits.clone(),
                    };

                    match apply_edit_proposal(&repo_root, context_manager, &proposal) {
                        Ok(changed) => {
                            for f in changed {
                                if !all_changed_files.contains(&f) {
                                    all_changed_files.push(f);
                                }
                            }
                        }
                        Err(err) => {
                            if let Some(cp) = &checkpoint {
                                let _ = restore_checkpoint(&repo_root, cp);
                            }
                            return Ok(WorkflowExecutionResult {
                                result: ModelResult::Edit {
                                    summary,
                                    edits,
                                    proposal: None,
                                    applied: false,
                                    error: Some(err.to_string()),
                                    changed_files: Vec::new(),
                                    commit_hash: None,
                                },
                                assistant_text: last_assistant_text,
                                session_history_update: pipeline_out.session_history_update,
                            });
                        }
                    }

                    // Deterministic toolchain validation
                    let (_, detected_cmd) =
                        detect_toolchain(&repo_root, self.custom_check_command.as_deref());

                    if let Some(check_cmd) = detected_cmd {
                        let _ = stream_tx
                            .send(StreamEvent::ToolchainStarted {
                                command: check_cmd.clone(),
                            })
                            .await;

                        let check_result = run_toolchain_check(&repo_root, &check_cmd).await;

                        let _ = stream_tx
                            .send(StreamEvent::ToolchainResult {
                                command: check_cmd.clone(),
                                success: check_result.success,
                                output: check_result.combined_output.clone(),
                            })
                            .await;

                        if check_result.success {
                            let commit_hash = match create_ai_commit(
                                &repo_root,
                                &all_changed_files,
                                &final_summary,
                            ) {
                                Ok(hash) => Some(hash),
                                Err(err) => {
                                    tracing::warn!("Failed to create AI commit: {}", err);
                                    None
                                }
                            };

                            return Ok(WorkflowExecutionResult {
                                result: ModelResult::Edit {
                                    summary: final_summary.clone(),
                                    edits: final_edits,
                                    proposal: None,
                                    applied: true,
                                    error: None,
                                    changed_files: all_changed_files,
                                    commit_hash,
                                },
                                assistant_text: last_assistant_text.clone(),
                                session_history_update: Some((
                                    ChatMessage::user(prompt),
                                    ChatMessage::assistant(if !last_assistant_text.trim().is_empty() {
                                        last_assistant_text
                                    } else {
                                        final_summary
                                    }),
                                )),
                            });
                        } else {
                            if attempt < max_heal_attempts {
                                attempt += 1;

                                if let Some((u, a)) = pipeline_out.session_history_update {
                                    accumulated_history.push(u);
                                    accumulated_history.push(a);
                                }

                                let err_preview = if check_result.combined_output.chars().count() > 2500 {
                                    format!(
                                        "{}\n... (truncated)",
                                        truncate_chars(&check_result.combined_output, 2500)
                                    )
                                } else {
                                    check_result.combined_output.clone()
                                };

                                current_prompt = format!(
                                    "The toolchain verification command `{}` failed with the following errors:\n```\n{}\n```\nPlease fix the compilation/verification errors by proposing updated edits.",
                                    check_cmd, err_preview
                                );
                                continue;
                            } else {
                                // Exhausted retries or auto_heal disabled: rollback to keep repo clean
                                if let Some(cp) = &checkpoint {
                                    let _ = restore_checkpoint(&repo_root, cp);
                                }
                                context_manager.prune_missing_files();

                                let err_msg = format!(
                                    "Toolchain check '{}' failed. Changes rolled back.\nErrors:\n{}",
                                    check_cmd,
                                    truncate_chars(&check_result.combined_output, 1000)
                                );

                                return Ok(WorkflowExecutionResult {
                                    result: ModelResult::Edit {
                                        summary: final_summary,
                                        edits: final_edits,
                                        proposal: None,
                                        applied: false,
                                        error: Some(err_msg),
                                        changed_files: Vec::new(),
                                        commit_hash: None,
                                    },
                                    assistant_text: last_assistant_text,
                                    session_history_update: pipeline_out.session_history_update,
                                });
                            }
                        }
                    } else {
                        // No toolchain detected, commit directly like GitEditWorkflow
                        let commit_hash = match create_ai_commit(
                            &repo_root,
                            &all_changed_files,
                            &final_summary,
                        ) {
                            Ok(hash) => Some(hash),
                            Err(err) => {
                                tracing::warn!("Failed to create AI commit: {}", err);
                                None
                            }
                        };

                        return Ok(WorkflowExecutionResult {
                            result: ModelResult::Edit {
                                summary: final_summary.clone(),
                                edits: final_edits,
                                proposal: None,
                                applied: true,
                                error: None,
                                changed_files: all_changed_files,
                                commit_hash,
                            },
                            assistant_text: last_assistant_text.clone(),
                            session_history_update: Some((
                                ChatMessage::user(prompt),
                                ChatMessage::assistant(if !last_assistant_text.trim().is_empty() {
                                    last_assistant_text
                                } else {
                                    final_summary
                                }),
                            )),
                        });
                    }
                }
            }
        }

        // Fallback rollback if loop terminated without success
        if let Some(cp) = &checkpoint {
            let _ = restore_checkpoint(&repo_root, cp);
        }
        context_manager.prune_missing_files();

        Ok(WorkflowExecutionResult {
            result: ModelResult::Edit {
                summary: final_summary,
                edits: final_edits,
                proposal: None,
                applied: false,
                error: Some("Toolchain verification failed and could not be resolved.".to_string()),
                changed_files: Vec::new(),
                commit_hash: None,
            },
            assistant_text: last_assistant_text,
            session_history_update: None,
        })
    }
}
