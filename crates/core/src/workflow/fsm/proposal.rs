//! Model proposal resolution and normalized command accumulation.
//!
//! A proposal round is one atomic model interaction: the prompt is assembled
//! from the active context, the repo map, and history, sent to the provider, and
//! decoded into a uniform [`LlmCommand`] vocabulary. This module owns the
//! per-round proposal value, the turn-wide state accumulated across rounds, and
//! the helpers that normalize commands into verification targets, terminal
//! outcomes, and conversational text.

use tokio::sync::{mpsc, watch};
use tauqe_protocol::{
    ContextAccess, DocTopic, EditOperation, LlmCommand, LlmOutcome, ModelRef, RepoSearchRequest,
    VerifyOnSuccess, VerifyTarget,
};

use crate::context::{ContextFileContent, ContextManager};
use crate::edits::protocol::{
    ContextRequest, VerifyOnSuccess as EditVerifyOnSuccess,
    VerifyRequest as EditVerifyRequest, VerifyTarget as EditVerifyTarget,
};
use crate::edits::StagedEditsState;
use crate::history::HistoryManager;
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::PromptAssembly;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::WorkflowOptions;

use super::cache::TurnInputCache;
use super::is_readonly_discussion_prompt;

/// Normalized proposal returned by a single model interaction.
///
/// Decoded commands are partitioned once at construction into typed buckets, so
/// every accessor reads pre-collected data instead of rescanning the raw
/// command list on each call. The explicit buckets also keep the proposal shape
/// self-documenting and make a new command kind impossible to miss silently.
#[derive(Default)]
pub(super) struct ProposalStep {
    text: String,
    edits: Vec<EditOperation>,
    context_requests: Vec<ContextRequest>,
    doc_requests: Vec<DocTopic>,
    repo_searches: Vec<RepoSearchRequest>,
    verify_request: Option<EditVerifyRequest>,
    terminal_outcome: Option<LlmOutcome>,
    blocked: Option<String>,
    scratchpad: Option<String>,
    language: Option<String>,
    plans: Vec<tauqe_protocol::DiscussionPlanUpdate>,
    protocol_errors: Vec<String>,
    pub(super) is_cancelled: bool,
}

impl ProposalStep {
    /// Partitions decoded commands into typed buckets in a single pass.
    ///
    /// Singleton commands (verification, terminal outcome, block, scratchpad,
    /// language) keep their first occurrence, matching the previous scan order.
    fn from_commands(commands: Vec<LlmCommand>) -> Self {
        let mut step = ProposalStep::default();
        for command in commands {
            match command {
                LlmCommand::Text { content } => {
                    if !step.text.is_empty() {
                        step.text.push('\n');
                    }
                    step.text.push_str(&content);
                }
                LlmCommand::Edit { operation } => step.edits.push(operation),
                LlmCommand::ContextRequest { path, access } => {
                    step.context_requests.push(ContextRequest { path, access });
                }
                LlmCommand::DocRequest { topic } => step.doc_requests.push(topic),
                LlmCommand::RepoSearch { path, regex } => {
                    step.repo_searches.push(RepoSearchRequest::new(path, regex));
                }
                LlmCommand::Verify { target, on_success } => {
                    if step.verify_request.is_none() {
                        step.verify_request = Some(to_edit_verify_request(target, on_success));
                    }
                }
                LlmCommand::Outcome { outcome } => {
                    if step.terminal_outcome.is_none() {
                        step.terminal_outcome = Some(outcome);
                    }
                }
                LlmCommand::Blocked { reason, details } => {
                    if step.blocked.is_none() {
                        step.blocked = Some(match details {
                            Some(details) if !details.trim().is_empty() => {
                                format!("{}\n\n{}", reason.trim(), details.trim())
                            }
                            _ => reason,
                        });
                    }
                }
                LlmCommand::Scratchpad { notes } => {
                    if step.scratchpad.is_none() {
                        step.scratchpad = Some(notes);
                    }
                }
                LlmCommand::UserLanguage { language } => {
                    if step.language.is_none() {
                        step.language = Some(language);
                    }
                }
                LlmCommand::Plan { update } => step.plans.push(update),
                LlmCommand::ProtocolError { message } => step.protocol_errors.push(message),
            }
        }
        step
    }

    /// Cleaned conversational text assembled from the decoded text commands.
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// File edit operations carried by the proposal.
    pub(super) fn edits(&self) -> &[EditOperation] {
        &self.edits
    }

    /// Context additions requested by the model.
    pub(super) fn context_requests(&self) -> &[ContextRequest] {
        &self.context_requests
    }

    /// Documentation topics requested by the model.
    pub(super) fn doc_requests(&self) -> &[DocTopic] {
        &self.doc_requests
    }

    /// Repository search requests carried by the proposal, if any.
    pub(super) fn repo_searches(&self) -> &[RepoSearchRequest] {
        &self.repo_searches
    }

    /// Toolchain verification request carried by the proposal, if any.
    pub(super) fn verify_request(&self) -> Option<&EditVerifyRequest> {
        self.verify_request.as_ref()
    }

    /// Explicit terminal outcome carried by the proposal, if any.
    fn terminal_outcome(&self) -> Option<&LlmOutcome> {
        self.terminal_outcome.as_ref()
    }

    /// Semantic block report carried by the proposal, if any.
    fn blocked_reason(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    /// Ephemeral working-memory notes carried by the proposal, if any.
    fn scratchpad(&self) -> Option<&str> {
        self.scratchpad.as_deref()
    }

    /// Detected user language carried by the proposal, if any.
    fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    /// Plan updates carried by the proposal, if any.
    fn plans(&self) -> &[tauqe_protocol::DiscussionPlanUpdate] {
        &self.plans
    }

    /// Protocol syntax or structure errors encountered during decoding.
    pub(super) fn protocol_errors(&self) -> &[String] {
        &self.protocol_errors
    }
}

/// Mutable state accumulated across the proposal rounds of a single turn.
#[derive(Default)]
pub(super) struct TurnState {
    pub(super) language: Option<String>,
    pub(super) scratchpad: Option<String>,
    pub(super) verify_request: Option<EditVerifyRequest>,
    pub(super) terminal_outcome: Option<LlmOutcome>,
    pub(super) blocked: Option<String>,
    pub(super) plans: Vec<tauqe_protocol::DiscussionPlanUpdate>,
    pub(super) repo_searches: Vec<RepoSearchRequest>,
}

impl TurnState {
    /// Folds the latest proposal round into the accumulated turn state.
    pub(super) fn absorb(&mut self, step: &ProposalStep) {
        if let Some(language) = step.language() {
            self.language = Some(language.to_string());
        }
        if let Some(scratchpad) = step.scratchpad() {
            self.scratchpad = Some(scratchpad.to_string());
        }
        if let Some(request) = step.verify_request() {
            self.verify_request = Some(request.clone());
        }
        if let Some(outcome) = step.terminal_outcome() {
            self.terminal_outcome = Some(outcome.clone());
        }
        if let Some(reason) = step.blocked_reason() {
            self.blocked = Some(reason.to_string());
        }
        self.plans.extend(step.plans().iter().cloned());
        self.repo_searches
            .extend(step.repo_searches().iter().cloned());
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
pub(super) fn extract_success_summary(outcome: Option<&LlmOutcome>) -> Option<String> {
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
pub(super) fn extract_error_message(outcome: Option<&LlmOutcome>) -> Option<String> {
    match outcome {
        Some(LlmOutcome::Error { message }) => Some(message.clone()),
        _ => None,
    }
}

/// Appends a round banner and the new conversational text to the accumulator.
pub(super) fn merge_text(accumulated: String, banner: &str, new_text: &str) -> String {
    format!(
        "{}{}{}",
        accumulated.trim_end(),
        banner,
        new_text.trim_start()
    )
}

/// Logs an FSM phase transition with its bounded-turn metadata.
///
/// Only metadata is recorded: workflow name, model, phase label, round
/// counters, and the isolation flag. Prompt text, file contents, diffs, and
/// repo maps are never written, honoring the anti-bloat logging invariant.
pub(super) fn log_fsm_phase(
    workflow: &str,
    model: &ModelRef,
    is_isolated_step: bool,
    phase: tauqe_protocol::TurnPhase,
    round: Option<usize>,
    max_rounds: Option<usize>,
) {
    tracing::info!(
        workflow = workflow,
        model = %model,
        isolated_step = is_isolated_step,
        phase = %phase,
        round = ?round,
        max_rounds = ?max_rounds,
        "turn phase"
    );
}

/// Runs a single atomic model interaction and decodes it into commands.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_proposal_step(
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
            .await?
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
    .await?;

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
    tracing::info!(
        workflow = workflow_name,
        model = %model,
        isolated_step = is_isolated_step,
        repo_map_tokens = repomap_tokens,
        "proposal round prepared"
    );
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
    assembly.is_discussion = is_readonly_discussion_prompt(prompt);
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
            is_cancelled: true,
            ..Default::default()
        });
    }

    if !completed {
        anyhow::bail!("LLM stream closed unexpectedly without completion");
    }

    let commands = protocol.decode(&assistant_text);

    Ok(ProposalStep::from_commands(commands))
}
