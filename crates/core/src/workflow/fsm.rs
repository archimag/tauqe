//! Orthogonal FSM turn facade built on normalized [`LlmCommand`] values.
//!
//! This module owns the bounded-turn lifecycle: session bookkeeping, the
//! orchestration that sequences the phases, and the shared [`PhaseContext`]
//! threaded through the phase executors. Discovery and staging live in
//! [`staging`]; verification and healing live in [`healing`]. Transactional
//! persistence is delegated to an [`ApplyPolicy`], so the FSM never touches Git
//! directly and stays independent of any single wire format.

mod cache;
pub(crate) mod guard;
mod healing;
mod proposal;
mod staging;
mod tools;

use std::collections::HashMap;
use std::path::Path;

use tauqe_protocol::{EditOperation, EditOutcome, ModelRef, ModelResult, TurnIntent};
use tokio::sync::{mpsc, watch};

use crate::context::ContextManager;
use crate::edits::{EditError, StagedEditsState};
use crate::history::HistoryManager;
use crate::plan::storage::PlanMutationMode;
use crate::prompt::protocol::PromptProtocol;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::common::{compact_history_if_needed_with_budget, record_workflow_response};
use crate::workflow::discovery::journal::TurnDiscoveryJournal;
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::retry::format_patch_retry_reason;
use crate::workflow::verification::run_workflow_verification_with_config;
use crate::workflow::WorkflowOptions;

use cache::TurnInputCache;
use guard::{finalize_blocked_turn, finish_turn, rollback_or_log};
use healing::run_apply_and_verify_phase;
use proposal::{
    extract_error_message, log_fsm_phase, merge_text, outcome_carries_result, run_proposal_step,
    ProposalStep, TurnState,
};
use staging::{
    reask_after_empty_round, run_discovery_phase, run_staging_phase, DiscoveryPhase,
    EmptyStepRetry, StagingPhase, EMPTY_STEP_REMINDER, MAX_EMPTY_STEP_RETRIES,
};
use tools::{run_custom_tool_phase, CustomToolPhase};

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
    /// Mutations the plan storage engine refused during this turn, in the order
    /// it reported them.
    ///
    /// A rejected mutation never fails the turn — the code change it travelled
    /// with may be valid while the plan payload is not — but it is never silent
    /// either: the reasons are recorded in session history, so the next round's
    /// prompt carries them, and the orchestration reports them to the
    /// developer.
    pub plan_errors: Vec<String>,
    /// The model's skip report when the turn ended with a semantic skip
    /// signal.
    ///
    /// A skip is a validated outcome rather than a failed answer: the model
    /// asserts the work is already satisfied. Carrying the report lets the
    /// review supervisor mark the finding as skipped instead of treating the
    /// empty turn as an unverified reply.
    pub skip_reason: Option<String>,
    /// The model's abort report when the turn ended with a semantic abort
    /// signal.
    ///
    /// An abort is neither a blocker nor a failure: the model asserts that the
    /// requested work needs an action only the developer can perform —
    /// approving a native build, installing a package, starting a daemon.
    /// Carrying the report lets a supervisor stop its queue without architect
    /// escalation while the developer receives the exact command to run.
    pub abort: Option<AbortReport>,
}

/// Developer-facing report of a semantic `<abort>` outcome.
///
/// The report carries two human-facing payloads: `reason` names what blocked
/// the work in the host environment, and `recommendation` holds the exact
/// command or instruction the developer must run. It travels on
/// [`FsmTurnOutcome`] so a supervisor can tell an abort from a `blocked`
/// outcome, which instead starts a controlled replanning cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortReport {
    /// What blocked the work in the host environment.
    pub reason: String,
    /// The exact command or instruction the developer must run.
    pub recommendation: String,
}

impl AbortReport {
    /// Renders the report as a developer-facing message.
    ///
    /// The recommendation keeps its own paragraph, so a multi-line instruction
    /// or command stays readable and copyable instead of being folded into the
    /// reason.
    pub fn to_message(&self) -> String {
        format!(
            "Execution aborted: {}\n\nRecommended action:\n{}",
            self.reason.trim(),
            self.recommendation.trim()
        )
    }
}

impl FsmTurnOutcome {
    /// Reports a turn ended by the cancel signal before its phase completed.
    ///
    /// A phase boundary that observes the signal and a provider stream that
    /// reports it therefore surface the same outcome, so callers never
    /// distinguish where the interruption happened.
    pub fn cancelled(assistant_text: String) -> Self {
        Self {
            assistant_text,
            result: ModelResult::Answer {
                text: String::new(),
            },
            is_cancelled: true,
            blocked: None,
            abort: None,
            has_terminal_success: false,
            plan_saved: false,
            plan_errors: Vec::new(),
            skip_reason: None,
        }
    }
    /// Whether the turn produced a conversational answer without edits and
    /// without an explicit terminal success signal.
    ///
    /// Such a turn is an unverified reply rather than a validated conclusion:
    /// with coder-ladder escalation enabled the harness re-runs it on a
    /// stronger tier instead of accepting the empty answer.
    pub fn is_unverified_answer(&self) -> bool {
        matches!(self.result, ModelResult::Answer { .. })
            && !self.has_terminal_success
            && self.skip_reason.is_none()
    }

    /// Renders the plan mutations the storage engine refused, if any.
    ///
    /// The notice is compact by construction: each line is the storage engine's
    /// own reason, which already names the plan identifier and the action, so no
    /// payload is ever echoed back. A turn whose plan payload was accepted
    /// produces no notice at all.
    pub fn plan_rejection_notice(&self) -> Option<String> {
        if self.plan_errors.is_empty() {
            return None;
        }
        let header = if self.plan_errors.len() == 1 {
            "Plan mutation rejected:"
        } else {
            "Plan mutations rejected:"
        };
        let mut notice = String::from(header);
        for error in &self.plan_errors {
            notice.push_str("\n- ");
            notice.push_str(error);
        }
        Some(notice)
    }
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
    intent: TurnIntent,
    plan_mode: PlanMutationMode,
    policy: &'a mut dyn ApplyPolicy,
    context_manager: &'a mut ContextManager,
    input_cache: &'a mut TurnInputCache,
    state: &'a mut TurnState,
    /// Cumulative epistemic tail of the turn.
    ///
    /// Discovery findings are not repository files, so they never enter the
    /// context block. They accumulate here and are replayed with the staging
    /// and healing rounds, so a later phase keeps the knowledge an earlier
    /// phase already paid for.
    journal: TurnDiscoveryJournal,
    /// Investigation rounds consumed by the turn so far.
    ///
    /// Discovery and project-command execution widen what the model knows and
    /// therefore draw from one shared budget: the model may freely alternate
    /// file requests, searches, documentation, and diagnostic command runs
    /// without one kind of investigation starving the other.
    rounds_used: usize,
    accumulated_text: String,
}

impl PhaseContext<'_> {
    /// Returns the project configuration frozen at the start of the turn.
    ///
    /// The first proposal round resolves the effective configuration — reading
    /// `tauqe.toml` and, when a task runner is declared, running its dump once —
    /// and the turn cache freezes that result. Every later phase reads the same
    /// snapshot, so a patch retry, a healing round, and a project-command round
    /// never re-read the declaration or re-run the runner.
    async fn project(&mut self) -> tauqe_protocol::ProjectConfig {
        self.input_cache
            .project(self.repo_root, Some(self.cancel_rx))
            .await
    }
}

/// Whether the step's cancel signal is already raised.
///
/// Phase boundaries that are about to produce work read it, so a cancellation
/// observed inside a phase ends the step before the next phase starts instead
/// of only interrupting the provider stream.
fn cancel_requested(cancel_rx: &watch::Receiver<bool>) -> bool {
    *cancel_rx.borrow()
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
    intent: TurnIntent,
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
        intent,
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
///
/// Before the cycle starts, the governing project configuration is validated:
/// a `tauqe.toml` whose model roles or `[project]` command chain are broken
/// fails the turn up front, so no provider request is spent against an
/// environment the harness refuses to run in.
#[allow(clippy::too_many_arguments)]
pub async fn run_fsm_turn(
    prompt: &str,
    intent: TurnIntent,
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
    // The turn executes against the project configuration, so its integrity
    // is verified before any provider request is spent: a `tauqe.toml` whose
    // model roles or `[project]` command chain are broken fails the turn up
    // front with the recorded diagnostic instead of surfacing mid-flight. A
    // repository without a configuration file is governed by built-in
    // defaults and passes; the check self-offloads its blocking file reads
    // off the async executor thread. The gate sits before session bookkeeping,
    // so a refused turn records no history entry and needs no rollback.
    let repo_root = context_manager.repo_root().to_path_buf();
    if let Err(report) = crate::config::validate_project_config_on_disk(&repo_root) {
        tracing::error!(
            workflow = workflow_name,
            model = %model,
            isolated_step = intent.is_isolated_step(),
            error = %report,
            "bounded turn refused: invalid project configuration"
        );
        return Err(anyhow::anyhow!("{}", report));
    }
    let turn_id = begin_session_turn(
        prompt,
        intent,
        context_manager,
        history_manager,
        provider,
        &cancel_rx,
        options,
    )
    .await;
    let turn_res = run_fsm_turn_body(
        prompt,
        intent,
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
    )
    .await;
    let outcome = match turn_res {
        Ok(outcome) => outcome,
        Err(err) => {
            rollback_or_log(policy, context_manager.repo_root()).await;
            // The sweep only drops context paths that no longer exist on disk.
            // The auto layer is never cleared here: a failed turn must not
            // discard the files discovery already gathered.
            crate::blocking::run_blocking(|| {
                context_manager.prune_missing_files();
            });
            if let Some(id) = turn_id {
                let _ = history_manager.rollback_entry(id, format!("Turn failed: {}", err));
            }
            return Err(err);
        }
    };
    if outcome.is_cancelled {
        if let Some(id) = turn_id {
            let _ = history_manager.rollback_entry(id, "Turn cancelled by the developer");
        }
    } else {
        record_session_response(history_manager, &outcome);
    }
    Ok(outcome)
}

/// Records the bounded turn prompt in session history before compacting it.
///
/// Session bookkeeping runs once per bounded turn, independently of the number
/// of proposal attempts, so retried turns never duplicate the user prompt or
/// trigger redundant compaction passes. The prompt is recorded first and the
/// compaction pass follows, so the running turn reaches the developer's
/// chronicle the moment it starts instead of after a summarization call that
/// costs a whole provider round trip. Compaction keeps the newest turns in its
/// tail, so the fresh record survives the rewrite untouched. The turn declares
/// its mode as a typed [`TurnIntent`], so an isolated step suppresses history
/// bookkeeping entirely without being recognized from the shape of its prompt
/// text.
pub(super) async fn begin_session_turn(
    prompt: &str,
    intent: TurnIntent,
    context_manager: &ContextManager,
    history_manager: &mut HistoryManager,
    provider: &dyn LlmProvider,
    cancel_rx: &watch::Receiver<bool>,
    options: &WorkflowOptions,
) -> Option<u64> {
    if intent.is_isolated_step() {
        return None;
    }

    let context_paths: Vec<String> = context_manager
        .get_state()
        .items
        .iter()
        .map(|item| item.path.clone())
        .collect();
    let turn_id = match history_manager.record_turn(prompt, context_paths) {
        Ok(entry) => entry.id(),
        Err(err) => {
            tracing::warn!(
                "Failed to record the turn prompt in session history: {}",
                err
            );
            None
        }
    };

    let _ = compact_history_if_needed_with_budget(
        history_manager,
        provider,
        &options.history_model,
        cancel_rx.clone(),
        options.history_budget_tokens,
        options.history_tail_turns,
    )
    .await;

    turn_id
}

/// Records the completed turn outcome in session history.
///
/// Cancelled turns are skipped by the caller, so a partial turn never leaves a
/// response entry behind. Plan mutations the storage engine refused are appended
/// afterwards as one diagnostic record.
pub(super) fn record_session_response(
    history_manager: &mut HistoryManager,
    outcome: &FsmTurnOutcome,
) {
    record_workflow_response(
        history_manager,
        &outcome.result,
        &outcome.assistant_text,
        Vec::new(),
    );
    record_plan_rejections(history_manager, outcome);
}

/// Records the plan mutations the storage engine refused as one diagnostic.
///
/// A rejected mutation leaves the turn's answer intact, so without this record
/// neither the developer nor the model ever learns why the plan payload was
/// dropped. The reasons are aggregated into a single history entry — one per
/// turn, never one per rejection — so the next round's `<history>` block carries
/// the exact cause the model must repair in its next explicit plan payload.
fn record_plan_rejections(history_manager: &mut HistoryManager, outcome: &FsmTurnOutcome) {
    let Some(notice) = outcome.plan_rejection_notice() else {
        return;
    };
    if let Err(err) = history_manager.record_response(
        notice,
        None,
        Some("Plan mutation rejected".to_string()),
        Vec::new(),
    ) {
        tracing::warn!(
            "Failed to record the rejected plan mutation in session history: {}",
            err
        );
    }
}

/// Returns the capability set of an isolated step, narrowed from `protocol`.
///
/// The step profile is the single source of truth for what an isolated step may
/// do: plan authoring is excluded because the plan is immutable while the step
/// runs, and documentation requests are excluded because a step implements an
/// approved design inside the codebase instead of reasoning about the harness.
/// Intersecting the advertised surface with that profile keeps the narrowing
/// protocol-agnostic — no caller branches on a protocol name or on a concrete
/// protocol type — and a capability the protocol never advertises is simply
/// absent from the result.
fn isolated_step_capabilities(protocol: &dyn PromptProtocol) -> tauqe_protocol::CapabilitySet {
    let allowed = tauqe_protocol::CapabilitySet::isolated_step();
    let advertised = protocol.capability_set();
    let kept: Vec<tauqe_protocol::LlmCapability> = advertised
        .iter()
        .copied()
        .filter(|capability| allowed.contains(*capability))
        .collect();
    tauqe_protocol::CapabilitySet::from_slice(&kept)
}

/// Reports whether a proposal round still carries investigation commands.
///
/// Investigation commands widen what the model knows — context files,
/// repository searches, documentation, directory listings, targeted maps, and
/// project commands — and never mutate the repository. While one of them is
/// present, the turn keeps cycling instead of mistaking an intermediate step
/// for a final answer.
fn has_investigation_commands(step: &ProposalStep) -> bool {
    step.has_discovery_commands() || !step.custom_tools().is_empty()
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
    intent: TurnIntent,
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
) -> anyhow::Result<FsmTurnOutcome> {
    let repo_root = context_manager.repo_root().to_path_buf();

    let plan_mode = if intent.is_isolated_step() {
        PlanMutationMode::IsolatedStep
    } else if intent.is_plan_discussion() {
        PlanMutationMode::Discussion
    } else {
        PlanMutationMode::Develop
    };

    let isolated_protocol;
    let protocol: &dyn PromptProtocol = if intent.is_isolated_step() {
        isolated_protocol = protocol.with_capability_set(isolated_step_capabilities(protocol));
        isolated_protocol.as_ref()
    } else {
        protocol
    };

    log_fsm_phase(
        workflow_name,
        model,
        intent.is_isolated_step(),
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
        intent,
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
            abort: None,
            has_terminal_success: false,
            plan_saved: false,
            plan_errors: Vec::new(),
            skip_reason: None,
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
        intent,
        plan_mode,
        policy,
        context_manager,
        input_cache: &mut input_cache,
        state: &mut state,
        journal: TurnDiscoveryJournal::default(),
        rounds_used: 0,
        accumulated_text,
    };

    if let Some(outcome) = finish_on_abort_outcome(&mut ctx).await {
        return Ok(outcome);
    }

    let mut pending = step;
    let mut empty_retries = 0;
    // Corrective reminders accumulate across the empty rounds of the turn. The
    // corrective request is composed from the task and the turn journal every
    // time, so the findings Discovery and the project commands already produced
    // are never dropped by a re-ask.
    let mut empty_reminders = String::new();
    let step = loop {
        let step = match run_discovery_phase(&mut ctx, pending).await? {
            DiscoveryPhase::Terminate(outcome) => return Ok(*outcome),
            DiscoveryPhase::Continue(step) => *step,
        };
        if let Some(outcome) = finish_on_abort_outcome(&mut ctx).await {
            return Ok(outcome);
        }
        if let Some(outcome) = finish_on_error_outcome(&mut ctx).await {
            return Ok(outcome);
        }
        // A project command runs before anything is staged: its output is
        // answered through the turn journal, so an invocation never becomes
        // part of the change set.
        let rounds_before_tools = ctx.rounds_used;
        let step = match run_custom_tool_phase(&mut ctx, step).await? {
            CustomToolPhase::Absent(step) => *step,
            CustomToolPhase::Executed(step) => *step,
            CustomToolPhase::Terminate(outcome) => return Ok(*outcome),
        };

        // A terminal error declared by the project-command answer round ends the
        // turn at the same phase boundary every other round uses, so the
        // investigation hand-back below never spends further rounds on a turn
        // that already declared its failure.
        if let Some(outcome) = finish_on_error_outcome(&mut ctx).await {
            return Ok(outcome);
        } // A project-command round hands control back to the model. If the
          // resulting proposal still carries investigation commands, the turn
          // keeps cycling through discovery and diagnostic execution until the
          // shared round budget is spent or the model proposes code edits or a
          // terminal answer. Without this hand-back, a discovery request emitted
          // right after a command run would be mistaken for a final answer.
        let tool_executed = ctx.rounds_used > rounds_before_tools;
        if tool_executed
            && step.edits().is_empty()
            && step.protocol_errors().is_empty()
            && has_investigation_commands(&step)
        {
            pending = step;
            continue;
        }
        if !step.is_empty_step() {
            break step;
        }
        // An empty round carries no text, no edits, and no command, so it can
        // never be recorded as a validated answer. Re-ask with a corrective
        // reminder and surface a failure only once the budget is spent.
        if empty_retries >= MAX_EMPTY_STEP_RETRIES {
            return Ok(finish_empty_step(&mut ctx).await);
        }
        empty_retries += 1;
        empty_reminders.push_str(EMPTY_STEP_REMINDER);
        // The corrective round keeps the knowledge the turn already paid for:
        // the task first, the accumulated journal next, the reminder last.
        let retry_prompt = ctx
            .journal
            .compose_user_prompt(ctx.prompt, Some(&empty_reminders));
        pending = match reask_after_empty_round(&mut ctx, &retry_prompt, empty_retries).await? {
            EmptyStepRetry::Proposed(step) => *step,
            EmptyStepRetry::Terminate(outcome) => return Ok(*outcome),
        };
    };

    // A turn that closes its investigation owes a readable answer: the
    // guarantee runs once, before anything is staged or recorded, so a review
    // of an already-visible context never ends as a blank conclusion.
    let proposes_code = !step.edits().is_empty() || !step.protocol_errors().is_empty();
    let step = if proposes_code {
        step
    } else {
        match ensure_written_answer(&mut ctx, step).await? {
            WrittenAnswer::Ready(step) => *step,
            WrittenAnswer::Terminate(outcome) => return Ok(*outcome),
        }
    };

    // An abort collected by any preceding round asserts that no proposed change
    // can complete the work, so nothing is staged and nothing reaches disk.
    if let Some(outcome) = finish_on_abort_outcome(&mut ctx).await {
        return Ok(outcome);
    }
    // The corrective answer round is guarded exactly like the abort above: an
    // error it declared is terminal, so it ends the turn before anything is
    // staged or recorded instead of falling through to the answer finisher.
    if let Some(outcome) = finish_on_error_outcome(&mut ctx).await {
        return Ok(outcome);
    }
    let current_edits = step.edits();
    let current_protocol_errors = step.protocol_errors();
    if current_edits.is_empty() && current_protocol_errors.is_empty() {
        return Ok(finish_answer_without_edits(&mut ctx).await);
    }

    // Phase boundary: staging is about to validate the change set, so a cancel
    // observed during discovery ends the turn before staging produces work.
    if cancel_requested(ctx.cancel_rx) {
        let _ = ctx.stream_tx.send(StreamEvent::Done).await;
        let accumulated = std::mem::take(&mut ctx.accumulated_text);
        return Ok(FsmTurnOutcome::cancelled(accumulated));
    }

    match run_staging_phase(&mut ctx, &step).await? {
        StagingPhase::Terminate(outcome) => Ok(*outcome),
        StagingPhase::Ready(staging) => {
            if let Some(outcome) = finish_on_abort_outcome(&mut ctx).await {
                return Ok(outcome);
            }
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
            run_apply_and_verify_phase(&mut ctx, cumulative_edits, current_verify).await
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
        isolated_step = ctx.intent.is_isolated_step(),
        error = %message,
        "bounded turn terminated by an error outcome"
    );
    rollback_or_log(ctx.policy, ctx.repo_root).await;
    // The sweep only drops context paths that no longer exist on disk. The
    // accumulated auto layer survives the error outcome, so the research a
    // later turn already paid for stays available.
    crate::blocking::run_blocking(|| {
        ctx.context_manager.prune_missing_files();
    });
    let ctx_state = ctx.context_manager.get_state();
    let _ = ctx
        .stream_tx
        .send(StreamEvent::ContextChanged(ctx_state))
        .await;
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

/// Finalizes the turn when the model emitted a semantic `<abort>` signal.
///
/// An abort is not a blocker: a blocker reports a failed precondition the
/// harness can replan around, while an abort reports work the harness cannot
/// perform at all. The turn is rolled back, the developer receives the exact
/// command to run, and the report rides on the outcome so a supervisor stops
/// its queue without architect escalation. Returns `None` when no abort is
/// present so the caller can continue.
async fn finish_on_abort_outcome(ctx: &mut PhaseContext<'_>) -> Option<FsmTurnOutcome> {
    let abort = ctx.state.abort.clone()?;
    Some(finish_aborted_turn(ctx, abort).await)
}

/// Rolls the open transaction back and packages an aborted turn.
///
/// The remediation the model reported is unconditional: no code change can
/// satisfy it, so the working tree returns to its pre-turn checkpoint. The auto
/// context keeps the material the turn already gathered, because the developer
/// resumes the work after performing the requested host action.
async fn finish_aborted_turn(ctx: &mut PhaseContext<'_>, abort: AbortReport) -> FsmTurnOutcome {
    let message = abort.to_message();
    tracing::error!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.intent.is_isolated_step(),
        reason = %abort.reason,
        "bounded turn terminated by an abort outcome"
    );
    let notice = format!("\n\n---\n⛔ **[Execution Aborted]**\n\n{}\n\n", message);
    let _ = ctx.stream_tx.send(StreamEvent::TextDelta(notice)).await;
    rollback_or_log(ctx.policy, ctx.repo_root).await;
    crate::blocking::run_blocking(|| {
        ctx.context_manager.prune_missing_files();
    });
    let ctx_state = ctx.context_manager.get_state();
    let _ = ctx
        .stream_tx
        .send(StreamEvent::ContextChanged(ctx_state))
        .await;
    let _ = ctx.stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary: "Execution aborted".to_string(),
        edits: Vec::new(),
        proposal: None,
        outcome: EditOutcome {
            applied: false,
            error: Some(message),
            changed_files: Vec::new(),
            commit_hash: None,
        },
    };
    let mut outcome = finish_turn(
        std::mem::take(&mut ctx.accumulated_text),
        result,
        ctx.repo_root,
        &ctx.state.plans,
        ctx.plan_mode,
    )
    .await;
    outcome.abort = Some(abort);
    outcome
}

/// Finalizes a turn that produced conversational text without edits.
///
/// When the model also requested toolchain verification, the verification
/// report is appended to the answer text before the turn is recorded. The
/// verification round reads the project configuration the turn froze on its
/// first proposal round, so it never re-reads `tauqe.toml` or re-runs a
/// declared task runner's dump.
async fn finish_answer_without_edits(ctx: &mut PhaseContext<'_>) -> FsmTurnOutcome {
    // A terminal error that reaches the answer-only finisher keeps its declared
    // semantics instead of degrading into a plain answer. This finisher is the
    // single exit every answer-only path must pass through, so the invariant no
    // longer depends on each caller remembering the check, and only `success`
    // and `skip` outcomes reach the reading below.
    if let Some(outcome) = finish_on_error_outcome(ctx).await {
        return outcome;
    }
    // A bare answer is a validated conclusion only when the model marked it
    // with an explicit `<success>` signal; otherwise it is an unverified reply.
    let has_terminal_success = matches!(
        ctx.state.terminal_outcome,
        Some(tauqe_protocol::LlmOutcome::Success { .. })
    );
    // A semantic skip is a validated outcome, so its report rides on the turn
    // and is exempt from coder-ladder escalation.
    let skip_reason = match &ctx.state.terminal_outcome {
        Some(tauqe_protocol::LlmOutcome::Skip { reason }) => Some(reason.clone()),
        _ => None,
    };
    let mut answer_text = std::mem::take(&mut ctx.accumulated_text);
    if let Some(request) = ctx.state.verify_request.clone() {
        // An answer without edits touches no file, so the project command
        // receives no change set and runs its full suite.
        let project = ctx.project().await;
        let outcome = run_workflow_verification_with_config(
            ctx.repo_root,
            &project,
            &[],
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
                abort: None,
                has_terminal_success: false,
                plan_saved: false,
                plan_errors: Vec::new(),
                skip_reason: None,
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
        isolated_step = ctx.intent.is_isolated_step(),
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
    outcome.skip_reason = skip_reason;
    outcome
}

/// Number of corrective rounds granted to a turn that finished investigating
/// without leaving any readable text.
const MAX_WRITTEN_ANSWER_RETRIES: usize = 1;

/// Reminder appended to the request when the turn holds no readable answer.
///
/// It names plain conversational text as the expected outcome and states that
/// the material already gathered is all this turn will hold, so a model that
/// finished its investigation answers the task instead of asking for the same
/// context once more. The wording covers both reasons an investigation closes:
/// a request the context already satisfied and a discovery round limit that
/// will load nothing further.
const MISSING_ANSWER_REMINDER: &str = "\n\nYour investigation for this turn is complete: no further repository material will be loaded into the context. Answer the task now: write the analysis or the conclusion in plain conversational text, using the material you already hold. Do not request additional context, and do not emit file edits unless the code itself must change.";

/// Outcome of guaranteeing a readable answer before the turn closes.
enum WrittenAnswer {
    /// The turn carries readable text or a fresh proposal to interpret.
    Ready(Box<ProposalStep>),
    /// The turn must terminate immediately with the carried outcome.
    Terminate(Box<FsmTurnOutcome>),
}

/// Guarantees that a turn closing its investigation ends with readable text.
///
/// A round that only repeated requests the context already satisfied, or that
/// kept asking for repository material until the discovery round limit was
/// spent, leaves nothing to stage and no prose behind. Closing the turn there
/// would record a blank or truncated answer, so the model is asked once more
/// with the accumulated journal and an explicit answer directive in view.
///
/// Only prose the model itself authored closes the guarantee. The accumulated
/// turn text is deliberately not consulted: it also carries the harness banners
/// (round headers, patch-retry notices, the discovery-limit warning), so its
/// presence proves that the turn ran, never that the model concluded. A turn
/// that declared a terminal outcome carrying its result, or that answered
/// with a concrete proposal, is returned unchanged.
async fn ensure_written_answer(
    ctx: &mut PhaseContext<'_>,
    step: ProposalStep,
) -> anyhow::Result<WrittenAnswer> {
    if !step.text().trim().is_empty()
        || outcome_carries_result(ctx.state.terminal_outcome.as_ref())
        || ctx.state.abort.is_some()
    {
        return Ok(WrittenAnswer::Ready(Box::new(step)));
    }

    tracing::info!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.intent.is_isolated_step(),
        "investigation closed without a readable answer; requesting the analysis"
    );

    let banner = format!(
        "\n\n---\n**[Answer Requested 1/{}]** The investigation gathered context but produced no written conclusion; requesting the analysis...\n\n",
        MAX_WRITTEN_ANSWER_RETRIES
    );
    let _ = ctx
        .stream_tx
        .send(StreamEvent::TextDelta(banner.clone()))
        .await;
    log_fsm_phase(
        ctx.workflow_name,
        ctx.model,
        ctx.intent.is_isolated_step(),
        tauqe_protocol::TurnPhase::Proposal,
        Some(MAX_WRITTEN_ANSWER_RETRIES),
        Some(MAX_WRITTEN_ANSWER_RETRIES),
    );
    let _ = ctx
        .stream_tx
        .send(StreamEvent::TurnPhase {
            phase: tauqe_protocol::TurnPhase::Proposal,
            round: Some(MAX_WRITTEN_ANSWER_RETRIES),
            max_rounds: Some(MAX_WRITTEN_ANSWER_RETRIES),
            detail: Some("Answer requested".to_string()),
        })
        .await;

    // The answer request keeps the knowledge the turn already paid for: the
    // task first, the accumulated journal next, the answer directive last.
    let answer_prompt = ctx
        .journal
        .compose_user_prompt(ctx.prompt, Some(MISSING_ANSWER_REMINDER));
    let next = propose(
        ctx.provider,
        ctx.model,
        ctx.protocol,
        ctx.history_manager,
        &*ctx.context_manager,
        ctx.workflow_name,
        ctx.stream_tx,
        ctx.cancel_rx,
        ctx.options,
        &mut *ctx.input_cache,
        ctx.state,
        ctx.intent,
        &answer_prompt,
        None,
    )
    .await?;

    if next.is_cancelled {
        let _ = ctx.stream_tx.send(StreamEvent::Done).await;
        return Ok(WrittenAnswer::Terminate(Box::new(FsmTurnOutcome {
            assistant_text: std::mem::take(&mut ctx.accumulated_text),
            result: ModelResult::Answer {
                text: String::new(),
            },
            is_cancelled: true,
            blocked: None,
            abort: None,
            has_terminal_success: false,
            plan_saved: false,
            plan_errors: Vec::new(),
            skip_reason: None,
        })));
    }

    ctx.state.absorb(&next);
    ctx.accumulated_text = merge_text(
        std::mem::take(&mut ctx.accumulated_text),
        &banner,
        next.text(),
    );

    if let Some(reason) = ctx.state.blocked.clone() {
        let accumulated = std::mem::take(&mut ctx.accumulated_text);
        let outcome = finalize_blocked_turn(
            ctx.policy,
            ctx.context_manager,
            ctx.stream_tx,
            ctx.repo_root,
            reason,
            accumulated,
            &ctx.state.plans,
            ctx.plan_mode,
        )
        .await;
        return Ok(WrittenAnswer::Terminate(Box::new(outcome)));
    }

    // A model that stays silent after the explicit answer request closed the
    // turn with nothing at all: that is a genuine non-answer rather than a dead
    // end, so the turn fails instead of recording a blank conclusion.
    if next.is_empty_step() {
        return Ok(WrittenAnswer::Terminate(Box::new(
            finish_empty_step(ctx).await,
        )));
    }

    Ok(WrittenAnswer::Ready(Box::new(next)))
}

/// Finalizes the turn when patch application failed after every retry.
///
/// Every reason is rendered by the profile of the declared patch strategy, so a
/// rejected change set is described in the dialect the turn actually speaks: an
/// anchored project never receives a search/replace diagnosis.
async fn finish_patch_failure(
    ctx: &mut PhaseContext<'_>,
    cumulative_edits: Vec<EditOperation>,
    last_failed_errors: &HashMap<String, EditError>,
    current_protocol_errors: &[String],
) -> FsmTurnOutcome {
    tracing::warn!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.intent.is_isolated_step(),
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
        let reason = format_patch_retry_reason(error, ctx.options.patch_strategy());
        err_lines.push(format!("{}: {}", path, reason));
    }
    let error_desc = format!(
        "Failed to apply edits after {} retries:\n{}",
        ctx.options.max_retries,
        err_lines.join("\n")
    );
    crate::blocking::run_blocking(|| {
        ctx.context_manager.prune_missing_files();
    });
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
        isolated_step = ctx.intent.is_isolated_step(),
        "no edits were successfully staged"
    );
    crate::blocking::run_blocking(|| {
        ctx.context_manager.prune_missing_files();
    });
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

/// Finalizes the turn when the model kept generating an empty response.
///
/// A round without text, edits, or any protocol command is a transport-level
/// non-answer: it cannot be recorded as a validated `ModelResult::Answer`. The
/// turn surfaces it as a failed edit, which is what the coder ladder escalates
/// and what the batch supervisor reports as a failed step, instead of accepting
/// silence as a conclusion.
async fn finish_empty_step(ctx: &mut PhaseContext<'_>) -> FsmTurnOutcome {
    tracing::warn!(
        workflow = ctx.workflow_name,
        model = %ctx.model,
        isolated_step = ctx.intent.is_isolated_step(),
        retries = MAX_EMPTY_STEP_RETRIES,
        "model produced an empty response after every retry"
    );
    crate::blocking::run_blocking(|| {
        ctx.context_manager.prune_missing_files();
    });
    let _ = ctx.stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary: "Empty model response".to_string(),
        edits: Vec::new(),
        proposal: None,
        outcome: EditOutcome {
            applied: false,
            error: Some(format!(
                "The model returned an empty response after {} additional attempts: no text, no file edits, and no protocol command were produced.",
                MAX_EMPTY_STEP_RETRIES
            )),
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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        assert!(
            outcome.plan_errors.is_empty(),
            "an accepted plan mutation must not raise a diagnostic: {:?}",
            outcome.plan_errors
        );
        assert!(
            outcome.plan_rejection_notice().is_none(),
            "an accepted plan mutation must produce no notice"
        );
        let loaded = crate::plan::storage::PlanStorage::load_plan(&root, "test-fsm-plan").unwrap();
        assert!(loaded.is_some());
        let plan = loaded.unwrap();
        assert_eq!(plan.title, "FSM Plan");
        assert_eq!(plan.items.len(), 1);
        // The turn prompt and the answer are the only recorded entries: a
        // successful mutation adds no diagnostic record to the history.
        assert_eq!(
            history.get_entries().unwrap().len(),
            2,
            "an accepted plan mutation must not append a diagnostic record"
        );
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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
                outcome, summary, ..
            } => {
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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
    async fn forwards_configured_session_id_to_the_provider() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([MockResponse::text("Ready.")]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            session_id: Some("tauqe-repl-session".to_string()),
            ..WorkflowOptions::default()
        };
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let _outcome = run_fsm_turn(
            "Explain",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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

        let request = provider.last_request().expect("recorded request");
        assert_eq!(request.session_id.as_deref(), Some("tauqe-repl-session"));
    }

    #[tokio::test]
    async fn stream_error_during_proposal_aborts_and_rolls_back() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider =
            MockLlmProvider::scripted([MockResponse::error("Connection terminated prematurely")]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome_res = run_fsm_turn(
            "Change compute to 42",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
    async fn invalid_project_config_fails_the_turn_up_front() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // A configuration that parses but declares no verification command
        // is semantically invalid, so the turn must be refused before any
        // provider request is spent.
        std::fs::write(
            root.join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:a/b\"]\n",
        )
        .unwrap();

        let provider = MockLlmProvider::scripted([MockResponse::text("Ready.")]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome_res = run_fsm_turn(
            "Explain a.rs",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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

        let err = outcome_res.expect_err("an invalid configuration must fail the turn");
        assert!(err.to_string().contains("verify"), "got: {}", err);
        // The turn was refused before the proposal phase: no provider request
        // was spent and no history entry was recorded.
        assert!(provider.requests().is_empty());
        assert!(history.get_entries().unwrap().is_empty());
    }

    #[test]
    fn isolated_step_capabilities_are_protocol_agnostic() {
        // Both transports advertise one intent-oriented surface, so the
        // narrowing an isolated step applies is identical for each of them and
        // equals the intent profile.
        let xml = isolated_step_capabilities(&XmlPromptProtocol);
        assert_eq!(xml, tauqe_protocol::CapabilitySet::isolated_step());
        assert!(!xml.contains(tauqe_protocol::LlmCapability::PlanCreation));
        assert!(!xml.contains(tauqe_protocol::LlmCapability::PlanUpdate));
        assert!(xml.contains(tauqe_protocol::LlmCapability::FileEdits));

        let structured =
            isolated_step_capabilities(&crate::prompt::json::JsonPromptProtocol::edits());
        assert_eq!(structured, xml);
        assert!(structured.contains(tauqe_protocol::LlmCapability::FileEdits));
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
            tauqe_protocol::TurnIntent::IsolatedStep,
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
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.result,
            ModelResult::Edit { outcome, .. } if outcome.applied
        ));
        // The code edit was applied, but the plan must NOT have been saved
        let loaded =
            crate::plan::storage::PlanStorage::load_plan(&root, "isolated-turn-plan").unwrap();
        assert!(
            loaded.is_none(),
            "isolated step must not persist plan mutations"
        );
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
            architect: None,
            last_replanned_step: None,
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
            tauqe_protocol::TurnIntent::InteractiveRepl,
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

    /// Builds the workflow options a project declares through `[develop]
    /// patch_strategy`.
    ///
    /// The declaration is decoded exactly as the daemon decodes `tauqe.toml`, so
    /// the test observes the same single source of truth a project provides.
    fn line_anchor_options() -> WorkflowOptions {
        let config: crate::config::AppConfig =
            toml::from_str("[develop]\npatch_strategy = \"line_anchor\"\n")
                .expect("parse the patch strategy declaration");
        WorkflowOptions::from_app_config(&config)
    }

    /// A rejected change set is reported in the dialect of the declared strategy:
    /// an anchored project never receives a search/replace diagnosis, even when
    /// the rejected payload arrived in that shape.
    #[tokio::test]
    async fn patch_failure_is_reported_in_the_active_strategy_dialect() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let broken = edit_block("nonexistent text", "irrelevant");
        let provider = MockLlmProvider::scripted([
            MockResponse::text(broken.clone()),
            MockResponse::text(broken),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            max_retries: 1,
            ..line_anchor_options()
        };
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Change compute to 42",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
            ModelResult::Edit { outcome, .. } => {
                let error = outcome.error.unwrap_or_default();
                assert!(
                    error.contains("addressed no line of the file"),
                    "the rejection must speak the anchored dialect: {error}"
                );
                for foreign in ["Search block", "old_text", "new_text"] {
                    assert!(
                        !error.contains(foreign),
                        "the anchored diagnostic named {foreign}: {error}"
                    );
                }
            }
            other => panic!("expected a failed change set, got {other:?}"),
        }

        assert_eq!(
            provider.requests().len(),
            2,
            "the retry budget must be spent on the rejected change set"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            TARGET,
            "a rejected change set must never reach the working tree"
        );
    }
    /// A `success` status without a summary states an outcome but no result, so
    /// the round is empty: the corrective machine re-asks the model, and a model
    /// that keeps answering that way finishes the turn with a visible error
    /// instead of recording a silent conclusion.
    #[tokio::test]
    async fn a_payload_free_success_never_concludes_the_turn() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let provider = MockLlmProvider::scripted([
            MockResponse::text("<success />"),
            MockResponse::text("<success />"),
            MockResponse::text("<success />"),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Summarize the change",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
                assert!(
                    outcome.error.unwrap_or_default().contains("empty response"),
                    "the turn must fail with a visible error"
                );
            }
            other => panic!("a payload-free success must not conclude the turn: {other:?}"),
        }
        assert_eq!(
            provider.requests().len(),
            1 + MAX_EMPTY_STEP_RETRIES,
            "every corrective round must be spent before the turn fails"
        );
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), TARGET);
    }

    /// A `success` that states its result, and a written answer that carries the
    /// status, both remain validated conclusions of the turn.
    #[tokio::test]
    async fn a_summarized_success_concludes_the_turn() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let response = concat!(
            "Nothing to change: the helper already returns the expected value.\n",
            "<success summary=\"Confirmed the helper returns the expected value\" />",
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
            "Check the helper",
            tauqe_protocol::TurnIntent::InteractiveRepl,
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
        assert!(outcome.has_terminal_success);
        assert_eq!(provider.requests().len(), 1);
    }
}
