use std::path::Path;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tauqe_protocol::{EditProposal, ModelRef, ModelResult};

use super::common::{
    execute_edit_pipeline, record_workflow_response, run_verification_healing_loop,
    verify_and_append_to_answer, VerificationHealingResult,
};
use crate::edits::protocol::xml::tags::TerminalOutcome;
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
    let is_isolated_plan_step = crate::plan::prompt::is_plan_step_execution_prompt(prompt);
    let is_isolated_review_step = tauqe_protocol::review::is_review_step_execution_prompt(prompt);
    let is_isolated_step = is_isolated_plan_step || is_isolated_review_step;

    if !is_isolated_step {
        let _ = super::common::compact_history_if_needed_with_budget(
            history_manager,
            provider,
            &options.history_model,
            cancel_rx.clone(),
            options.history_budget_tokens,
            options.history_tail_turns,
        )
        .await;
    }

    let marked_protocol = if protocol.turn_marker().is_some() {
        None
    } else {
        let marker = crate::edits::protocol::generate_turn_marker();
        protocol.with_turn_marker(&marker)
    };
    let protocol: &dyn EditProtocol = marked_protocol.as_deref().unwrap_or(protocol);
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
    let mut plan_error_notice = None;
    let mut plan_outcome = crate::plan::storage::PlanApplicationOutcome::default();
    let mut last_terminal_outcome: Option<TerminalOutcome>;

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

        let terminal_outcome = protocol.parse_terminal_outcome(&pipeline_out.assistant_text);
        last_terminal_outcome = terminal_outcome.clone();
        if let Some(TerminalOutcome::Error { ref message }) = terminal_outcome {
            let _ = transaction.on_rollback(&repo_root);
            context_manager.clear_auto();
            context_manager.prune_missing_files();
            let ctx_state = context_manager.get_state();
            let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;

            final_result = ModelResult::Edit {
                summary: "Execution blocked".to_string(),
                edits: Vec::new(),
                proposal: None,
                applied: false,
                error: Some(message.clone()),
                changed_files: Vec::new(),
                commit_hash: None,
            };
            break;
        }

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

                            let turn_scratchpad = pipeline_out.turn_scratchpad.clone();
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
                                turn_scratchpad,
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
                                    let outcome_after_healing = protocol.parse_terminal_outcome(&pipeline_out.assistant_text);
                                    if let Some(ref o) = outcome_after_healing {
                                        last_terminal_outcome = Some(o.clone());
                                    }
                                    if let Some(TerminalOutcome::Error { ref message }) = outcome_after_healing {
                                        let _ = transaction.on_rollback(&repo_root);
                                        context_manager.clear_auto();
                                        context_manager.prune_missing_files();
                                        let ctx_state = context_manager.get_state();
                                        let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;

                                        final_result = ModelResult::Edit {
                                            summary,
                                            edits: Vec::new(),
                                            proposal: None,
                                            applied: false,
                                            error: Some(message.clone()),
                                            changed_files: Vec::new(),
                                            commit_hash: None,
                                        };
                                        break;
                                    }

                                    let commit_summary = outcome_after_healing
                                        .as_ref()
                                        .and_then(|t| t.summary())
                                        .or_else(|| terminal_outcome.as_ref().and_then(|t| t.summary()))
                                        .unwrap_or(&summary);

                                    let commit_hash = transaction
                                        .on_success(&repo_root, &total_changed_files, commit_summary)
                                        .unwrap_or(None);

                                    context_manager.clear_auto();
                                    let ctx_state = context_manager.get_state();
                                    let _ = stream_tx
                                        .send(StreamEvent::ContextChanged(ctx_state))
                                        .await;

                                    if is_isolated_plan_step {
                                        let is_step_done = matches!(outcome_after_healing, Some(TerminalOutcome::Success { .. }))
                                            || matches!(terminal_outcome, Some(TerminalOutcome::Success { .. }))
                                            || target_step_id.is_some();

                                        if is_step_done {
                                            if let Some(ref step_id) = target_step_id {
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

                                    if is_isolated_review_step {
                                        let is_review_fixed = matches!(outcome_after_healing, Some(TerminalOutcome::Success { .. }))
                                            || matches!(terminal_outcome, Some(TerminalOutcome::Success { .. }))
                                            || target_review_item_id.is_some();

                                        if is_review_fixed {
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
                if is_isolated_plan_step {
                    let is_step_done = matches!(terminal_outcome, Some(TerminalOutcome::Success { .. }));
                    if is_step_done {
                        if let Some(ref step_id) = target_step_id {
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

                if is_isolated_review_step {
                    let is_review_fixed = matches!(terminal_outcome, Some(TerminalOutcome::Success { .. }));
                    if is_review_fixed {
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
    if is_isolated_plan_step {
        let is_step_done = matches!(last_terminal_outcome, Some(TerminalOutcome::Success { .. }))
            || matches!(final_result, ModelResult::Edit { applied: true, .. });
        if is_step_done {
            let step_id = target_step_id.as_deref().unwrap_or("?");
            extra_history_files.push(format!("[STEP DONE] #{}", step_id));
        }
    }
    if is_isolated_review_step {
        let is_review_fixed = matches!(last_terminal_outcome, Some(TerminalOutcome::Success { .. }))
            || matches!(final_result, ModelResult::Edit { applied: true, .. });
        if is_review_fixed {
            let item_id = target_review_item_id
                .map(|i| i.to_string())
                .unwrap_or_else(|| "?".to_string());
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
    use crate::providers::ChatMessage;

    #[test]
    fn test_next_tier_escalation_chain() {
        assert_eq!(next_tier(ModelTier::Junior), Some(ModelTier::Middle));
        assert_eq!(next_tier(ModelTier::Middle), Some(ModelTier::Senior));
        assert_eq!(next_tier(ModelTier::Senior), None);
    }

    struct MockReviewStreamProvider {
        chunks: std::sync::Mutex<Vec<String>>,
    }

    impl MockReviewStreamProvider {
        fn new(chunks: Vec<String>) -> Self {
            Self {
                chunks: std::sync::Mutex::new(chunks),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockReviewStreamProvider {
        fn kind(&self) -> tauqe_protocol::ProviderKind {
            tauqe_protocol::ProviderKind::OpenRouter
        }

        async fn stream_chat(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            _response_format: Option<crate::providers::ResponseFormat>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let chunk = {
                let mut list = self.chunks.lock().unwrap();
                if list.is_empty() {
                    String::new()
                } else {
                    list.remove(0)
                }
            };
            let _ = tx.send(StreamEvent::TextDelta(chunk)).await;
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }

        async fn stream_text(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }
    }

    struct MockTx {
        commit_hash: Option<String>,
    }

    impl WorkflowTransaction for MockTx {
        fn on_success(
            &mut self,
            _repo_root: &Path,
            _changed_files: &[String],
            _summary: &str,
        ) -> anyhow::Result<Option<String>> {
            Ok(self.commit_hash.clone())
        }
    }

    #[tokio::test]
    async fn test_review_step_lifecycle_discovery_and_done() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let _ = crate::git::init_repository(Some(&repo_root), true).unwrap();

        std::fs::write(repo_root.join("target.rs"), "pub fn compute() -> i32 { 0 }\n").unwrap();
        std::fs::write(repo_root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n").unwrap();
        crate::git::cmd::stage_files(&repo_root, &["target.rs".to_string(), "helper.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(&repo_root, "Initial commit", false).unwrap();

        let session = tauqe_protocol::ReviewSession {
            id: "rev-test-1".to_string(),
            title: "Code review session".to_string(),
            created_at: 1000,
            model: "test-model".to_string(),
            description: None,
            user_prompt: None,
            target_files: vec!["target.rs".to_string()],
            items: vec![tauqe_protocol::ReviewItem {
                id: 1,
                title: "Fix compute return value".to_string(),
                model: None,
                severity: tauqe_protocol::ReviewSeverity::Critical,
                status: tauqe_protocol::ReviewItemStatus::Todo,
                file_path: Some("target.rs".to_string()),
                line_range: Some((1, 1)),
                body: "Return ANSWER from helper".to_string(),
            }],
            raw_markdown: String::new(),
        };
        crate::review::storage::ReviewStorage::save_session(&repo_root, &session).unwrap();

        let mut context_manager = ContextManager::new(repo_root.clone());
        context_manager.add_file("target.rs", tauqe_protocol::ContextAccess::Editable).unwrap();

        let mut history_manager = HistoryManager::new(repo_root.clone());
        let _ = history_manager.record_turn("Prior conversation prompt", vec!["unrelated.rs".to_string()]);

        let marker = "MOCKREV123";
        let protocol = crate::edits::protocol::xml::MarkedXmlEditProtocol::new(marker);

        let prompt = tauqe_protocol::review::format_review_step_execution_prompt(&session, 1, marker).unwrap();
        assert!(tauqe_protocol::review::is_review_step_execution_prompt(&prompt));

        let round1_output = format!(
            "I need helper definitions.\n<context_request_{m} path=\"helper.rs\" access=\"read_only\" />",
            m = marker
        );
        let round2_output = format!(
            "Updated return value.\n<tauqe_edits_{m} summary=\"Fix return value\">\n<edit_{m} path=\"target.rs\">\n<search_{m}>\npub fn compute() -> i32 {{ 0 }}\n</search_{m}>\n<replace_{m}>\npub fn compute() -> i32 {{ 42 }}\n</replace_{m}>\n</edit_{m}>\n</tauqe_edits_{m}>\n<success_{m} summary=\"Fixed return to 42\" />",
            m = marker
        );

        let provider = MockReviewStreamProvider::new(vec![round1_output, round2_output]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let options = WorkflowOptions {
            max_discovery_rounds: 3,
            ..Default::default()
        };

        let tx_mock = MockTx {
            commit_hash: Some("mock-commit-123".to_string()),
        };

        let res = execute_workflow_lifecycle(
            "git",
            &options,
            tx_mock,
            &prompt,
            &provider,
            &model,
            &mut context_manager,
            &mut history_manager,
            &protocol,
            tx,
            cancel_rx,
        )
        .await
        .unwrap();

        assert!(matches!(res.result, ModelResult::Edit { applied: true, .. }));
        let updated_content = std::fs::read_to_string(repo_root.join("target.rs")).unwrap();
        assert!(updated_content.contains("pub fn compute() -> i32 { 42 }"));

        let updated_session = crate::review::storage::ReviewStorage::load_review(&repo_root, "rev-test-1")
            .unwrap()
            .unwrap();
        assert_eq!(updated_session.items[0].status, tauqe_protocol::ReviewItemStatus::Fixed);

        let history_entries = history_manager.get_entries().unwrap();
        assert_eq!(history_entries.len(), 2);
        match &history_entries[1] {
            crate::history::entry::HistoryEntry::Response { files, .. } => {
                assert!(files.contains(&"[REVIEW ITEM FIXED] #1".to_string()));
            }
            _ => panic!("Expected Response entry"),
        }
    }

    #[tokio::test]
    async fn test_review_step_lifecycle_answer_with_review_step_done() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let _ = crate::git::init_repository(Some(&repo_root), true).unwrap();

        let session = tauqe_protocol::ReviewSession {
            id: "rev-answer-1".to_string(),
            title: "Analysis review session".to_string(),
            created_at: 2000,
            model: "test-model".to_string(),
            description: None,
            user_prompt: None,
            target_files: vec!["analysis.rs".to_string()],
            items: vec![tauqe_protocol::ReviewItem {
                id: 42,
                title: "Validate false positive".to_string(),
                model: None,
                severity: tauqe_protocol::ReviewSeverity::Suggestion,
                status: tauqe_protocol::ReviewItemStatus::Todo,
                file_path: Some("analysis.rs".to_string()),
                line_range: None,
                body: "Check if logic is correct".to_string(),
            }],
            raw_markdown: String::new(),
        };
        crate::review::storage::ReviewStorage::save_session(&repo_root, &session).unwrap();

        let mut context_manager = ContextManager::new(repo_root.clone());
        let mut history_manager = HistoryManager::new(repo_root.clone());

        let marker = "MOCKANS123";
        let protocol = crate::edits::protocol::xml::MarkedXmlEditProtocol::new(marker);

        let prompt = tauqe_protocol::review::format_review_step_execution_prompt(&session, 42, marker).unwrap();

        let answer_output = format!(
            "The logic is already verified and valid.\n<success_{m} summary=\"Confirmed false positive\" />",
            m = marker
        );
        let provider = MockReviewStreamProvider::new(vec![answer_output]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let options = WorkflowOptions::default();
        let tx_mock = MockTx { commit_hash: None };

        let res = execute_workflow_lifecycle(
            "git",
            &options,
            tx_mock,
            &prompt,
            &provider,
            &model,
            &mut context_manager,
            &mut history_manager,
            &protocol,
            tx,
            cancel_rx,
        )
        .await
        .unwrap();

        assert!(matches!(res.result, ModelResult::Answer { .. }));

        let updated_session = crate::review::storage::ReviewStorage::load_review(&repo_root, "rev-answer-1")
            .unwrap()
            .unwrap();
        assert_eq!(updated_session.items[0].status, tauqe_protocol::ReviewItemStatus::Fixed);
    }

    #[tokio::test]
    async fn test_step_lifecycle_with_success_terminal_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let _ = crate::git::init_repository(Some(&repo_root), true).unwrap();

        let plan = tauqe_protocol::Plan {
            id: "plan-term-success".to_string(),
            title: "Terminal Success Plan".to_string(),
            description: None,
            created_at: 100,
            updated_at: 100,
            concept: vec![],
            items: vec![tauqe_protocol::PlanItem {
                id: "step-1".to_string(),
                title: "Run diagnostic inspection".to_string(),
                details: None,
                status: tauqe_protocol::PlanItemStatus::Todo,
                files: vec![],
                concept_refs: vec![],
                children: vec![],
            }],
        };
        crate::plan::storage::PlanStorage::save_plan(&repo_root, &plan).unwrap();
        crate::plan::storage::PlanStorage::save_active_id(&repo_root, Some("plan-term-success")).unwrap();

        let mut context_manager = ContextManager::new(repo_root.clone());
        let mut history_manager = HistoryManager::new(repo_root.clone());

        let marker = "MOCKSUCCESS";
        let protocol = crate::edits::protocol::xml::MarkedXmlEditProtocol::new(marker);

        let prompt = crate::plan::prompt::format_plan_step_execution_prompt(&plan, "step-1", marker).unwrap();

        let answer_output = format!(
            "Inspection verified cleanly.\n<success_{m} summary=\"All components verified\" />",
            m = marker
        );
        let provider = MockReviewStreamProvider::new(vec![answer_output]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let options = WorkflowOptions::default();
        let tx_mock = MockTx { commit_hash: None };

        let res = execute_workflow_lifecycle(
            "git",
            &options,
            tx_mock,
            &prompt,
            &provider,
            &model,
            &mut context_manager,
            &mut history_manager,
            &protocol,
            tx,
            cancel_rx,
        )
        .await
        .unwrap();

        assert!(matches!(res.result, ModelResult::Answer { .. }));

        let updated_plan = crate::plan::storage::PlanStorage::load_plan(&repo_root, "plan-term-success")
            .unwrap()
            .unwrap();
        assert_eq!(updated_plan.items[0].status, tauqe_protocol::PlanItemStatus::Done);

        let history_entries = history_manager.get_entries().unwrap();
        assert_eq!(history_entries.len(), 1);
        match &history_entries[0] {
            crate::history::entry::HistoryEntry::Response { files, .. } => {
                assert!(files.contains(&"[STEP DONE] #step-1".to_string()));
            }
            _ => panic!("Expected Response entry"),
        }
    }

    #[tokio::test]
    async fn test_step_lifecycle_with_error_terminal_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();
        let _ = crate::git::init_repository(Some(&repo_root), true).unwrap();

        let plan = tauqe_protocol::Plan {
            id: "plan-term-error".to_string(),
            title: "Terminal Error Plan".to_string(),
            description: None,
            created_at: 100,
            updated_at: 100,
            concept: vec![],
            items: vec![tauqe_protocol::PlanItem {
                id: "step-err".to_string(),
                title: "Blocked step".to_string(),
                details: None,
                status: tauqe_protocol::PlanItemStatus::Todo,
                files: vec![],
                concept_refs: vec![],
                children: vec![],
            }],
        };
        crate::plan::storage::PlanStorage::save_plan(&repo_root, &plan).unwrap();
        crate::plan::storage::PlanStorage::save_active_id(&repo_root, Some("plan-term-error")).unwrap();

        let mut context_manager = ContextManager::new(repo_root.clone());
        let mut history_manager = HistoryManager::new(repo_root.clone());

        let marker = "MOCKERROR";
        let protocol = crate::edits::protocol::xml::MarkedXmlEditProtocol::new(marker);

        let prompt = crate::plan::prompt::format_plan_step_execution_prompt(&plan, "step-err", marker).unwrap();

        let error_output = format!(
            "I cannot continue.\n<error_{m} message=\"Required native library libfoo not found\" />",
            m = marker
        );
        let provider = MockReviewStreamProvider::new(vec![error_output]);
        let model = ModelRef::openrouter("test/model");
        let (tx, _rx) = mpsc::channel(100);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let options = WorkflowOptions::default();
        let tx_mock = MockTx { commit_hash: None };

        let res = execute_workflow_lifecycle(
            "git",
            &options,
            tx_mock,
            &prompt,
            &provider,
            &model,
            &mut context_manager,
            &mut history_manager,
            &protocol,
            tx,
            cancel_rx,
        )
        .await
        .unwrap();

        match res.result {
            ModelResult::Edit { applied, error, .. } => {
                assert!(!applied);
                assert_eq!(error.as_deref(), Some("Required native library libfoo not found"));
            }
            other => panic!("Expected ModelResult::Edit with applied: false, got: {:?}", other),
        }

        // Plan item status must remain Todo
        let updated_plan = crate::plan::storage::PlanStorage::load_plan(&repo_root, "plan-term-error")
            .unwrap()
            .unwrap();
        assert_eq!(updated_plan.items[0].status, tauqe_protocol::PlanItemStatus::Todo);

        // History entry should not claim the step is done
        let history_entries = history_manager.get_entries().unwrap();
        assert_eq!(history_entries.len(), 1);
        match &history_entries[0] {
            crate::history::entry::HistoryEntry::Response { files, .. } => {
                assert!(!files.contains(&"[STEP DONE] #step-err".to_string()));
            }
            _ => panic!("Expected Response entry"),
        }
    }
}
