use std::path::Path;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::{EditProposal, ModelRef, ModelResult};

use super::common::{
    execute_edit_pipeline, record_workflow_response, run_verification_healing_loop,
    verify_and_append_to_answer, VerificationHealingResult,
};
use super::WorkflowExecutionResult;
use crate::context::ContextManager;
use crate::edits::{apply_edit_proposal, EditProtocol};
use crate::history::HistoryManager;
use crate::providers::{LlmProvider, StreamEvent};

/// Execution options and token budgets for edit workflows.
#[derive(Debug, Clone)]
pub struct WorkflowOptions {
    pub max_retries: usize,
    pub max_discovery_rounds: usize,
    pub max_auto_files_per_round: Option<usize>,
    pub history_budget_tokens: u64,
    pub history_tail_turns: usize,
    pub repomap_token_budget: usize,
    pub history_model: ModelRef,
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        Self {
            max_retries: 3,
            max_discovery_rounds: crate::config::DEFAULT_MAX_DISCOVERY_ROUNDS,
            max_auto_files_per_round: None,
            history_budget_tokens: crate::history::DEFAULT_HISTORY_BUDGET_TOKENS,
            history_tail_turns: crate::history::DEFAULT_TAIL_TURNS_COUNT,
            repomap_token_budget: crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
            history_model: ModelRef::openrouter(crate::config::BUILTIN_DEFAULT_MODEL),
        }
    }
}

impl WorkflowOptions {
    /// Constructs workflow options from the unified application configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        let max_retries = config
            .toolchain
            .max_retries
            .unwrap_or(config.develop.max_retries);
        Self {
            max_retries,
            max_discovery_rounds: config.context.max_discovery_rounds,
            max_auto_files_per_round: config.context.max_auto_files_per_round,
            history_budget_tokens: config.history.budget_tokens,
            history_tail_turns: config.history.tail_turns,
            repomap_token_budget: config.context.repomap_token_budget,
            history_model: config.history_model(),
        }
    }
}

/// Transactional hook interface allowing workflows to customize checkpointing and commit finalization.
pub(crate) trait WorkflowTransaction: Send + Sync {
    /// Invoked before applying the initial edits to the working copy.
    fn on_before_apply(&mut self, _repo_root: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    /// Invoked after initial edits are successfully applied to the working copy.
    fn on_initial_applied(
        &mut self,
        _repo_root: &Path,
        _changed_files: &[String],
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Invoked during the verification healing loop whenever a repair step is applied.
    fn on_step_applied(
        &mut self,
        _repo_root: &Path,
        _changed_files: &[String],
        _attempt: usize,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Invoked upon successful verification to finalize the commit, if supported.
    fn on_success(
        &mut self,
        _repo_root: &Path,
        _changed_files: &[String],
        _summary: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Invoked upon failure or cancellation to rollback changes.
    /// Returns true if modifications were rolled back to the pre-edit state.
    fn on_rollback(&mut self, _repo_root: &Path) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// Orchestrates the universal workflow lifecycle: prompt assembly, streaming, patch application,
/// verification healing loop, transactional commit/rollback, and history recording.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_workflow_lifecycle<T: WorkflowTransaction>(
    workflow_name: &str,
    options: &WorkflowOptions,
    mut transaction: T,
    prompt: &str,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn EditProtocol,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<WorkflowExecutionResult> {
    let _ = super::common::compact_history_if_needed_with_budget(
        history_manager,
        provider,
        &options.history_model,
        cancel_rx.clone(),
        options.history_budget_tokens,
        options.history_tail_turns,
    )
    .await;

    let mut pipeline_out = execute_edit_pipeline(
        prompt,
        provider,
        model,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx.clone(),
        cancel_rx.clone(),
        options,
    )
    .await?;

    if pipeline_out.is_cancelled {
        return Ok(WorkflowExecutionResult {
            result: pipeline_out.parsed_result,
            assistant_text: protocol.clean_assistant_text(&pipeline_out.assistant_text),
        });
    }

    let mut turn_detected_language = pipeline_out.detected_language.clone();
    let repo_root = context_manager.repo_root().to_path_buf();

    let (parsed_plans, plan_errors) = protocol.parse_plan_tags(&pipeline_out.assistant_text);
    let plan_outcome = crate::plan::storage::PlanStorage::apply_parsed_plans(
        &repo_root,
        parsed_plans,
        plan_errors,
    );
    let plan_error_notice = if !plan_outcome.errors.is_empty() {
        let mut msg = String::from("\n\n> ⚠️ **Plan Notice:**\n");
        for err in &plan_outcome.errors {
            msg.push_str(&format!("> - {}\n", err));
        }
        Some(msg)
    } else {
        None
    };

    if let Some(ref notice) = plan_error_notice {
        let _ = stream_tx.send(StreamEvent::TextDelta(notice.clone())).await;
    }

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

                let _ = transaction.on_before_apply(&repo_root);

                match apply_edit_proposal(&repo_root, context_manager, &proposal) {
                    Ok(changed_files) => {
                        let mut total_changed_files = changed_files.clone();
                        let _ = transaction.on_initial_applied(&repo_root, &changed_files);

                        let verify_req = protocol.parse_verify_request(&pipeline_out.assistant_text);

                        let healing_result = run_verification_healing_loop(
                            &repo_root,
                            context_manager,
                            history_manager,
                            provider,
                            model,
                            protocol,
                            workflow_name,
                            &stream_tx,
                            &cancel_rx,
                            options,
                            verify_req,
                            &mut pipeline_out.assistant_text,
                            &mut turn_detected_language,
                            &mut total_changed_files,
                            |new_files, attempt| {
                                let _ = transaction.on_step_applied(&repo_root, new_files, attempt);
                            },
                        )
                        .await?;

                        match healing_result {
                            VerificationHealingResult::Cancelled(mut res) => {
                                if transaction.on_rollback(&repo_root).unwrap_or(false) {
                                    context_manager.clear_auto();
                                    context_manager.prune_missing_files();
                                    let ctx_state = context_manager.get_state();
                                    let _ = stream_tx
                                        .send(StreamEvent::ContextChanged(ctx_state))
                                        .await;
                                }
                                res.assistant_text = protocol.clean_assistant_text(&res.assistant_text);
                                return Ok(*res);
                            }
                            VerificationHealingResult::Success(_) => {
                                let commit_hash = transaction
                                    .on_success(&repo_root, &total_changed_files, &summary)
                                    .unwrap_or(None);

                                context_manager.clear_auto();
                                let ctx_state = context_manager.get_state();
                                let _ = stream_tx
                                    .send(StreamEvent::ContextChanged(ctx_state))
                                    .await;

                                ModelResult::Edit {
                                    summary,
                                    edits,
                                    proposal: None,
                                    applied: true,
                                    error: None,
                                    changed_files: total_changed_files,
                                    commit_hash,
                                }
                            }
                            VerificationHealingResult::Failed(outcome) => {
                                let changed_files =
                                    if transaction.on_rollback(&repo_root).unwrap_or(false) {
                                        context_manager.clear_auto();
                                        context_manager.prune_missing_files();
                                        let ctx_state = context_manager.get_state();
                                        let _ = stream_tx
                                            .send(StreamEvent::ContextChanged(ctx_state))
                                            .await;
                                        Vec::new()
                                    } else {
                                        total_changed_files
                                    };

                                ModelResult::Edit {
                                    summary,
                                    edits,
                                    proposal: None,
                                    applied: false,
                                    error: Some(format!(
                                        "Code verification failed:\n{}",
                                        outcome.report
                                    )),
                                    changed_files,
                                    commit_hash: None,
                                }
                            }
                        }
                    }
                    Err(err) => {
                        if transaction.on_rollback(&repo_root).unwrap_or(false) {
                            context_manager.clear_auto();
                            context_manager.prune_missing_files();
                            let ctx_state = context_manager.get_state();
                            let _ = stream_tx
                                .send(StreamEvent::ContextChanged(ctx_state))
                                .await;
                        }

                        ModelResult::Edit {
                            summary,
                            edits,
                            proposal: None,
                            applied: false,
                            error: Some(err.to_string()),
                            changed_files: Vec::new(),
                            commit_hash: None,
                        }
                    }
                }
            }
        }
        ModelResult::Answer { text } => {
            let mut text = verify_and_append_to_answer(
                &repo_root,
                &mut pipeline_out.assistant_text,
                text,
                &stream_tx,
                protocol,
            )
            .await;
            if let Some(ref notice) = plan_error_notice {
                text.push_str(notice);
            }
            ModelResult::Answer {
                text: protocol.clean_assistant_text(&text),
            }
        }
    };

    pipeline_out.assistant_text = protocol.clean_assistant_text(&pipeline_out.assistant_text);
    if let Some(ref notice) = plan_error_notice {
        pipeline_out.assistant_text.push_str(notice);
    }

    record_workflow_response(
        history_manager,
        &final_result,
        &pipeline_out.assistant_text,
    );

    Ok(WorkflowExecutionResult {
        result: final_result,
        assistant_text: pipeline_out.assistant_text,
    })
}
