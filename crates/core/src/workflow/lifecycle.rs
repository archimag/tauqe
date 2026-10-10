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
    pub max_files: usize,
    pub history_budget_tokens: u64,
    pub history_tail_turns: usize,
    pub repomap_token_budget: usize,
    pub history_model: ModelRef,
    pub auto_level_up: bool,
    pub models_config: crate::config::ModelsConfig,
    pub initial_selection: Option<tauqe_protocol::ModelSelection>,
    pub app_config: Option<crate::config::AppConfig>,
    pub discovery_mode: crate::config::DiscoveryMode,
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        Self {
            max_retries: 3,
            max_discovery_rounds: crate::config::DEFAULT_MAX_DISCOVERY_ROUNDS,
            max_files: crate::config::DEFAULT_MAX_CONTEXT_FILES,
            history_budget_tokens: crate::history::DEFAULT_HISTORY_BUDGET_TOKENS,
            history_tail_turns: crate::history::DEFAULT_TAIL_TURNS_COUNT,
            repomap_token_budget: crate::repomap::DEFAULT_REPOMAP_TOKEN_BUDGET,
            history_model: ModelRef::openrouter(crate::config::BUILTIN_DEFAULT_JUNIOR_MODEL),
            auto_level_up: false,
            models_config: crate::config::ModelsConfig::default(),
            initial_selection: None,
            app_config: None,
            discovery_mode: crate::config::DiscoveryMode::default(),
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
            max_files: config.context.max_files,
            history_budget_tokens: config.history.budget_tokens,
            history_tail_turns: config.history.tail_turns,
            repomap_token_budget: config.context.repomap_token_budget,
            history_model: config.resolve_tier(tauqe_protocol::ModelTier::Junior),
            auto_level_up: config.models.auto_level_up,
            models_config: config.models.clone(),
            initial_selection: None,
            app_config: Some(config.clone()),
            discovery_mode: config.context.discovery_mode,
        }
    }
}

fn next_tier(tier: tauqe_protocol::ModelTier) -> Option<tauqe_protocol::ModelTier> {
    match tier {
        tauqe_protocol::ModelTier::Junior => Some(tauqe_protocol::ModelTier::Middle),
        tauqe_protocol::ModelTier::Middle => Some(tauqe_protocol::ModelTier::Senior),
        tauqe_protocol::ModelTier::Senior => None,
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

    let marker = crate::edits::protocol::generate_turn_marker();
    let marked_protocol = protocol.with_turn_marker(&marker);
    let protocol: &dyn EditProtocol = marked_protocol.as_deref().unwrap_or(protocol);

    let is_isolated_plan_step = crate::plan::prompt::is_plan_step_execution_prompt(prompt);
    let is_isolated_review_step = tauqe_protocol::review::is_review_step_execution_prompt(prompt);
    let is_isolated_step = is_isolated_plan_step || is_isolated_review_step;
    let is_discussion = tauqe_protocol::is_discussion_prompt(prompt);

    let target_step_target = if is_isolated_plan_step {
        crate::plan::prompt::extract_plan_step_execution_target(prompt)
    } else {
        None
    };
    let target_step_id = target_step_target.as_ref().map(|t| t.step_id.clone());
    let target_plan_id = target_step_target.as_ref().and_then(|t| t.plan_id.clone());

    let target_review_target = if is_isolated_review_step {
        tauqe_protocol::review::extract_review_step_execution_target(prompt)
    } else {
        None
    };
    let target_review_id = target_review_target.as_ref().and_then(|t| t.review_id.clone());
    let target_review_item_id = target_review_target.as_ref().map(|t| t.item_id);

    let mut current_model = model.clone();
    let mut current_tier = if is_isolated_step {
        match options.initial_selection {
            Some(tauqe_protocol::ModelSelection::Tier(t)) => Some(t),
            Some(tauqe_protocol::ModelSelection::Specific(_)) => None,
            None => Some(options.models_config.default),
        }
    } else {
        None
    };

    let mut dynamic_provider: Option<Box<dyn LlmProvider>> = None;
    let mut pipeline_out;
    let final_result;
    let mut plan_error_notice;
    let mut plan_outcome = crate::plan::storage::PlanApplicationOutcome::default();

    loop {
        let active_provider: &dyn LlmProvider = if let Some(ref p) = dynamic_provider {
            p.as_ref()
        } else {
            provider
        };

        pipeline_out = execute_edit_pipeline(
            prompt,
            active_provider,
            &current_model,
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

        if is_discussion {
            plan_outcome = if let Some(update) = pipeline_out
                .discussion_response
                .as_ref()
                .and_then(|r| r.plan_update.as_ref())
            {
                crate::plan::storage::PlanStorage::apply_discussion_plan_update(&repo_root, update)
            } else {
                let (parsed_plans, plan_errors) = protocol.parse_plan_tags(&pipeline_out.assistant_text);
                crate::plan::storage::PlanStorage::apply_parsed_plans(&repo_root, parsed_plans, plan_errors)
            };
        } else if !is_isolated_step {
            let (parsed_plans, mut plan_errors) = protocol.parse_plan_tags(&pipeline_out.assistant_text);
            let mut allowed_plans = Vec::new();
            for plan in parsed_plans {
                if plan.action.eq_ignore_ascii_case("save") {
                    allowed_plans.push(plan);
                } else {
                    plan_errors.push(format!(
                        "Plan action '{}' is forbidden in Develop mode. Modifying existing plans or closing steps is only allowed via Discussion mode or isolated step execution.",
                        plan.action
                    ));
                }
            }
            plan_outcome = crate::plan::storage::PlanStorage::apply_parsed_plans(&repo_root, allowed_plans, plan_errors);
        }

        plan_error_notice = if !plan_outcome.errors.is_empty() {
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

        match pipeline_out.parsed_result {
            ModelResult::Edit {
                summary,
                edits,
                error,
                ..
            } => {
                let escalation = if is_isolated_step && options.auto_level_up {
                    current_tier.and_then(|cur| next_tier(cur).map(|next| (cur, next)))
                } else {
                    None
                };

                if let Some(err_msg) = error {
                    if let Some((cur_t, next_t)) = escalation {
                        let next_m = options.models_config.resolve_tier(next_t);
                        if next_m != current_model {
                            let _ = transaction.on_rollback(&repo_root);
                            context_manager.prune_missing_files();
                            let notice = format!(
                                "\n\n> ⚠️ **Level Up Escalation:** Step generation failed on `{}` ({}: {}). Escalating to `{}` ({})...\n\n",
                                cur_t, current_model, err_msg, next_t, next_m
                            );
                            let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;
                            current_tier = Some(next_t);
                            if next_m.provider != current_model.provider {
                                if let Some(ref app_cfg) = options.app_config {
                                    if let Ok(np) = crate::providers::create_provider(next_m.provider, app_cfg) {
                                        dynamic_provider = Some(np);
                                    }
                                }
                            }
                            current_model = next_m;
                            continue;
                        }
                    }

                    final_result = ModelResult::Edit {
                        summary,
                        edits: Vec::new(),
                        proposal: None,
                        applied: false,
                        error: Some(err_msg),
                        changed_files: Vec::new(),
                        commit_hash: None,
                    };
                    break;
                } else if edits.is_empty() {
                    if let Some((cur_t, next_t)) = escalation {
                        let next_m = options.models_config.resolve_tier(next_t);
                        if next_m != current_model {
                            let _ = transaction.on_rollback(&repo_root);
                            context_manager.prune_missing_files();
                            let notice = format!(
                                "\n\n> ⚠️ **Level Up Escalation:** Model `{}` ({}) produced no valid edits. Escalating to `{}` ({})...\n\n",
                                cur_t, current_model, next_t, next_m
                            );
                            let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;
                            current_tier = Some(next_t);
                            if next_m.provider != current_model.provider {
                                if let Some(ref app_cfg) = options.app_config {
                                    if let Ok(np) = crate::providers::create_provider(next_m.provider, app_cfg) {
                                        dynamic_provider = Some(np);
                                    }
                                }
                            }
                            current_model = next_m;
                            continue;
                        }
                    }

                    final_result = ModelResult::Edit {
                        summary,
                        edits: Vec::new(),
                        proposal: None,
                        applied: false,
                        error: Some("No valid edit operations found in model output".to_string()),
                        changed_files: Vec::new(),
                        commit_hash: None,
                    };
                    break;
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
                            let _ = stream_tx
                                .send(StreamEvent::TurnPhase {
                                    phase: tauqe_protocol::TurnPhase::Verification,
                                    round: None,
                                    max_rounds: None,
                                    detail: None,
                                })
                                .await;

                            let healing_result = run_verification_healing_loop(
                                &repo_root,
                                context_manager,
                                history_manager,
                                active_provider,
                                &current_model,
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

                                    let step_done_tags = if is_isolated_plan_step {
                                        let mut tags = protocol.parse_plan_step_done(&pipeline_out.assistant_text);
                                        if tags.is_empty() {
                                            if let Some(ref target_id) = target_step_id {
                                                tags.push(crate::edits::protocol::PlanStepDone {
                                                    id: Some(target_id.clone()),
                                                    note: Some("Step executed and verified cleanly".to_string()),
                                                });
                                            }
                                        }
                                        tags
                                    } else {
                                        Vec::new()
                                    };

                                    if !step_done_tags.is_empty() {
                                        if let Ok(all_plans) =
                                            crate::plan::storage::PlanStorage::load_all(&repo_root)
                                        {
                                            let active_id = target_plan_id
                                                .clone()
                                                .or_else(|| {
                                                    crate::plan::storage::PlanStorage::load_active_id(&repo_root)
                                                        .ok()
                                                        .flatten()
                                                });
                                            for step in &step_done_tags {
                                                let effective_step_id = step.id.as_deref().or(target_step_id.as_deref());
                                                if let Some(step_id) = effective_step_id {
                                                    let target_plan = active_id
                                                        .as_deref()
                                                        .and_then(|id| {
                                                            all_plans
                                                                .iter()
                                                                .find(|p| p.id == id && p.find_item(step_id).is_some())
                                                        })
                                                        .or_else(|| {
                                                            all_plans
                                                                .iter()
                                                                .find(|p| p.find_item(step_id).is_some())
                                                        });

                                                    if let Some(plan) = target_plan {
                                                        let _ = crate::plan::storage::PlanStorage::update_item(
                                                            &repo_root,
                                                            &plan.id,
                                                            step_id,
                                                            Some(tauqe_protocol::PlanItemStatus::Done),
                                                        );
                                                        let _ = crate::plan::storage::PlanStorage::save_active_id(
                                                            &repo_root,
                                                            Some(&plan.id),
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    if let Some(item_id) = target_review_item_id {
                                        let rev_id = target_review_id.as_deref();
                                        let _ = crate::review::storage::ReviewStorage::update_session_item_status(
                                            &repo_root,
                                            rev_id,
                                            item_id,
                                            tauqe_protocol::ReviewItemStatus::Fixed,
                                        );
                                        if let Some(id) = rev_id {
                                            let _ = crate::review::storage::ReviewStorage::save_active_id(
                                                &repo_root,
                                                Some(id),
                                            );
                                        }
                                    }

                                    final_result = ModelResult::Edit {
                                        summary,
                                        edits,
                                        proposal: None,
                                        applied: true,
                                        error: None,
                                        changed_files: total_changed_files,
                                        commit_hash,
                                    };
                                    break;
                                }
                                VerificationHealingResult::Failed(outcome) => {
                                    if let Some((cur_t, next_t)) = escalation {
                                        let next_m = options.models_config.resolve_tier(next_t);
                                        if next_m != current_model {
                                            let _ = transaction.on_rollback(&repo_root);
                                            context_manager.prune_missing_files();
                                            let notice = format!(
                                                "\n\n> ⚠️ **Level Up Escalation:** Verification failed on `{}` ({})\n> Rolling back working tree to pre-step checkpoint and escalating to `{}` ({})...\n\n",
                                                cur_t, current_model, next_t, next_m
                                            );
                                            let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;
                                            current_tier = Some(next_t);
                                            if next_m.provider != current_model.provider {
                                                if let Some(ref app_cfg) = options.app_config {
                                                    if let Ok(np) = crate::providers::create_provider(next_m.provider, app_cfg) {
                                                        dynamic_provider = Some(np);
                                                    }
                                                }
                                            }
                                            current_model = next_m;
                                            continue;
                                        }
                                    }

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

                                    final_result = ModelResult::Edit {
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
                                    };
                                    break;
                                }
                            }
                        }
                        Err(err) => {
                            if let Some((cur_t, next_t)) = escalation {
                                let next_m = options.models_config.resolve_tier(next_t);
                                if next_m != current_model {
                                    let _ = transaction.on_rollback(&repo_root);
                                    context_manager.prune_missing_files();
                                    let notice = format!(
                                        "\n\n> ⚠️ **Level Up Escalation:** Patch application failed on `{}` ({}). Escalating to `{}` ({})...\n\n",
                                        cur_t, current_model, next_t, next_m
                                    );
                                    let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;
                                    current_tier = Some(next_t);
                                    if next_m.provider != current_model.provider {
                                        if let Some(ref app_cfg) = options.app_config {
                                            if let Ok(np) = crate::providers::create_provider(next_m.provider, app_cfg) {
                                                dynamic_provider = Some(np);
                                            }
                                        }
                                    }
                                    current_model = next_m;
                                    continue;
                                }
                            }

                            if transaction.on_rollback(&repo_root).unwrap_or(false) {
                                context_manager.clear_auto();
                                context_manager.prune_missing_files();
                                let ctx_state = context_manager.get_state();
                                let _ = stream_tx
                                    .send(StreamEvent::ContextChanged(ctx_state))
                                    .await;
                            }

                            final_result = ModelResult::Edit {
                                summary,
                                edits,
                                proposal: None,
                                applied: false,
                                error: Some(err.to_string()),
                                changed_files: Vec::new(),
                                commit_hash: None,
                            };
                            break;
                        }
                    }
                }
            }
            ModelResult::Answer { text } => {
                let mut text = if is_discussion {
                    text
                } else {
                    verify_and_append_to_answer(
                        &repo_root,
                        &mut pipeline_out.assistant_text,
                        text,
                        &stream_tx,
                        protocol,
                    )
                    .await
                };
                if let Some(ref notice) = plan_error_notice {
                    text.push_str(notice);
                }
                let step_done_tags = if is_isolated_plan_step {
                    protocol.parse_plan_step_done(&pipeline_out.assistant_text)
                } else {
                    Vec::new()
                };
                if !step_done_tags.is_empty() {
                    if let Ok(all_plans) =
                        crate::plan::storage::PlanStorage::load_all(&repo_root)
                    {
                        let active_id = target_plan_id
                            .clone()
                            .or_else(|| {
                                crate::plan::storage::PlanStorage::load_active_id(&repo_root)
                                    .ok()
                                    .flatten()
                            });
                        for step in &step_done_tags {
                            let effective_step_id = step.id.as_deref().or(target_step_id.as_deref());
                            if let Some(step_id) = effective_step_id {
                                let target_plan = active_id
                                    .as_deref()
                                    .and_then(|id| {
                                        all_plans
                                            .iter()
                                            .find(|p| p.id == id && p.find_item(step_id).is_some())
                                    })
                                    .or_else(|| {
                                        all_plans
                                            .iter()
                                            .find(|p| p.find_item(step_id).is_some())
                                    });

                                if let Some(plan) = target_plan {
                                    let _ = crate::plan::storage::PlanStorage::update_item(
                                        &repo_root,
                                        &plan.id,
                                        step_id,
                                        Some(tauqe_protocol::PlanItemStatus::Done),
                                    );
                                    let _ = crate::plan::storage::PlanStorage::save_active_id(
                                        &repo_root,
                                        Some(&plan.id),
                                    );
                                }
                            }
                        }
                    }
                }
                final_result = ModelResult::Answer {
                    text: protocol.clean_assistant_text(&text),
                };
                break;
            }
        }
    }

    pipeline_out.assistant_text = protocol.clean_assistant_text(&pipeline_out.assistant_text);
    if let Some(ref notice) = plan_error_notice {
        pipeline_out.assistant_text.push_str(notice);
    }

    let mut extra_history_files = Vec::new();
    for plan in &plan_outcome.applied {
        extra_history_files.push(format!("[PLAN] {}", plan.id));
    }
    let all_step_done = if is_isolated_plan_step {
        let mut list = protocol.parse_plan_step_done(&pipeline_out.assistant_text);
        if list.is_empty() {
            if let Some(ref target_id) = target_step_id {
                if matches!(final_result, ModelResult::Edit { applied: true, .. }) {
                    list.push(crate::edits::protocol::PlanStepDone {
                        id: Some(target_id.clone()),
                        note: None,
                    });
                }
            }
        }
        list
    } else {
        Vec::new()
    };
    for step in &all_step_done {
        let step_id = step.id.as_deref().or(target_step_id.as_deref()).unwrap_or("?");
        extra_history_files.push(format!("[STEP DONE] #{}", step_id));
    }
    if let Some(item_id) = target_review_item_id {
        if matches!(final_result, ModelResult::Edit { applied: true, .. }) {
            extra_history_files.push(format!("[REVIEW ITEM FIXED] #{}", item_id));
        }
    }

    record_workflow_response(
        history_manager,
        &final_result,
        &pipeline_out.assistant_text,
        extra_history_files,
    );

    Ok(WorkflowExecutionResult {
        result: final_result,
        assistant_text: pipeline_out.assistant_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::ModelTier;

    #[test]
    fn test_next_tier_escalation_chain() {
        assert_eq!(next_tier(ModelTier::Junior), Some(ModelTier::Middle));
        assert_eq!(next_tier(ModelTier::Middle), Some(ModelTier::Senior));
        assert_eq!(next_tier(ModelTier::Senior), None);
    }
}
