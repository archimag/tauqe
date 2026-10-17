//! Orthogonal FSM turn facade built on normalized [`LlmCommand`] values.
//!
//! This module owns the bounded-turn lifecycle: session bookkeeping, the
//! orchestration that sequences the phases, and the shared [`PhaseContext`]
//! threaded through the phase executors. Discovery and staging live in
//! [`staging`]; verification and healing live in [`healing`]. Transactional
//! persistence is delegated to an [`ApplyPolicy`], so the FSM never touches Git
//! directly and stays independent of any single wire format.

mod cache;
mod guard;
mod healing;
mod proposal;
mod staging;

use std::collections::HashMap;
use std::path::Path;

use tokio::sync::{mpsc, watch};
use tauqe_protocol::{EditOperation, EditOutcome, ModelRef, ModelResult};

use crate::context::ContextManager;
use crate::edits::{EditError, StagedEditsState};
use crate::history::HistoryManager;
use crate::plan::storage::PlanMutationMode;
use crate::prompt::protocol::PromptProtocol;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::common::{compact_history_if_needed_with_budget, record_workflow_response};
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::verification::run_workflow_verification;
use crate::workflow::WorkflowOptions;

use cache::TurnInputCache;
use guard::{finalize_blocked_turn, finish_turn, rollback_or_log};
use healing::run_apply_and_verify_phase;
use proposal::{
    extract_error_message, log_fsm_phase, run_proposal_step, ProposalStep, TurnState,
};
use staging::{run_discovery_phase, run_staging_phase, DiscoveryPhase, StagingPhase};

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
    /// Whether the model emitted an explicit terminal success signal
    /// (`<success>`).
    ///
    /// A conversational answer without edits is a validated conclusion only
    /// when the model marked it as such; otherwise the turn is an unverified
    /// reply that the coder ladder may escalate.
    pub has_terminal_success: bool,
    /// Whether the turn applied a plan `save` action that created or fully
    /// replaced a plan. Propagated to the workflow result so the server can
    /// mark the emitted `plan/updated` event as a creation.
    pub plan_saved: bool,
}

impl FsmTurnOutcome {
    /// Whether the turn produced a conversational answer without edits and
    /// without an explicit terminal success signal.
    ///
    /// Such a turn is an unverified reply rather than a validated conclusion:
    /// with coder-ladder escalation enabled the harness re-runs it on a
    /// stronger tier instead of accepting the empty answer.
    pub fn is_unverified_answer(&self) -> bool {
        matches!(self.result, ModelResult::Answer { .. }) && !self.has_terminal_success
    }
}

/// Whether a prompt opens a typed focused dialogue turn rather than a plain
/// develop turn.
///
/// Focused dialogue covers both plan discussions and review focus dialogues, so
/// the REPL recognises a focused turn from its typed heading instead of
/// re-deriving the mode from free-form text. Read-only routing is narrower and
/// lives in [`is_readonly_discussion_prompt`]; the two are deliberately distinct
/// because a review focus keeps code-editing capability.
pub fn is_discussion_prompt(prompt: &str) -> bool {
    crate::plan::prompt::is_plan_discussion_prompt(prompt)
        || crate::review::prompt::is_review_discussion_prompt(prompt)
}

/// Whether a prompt opens a strictly read-only architectural discussion turn.
///
/// Only a plan discussion is read-only: the model reasons about and proposes
/// plan structure but is barred from editing code, so the turn is routed through
/// the text-only structured discussion protocol.
///
/// A review focus dialogue is deliberately excluded. It keeps dual capability
/// (addressed concept c-3): the developer may reason about a finding and directly
/// request a code fix, so the turn is routed through the configured edit protocol
/// — carrying file edits and toolchain verification — and prompt assembly stays
/// in develop mode. This predicate is the single source of truth consulted by
/// prompt assembly, the plan-mutation mode, and the server-side protocol
/// resolver.
pub fn is_readonly_discussion_prompt(prompt: &str) -> bool {
    crate::plan::prompt::is_plan_discussion_prompt(prompt)
}

/// Bookkeeping metadata returned when beginning a session turn.
pub(super) struct SessionTurn {
    pub is_isolated_step: bool,
    pub turn_id: Option<u64>,
}

/// Shared, mutable state threaded through the FSM phase executors.
///
/// The phase executors in [`staging`] and [`healing`] operate purely on this
/// context so they can drive discovery, staging, verification, and healing
/// without re-deriving collaborators or threading a long positional argument
/// list. The owned [`PhaseContext::accumulated_text`] accumulates the streamed
/// conversational text across every proposal round of the turn.
struct PhaseContext<'a> {
    prompt: &'a str,
    provider: &'a dyn LlmProvider,
    model: &'a ModelRef,
    protocol: &'a dyn PromptProtocol,
    history_manager: &'a HistoryManager,
    workflow_name: &'a str,
    stream_tx: &'a mpsc::Sender<StreamEvent>,
    cancel_rx: &'a watch::Receiver<bool>,
    options: &'a WorkflowOptions,
    repo_root: &'a Path,
    is_isolated_step: bool,
    plan_mode: PlanMutationMode,
    policy: &'a mut dyn ApplyPolicy,
    context_manager: &'a mut ContextManager,
    input_cache: &'a mut TurnInputCache,
    state: &'a mut TurnState,
    accumulated_text: String,
}

/// Runs a single proposal round with the turn's accumulated language and
/// scratchpad context.
///
/// The phase executors call this helper instead of [`run_proposal_step`]
/// directly so the turn-scoped language and scratchpad are always injected
/// without repeating the extraction at every call site.
#[allow(clippy::too_many_arguments)]
async fn propose(
    provider: &dyn LlmProvider,
    model: &ModelRef,
    protocol: &dyn PromptProtocol,
    history_manager: &HistoryManager,
    context_manager: &ContextManager,
    workflow_name: &str,
    stream_tx: &mpsc::Sender<StreamEvent>,
    cancel_rx: &watch::Receiver<bool>,
    options: &WorkflowOptions,
    input_cache: &mut TurnInputCache,
    state: &TurnState,
    is_isolated_step: bool,
    prompt: &str,
    staged: Option<&StagedEditsState>,
) -> anyhow::Result<ProposalStep> {
    let language = state.language.clone();
    let scratchpad = state.scratchpad.clone();
    run_proposal_step(
        prompt,
        provider,
        model,
        context_manager,
        history_manager,
        protocol,
        workflow_name,
        stream_tx,
        cancel_rx.clone(),
        options,
        input_cache,
        staged,
        language.as_deref(),
        scratchpad.as_deref(),
        is_isolated_step,
    )
    .await
}

/// Runs the orthogonal FSM turn: discovery, staging, verification, and healing.
///
/// Composes the session-bookkeeping preamble, the FSM cycle, and the response
/// recording so a single bounded turn records exactly one prompt and one
/// response, regardless of how many proposal rounds the cycle performs. The
/// engine consumes normalized [`LlmCommand`] values produced by
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
    let session_turn =
        begin_session_turn(prompt, context_manager, history_manager, provider, &cancel_rx, options)
            .await;
    let turn_res = run_fsm_turn_body(
        prompt,
        provider,
        model,
        context_manager,
        history_manager,
        protocol,
        policy,
        workflow_name,
        stream_tx,
        cancel_rx,
        options,
        session_turn.is_isolated_step,
    )
    .await;
    let outcome = match turn_res {
        Ok(outcome) => outcome,
        Err(err) => {
            rollback_or_log(policy, context_manager.repo_root()).await;
            context_manager.clear_auto();
            // Metadata pruning walks every context path, so it runs off the
            // async executor thread.
            crate::blocking::run_blocking(|| context_manager.prune_missing_files());
            if let Some(id) = session_turn.turn_id {
                let _ = history_manager.rollback_entry(id);
            }
            return Err(err);
        }
    };
    if outcome.is_cancelled {
        if let Some(id) = session_turn.turn_id {
            let _ = history_manager.rollback_entry(id);
        }
    } else {
        record_session_response(history_manager, &outcome);
    }
    Ok(outcome)
}

/// Records the bounded turn prompt in session history after compacting it.
///
/// Session bookkeeping runs once per bounded turn, independently of the number
/// of proposal attempts, so retried turns never duplicate the user prompt or
/// trigger redundant compaction passes. Returns whether the prompt is an
/// isolated step execution, which suppresses history bookkeeping entirely.
pub(super) async fn begin_session_turn(
    prompt: &str,
    context_manager: &ContextManager,
    history_manager: &mut HistoryManager,
    provider: &dyn LlmProvider,
    cancel_rx: &watch::Receiver<bool>,
    options: &WorkflowOptions,
) -> SessionTurn {
    let is_isolated_step = crate::plan::prompt::is_plan_step_execution_prompt(prompt)
        || crate::review::is_review_step_execution_prompt(prompt);

    let mut turn_id = None;
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
        match history_manager.record_turn(prompt, context_paths) {
            Ok(entry) => turn_id = entry.id(),
            Err(err) => {
                tracing::warn!(
                    "Failed to record the turn prompt in session history: {}",
                    err
                );
            }
        }
    }

    SessionTurn {
        is_isolated_step,
        turn_id,
    }
}

/// Records the completed turn outcome in session history.
///
/// Cancelled turns are skipped by the caller, so a partial turn never leaves a
/// response entry behind.
pub(super) fn record_session_response(
    history_manager: &mut HistoryManager,
    outcome: &FsmTurnOutcome,
) {
    record_workflow_response(history_manager, &outcome.result, &outcome.assistant_text, Vec::new());
}

/// Runs the FSM turn cycle without session bookkeeping.
///
/// Performs discovery, staging, verification, and healing against the shared
/// collaborators and returns the turn outcome. It neither compacts session
/// history nor records the turn prompt or response, so callers that run several
/// attempts within one bounded turn reuse it without polluting the session
/// history or re-running compaction.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_fsm_turn_body(
    prompt: &str,
    provider: &dyn LlmProvider,
    model: &ModelRef,
    context_manager: &mut ContextManager,
    history_manager: &HistoryManager,
    protocol: &dyn PromptProtocol,
    policy: &mut dyn ApplyPolicy,
    workflow_name: &str,
    stream_tx: mpsc::Sender<StreamEvent>,
    cancel_rx: watch::Receiver<bool>,
    options: &WorkflowOptions,
    is_isolated_step: bool,
) -> anyhow::Result<FsmTurnOutcome> {
    let repo_root = context_manager.repo_root().to_path_buf();

    let plan_mode = if is_isolated_step {
        PlanMutationMode::IsolatedStep
    } else if is_readonly_discussion_prompt(prompt) {
        PlanMutationMode::Discussion
    } else {
        PlanMutationMode::Develop
    };

    log_fsm_phase(
        workflow_name,
        model,
        is_isolated_step,
        tauqe_protocol::TurnPhase::Proposal,
        Some(1),
        None,
    );
    let _ = stream_tx
        .send(StreamEvent::TurnPhase {
            phase: tauqe_protocol::TurnPhase::Proposal,
            round: Some(1),
            max_rounds: None,
            detail: None,
        })
        .await;

    let mut state = TurnState::default();
    let mut input_cache = TurnInputCache::default();
    let step = run_proposal_step(
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
        &mut input_cache,
        None,
        None,
        None,
        is_isolated_step,
    )
    .await?;

    if step.is_cancelled {
        let _ = stream_tx.send(StreamEvent::Done).await;
        return Ok(FsmTurnOutcome {
            assistant_text: String::new(),
            result: ModelResult::Answer {
                text: String::new(),
            },
            is_cancelled: true,
            blocked: None,
            has_terminal_success: false,
            plan_saved: false,
        });
    }

    state.absorb(&step);
    let accumulated_text = step.text().to_string();

    if let Some(reason) = state.blocked.clone() {
        return Ok(finalize_blocked_turn(
            policy,
            context_manager,
            &stream_tx,
            &repo_root,
            reason,
            accumulated_text,
            &state.plans,
            plan_mode,
        )
        .await);
    }

    let mut ctx = PhaseContext {
        prompt,
        provider,
        model,
        protocol,
        history_manager,
        workflow_name,
        stream_tx: &stream_tx,
        cancel_rx: &cancel_rx,
        options,
        repo_root: &repo_root,
        is_isolated_step,
        plan_mode,
        policy,
        context_manager,
        input_cache: &mut input_cache,
        state: &mut state,
        accumulated_text,
    };

    match run_discovery_phase(&mut ctx, step).await? {
        DiscoveryPhase::Terminate(outcome) => Ok(*outcome),
        DiscoveryPhase::Continue(step) => {
            if let Some(outcome) = finish_on_error_outcome(&mut ctx).await {
                return Ok(outcome);
            }

            let current_edits = step.edits();
            let current_protocol_errors = step.protocol_errors();
            if current_edits.is_empty() && current_protocol_errors.is_empty() {
                return Ok(finish_answer_without_edits(&mut ctx).await);
            }

            match run_staging_phase(&mut ctx, &step).await? {
                StagingPhase::Terminate(outcome) => Ok(*outcome),
                StagingPhase::Ready(staging) => {
                    if let Some(outcome) = finish_on_error_outcome(&mut ctx).await {
                        return Ok(outcome);
                    }
                    let cumulative_edits = staging.cumulative_edits;
                    let last_failed_errors = staging.last_failed_errors;
                    let current_protocol_errors = staging.current_protocol_errors;
                    if !last_failed_errors.is_empty() || !current_protocol_errors.is_empty() {
                        return Ok(finish_patch_failure(
                            &mut ctx,
                            cumulative_edits,
                            &last_failed_errors,
                            &current_protocol_errors,
                        )
                        .await);
                    }
                    if cumulative_edits.is_empty() {
                        return Ok(finish_no_edits(&mut ctx).await);
                    }
                    let current_verify = ctx.state.verify_request.clone();
                    return run_apply_and_verify_phase(&mut ctx, cumulative_edits, current_verify)
                        .await;
                }
            }
        }
    }
}

/// Finalizes the turn when the model emitted a terminal `<error>` outcome.
///
/// Rolled back and recorded identically to a semantic block: the working tree
/// returns to its pre-turn checkpoint, the auto context is cleared, and the
/// turn surfaces an unapplied edit carrying the error message. Returns `None`
/// when no terminal error outcome is present so the caller can continue.
async fn finish_on_error_outcome(ctx: &mut PhaseContext<'_>) -> Option<FsmTurnOutcome> {
    let message = extract_error_message(ctx.state.terminal_outcome.as_ref())?;
    tracing::error!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.is_isolated_step,
        error = %message,
        "bounded turn terminated by an error outcome"
    );
    rollback_or_log(ctx.policy, ctx.repo_root).await;
    ctx.context_manager.clear_auto();
    crate::blocking::run_blocking(|| ctx.context_manager.prune_missing_files());
    let ctx_state = ctx.context_manager.get_state();
    let _ = ctx.stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
    let _ = ctx.stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary: "Execution blocked".to_string(),
        edits: Vec::new(),
        proposal: None,
        outcome: EditOutcome {
            applied: false,
            error: Some(message),
            changed_files: Vec::new(),
            commit_hash: None,
        },
    };
    Some(
        finish_turn(
            std::mem::take(&mut ctx.accumulated_text),
            result,
            ctx.repo_root,
            &ctx.state.plans,
            ctx.plan_mode,
        )
        .await,
    )
}

/// Finalizes a turn that produced conversational text without edits.
///
/// When the model also requested toolchain verification, the verification
/// report is appended to the answer text before the turn is recorded.
async fn finish_answer_without_edits(ctx: &mut PhaseContext<'_>) -> FsmTurnOutcome {
    // A bare answer is a validated conclusion only when the model marked it
    // with an explicit `<success>` signal; otherwise it is an unverified reply.
    let has_terminal_success = matches!(
        ctx.state.terminal_outcome,
        Some(tauqe_protocol::LlmOutcome::Success { .. })
    );
    let mut answer_text = std::mem::take(&mut ctx.accumulated_text);
    if let Some(request) = ctx.state.verify_request.clone() {
        let outcome = run_workflow_verification(
            ctx.repo_root,
            &request,
            ctx.stream_tx,
            true,
            Some(ctx.cancel_rx),
        )
        .await;
        if outcome.is_cancelled {
            let _ = ctx.stream_tx.send(StreamEvent::Done).await;
            return FsmTurnOutcome {
                assistant_text: answer_text,
                result: ModelResult::Answer {
                    text: String::new(),
                },
                is_cancelled: true,
                blocked: None,
                has_terminal_success: false,
                plan_saved: false,
            };
        }
        if !outcome.report.trim().is_empty() {
            if answer_text.trim().is_empty() {
                answer_text = outcome.report;
            } else {
                answer_text = format!("{}\n\n{}", answer_text.trim(), outcome.report.trim());
            }
        }
    }
    tracing::info!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.is_isolated_step,
        "bounded turn produced an answer without edits"
    );
    let _ = ctx.stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Answer {
        text: answer_text.clone(),
    };
    let mut outcome = finish_turn(
        answer_text,
        result,
        ctx.repo_root,
        &ctx.state.plans,
        ctx.plan_mode,
    )
    .await;
    outcome.has_terminal_success = has_terminal_success;
    outcome
}

/// Finalizes the turn when patch application failed after every retry.
async fn finish_patch_failure(
    ctx: &mut PhaseContext<'_>,
    cumulative_edits: Vec<EditOperation>,
    last_failed_errors: &HashMap<String, EditError>,
    current_protocol_errors: &[String],
) -> FsmTurnOutcome {
    tracing::warn!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.is_isolated_step,
        max_retries = ctx.options.max_retries,
        failed_files = last_failed_errors.len(),
        protocol_errors = current_protocol_errors.len(),
        "patch application attempts exhausted"
    );
    let mut err_lines = Vec::new();
    for err in current_protocol_errors {
        err_lines.push(format!("Protocol error: {}", err));
    }
    let mut failed_sorted: Vec<(&String, &EditError)> = last_failed_errors.iter().collect();
    failed_sorted.sort_by_key(|(path, _)| *path);
    for (path, error) in failed_sorted {
        err_lines.push(format!("{}: {}", path, error));
    }
    let error_desc = format!(
        "Failed to apply edits after {} retries:\n{}",
        ctx.options.max_retries,
        err_lines.join("\n")
    );
    ctx.context_manager.clear_auto();
    crate::blocking::run_blocking(|| ctx.context_manager.prune_missing_files());
    let _ = ctx.stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary: "FSM edits failed".to_string(),
        edits: cumulative_edits,
        proposal: None,
        outcome: EditOutcome {
            applied: false,
            error: Some(error_desc),
            changed_files: Vec::new(),
            commit_hash: None,
        },
    };
    finish_turn(
        std::mem::take(&mut ctx.accumulated_text),
        result,
        ctx.repo_root,
        &ctx.state.plans,
        ctx.plan_mode,
    )
    .await
}

/// Finalizes the turn when no edits were successfully staged.
async fn finish_no_edits(ctx: &mut PhaseContext<'_>) -> FsmTurnOutcome {
    tracing::warn!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.is_isolated_step,
        "no edits were successfully staged"
    );
    ctx.context_manager.clear_auto();
    crate::blocking::run_blocking(|| ctx.context_manager.prune_missing_files());
    let _ = ctx.stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary: "FSM edits failed".to_string(),
        edits: Vec::new(),
        proposal: None,
        outcome: EditOutcome {
            applied: false,
            error: Some("No edits were successfully applied".to_string()),
            changed_files: Vec::new(),
            commit_hash: None,
        },
    };
    finish_turn(
        std::mem::take(&mut ctx.accumulated_text),
        result,
        ctx.repo_root,
        &ctx.state.plans,
        ctx.plan_mode,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::xml::XmlPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::policy::DirectApplyPolicy;
    use tauqe_protocol::ContextAccess;

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
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
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
            ModelResult::Edit { outcome, edits, .. } => {
                assert!(!outcome.applied);
                assert!(edits.is_empty());
                assert_eq!(outcome.error.as_deref(), Some("Missing dependency"));
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
            ModelResult::Edit { outcome, summary, .. } => {
                assert!(outcome.applied);
                assert_eq!(summary, "Custom squash summary");
            }
            other => panic!("expected an applied edit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emits_repomap_tokens_event() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", TARGET),
            ("other.rs", "pub fn other() -> i32 { 10 }\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text("Ready.")]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, mut rx) = mpsc::channel(256);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let _outcome = run_fsm_turn(
            "Explain",
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

        let mut saw_repomap_tokens = false;
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::RepoMapTokens(tokens) = event {
                if tokens > 0 {
                    saw_repomap_tokens = true;
                }
            }
        }
        assert!(
            saw_repomap_tokens,
            "turn must emit StreamEvent::RepoMapTokens with estimated tokens"
        );
    }

    #[tokio::test]
    async fn stream_error_during_proposal_aborts_and_rolls_back() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::error("Connection terminated prematurely")]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome_res = run_fsm_turn(
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
        .await;

        assert!(outcome_res.is_err(), "turn must fail fast on stream error");
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), TARGET);
        assert!(history.get_entries().unwrap().is_empty());
    }

    #[tokio::test]
    async fn isolated_step_rejects_plan_mutations() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let history = HistoryManager::new(root.clone());

        let plan_xml = concat!(
            "<tauqe_edits summary=\"Edit\">\n",
            "<edit path=\"a.rs\">\n",
            "<search>\npub fn compute() -> i32 { 0 }\n</search>\n",
            "<replace>\npub fn compute() -> i32 { 42 }\n</replace>\n",
            "</edit>\n",
            "</tauqe_edits>\n",
            "<plan action=\"save\" id=\"isolated-turn-plan\" title=\"Isolated\">\n",
            "  <concept>\n",
            "    <concept_item id=\"c-1\" title=\"Forbidden\" />\n",
            "  </concept>\n",
            "</plan>",
        );
        let provider = MockLlmProvider::scripted([MockResponse::text(plan_xml)]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        // Run isolated step execution
        let outcome = run_fsm_turn_body(
            "Execute step: isolated",
            &provider,
            &model,
            &mut context,
            &history,
            &protocol,
            &mut policy,
            "git",
            stream_tx,
            cancel_rx,
            &options,
            true, // is_isolated_step
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        // The code edit was applied, but the plan must NOT have been saved
        let loaded = crate::plan::storage::PlanStorage::load_plan(&root, "isolated-turn-plan").unwrap();
        assert!(loaded.is_none(), "isolated step must not persist plan mutations");
    }

    #[tokio::test]
    async fn develop_mode_rejects_plan_update() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // First save a plan
        let existing = tauqe_protocol::Plan {
            id: "develop-plan".to_string(),
            title: "Develop Plan".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            concept: vec![],
            items: vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step 1".to_string(),
                status: tauqe_protocol::PlanItemStatus::Todo,
                ..Default::default()
            }],
        };
        crate::plan::storage::PlanStorage::save_plan(&root, &existing).unwrap();

        // Model in develop mode attempts to update the plan
        let update_xml = concat!(
            "Updating plan from develop:\n",
            "<plan action=\"update\" id=\"develop-plan\">\n",
            "  <item id=\"1\" status=\"done\" />\n",
            "</plan>\n",
        );
        let provider = MockLlmProvider::scripted([MockResponse::text(update_xml)]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Update plan",
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
        let loaded = crate::plan::storage::PlanStorage::load_plan(&root, "develop-plan")
            .unwrap()
            .unwrap();
        // The item must remain Todo because update is rejected in Develop mode
        assert_eq!(loaded.items[0].status, tauqe_protocol::PlanItemStatus::Todo);
    }
}
