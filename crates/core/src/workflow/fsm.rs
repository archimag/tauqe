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
    ContextAccess, DocTopic, EditOperation, EditProposal, LlmCommand, LlmOutcome, ModelRef,
    ModelResult, VerifyOnSuccess, VerifyTarget,
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
use crate::repomap::TreeSitterExtractor;
use crate::workflow::common::{compact_history_if_needed_with_budget, record_workflow_response};
use crate::workflow::discovery::reconcile_discovery_context;
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::retry::{
    build_patch_retry_prompt, format_patch_retry_reason, prepare_staged_for_retry,
    retain_retried_failed_files,
};
use crate::workflow::verification::{
    build_verification_missing_edits_prompt, build_verification_retry_prompt, extract_error_paths,
    run_workflow_verification, VerificationOutcome,
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

    /// Documentation topics requested by the model.
    fn doc_requests(&self) -> Vec<DocTopic> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::DocRequest { topic } => Some(*topic),
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

    /// Protocol syntax or structure errors encountered during decoding.
    fn protocol_errors(&self) -> Vec<String> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                LlmCommand::ProtocolError { message } => Some(message.clone()),
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

/// Per-turn memoization of deterministic proposal inputs.
///
/// The tracked-file list and the generated repo map are pure functions of the
/// repository HEAD and the active context. Consecutive rounds that leave both
/// unchanged (typical of patch retries) reuse the cached values instead of
/// re-running `git ls-files` and a full Tree-sitter pass over the repository on
/// every round. The retained extractor additionally reuses per-file symbols
/// across regenerations within the turn.
#[derive(Default)]
struct TurnInputCache {
    files: Option<TrackedFiles>,
    repo_map: Option<RepoMapCacheEntry>,
    extractor: TreeSitterExtractor,
}

/// Tracked-file list memoized at the repository HEAD it was read from.
struct TrackedFiles {
    head: String,
    files: Vec<String>,
}

/// Repo map memoized against the inputs that determine it.
///
/// The map excludes every file in context, so the cached key stores the exact
/// set of targetable paths alongside the repository HEAD and the token budget.
struct RepoMapCacheEntry {
    head: String,
    targetable_paths: Vec<String>,
    token_budget: usize,
    map: Option<String>,
}

impl TurnInputCache {
    /// Returns the repository's tracked file list, re-reading it only when the
    /// repository HEAD has moved since the previous read.
    async fn tracked_files(&mut self, repo_root: &Path, head: &str) -> Vec<String> {
        let stale = self
            .files
            .as_ref()
            .map(|cached| cached.head != head)
            .unwrap_or(true);
        if stale {
            let root = repo_root.to_path_buf();
            let files = crate::blocking::run_blocking_async(move || {
                crate::git::list_repository_files(Some(&root)).unwrap_or_default()
            })
            .await;
            self.files = Some(TrackedFiles {
                head: head.to_string(),
                files,
            });
        }
        self.files
            .as_ref()
            .map(|cached| cached.files.clone())
            .unwrap_or_default()
    }

    /// Returns the repo map for the given inputs, regenerating it only when the
    /// HEAD, the targetable path set, or the token budget has changed.
    async fn repo_map(
        &mut self,
        repo_root: &Path,
        head: &str,
        targetable_paths: &[String],
        token_budget: usize,
    ) -> Option<String> {
        let cache_hit = self
            .repo_map
            .as_ref()
            .map(|entry| {
                entry.head == head
                    && entry.token_budget == token_budget
                    && entry.targetable_paths == targetable_paths
            })
            .unwrap_or(false);
        if cache_hit {
            return self.repo_map.as_ref().and_then(|entry| entry.map.clone());
        }

        let available_files = self.tracked_files(repo_root, head).await;
        let root = repo_root.to_path_buf();
        let targetable = targetable_paths.to_vec();
        let extractor = self.extractor.clone();
        let map = crate::blocking::run_blocking_async(move || {
            crate::repomap::generate_repo_map_with_extractor(
                &root,
                &available_files,
                &targetable,
                token_budget,
                &extractor,
            )
            .ok()
            .filter(|map| !map.trim().is_empty())
        })
        .await;

        self.repo_map = Some(RepoMapCacheEntry {
            head: head.to_string(),
            targetable_paths: targetable_paths.to_vec(),
            token_budget,
            map: map.clone(),
        });
        map
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

/// Whether a prompt opens a discussion turn rather than a develop turn.
///
/// Discussion turns are assembled by the server with a typed discussion heading,
/// so prompt assembly activates discussion mode from the heading instead of
/// re-deriving the mode from free-form text. The server also consults this
/// predicate to route discussion turns through the structured discussion
/// protocol regardless of the configured base protocol.
pub fn is_discussion_prompt(prompt: &str) -> bool {
    crate::plan::prompt::is_plan_discussion_prompt(prompt)
        || crate::review::prompt::is_review_discussion_prompt(prompt)
}

async fn apply_turn_plans(
    repo_root: &Path,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
    mode: crate::plan::storage::PlanMutationMode,
) {
    if plans.is_empty() {
        return;
    }
    let root = repo_root.to_path_buf();
    let plans_vec = plans.to_vec();
    crate::blocking::run_blocking_async(move || {
        for plan_update in &plans_vec {
            let outcome = crate::plan::storage::PlanStorage::apply_discussion_plan_update_with_mode(
                &root,
                plan_update,
                mode,
            );
            for err in &outcome.errors {
                tracing::warn!("Plan update rejected: {}", err);
            }
            for plan in outcome.applied {
                let _ = crate::plan::storage::PlanStorage::save_active_id(
                    &root,
                    Some(&plan.id),
                );
            }
        }
    })
    .await;
}

/// Applies the turn's plan updates and packages the outcome.
///
/// History recording is deliberately excluded here: it runs once per bounded
/// turn in [`run_fsm_turn`], so repeated proposal rounds never duplicate
/// history entries.
///
/// Invariant: Plan mutations are applied strictly when the turn succeeds without
/// rollback or error (`applied == true` for edits, or `ModelResult::Answer`).
async fn finish_turn(
    assistant_text: String,
    result: ModelResult,
    repo_root: &Path,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
    mode: crate::plan::storage::PlanMutationMode,
) -> FsmTurnOutcome {
    let should_apply_plans = match &result {
        ModelResult::Answer { .. } => true,
        ModelResult::Edit { applied, .. } => *applied,
    };
    if should_apply_plans {
        apply_turn_plans(repo_root, plans, mode).await;
    }
    FsmTurnOutcome {
        assistant_text,
        result,
        is_cancelled: false,
        blocked: None,
    }
}

/// Rolls back an open transaction, logging a failure instead of discarding it.
///
/// A failed rollback leaves the working tree modified while the caller reports
/// that the turn was reverted, so the discrepancy must stay visible rather than
/// being silently dropped. Returns whether the working tree was actually
/// restored.
async fn rollback_or_log(policy: &mut dyn ApplyPolicy, repo_root: &Path) -> bool {
    match policy.rollback(repo_root).await {
        Ok(rolled_back) => rolled_back,
        Err(err) => {
            tracing::error!(
                "Failed to roll back the turn transaction; the working tree may still be modified: {}",
                err
            );
            false
        }
    }
}

/// RAII guard that rolls back an open transaction and cleans auto context if
/// dropped before being explicitly disarmed or finalized.
///
/// Ensures that early `?`-returns (e.g. LLM provider errors during verification
/// healing), task cancellations, or panics do not leave dirty working trees,
/// step commits, or transaction markers behind.
struct TransactionRollbackGuard<'a> {
    policy: &'a mut dyn ApplyPolicy,
    context_manager: &'a mut ContextManager,
    repo_root: &'a Path,
    armed: bool,
}

impl<'a> TransactionRollbackGuard<'a> {
    fn new(
        policy: &'a mut dyn ApplyPolicy,
        context_manager: &'a mut ContextManager,
        repo_root: &'a Path,
    ) -> Self {
        Self {
            policy,
            context_manager,
            repo_root,
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    async fn rollback(&mut self) -> bool {
        self.armed = false;
        let rolled_back = rollback_or_log(self.policy, self.repo_root).await;
        self.context_manager.clear_auto();
        self.context_manager.prune_missing_files();
        rolled_back
    }

    async fn finalize(
        &mut self,
        changed_files: &[String],
        summary: &str,
    ) -> (Option<String>, Option<String>) {
        self.armed = false;
        self.context_manager.clear_auto();
        finalize_applied(self.policy, self.repo_root, changed_files, summary).await
    }

    async fn apply_temporary(&mut self, proposal: &EditProposal) -> anyhow::Result<Vec<String>> {
        self.policy
            .apply_temporary(self.repo_root, self.context_manager, proposal)
            .await
    }
}

impl Drop for TransactionRollbackGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            tracing::warn!(
                "TransactionRollbackGuard dropped while armed; rolling back open transaction"
            );
            let _ = self.policy.rollback_sync(self.repo_root);
            self.context_manager.clear_auto();
            self.context_manager.prune_missing_files();
        }
    }
}

/// Finalizes an applied turn and surfaces a commit failure as a diagnostic.
///
/// The validated changes already sit in the working tree, so the turn stays
/// `applied`, but a Git transaction that could not be finalized must not be
/// hidden behind a silent `commit_hash: None`. The failure is logged and
/// returned so the caller can attach it to `ModelResult::Edit.error`.
async fn finalize_applied(
    policy: &mut dyn ApplyPolicy,
    repo_root: &Path,
    changed_files: &[String],
    summary: &str,
) -> (Option<String>, Option<String>) {
    match policy.apply_final(repo_root, changed_files, summary).await {
        Ok(commit_hash) => (commit_hash, None),
        Err(err) => {
            let message = format!(
                "Changes were applied but the Git transaction could not be finalized: {err}"
            );
            tracing::error!("{}", message);
            (None, Some(message))
        }
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
    repo_root: &Path,
    reason: String,
    accumulated_text: String,
    plans: &[tauqe_protocol::DiscussionPlanUpdate],
    mode: crate::plan::storage::PlanMutationMode,
) -> FsmTurnOutcome {
    rollback_or_log(policy, repo_root).await;
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
    let mut outcome = finish_turn(accumulated_text, result, repo_root, plans, mode).await;
    outcome.blocked = Some(reason);
    outcome
}

/// Bookkeeping metadata returned when beginning a session turn.
pub(super) struct SessionTurn {
    pub is_isolated_step: bool,
    pub turn_id: Option<u64>,
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
            context_manager.prune_missing_files();
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
        crate::plan::storage::PlanMutationMode::IsolatedStep
    } else if is_discussion_prompt(prompt) {
        crate::plan::storage::PlanMutationMode::Discussion
    } else {
        crate::plan::storage::PlanMutationMode::Develop
    };

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
        &mut input_cache,
        None,
        state.language.as_deref(),
        state.scratchpad.as_deref(),
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
        });
    }

    state.absorb(&step);
    let mut accumulated_text = step.text();

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

    // Discovery loop: the model may request files before proposing edits.
    let mut discovery_round = 0;
    let mut round_prompt = prompt.to_string();
    while discovery_round < options.max_discovery_rounds
        && step.edits().is_empty()
        && step.protocol_errors().is_empty()
    {
        let requests = step.context_requests();
        let doc_topics = step.doc_requests();
        if requests.is_empty() && doc_topics.is_empty() {
            break;
        }

        let available_files = {
            let root = repo_root.clone();
            crate::blocking::run_blocking_async(move || {
                crate::git::list_repository_files(Some(&root)).unwrap_or_default()
            })
            .await
        };
        let outcome = reconcile_discovery_context(
            context_manager,
            &requests,
            &available_files,
            options.files_token_budget,
        );

        if let Some(limit) = outcome.limit_exceeded {
            let warn = format!(
                "Context files token budget exceeded (files_token_budget = {}). The model requested {} tokens in new file(s) [{}], but the context already contains {} tokens. Scope the task down or decompose it into a plan.",
                limit.token_budget,
                limit.requested_tokens,
                limit.requested_files.join(", "),
                limit.current_tokens
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
                .map(|topic| crate::docs::get_documentation(*topic))
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
            "\n\n---\n**[Round {}]** {} file(s) added.\n\n",
            discovery_round + 1,
            outcome.added.len()
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
            &mut input_cache,
            None,
            state.language.as_deref(),
            state.scratchpad.as_deref(),
            is_isolated_step,
        )
        .await?;

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

        state.absorb(&step);
        accumulated_text = merge_text(accumulated_text, &banner, &step.text());

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
    }

    // Terminal error short-circuits the turn before any staging.
    if let Some(message) = extract_error_message(state.terminal_outcome.as_ref()) {
        rollback_or_log(policy, &repo_root).await;
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
        return Ok(finish_turn(accumulated_text, result, &repo_root, &state.plans, plan_mode).await);
    }

    let mut current_edits = step.edits();
    let mut current_protocol_errors = step.protocol_errors();
    let mut current_verify = state.verify_request.clone();

    if current_edits.is_empty() && current_protocol_errors.is_empty() {
        let mut answer_text = accumulated_text;
        if let Some(request) = current_verify.take() {
            let outcome =
                run_workflow_verification(&repo_root, &request, &stream_tx, true, Some(&cancel_rx))
                    .await;
            if outcome.is_cancelled {
                let _ = stream_tx.send(StreamEvent::Done).await;
                return Ok(FsmTurnOutcome {
                    assistant_text: answer_text,
                    result: ModelResult::Answer {
                        text: String::new(),
                    },
                    is_cancelled: true,
                    blocked: None,
                });
            }
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
        return Ok(finish_turn(answer_text, result, &repo_root, &state.plans, plan_mode).await);
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
        if !current_edits.is_empty() {
            let stage_res =
                policy.staging(&repo_root, context_manager, &current_edits, staged_state.as_ref()).await;
            staged_state = Some(stage_res.staged_state);
            succeeded_files.extend(stage_res.successful_paths);
            cumulative_edits.extend(stage_res.successful_edits);

            for path in &succeeded_files {
                last_failed_errors.remove(path);
            }
            for failed in stage_res.failed_files {
                last_failed_errors.insert(failed.path, failed.error);
            }
        }

        if (last_failed_errors.is_empty() && current_protocol_errors.is_empty()) || attempt >= options.max_retries {
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
            &current_protocol_errors,
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
            &mut input_cache,
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
                &repo_root,
                reason,
                accumulated_text,
                &state.plans,
                plan_mode,
            )
            .await);
        }

        current_edits = step_out.edits();
        current_protocol_errors = step_out.protocol_errors();
        current_verify = state.verify_request.clone();
        if !current_edits.is_empty() {
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
    }

    if let Some(message) = extract_error_message(state.terminal_outcome.as_ref()) {
        rollback_or_log(policy, &repo_root).await;
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
        return Ok(finish_turn(accumulated_text, result, &repo_root, &state.plans, plan_mode).await);
    }

    if !last_failed_errors.is_empty() || !current_protocol_errors.is_empty() {
        let mut err_lines = Vec::new();
        for err in &current_protocol_errors {
            err_lines.push(format!("Protocol error: {}", err));
        }
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
        return Ok(finish_turn(accumulated_text, result, &repo_root, &state.plans, plan_mode).await);
    }

    if cumulative_edits.is_empty() {
        let _ = stream_tx.send(StreamEvent::Done).await;
        let result = ModelResult::Edit {
            summary: "FSM edits failed".to_string(),
            edits: Vec::new(),
            proposal: None,
            applied: false,
            error: Some("No edits were successfully applied".to_string()),
            changed_files: Vec::new(),
            commit_hash: None,
        };
        return Ok(finish_turn(accumulated_text, result, &repo_root, &state.plans, plan_mode).await);
    }

    let mut summary = extract_success_summary(state.terminal_outcome.as_ref())
        .unwrap_or_else(|| "Apply FSM turn edits".to_string());

    let proposal = EditProposal {
        summary: summary.clone(),
        edits: cumulative_edits.clone(),
    };

    let mut guard = TransactionRollbackGuard::new(policy, context_manager, &repo_root);

    let mut total_changed_files = match guard.apply_temporary(&proposal).await {
        Ok(files) => {
            guard.arm();
            files
        }
        Err(error) => {
            guard.rollback().await;
            let ctx_state = guard.context_manager.get_state();
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
            return Ok(finish_turn(accumulated_text, result, &repo_root, &state.plans, plan_mode).await);
        }
    };

    let toolchain_config = crate::config::load_toolchain_config(&repo_root);
    if current_verify.is_none() && toolchain_config.effective_auto_verify() {
        let target = match toolchain_config.effective_target() {
            tauqe_protocol::VerifyTarget::All => EditVerifyTarget::All,
            tauqe_protocol::VerifyTarget::Check => EditVerifyTarget::Check,
            tauqe_protocol::VerifyTarget::Test => EditVerifyTarget::Test,
            tauqe_protocol::VerifyTarget::Clippy => EditVerifyTarget::Clippy,
        };
        current_verify = Some(EditVerifyRequest {
            target,
            on_success: EditVerifyOnSuccess::Silent,
        });
    }

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
            run_workflow_verification(&repo_root, &verify_request, &stream_tx, false, Some(&cancel_rx)).await;
        if verify_outcome.is_cancelled {
            if guard.rollback().await {
                let ctx_state = guard.context_manager.get_state();
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
        if !verify_outcome.success {
            auto_register_error_files(
                guard.context_manager,
                &verify_outcome.report,
                &repo_root,
                &stream_tx,
            )
            .await;
        }

        // The `[toolchain] auto_heal` flag gates the self-healing retry loop.
        // It defaults to on, preserving the standard bounded-turn behavior.
        let auto_heal = toolchain_config
            .auto_heal
            .unwrap_or(true);
        let mut healing_attempt = 0;
        let mut missing_edits = false;
        let mut healing_protocol_errors = Vec::new();
        while !verify_outcome.success && auto_heal && healing_attempt < options.max_retries {
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

            let retry_prompt = if !healing_protocol_errors.is_empty() {
                format!(
                    "The proposed verification repair edits could not be parsed due to protocol syntax errors:\n{}\n\nPlease provide corrected edit blocks to fix the verification failure:\n{}",
                    healing_protocol_errors.iter().map(|e| format!("- {}", e)).collect::<Vec<_>>().join("\n"),
                    verify_outcome.report
                )
            } else if missing_edits {
                build_verification_missing_edits_prompt(
                    &verify_outcome.report,
                    state.language.as_deref(),
                )
            } else {
                build_verification_retry_prompt(
                    &verify_outcome.report,
                    state.language.as_deref(),
                )
            };
            let step_out = run_proposal_step(
                &retry_prompt,
                provider,
                model,
                guard.context_manager,
                history_manager,
                protocol,
                workflow_name,
                &stream_tx,
                cancel_rx.clone(),
                options,
                &mut input_cache,
                None,
                state.language.as_deref(),
                state.scratchpad.as_deref(),
                is_isolated_step,
            )
            .await?;

            if step_out.is_cancelled {
                if guard.rollback().await {
                    let ctx_state = guard.context_manager.get_state();
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
                guard.rollback().await;
                let ctx_state = guard.context_manager.get_state();
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
                let mut outcome = finish_turn(accumulated_text, result, &repo_root, &state.plans, plan_mode).await;
                outcome.blocked = Some(reason);
                return Ok(outcome);
            }

            let requests = step_out.context_requests();
            if !requests.is_empty() {
                let available_files = {
                    let root = repo_root.clone();
                    crate::blocking::run_blocking_async(move || {
                        crate::git::list_repository_files(Some(&root)).unwrap_or_default()
                    })
                    .await
                };
                let outcome = reconcile_discovery_context(
                    guard.context_manager,
                    &requests,
                    &available_files,
                    options.files_token_budget,
                );
                if !outcome.added.is_empty() {
                    let ctx_state = guard.context_manager.get_state();
                    let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
                }
            }

            let retry_edits = step_out.edits();
            healing_protocol_errors = step_out.protocol_errors();
            if retry_edits.is_empty() {
                missing_edits = true;
                continue;
            }
            missing_edits = false;

            let retry_proposal = EditProposal {
                summary: summary.clone(),
                edits: retry_edits,
            };
            match guard.apply_temporary(&retry_proposal).await {
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
                        run_workflow_verification(&repo_root, &verify_request, &stream_tx, false, Some(&cancel_rx))
                            .await;
                    if verify_outcome.is_cancelled {
                        if guard.rollback().await {
                            let ctx_state = guard.context_manager.get_state();
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
                    if !verify_outcome.success {
                        auto_register_error_files(
                            guard.context_manager,
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
                        is_cancelled: false,
                        report: format!("Failed to apply repair edits: {}", error),
                    };
                }
            }
        }

        if verify_outcome.success {
            if let Some(text) = extract_success_summary(state.terminal_outcome.as_ref()) {
                summary = text;
            }
            let (commit_hash, commit_error) =
                guard.finalize(&total_changed_files, &summary).await;
            let ctx_state = guard.context_manager.get_state();
            let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
            let _ = stream_tx.send(StreamEvent::Done).await;
            let result = ModelResult::Edit {
                summary,
                edits: cumulative_edits,
                proposal: None,
                applied: true,
                error: commit_error,
                changed_files: total_changed_files,
                commit_hash,
            };
            return Ok(finish_turn(accumulated_text, result, &repo_root, &state.plans, plan_mode).await);
        }

        let changed_files = if guard.rollback().await {
            let ctx_state = guard.context_manager.get_state();
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
        return Ok(finish_turn(accumulated_text, result, &repo_root, &state.plans, plan_mode).await);
    }

    // No verification requested: finalize immediately.
    if let Some(text) = extract_success_summary(state.terminal_outcome.as_ref()) {
        summary = text;
    }
    let (commit_hash, commit_error) =
        guard.finalize(&total_changed_files, &summary).await;
    let ctx_state = guard.context_manager.get_state();
    let _ = stream_tx.send(StreamEvent::ContextChanged(ctx_state)).await;
    let _ = stream_tx.send(StreamEvent::Done).await;
    let result = ModelResult::Edit {
        summary,
        edits: cumulative_edits,
        proposal: None,
        applied: true,
        error: commit_error,
        changed_files: total_changed_files,
        commit_hash,
    };
    Ok(finish_turn(accumulated_text, result, &repo_root, &state.plans, plan_mode).await)
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
    cache: &mut TurnInputCache,
    staged_state: Option<&StagedEditsState>,
    target_language: Option<&str>,
    turn_scratchpad: Option<&str>,
    is_isolated_step: bool,
) -> anyhow::Result<ProposalStep> {
    let repo_root = context_manager.repo_root().to_path_buf();
    let repo_state = {
        let root = repo_root.clone();
        crate::blocking::run_blocking_async(move || crate::git::get_repository_state(Some(&root)))
            .await
    };
    let ctx_state = context_manager.get_state();
    let root = repo_root.clone();
    let items = ctx_state.items.clone();
    let mut context_files = crate::blocking::run_blocking_async(move || {
        let mut results = Vec::new();
        for item in items {
            let full_path = root.join(&item.path);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                results.push(ContextFileContent {
                    path: item.path,
                    access: item.access,
                    content,
                });
            }
        }
        results
    })
    .await;

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

    let repo_map = cache
        .repo_map(
            &repo_root,
            &repo_state.head,
            &targetable_paths,
            options.repomap_token_budget,
        )
        .await;

    let repomap_tokens = repo_map
        .as_deref()
        .map(crate::prompt::estimate_tokens)
        .unwrap_or(0);
    let _ = stream_tx
        .send(StreamEvent::RepoMapTokens(repomap_tokens))
        .await;

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
    assembly.is_discussion = is_discussion_prompt(prompt);
    assembly.turn_scratchpad = turn_scratchpad.map(|notes| notes.to_string());

    let messages = assembly.assemble_chat_messages(prompt, protocol);
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
        let mut stream_error = None;
        let mut completed = false;
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
                StreamEvent::Error(ref err) => {
                    stream_error = Some(err.clone());
                    let _ = forward_stream_tx.send(StreamEvent::Error(err.clone())).await;
                }
                StreamEvent::Done => {
                    completed = true;
                }
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
        (assistant_text, cancelled, stream_error, completed)
    });

    let client_result = client_call.await;
    let (assistant_text, cancelled, stream_error, completed) = forward_task.await.map_err(|join_error| {
        tracing::error!(
            "Proposal streaming task terminated unexpectedly: {}",
            join_error
        );
        anyhow::anyhow!(
            "Proposal streaming task terminated unexpectedly: {}",
            join_error
        )
    })?;
    client_result?;

    if let Some(err) = stream_error {
        anyhow::bail!("LLM stream error: {}", err);
    }

    if cancelled {
        return Ok(ProposalStep {
            commands: Vec::new(),
            is_cancelled: true,
        });
    }

    if !completed {
        anyhow::bail!("LLM stream closed unexpectedly without completion");
    }

    let commands = protocol.decode(&assistant_text);

    Ok(ProposalStep {
        commands,
        is_cancelled: false,
    })
}

/// Registers files referenced by a verification failure into editable context.
async fn auto_register_error_files(
    context_manager: &mut ContextManager,
    report: &str,
    repo_root: &Path,
    stream_tx: &mpsc::Sender<StreamEvent>,
) {
    let available_files = {
        let root = repo_root.to_path_buf();
        crate::blocking::run_blocking_async(move || {
            crate::git::list_repository_files(Some(&root)).unwrap_or_default()
        })
        .await
    };
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
        assert!(follow_up.messages[0].content.contains("<scratchpad>"));
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
            .contains("<scratchpad>"));
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
    async fn retries_malformed_xml_edit_block_and_applies() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let malformed_xml = "<tauqe_edits summary=\"Broken\"><edit path=\"a.rs\"><search>broken";
        let valid_xml = edit_block(
            "pub fn compute() -> i32 { 0 }",
            "pub fn compute() -> i32 { 42 }",
        );

        let provider = MockLlmProvider::scripted([
            MockResponse::text(malformed_xml),
            MockResponse::text(valid_xml),
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
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 42 }\n"
        );
        assert_eq!(provider.requests().len(), 2);
        assert!(provider.requests()[1]
            .messages
            .iter()
            .any(|m| m.content.contains("Protocol parsing errors")));
    }

    #[tokio::test]
    async fn malformed_xml_edit_block_fails_as_edit_when_retries_exhausted() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        let malformed_xml = "<tauqe_edits summary=\"Broken\"><edit path=\"a.rs\"><search>broken";

        let provider = MockLlmProvider::scripted([
            MockResponse::text(malformed_xml),
            MockResponse::text(malformed_xml),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            max_retries: 1,
            ..Default::default()
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

        match outcome.result {
            ModelResult::Edit { applied, error, .. } => {
                assert!(!applied);
                assert!(error.unwrap().contains("Protocol error"));
            }
            other => panic!("expected failed Edit result, got {other:?}"),
        }
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
            ModelResult::Edit { applied: true, .. }
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

    #[tokio::test]
    async fn rolled_back_turn_does_not_apply_plans() {
        let (_dir, root) = setup_repo(&[("a.rs", TARGET)]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // Model emits a bad edit that fails staging and also emits a plan tag
        let text = concat!(
            "<tauqe_edits summary=\"Bad Edit\">\n",
            "<edit path=\"a.rs\">\n",
            "<search>\nNON_EXISTENT_CONTENT\n</search>\n",
            "<replace>\nREPLACEMENT\n</replace>\n",
            "</edit>\n",
            "</tauqe_edits>\n",
            "<plan action=\"save\" id=\"rollback-plan\" title=\"Should Not Save\">\n",
            "  <concept>\n",
            "    <concept_item id=\"c-1\" title=\"Concept\" />\n",
            "  </concept>\n",
            "</plan>",
        );
        let provider = MockLlmProvider::scripted([
            MockResponse::text(text),
            MockResponse::text("Still failing"),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = DirectApplyPolicy::new();
        let options = WorkflowOptions {
            max_retries: 1,
            ..Default::default()
        };
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome = run_fsm_turn(
            "Do bad edit",
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
            ModelResult::Edit { applied: false, .. }
        ));
        let loaded = crate::plan::storage::PlanStorage::load_plan(&root, "rollback-plan").unwrap();
        assert!(loaded.is_none(), "rolled back turn must not persist plan");
    }

    #[tokio::test]
    async fn error_during_healing_triggers_automatic_rollback() {
        let (_dir, root) = setup_repo(&[
            ("a.rs", "pub fn compute() -> i32 { 0 }\n"),
            ("Cargo.toml", "[package]\nname = \"fsm_heal_test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"a.rs\"\n"),
        ]);
        let mut context = context_with_target(&root);
        let mut history = HistoryManager::new(root.clone());

        // First attempt emits an edit with a syntax error and requests verification.
        // Cargo check fails, triggering healing. During healing, the provider
        // returns an error (e.g. network drop).
        let bad_edit = concat!(
            "<tauqe_edits summary=\"Edit\">\n",
            "<edit path=\"a.rs\">\n",
            "<search>\npub fn compute() -> i32 { 0 }\n</search>\n",
            "<replace>\npub fn compute() -> i32 { SYNTAX_ERROR }\n</replace>\n",
            "</edit>\n",
            "</tauqe_edits>\n",
            "<verify target=\"check\" on_success=\"silent\" />",
        );
        let provider = MockLlmProvider::scripted([
            MockResponse::text(bad_edit),
            MockResponse::error("OpenRouter connection reset"),
        ]);
        let model = ModelRef::openrouter("mock/model");
        let protocol = XmlPromptProtocol;
        let mut policy = crate::workflow::git_policy::GitTransactionPolicy::new();
        let options = WorkflowOptions::default();
        let (stream_tx, rx) = mpsc::channel(256);
        drop(rx);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let outcome_result = run_fsm_turn(
            "Change compute",
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

        assert!(outcome_result.is_err(), "turn must propagate provider error");
        // Verify working tree has been rolled back to pristine pre-turn state.
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "pub fn compute() -> i32 { 0 }\n"
        );
    }
}
