//! Orthogonal FSM turn engine built on normalized [`LlmCommand`] values.
//!
//! Unlike the legacy pipeline in [`super::common`], which inspects protocol
//! syntax inline, this engine decodes model output through
//! [`PromptProtocol::decode`] into a uniform command vocabulary and drives the
//! discovery, staging, verification, and healing phases purely on those
//! commands. Transactional persistence is delegated to an [`ApplyPolicy`], so
//! the FSM never touches Git directly. Keeping both contours side by side lets
//! the core migrate to normalized commands without breaking the existing build.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use tokio::sync::{mpsc, watch};
use tauqe_protocol::{
    ContextAccess, EditOperation, EditProposal, LlmCommand, LlmOutcome, ModelRef, ModelResult,
    VerifyOnSuccess, VerifyTarget,
};

use crate::context::{ContextFileContent, ContextManager};
use crate::edits::protocol::{
    ContextRequest, VerifyOnSuccess as EditVerifyOnSuccess,
    VerifyRequest as EditVerifyRequest, VerifyTarget as EditVerifyTarget,
};
use crate::edits::{EditError, StagedEditsState};
use crate::history::HistoryManager;
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::PromptAssembly;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::common::{compact_history_if_needed_with_budget, record_workflow_response};
use crate::workflow::discovery::reconcile_discovery_context;
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::retry::{
    build_patch_retry_prompt, format_patch_retry_reason, prepare_staged_for_retry,
    retain_retried_failed_files,
};
use crate::workflow::verification::{
    build_verification_retry_prompt, extract_error_paths, run_workflow_verification,
    VerificationOutcome,
};
use crate::workflow::WorkflowOptions;

/// Result of a single FSM turn executed by [`run_fsm_turn`].
#[derive(Debug, Clone)]
pub struct FsmTurnOutcome {
    /// Conversational text produced across every proposal round.
    pub assistant_text: String,
    /// Normalized outcome of the turn.
    pub result: ModelResult,
    /// Whether the turn was cancelled by the client.
    pub is_cancelled: bool,
    /// Coder's block report when the model emitted a semantic `<blocked>` signal.
    ///
    /// The report is preserved through the workflow pipeline so the plan
    /// supervisor can move the step to `Blocked` and surface the diagnostic
    /// without running a patch retry or healing cycle.
    pub blocked: Option<String>,
}

/// Normalized proposal returned by a single model interaction.
struct ProposalStep {
    commands: Vec<LlmCommand>,
    is_cancelled: bool,
}

impl ProposalStep {
    /// Cleaned conversational text assembled from the decoded text commands.
    fn text(&self) -> String {
        let mut out = String::new();
        for command in &self.commands {
            if let LlmCommand::Text { content } = command {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(content);
            }
        }
        out
    }

    /// File edit operations carried by the proposal.
    fn edits(&self) -> Vec<EditOperation> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::Edit { operation } => Some(operation.clone()),
                _ => None,
            })
            .collect()
    }

    /// Context additions requested by the model.
    fn context_requests(&self) -> Vec<ContextRequest> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::ContextRequest { path, access } => Some(ContextRequest {
                    path: path.clone(),
                    access: *access,
                }),
                _ => None,
            })
            .collect()
    }

    /// Context evictions requested by the model.
    fn context_drops(&self) -> Vec<String> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::ContextDrop { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// Documentation topics requested by the model.
    fn doc_requests(&self) -> Vec<String> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::DocRequest { topic } => Some(topic.clone()),
                _ => None,
            })
            .collect()
    }

    /// Toolchain verification request carried by the proposal, if any.
    fn verify_request(&self) -> Option<EditVerifyRequest> {
        self.commands.iter().find_map(|command| match command {
            LlmCommand::Verify {
                target,
                on_success,
            } => Some(to_edit_verify_request(*target, *on_success)),
            _ => None,
        })
    }

    /// Explicit terminal outcome carried by the proposal, if any.
    fn terminal_outcome(&self) -> Option<LlmOutcome> {
        self.commands.iter().find_map(|command| match command {
            LlmCommand::Outcome { outcome } => Some(outcome.clone()),
            _ => None,
        })
    }

    /// Semantic block report carried by the proposal, if any.
    fn blocked_reason(&self) -> Option<String> {
        self.commands.iter().find_map(|command| match command {
            LlmCommand::Blocked { reason, details } => Some(match details {
                Some(details) if !details.trim().is_empty() => {
                    format!("{}\n\n{}", reason.trim(), details.trim())
                }
                _ => reason.clone(),
            }),
            _ => None,
        })
    }

    /// Ephemeral working-memory notes carried by the proposal, if any.
    fn scratchpad(&self) -> Option<String> {
        self.commands.iter().find_map(|command| match command {
            LlmCommand::Scratchpad { notes } => Some(notes.clone()),
            _ => None,
        })
    }

    /// Detected user language carried by the proposal, if any.
    fn language(&self) -> Option<String> {
        self.commands.iter().find_map(|command| match command {
            LlmCommand::UserLanguage { language } => Some(language.clone()),
            _ => None,
        })
    }

    /// Plan updates carried by the proposal, if any.
    fn plans(&self) -> Vec<tauqe_protocol::DiscussionPlanUpdate> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::Plan { update } => Some(update.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Mutable state accumulated across the proposal rounds of a single turn.
#[derive(Default)]
struct TurnState {
    language: Option<String>,
    scratchpad: Option<String>,
    verify_request: Option<EditVerifyRequest>,
    terminal_outcome: Option<LlmOutcome>,
    blocked: Option<String>,
    plans: Vec<tauqe_protocol::DiscussionPlanUpdate>,
}

impl TurnState {
    /// Folds the latest proposal round into the accumulated turn state.
    fn absorb(&mut self, step: &ProposalStep) {
        if let Some(language) = step.language() {
            self.language = Some(language);
        }
        if let Some(scratchpad) = step.scratchpad() {
            self.scratchpad = Some(scratchpad);
        }
        if let Some(request) = step.verify_request() {
            self.verify_request = Some(request);
        }
        if let Some(outcome) = step.terminal_outcome() {
            self.terminal_outcome = Some(outcome);
        }
        if let Some(reason) = step.blocked_reason() {
            self.blocked = Some(reason);
        }
        self.plans.extend(step.plans());
    }
}

/// Maps a normalized verification target onto the edit-protocol target.
fn to_edit_verify_request(target: VerifyTarget, on_success: VerifyOnSuccess) -> EditVerifyRequest {
    let target = match target {
        VerifyTarget::All => EditVerifyTarget::All,
        VerifyTarget::Check => EditVerifyTarget::Check,
        VerifyTarget::Test => EditVerifyTarget::Test,
        VerifyTarget::Clippy => EditVerifyTarget::Clippy,
    };
    let on_success = match on_success {
        VerifyOnSuccess::Silent => EditVerifyOnSuccess::Silent,
        VerifyOnSuccess::Report => EditVerifyOnSuccess::Report,
    };
    EditVerifyRequest { target, on_success }
}

/// Extracts a non-empty success summary from a terminal outcome.
fn extract_success_summary(outcome: Option<&LlmOutcome>) -> Option<String> {
    match outcome {
        Some(LlmOutcome::Success { summary }) => summary
            .as_ref()
            .map(|text| text.trim())
            .filter(|text| !text.is_empty())
            .map(|text| text.to_string()),
        _ => None,
    }
}

/// Extracts the blocking message from an error terminal outcome.
fn extract_error_message(outcome: Option<&LlmOutcome>) -> Option<String> {
    match outcome {
        Some(LlmOutcome::Error { message }) => Some(message.clone()),
        _ => None,
    }
}

/// Appends a round banner and the new conversational text to the accumulator.
fn merge_text(accumulated: String, banner: &str, new_text: &str) -> String {
    format!(
        "{}{}{}",
        accumulated.trim_end(),
        banner,
        new_text.trim_start()
    )
}

fn apply_turn_plans(
    repo_root: &Path,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
    assistant_text: &str,
) {
    if !plans.is_empty() {
        for plan_update in plans {
            let outcome = crate::plan::storage::PlanStorage::apply_discussion_plan_update(
                repo_root,
                plan_update,
            );
            for plan in outcome.applied {
                let _ = crate::plan::storage::PlanStorage::save_active_id(
                    repo_root,
                    Some(&plan.id),
                );
            }
        }
    } else {
        let outcome =
            crate::plan::storage::PlanStorage::apply_plan_tags(repo_root, assistant_text);
        for plan in outcome.applied {
            let _ = crate::plan::storage::PlanStorage::save_active_id(
                repo_root,
                Some(&plan.id),
            );
        }
    }
}

/// Records the turn response and packages the outcome.
fn finish_turn(
    history_manager: &mut HistoryManager,
    assistant_text: String,
    result: ModelResult,
    repo_root: &Path,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
) -> FsmTurnOutcome {
    apply_turn_plans(repo_root, plans, &assistant_text);
    record_workflow_response(history_manager, &result, &assistant_text, Vec::new());
    FsmTurnOutcome {
        assistant_text,
        result,
        is_cancelled: false,
        blocked: None,
    }
}

/// Finalizes a turn terminated by an explicit semantic `<blocked>` signal.
///
/// The step transaction is rolled back to its pre-step checkpoint and the
/// transient auto context is cleared. The coder's report is preserved on the
/// outcome so the caller can move the step to `Blocked`; no patch retry or
/// healing cycle runs, because a block asserts that the step preconditions
/// cannot be satisfied as stated.
#[allow(clippy::too_many_arguments)]
async fn finalize_blocked_turn(
    policy: &mut dyn ApplyPolicy,
    context_manager: &mut ContextManager,
    stream_tx: &mpsc::Sender<StreamEvent>,
    history_manager: &mut HistoryManager,
    repo_root: &Path,
    reason: String,
    accumulated_text: String,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
) -> FsmTurnOutcome {
    let _ = policy.rollback(repo_root);
    context_manager.clear_auto();
    context_manager.prune_missing_files();
    let ctx_state = context_manager.get_state();
    let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
    let _ = stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary: "Execution blocked".to_string(),
        edits: Vec::new(),
        proposal: None,
        applied: false,
        error: Some(reason.clone()),
        changed_files: Vec::new(),
        commit_hash: None,
    };
    let mut outcome = finish_turn(history_manager, accumulated_text, result, repo_root, plans);
    outcome.blocked = Some(reason);
    outcome
}

/// Runs the orthogonal FSM turn: discovery, staging, verification, and healing.
///
/// The engine consumes normalized [`LlmCommand`] values produced by
/// [`PromptProtocol::decode`] and persists validated changes exclusively through
/// the supplied [`ApplyPolicy`]. It never parses protocol syntax and never
/// touches Git directly, which keeps the control flow independent of any single
/// wire format or transactional backend.
#[allow(clippy::too_many_arguments)]
pub async fn run_fsm_turn(
    prompt: &str,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    context_manager: &mut ContextManager,
    history_manager: &mut HistoryManager,
    protocol: &dyn PromptProtocol,
    policy: &mut dyn ApplyPolicy,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    options: &WorkflowOptions,
) -> anyhow::Result<FsmTurnOutcome> {
    let repo_root = context_manager.repo_root().to_path_buf();
    let is_isolated_step = crate::plan::prompt::is_plan_step_execution_prompt(prompt)
        || tauqe_protocol::review::is_review_step_execution_prompt(prompt);

    if !is_isolated_step {
        let _ = compact_history_if_needed_with_budget(
            history_manager,
            provider,
            &options.history_model,
            cancel_rx.clone(),
            options.history_budget_tokens,
            options.history_tail_turns,
        )
        .await;
        let context_paths: Vec<String> = context_manager
            .get_state()
            .items
            .iter()
            .map(|item| item.path.clone())
            .collect();
        let _ = history_manager.record_turn(prompt, context_paths);
    }

    let _ = stream_tx
        .send(StreamEvent::TurnPhase {
            phase: tauqe_protocol::TurnPhase::Proposal,
            round: Some(1),
            max_rounds: None,
            detail: None,
        })
        .await;

    let mut state = TurnState::default();
    let mut step = run_proposal_step(
        prompt,
        provider,
        model,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        &stream_tx,
        cancel_rx.clone(),
        options,
        None,
        state.language.as_deref(),
        state.scratchpad.as_deref(),
        is_isolated_step,
    )
    .await?;
    state.absorb(&step);

    let mut accumulated_text = step.text();

    if step.is_cancelled {
        let _ = stream_tx.send(StreamEvent::Done).await;
        return Ok(FsmTurnOutcome {
            assistant_text: accumulated_text,
            result: ModelResult::Answer {
                text: String::new(),
            },
            is_cancelled: true,
            blocked: None,
        });
    }

    if let Some(reason) = state.blocked.clone() {
        return Ok(finalize_blocked_turn(
            policy,
            context_manager,
            &stream_tx,
            history_manager,
            &repo_root,
            reason,
            accumulated_text,
            &state.plans,
        )
        .await);
    }

    // Discovery loop: the model may request files before proposing edits.
    let mut discovery_round = 0;
    let mut round_prompt = prompt.to_string();
    while discovery_round < options.max_discovery_rounds && step.edits().is_empty() {
        let requests = step.context_requests();
        let drops = step.context_drops();
        let doc_topics = step.doc_requests();
        if requests.is_empty() && drops.is_empty() && doc_topics.is_empty() {
            break;
        }

        let available_files =
            crate::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
        let outcome = reconcile_discovery_context(
            context_manager,
            options.discovery_mode,
            &requests,
            &drops,
            &available_files,
            options.max_files,
        );

        if let Some(limit) = outcome.limit_exceeded {
            let warn = format!(
                "Context files limit exceeded (max_files = {}). The model requested {} new file(s) [{}], but the context already contains {} files. Scope the task down or decompose it into a plan.",
                limit.max_files,
                limit.requested_count,
                limit.requested_files.join(", "),
                limit.current_count
            );
            tracing::warn!("{}", warn);
            let _ = stream_tx
                .send(StreamEvent::TextDelta(format!("\n\n⚠️ {}", warn)))
                .await;
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(FsmTurnOutcome {
                assistant_text: warn.clone(),
                result: ModelResult::Answer { text: warn },
                is_cancelled: false,
                blocked: None,
            });
        }

        let context_changed = !outcome.added.is_empty()
            || !outcome.dropped.is_empty()
            || !outcome.missing.is_empty();
        if !context_changed && doc_topics.is_empty() {
            break;
        }

        discovery_round += 1;
        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: tauqe_protocol::TurnPhase::Discovery,
                round: Some(discovery_round),
                max_rounds: Some(options.max_discovery_rounds),
                detail: None,
            })
            .await;

        if !outcome.missing.is_empty() {
            round_prompt.push_str(&format!(
                "\n\n<context_feedback>\nThe following requested files do not exist in the repository: [{}]\nPlease check <repo_map> for exact existing file paths.\n</context_feedback>",
                outcome.missing.join(", ")
            ));
        }

        if !doc_topics.is_empty() {
            let documentation = doc_topics
                .iter()
                .map(|topic| crate::docs::get_documentation(topic))
                .collect::<Vec<_>>()
                .join("\n\n");
            round_prompt.push_str(&format!(
                "\n\n<system_documentation>\n{}\n</system_documentation>",
                documentation
            ));
        }

        let ctx_state = context_manager.get_state();
        let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;

        let banner = format!(
            "\n\n---\n**[Round {}]** {} file(s) added, {} dropped.\n\n",
            discovery_round + 1,
            outcome.added.len(),
            outcome.dropped.len()
        );
        let _ = stream_tx
            .send(StreamEvent::TextDelta(banner.clone()))
            .await;
        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: tauqe_protocol::TurnPhase::Proposal,
                round: Some(discovery_round + 1),
                max_rounds: None,
                detail: None,
            })
            .await;

        step = run_proposal_step(
            &round_prompt,
            provider,
            model,
            context_manager,
            history_manager,
            protocol,
            workflow_name,
            &stream_tx,
            cancel_rx.clone(),
            options,
            None,
            state.language.as_deref(),
            state.scratchpad.as_deref(),
            is_isolated_step,
        )
        .await?;
        state.absorb(&step);
        accumulated_text = merge_text(accumulated_text, &banner, &step.text());

        if step.is_cancelled {
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(FsmTurnOutcome {
                assistant_text: accumulated_text,
                result: ModelResult::Answer {
                    text: String::new(),
                },
                is_cancelled: true,
                blocked: None,
            });
        }

        if let Some(reason) = state.blocked.clone() {
            return Ok(finalize_blocked_turn(
                policy,
                context_manager,
                &stream_tx,
                history_manager,
                &repo_root,
                reason,
                accumulated_text,
                &state.plans,
            )
            .await);
        }
    }

    // A proposal that already carries edits ends the discovery cycle before its
    // eviction commands are reconciled. Apply those drops here so a proposal
    // that both edits files and releases context never silently loses a
    // `<context_drop>` command; the reconciled context is what the subsequent
    // verification-healing rounds observe.
    let trailing_drops = step.context_drops();
    if !step.edits().is_empty() && !trailing_drops.is_empty() {
        let available_files =
            crate::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
        let outcome = reconcile_discovery_context(
            context_manager,
            options.discovery_mode,
            &[],
            &trailing_drops,
            &available_files,
            options.max_files,
        );
        if !outcome.dropped.is_empty() {
            let ctx_state = context_manager.get_state();
            let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
        }
    }

    // Terminal error short-circuits the turn before any staging.
    if let Some(message) = extract_error_message(state.terminal_outcome.as_ref()) {
        let _ = policy.rollback(&repo_root);
        context_manager.clear_auto();
        context_manager.prune_missing_files();
        let ctx_state = context_manager.get_state();
        let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
        let _ = stream_tx.send(StreamEvent::Done).await;
        let result = ModelResult::Edit {
            summary: "Execution blocked".to_string(),
            edits: Vec::new(),
            proposal: None,
            applied: false,
            error: Some(message),
            changed_files: Vec::new(),
            commit_hash: None,
        };
        return Ok(finish_turn(history_manager, accumulated_text, result, &repo_root, &state.plans));
    }

    let mut current_edits = step.edits();
    let mut current_verify = state.verify_request.clone();

    if current_edits.is_empty() {
        let mut answer_text = accumulated_text;
        if let Some(request) = current_verify.take() {
            let outcome = run_workflow_verification(&repo_root, &request, &stream_tx, true).await;
            if !outcome.report.trim().is_empty() {
                if answer_text.trim().is_empty() {
                    answer_text = outcome.report;
                } else {
                    answer_text = format!("{}\n\n{}", answer_text.trim(), outcome.report.trim());
                }
            }
        }
        let _ = stream_tx.send(StreamEvent::Done).await;
        let result = ModelResult::Answer {
            text: answer_text.clone(),
        };
        return Ok(finish_turn(history_manager, answer_text, result, &repo_root, &state.plans));
    }

    // Staging loop: validate edits in memory, repairing failures.
    let mut staged_state: Option<StagedEditsState> = None;
    let mut cumulative_edits: Vec<EditOperation> = Vec::new();
    let mut succeeded_files: HashSet<String> = HashSet::new();
    let mut last_failed_errors: HashMap<String, EditError> = HashMap::new();

    let _ = stream_tx
        .send(StreamEvent::TurnPhase {
            phase: tauqe_protocol::TurnPhase::Staging,
            round: Some(1),
            max_rounds: Some(options.max_retries),
            detail: None,
        })
        .await;

    let mut attempt = 0;
    loop {
        let stage_res =
            policy.staging(&repo_root, context_manager, &current_edits, staged_state.as_ref());
        staged_state = Some(stage_res.staged_state);
        succeeded_files.extend(stage_res.successful_paths);
        cumulative_edits.extend(stage_res.successful_edits);

        for path in &succeeded_files {
            last_failed_errors.remove(path);
        }
        for failed in stage_res.failed_files {
            last_failed_errors.insert(failed.path, failed.error);
        }

        if last_failed_errors.is_empty() || attempt >= options.max_retries {
            break;
        }
        attempt += 1;

        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: tauqe_protocol::TurnPhase::Staging,
                round: Some(attempt),
                max_rounds: Some(options.max_retries),
                detail: Some(format!("Patch retry {}/{}", attempt, options.max_retries)),
            })
            .await;
        for (path, error) in &last_failed_errors {
            let reason = format_patch_retry_reason(error);
            let _ = stream_tx
                .send(StreamEvent::EditFileRetrying {
                    path: path.clone(),
                    attempt,
                    max_retries: options.max_retries,
                    reason,
                })
                .await;
        }

        let retry_banner = format!(
            "\n\n---\n**[Patch Retry {}/{}]** Correcting failed edits...\n\n",
            attempt, options.max_retries
        );
        let _ = stream_tx
            .send(StreamEvent::TextDelta(retry_banner.clone()))
            .await;

        let repair_prompt = build_patch_retry_prompt(
            &succeeded_files,
            &last_failed_errors,
            state.language.as_deref(),
        );
        let step_out = run_proposal_step(
            &repair_prompt,
            provider,
            model,
            context_manager,
            history_manager,
            protocol,
            workflow_name,
            &stream_tx,
            cancel_rx.clone(),
            options,
            staged_state.as_ref(),
            state.language.as_deref(),
            state.scratchpad.as_deref(),
            is_isolated_step,
        )
        .await?;

        if step_out.is_cancelled {
            let _ = stream_tx.send(StreamEvent::Done).await;
            return Ok(FsmTurnOutcome {
                assistant_text: accumulated_text,
                result: ModelResult::Answer {
                    text: String::new(),
                },
                is_cancelled: true,
                blocked: None,
            });
        }
        state.absorb(&step_out);
        accumulated_text = merge_text(accumulated_text, &retry_banner, &step_out.text());

        if let Some(reason) = state.blocked.clone() {
            return Ok(finalize_blocked_turn(
                policy,
                context_manager,
                &stream_tx,
                history_manager,
                &repo_root,
                reason,
                accumulated_text,
                &state.plans,
            )
            .await);
        }

        current_edits = step_out.edits();
        current_verify = state.verify_request.clone();
        retain_retried_failed_files(&mut last_failed_errors, context_manager, &current_edits);
        if let Some(ref mut staged) = staged_state {
            prepare_staged_for_retry(
                staged,
                &mut cumulative_edits,
                &mut succeeded_files,
                context_manager,
                &current_edits,
            );
        }
    }

    if let Some(message) = extract_error_message(state.terminal_outcome.as_ref()) {
        let _ = policy.rollback(&repo_root);
        context_manager.clear_auto();
        context_manager.prune_missing_files();
        let ctx_state = context_manager.get_state();
        let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
        let _ = stream_tx.send(StreamEvent::Done).await;
        let result = ModelResult::Edit {
            summary: "Execution blocked".to_string(),
            edits: Vec::new(),
            proposal: None,
            applied: false,
            error: Some(message),
            changed_files: Vec::new(),
            commit_hash: None,
        };
        return Ok(finish_turn(history_manager, accumulated_text, result, &repo_root, &state.plans));
    }

    if !last_failed_errors.is_empty() {
        let mut err_lines = Vec::new();
        let mut failed_sorted: Vec<(&String, &EditError)> = last_failed_errors.iter().collect();
        failed_sorted.sort_by_key(|(path, _)| *path);
        for (path, error) in failed_sorted {
            err_lines.push(format!("{}: {}", path, error));
        }
        let error_desc = format!(
            "Failed to apply edits after {} retries:\n{}",
            options.max_retries,
            err_lines.join("\n")
        );
        let _ = stream_tx.send(StreamEvent::Done).await;
        let result = ModelResult::Edit {
            summary: "FSM edits failed".to_string(),
            edits: cumulative_edits,
            proposal: None,
            applied: false,
            error: Some(error_desc),
            changed_files: Vec::new(),
            commit_hash: None,
        };
        return Ok(finish_turn(history_manager, accumulated_text, result, &repo_root, &state.plans));
    }

    if cumulative_edits.is_empty() {
        let _ = stream_tx.send(StreamEvent::Done).await;
        let result = ModelResult::Answer {
            text: accumulated_text.clone(),
        };
        return Ok(finish_turn(history_manager, accumulated_text, result, &repo_root, &state.plans));
    }

    let mut summary = extract_success_summary(state.terminal_outcome.as_ref())
        .unwrap_or_else(|| "Apply FSM turn edits".to_string());

    let proposal = EditProposal {
        summary: summary.clone(),
        edits: cumulative_edits.clone(),
    };

    let mut total_changed_files =
        match policy.apply_temporary(&repo_root, context_manager, &proposal) {
            Ok(files) => files,
            Err(error) => {
                let _ = policy.rollback(&repo_root);
                context_manager.prune_missing_files();
                let ctx_state = context_manager.get_state();
                let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
                let _ = stream_tx.send(StreamEvent::Done).await;
                let result = ModelResult::Edit {
                    summary,
                    edits: cumulative_edits,
                    proposal: None,
                    applied: false,
                    error: Some(error.to_string()),
                    changed_files: Vec::new(),
                    commit_hash: None,
                };
                return Ok(finish_turn(history_manager, accumulated_text, result, &repo_root, &state.plans));
            }
        };

    if let Some(mut verify_request) = current_verify {
        let _ = stream_tx
            .send(StreamEvent::TurnPhase {
                phase: tauqe_protocol::TurnPhase::Verification,
                round: None,
                max_rounds: None,
                detail: None,
            })
            .await;
        let mut verify_outcome =
            run_workflow_verification(&repo_root, &verify_request, &stream_tx, false).await;
        if !verify_outcome.success {
            auto_register_error_files(
                context_manager,
                &verify_outcome.report,
                &repo_root,
                &stream_tx,
            )
            .await;
        }

        let mut healing_attempt = 0;
        while !verify_outcome.success && healing_attempt < options.max_retries {
            healing_attempt += 1;
            let _ = stream_tx
                .send(StreamEvent::TurnPhase {
                    phase: tauqe_protocol::TurnPhase::Healing,
                    round: Some(healing_attempt),
                    max_rounds: Some(options.max_retries),
                    detail: None,
                })
                .await;

            let retry_banner = format!(
                "\n\n---\n**[Verification Retry {}/{}]** Correcting verification failures...\n\n",
                healing_attempt, options.max_retries
            );
            let _ = stream_tx
                .send(StreamEvent::TextDelta(retry_banner.clone()))
                .await;

            let retry_prompt = build_verification_retry_prompt(
                &verify_outcome.report,
                state.language.as_deref(),
            );
            let step_out = run_proposal_step(
                &retry_prompt,
                provider,
                model,
                context_manager,
                history_manager,
                protocol,
                workflow_name,
                &stream_tx,
                cancel_rx.clone(),
                options,
                None,
                state.language.as_deref(),
                state.scratchpad.as_deref(),
                is_isolated_step,
            )
            .await?;

            if step_out.is_cancelled {
                if policy.rollback(&repo_root).unwrap_or(false) {
                    context_manager.clear_auto();
                    context_manager.prune_missing_files();
                    let ctx_state = context_manager.get_state();
                    let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
                }
                let _ = stream_tx.send(StreamEvent::Done).await;
                return Ok(FsmTurnOutcome {
                    assistant_text: accumulated_text,
                    result: ModelResult::Answer {
                        text: String::new(),
                    },
                    is_cancelled: true,
                    blocked: None,
                });
            }
            state.absorb(&step_out);
            accumulated_text = merge_text(accumulated_text, &retry_banner, &step_out.text());

            if let Some(reason) = state.blocked.clone() {
                return Ok(finalize_blocked_turn(
                    policy,
                    context_manager,
                    &stream_tx,
                    history_manager,
                    &repo_root,
                    reason,
                    accumulated_text,
                    &state.plans,
                )
                .await);
            }

            let retry_edits = step_out.edits();
            if retry_edits.is_empty() {
                break;
            }

            let retry_proposal = EditProposal {
                summary: summary.clone(),
                edits: retry_edits,
            };
            match policy.apply_temporary(&repo_root, context_manager, &retry_proposal) {
                Ok(new_files) => {
                    for file in new_files {
                        if !total_changed_files.contains(&file) {
                            total_changed_files.push(file);
                        }
                    }
                    if let Some(new_request) = step_out.verify_request() {
                        verify_request = new_request;
                    }
                    verify_outcome =
                        run_workflow_verification(&repo_root, &verify_request, &stream_tx, false)
                            .await;
                    if !verify_outcome.success {
                        auto_register_error_files(
                            context_manager,
                            &verify_outcome.report,
                            &repo_root,
                            &stream_tx,
                        )
                        .await;
                    }
                }
                Err(error) => {
                    verify_outcome = VerificationOutcome {
                        success: false,
                        report: format!("Failed to apply repair edits: {}", error),
                    };
                }
            }
        }

        if verify_outcome.success {
            if let Some(text) = extract_success_summary(state.terminal_outcome.as_ref()) {
                summary = text;
            }
            let commit_hash = policy
                .apply_final(&repo_root, &total_changed_files, &summary)
                .unwrap_or(None);
            context_manager.clear_auto();
            let ctx_state = context_manager.get_state();
            let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
            let _ = stream_tx.send(StreamEvent::Done).await;
            let result = ModelResult::Edit {
                summary,
                edits: cumulative_edits,
                proposal: None,
                applied: true,
                error: None,
                changed_files: total_changed_files,
                commit_hash,
            };
            return Ok(finish_turn(history_manager, accumulated_text, result, &repo_root, &state.plans));
        }

        let changed_files = if policy.rollback(&repo_root).unwrap_or(false) {
            context_manager.clear_auto();
            context_manager.prune_missing_files();
            let ctx_state = context_manager.get_state();
            let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
            Vec::new()
        } else {
            total_changed_files
        };
        let _ = stream_tx.send(StreamEvent::Done).await;
        let result = ModelResult::Edit {
            summary,
            edits: cumulative_edits,
            proposal: None,
            applied: false,
            error: Some(format!("Code verification failed:\n{}", verify_outcome.report)),
            changed_files,
            commit_hash: None,
        };
        return Ok(finish_turn(history_manager, accumulated_text, result, &repo_root, &state.plans));
    }

    // No verification requested: finalize immediately.
    if let Some(text) = extract_success_summary(state.terminal_outcome.as_ref()) {
        summary = text;
    }
    let commit_hash = policy
        .apply_final(&repo_root, &total_changed_files, &summary)
        .unwrap_or(None);
    context_manager.clear_auto();
    let ctx_state = context_manager.get_state();
    let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
    let _ = stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary,
        edits: cumulative_edits,
        proposal: None,
        applied: true,
        error: None,
        changed_files: total_changed_files,
        commit_hash,
    };
    Ok(finish_turn(history_manager, accumulated_text, result, &repo_root, &state.plans))
}

/// Runs a single atomic model interaction and decodes it into commands.
#[allow(clippy::too_many_arguments)]
async fn run_proposal_step(
    prompt: &str,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    context_manager: &ContextManager,
    history_manager: &HistoryManager,
    protocol: &dyn PromptProtocol,
    workflow_name: &str,
    stream_tx: &mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    options: &WorkflowOptions,
    staged_state: Option<&StagedEditsState>,
    target_language: Option<&str>,
    turn_scratchpad: Option<&str>,
    is_isolated_step: bool,
) -> anyhow::Result<ProposalStep> {
    let repo_root = context_manager.repo_root().to_path_buf();
    let repo_state = crate::git::get_repository_state(Some(&repo_root));
    let ctx_state = context_manager.get_state();
    let mut context_files = context_manager.read_context_files();

    if let Some(staged) = staged_state {
        for (path, content) in &staged.staged_files {
            if let Some(file) = context_files.iter_mut().find(|file| &file.path == path) {
                file.content = content.clone();
            } else {
                context_files.push(ContextFileContent {
                    path: path.clone(),
                    access: ContextAccess::Editable,
                    content: content.clone(),
                });
            }
        }
        context_files.retain(|file| !staged.deleted_paths.contains(&file.path));
        context_files.sort_by(|a, b| a.path.cmp(&b.path));
    }

    let targetable_paths: Vec<String> = context_files
        .iter()
        .map(|file| file.path.clone())
        .collect();

    let available_files = crate::git::list_repository_files(Some(&repo_root)).unwrap_or_default();
    let repo_map = crate::repomap::generate_repo_map(
        &repo_root,
        &available_files,
        &targetable_paths,
        options.repomap_token_budget,
    )
    .ok()
    .filter(|map| !map.trim().is_empty());

    let history_tag = if is_isolated_step {
        None
    } else {
        history_manager.format_history_tag().ok()
    };

    let mut assembly = PromptAssembly::new(
        Some(repo_state),
        ctx_state.revision,
        context_files,
        workflow_name,
        protocol.name(),
        history_tag,
        repo_map,
    );
    assembly.target_language = target_language.map(|language| language.to_string());
    assembly.is_isolated_step = is_isolated_step;
    assembly.discovery_mode = options.discovery_mode;
    assembly.turn_scratchpad = turn_scratchpad.map(|notes| notes.to_string());

    let messages = assembly.assemble_chat_messages_for(prompt, protocol);
    let response_format = protocol.response_format(&targetable_paths);

    let (llm_tx, mut llm_rx) = mpsc::channel::<StreamEvent>(100);
    let client_call = provider.stream_chat_with_effort(
        &model.name,
        messages,
        response_format,
        options.reasoning_effort,
        llm_tx,
        cancel_rx.clone(),
    );

    let staged_for_filter = staged_state
        .map(|staged| staged.staged_files.clone())
        .unwrap_or_default();
    let mut stream_filter =
        protocol.create_stream_filter(targetable_paths, repo_root, staged_for_filter);
    let forward_stream_tx = stream_tx.clone();

    let forward_task = tokio::spawn(async move {
        let mut assistant_text = String::new();
        let mut cancelled = false;
        while let Some(event) = llm_rx.recv().await {
            match event {
                StreamEvent::TextDelta(ref delta) => {
                    assistant_text.push_str(delta);
                    for forwarded in stream_filter.push_chunk(delta) {
                        if forward_stream_tx.send(forwarded).await.is_err() {
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
                    if forward_stream_tx.send(other).await.is_err() {
                        break;
                    }
                }
            }
        }
        for forwarded in stream_filter.finish() {
            let _ = forward_stream_tx.send(forwarded).await;
        }
        (assistant_text, cancelled)
    });

    let client_result = client_call.await;
    let (assistant_text, cancelled) = forward_task.await.unwrap_or_default();
    client_result?;

    let commands = protocol.decode(&assistant_text);

    Ok(ProposalStep {
        commands,
        is_cancelled: cancelled,
    })
}

/// Registers files referenced by a verification failure into editable context.
async fn auto_register_error_files(
    context_manager: &mut ContextManager,
    report: &str,
    repo_root: &Path,
    stream_tx: &mpsc::Sender<StreamEvent>,
) {
    let available_files = crate::git::list_repository_files(Some(repo_root)).unwrap_or_default();
    let error_paths = extract_error_paths(report, &available_files);
    let mut added = Vec::new();
    for path in error_paths {
        if context_manager.effective_access_for(&path) != Some(ContextAccess::Editable)
            && context_manager.add_auto_file(&path, ContextAccess::Editable).is_ok()
        {
            added.push(path);
        }
    }
    if !added.is_empty() {
        let ctx_state = context_manager.get_state();
        let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
        let notice = format!(
            "\n\n---\nAdded {} error file(s) to editable context: {}\n\n",
            added.len(),
            added.join(", ")
        );
        let _ = stream_tx.send(StreamEvent::TextDelta(notice)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::xml::XmlPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::policy::DirectApplyPolicy;

    const TARGET: &str = "pub fn compute() -> i32 { 0 }\n";

    fn setup_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        crate::git::init_repository(Some(&root), true).unwrap();
        for (path, content) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, content).unwrap();
        }
        let owned: Vec<String> = files.iter().map(|(path, _)| path.to_string()).collect();
        crate::git::cmd::stage_files(&root, &owned).unwrap();
        crate::git::cmd::commit_with_developer_author(&root, "Add fixtures", false).unwrap();
        (dir, root)
    }

    fn context_with_target(root: &Path) -> ContextManager {
        let mut context = ContextManager::new(root.to_path_buf());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        context
    }

    fn edit_block(search: &str, replace: &str) -> String {
        format!(
            "<tauqe_edits summary=\"Edit\">\n<edit path=\"a.rs\">\n<search>\n{search}\n</search>\n<replace>\n{replace}\n</replace>\n</edit>\n</tauqe_edits>"
        )
    }

    #[tokio::test]
    async fn applies_edit_through_direct_policy() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text(edit_block(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        ))]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { applied: true, .. }
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
    }

    #[tokio::test]
    async fn discovers_context_before_proposing_edits() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper.rs", "pub const ANSWER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "I need helper.rs.\n<context_request path=\"helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use the helper constant",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { applied: true, .. }
        ));
        assert_eq!(provider.requests().len(), 2);
        let second_request = &provider.requests()[1];
        assert!(second_request.messages[0]
            .content
            .contains("pub const ANSWER: i32 = 42;"));
    }

    #[tokio::test]
    async fn retries_failed_patch_before_applying() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(edit_block("nonexistent text", "irrelevant")),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { applied: true, .. }
        ));
        assert!(outcome.assistant_text.contains("Patch Retry"));
        assert_eq!(provider.requests().len(), 2);
    }

    #[tokio::test]
    async fn accepts_answer_without_edits() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text(
            "The function already returns the expected value.",
        )]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Explain a.rs",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(outcome.result, ModelResult::Answer { .. }));
    }

    #[tokio::test]
    async fn saves_plan_from_turn_output() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let plan_xml = concat!(
            "Creating plan:\n",
            "<plan action=\"save\" id=\"test-fsm-plan\" title=\"FSM Plan\">\n",
            "  <concept>\n",
            "    <concept_item id=\"c-1\" title=\"FSM Plan Architecture\" />\n",
            "  </concept>\n",
            "  <item id=\"1\" title=\"Initial Step\" />\n",
            "</plan>\n",
            "Done.",
        );
        let provider = MockLlmProvider::scripted([MockResponse::text(plan_xml)]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Plan work",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(outcome.result, ModelResult::Answer { .. }));
        let loaded = crate::plan::storage::PlanStorage::load_plan(&root, "test-fsm-plan")
            .unwrap();
        assert!(loaded.is_some());
        let plan = loaded.unwrap();
        assert_eq!(plan.title, "FSM Plan");
        assert_eq!(plan.items.len(), 1);
    }

    #[tokio::test]
    async fn context_drop_evicts_auto_file_before_follow_up_round() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("obsolete.rs", "pub const OBSOLETE: i32 = 7;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "Loading obsolete context.\n<context_request path=\"obsolete.rs\" access=\"read_only\" />",
            ),
            MockResponse::text("Releasing obsolete context.\n<context_drop path=\"obsolete.rs\" />"),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            discovery_mode: crate::config::DiscoveryMode::Incremental,
            ..WorkflowOptions::default()
        };
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { applied: true, .. }
        ));
        assert_eq!(provider.requests().len(), 3);
        let final_request = &provider.requests()[2];
        assert!(
            !final_request.messages[0].content.contains("OBSOLETE"),
            "the evicted file must not reach the follow-up proposal"
        );
    }

    #[tokio::test]
    async fn scratchpad_is_preserved_and_injected_across_rounds() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper.rs", "pub const ANSWER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "Recording my working notes.\n<scratchpad>\n- Hypothesis: helper.rs holds ANSWER\n</scratchpad>\n<context_request path=\"helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use the helper constant",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { applied: true, .. }
        ));
        assert_eq!(provider.requests().len(), 2);
        let follow_up = &provider.requests()[1];
        assert!(follow_up.messages[0].content.contains("<turn_scratchpad>"));
        assert!(follow_up.messages[0]
            .content
            .contains("Hypothesis: helper.rs holds ANSWER"));
    }

    #[tokio::test]
    async fn scratchpad_from_discovery_reaches_patch_retry() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper.rs", "pub const ANSWER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "Recording my working notes.\n<scratchpad>\n- Hypothesis: helper.rs holds ANSWER\n</scratchpad>\n<context_request path=\"helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_block("nonexistent text", "irrelevant")),
            MockResponse::text(edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Use the helper constant",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { applied: true, .. }
        ));
        assert_eq!(provider.requests().len(), 3);
        let retry_request = &provider.requests()[2];
        assert!(retry_request.messages[0]
            .content
            .contains("<turn_scratchpad>"));
        assert!(retry_request.messages[0]
            .content
            .contains("Hypothesis: helper.rs holds ANSWER"));
    }

    #[tokio::test]
    async fn terminal_error_outcome_blocks_the_turn() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text(
            "I cannot proceed.\n<error message=\"Missing dependency\" />",
        )]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Make the change",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        match outcome.result {
            ModelResult::Edit {
                applied,
                error,
                edits,
                ..
            } => {
                assert!(!applied);
                assert!(edits.is_empty());
                assert_eq!(error.as_deref(), Some("Missing dependency"));
            }
            other => panic!("expected a blocked turn, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), TARGET);
    }

    #[tokio::test]
    async fn terminal_success_outcome_sets_the_edit_summary() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let response = format!(
            "{}\n<success summary=\"Custom squash summary\" />",
            edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )
        );
        let provider = MockLlmProvider::scripted([MockResponse::text(response)]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        match outcome.result {
            ModelResult::Edit {
                applied, summary, ..
            } => {
                assert!(applied);
                assert_eq!(summary, "Custom squash summary");
            }
            other => panic!("expected an applied edit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_drop_with_edits_is_applied_mid_turn() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("helper.rs", "pub const ANSWER: i32 = 42;\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let edit_then_drop = format!(
            "{}\n<context_drop path=\"helper.rs\" />",
            edit_block(
                "pub fn compute() -> i32 { 0 }",
                "pub fn compute() -> i32 { 42 }",
            )
        );
        let provider = MockLlmProvider::scripted([
            MockResponse::text(
                "Loading helper context.\n<context_request path=\"helper.rs\" access=\"read_only\" />",
            ),
            MockResponse::text(edit_then_drop),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            discovery_mode: crate::config::DiscoveryMode::Incremental,
            ..WorkflowOptions::default()
        };
        let (stream_tx, mut rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            &provider,
            &model,
            &mut context,
            &mut history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { applied: true, .. }
        ));

        let mut saw_eviction = false;
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::ContextChanged(state) = event {
                let has_target = state.items.iter().any(|item| item.path == "a.rs");
                let has_evicted = state.items.iter().any(|item| item.path == "helper.rs");
                if has_target && !has_evicted {
                    saw_eviction = true;
                }
            }
        }
        assert!(
            saw_eviction,
            "the dropped file must leave the context before the turn finalizes"
        );
    }
}
