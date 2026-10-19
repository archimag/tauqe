//! Model proposal resolution and normalized command accumulation.
//!
//! A proposal round is one atomic model interaction: the prompt is assembled
//! from the active context, the repo map, and history, sent to the provider, and
//! decoded into a uniform [`LlmCommand`] vocabulary. This module owns the
//! per-round proposal value, the turn-wide state accumulated across rounds, and
//! the helpers that normalize commands into verification targets, terminal
//! outcomes, and conversational text.

use tauqe_protocol::{
    ContextAccess, DirListRequest, DocTopic, EditOperation, LlmCommand, LlmOutcome, ModelRef,
    RepoSearchRequest, TurnIntent, VerifyOnSuccess, VerifyTarget,
};
use tokio::sync::{mpsc, watch};

use crate::context::{ContextFileContent, ContextManager};
use crate::edits::protocol::{
    ContextRequest, VerifyOnSuccess as EditVerifyOnSuccess, VerifyRequest as EditVerifyRequest,
    VerifyTarget as EditVerifyTarget,
};
use crate::edits::StagedEditsState;
use crate::history::HistoryManager;
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::PromptAssembly;
use crate::providers::{LlmProvider, StreamEvent};
use crate::workflow::WorkflowOptions;

use super::cache::TurnInputCache;

/// Normalized proposal returned by a single model interaction.
///
/// Decoded commands are partitioned once at construction into typed buckets, so
/// every accessor reads pre-collected data instead of rescanning the raw
/// command list on each call. The explicit buckets also keep the proposal shape
/// self-documenting and make a new command kind impossible to miss silently.
#[derive(Default)]
pub(super) struct ProposalStep {
    commands: Vec<LlmCommand>,
    text: String,
    edits: Vec<EditOperation>,
    context_requests: Vec<ContextRequest>,
    doc_requests: Vec<DocTopic>,
    repo_searches: Vec<RepoSearchRequest>,
    repo_dirs: bool,
    repo_map_requests: Vec<String>,
    dir_list_requests: Vec<DirListRequest>,
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
        for command in &commands {
            match command {
                LlmCommand::Text { content } => {
                    if !step.text.is_empty() {
                        step.text.push('\n');
                    }
                    step.text.push_str(content);
                }
                LlmCommand::Edit { operation } => step.edits.push(operation.clone()),
                LlmCommand::ContextRequest { path, access } => {
                    step.context_requests.push(ContextRequest {
                        path: path.clone(),
                        access: *access,
                    });
                }
                LlmCommand::DocRequest { topic } => step.doc_requests.push(*topic),
                LlmCommand::RepoSearch { path, regex } => {
                    step.repo_searches
                        .push(RepoSearchRequest::new(path.clone(), regex.clone()));
                }
                LlmCommand::RepoDirs => step.repo_dirs = true,
                LlmCommand::RepoMap { path } => step.repo_map_requests.push(path.clone()),
                // The request is normalized during decoding, so a listing that
                // names a location outside the repository is dropped here
                // instead of reaching the discovery dispatcher.
                LlmCommand::DirList { path, recursive } => {
                    if let Some(request) =
                        DirListRequest::new(path.clone(), *recursive).normalized()
                    {
                        step.dir_list_requests.push(request);
                    }
                }
                LlmCommand::Verify { target, on_success } => {
                    if step.verify_request.is_none() {
                        step.verify_request = Some(to_edit_verify_request(*target, *on_success));
                    }
                }
                LlmCommand::Outcome { outcome } => {
                    if step.terminal_outcome.is_none() {
                        step.terminal_outcome = Some(outcome.clone());
                    }
                }
                LlmCommand::Blocked { reason, details } => {
                    if step.blocked.is_none() {
                        step.blocked = Some(match details {
                            Some(details) if !details.trim().is_empty() => {
                                format!("{}\n\n{}", reason.trim(), details.trim())
                            }
                            _ => reason.clone(),
                        });
                    }
                }
                LlmCommand::Scratchpad { notes } => {
                    if step.scratchpad.is_none() {
                        step.scratchpad = Some(notes.clone());
                    }
                }
                LlmCommand::UserLanguage { language } => {
                    if step.language.is_none() {
                        step.language = Some(language.clone());
                    }
                }
                LlmCommand::Plan { update } => step.plans.push(update.clone()),
                LlmCommand::ProtocolError { message } => step.protocol_errors.push(message.clone()),
            }
        }
        step.commands = commands;
        step
    }

    /// Cleaned conversational text assembled from the decoded text commands.
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// Raw decoded command vector preserved from the proposal round.
    pub(super) fn commands(&self) -> &[LlmCommand] {
        &self.commands
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

    /// Whether the proposal requested the compact repository directory tree.
    pub(super) fn repo_dirs(&self) -> bool {
        self.repo_dirs
    }

    /// Targeted repository map requests carried by the proposal.
    pub(super) fn repo_maps(&self) -> &[String] {
        &self.repo_map_requests
    }

    /// Directory listing requests carried by the proposal.
    ///
    /// Entries are normalized while decoding, so a listing that named a
    /// location outside the repository never reaches this accessor. The
    /// discovery loop reads it to recognize a round that only listed a
    /// directory as progress instead of a dead end.
    pub(super) fn dir_list_requests(&self) -> &[DirListRequest] {
        &self.dir_list_requests
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

    /// Reports whether the proposal carries any discovery capability command.
    ///
    /// Reads the pre-partitioned buckets, so the healing phase can decide
    /// whether to run the discovery dispatcher without rescanning the raw
    /// command list.
    pub(super) fn has_discovery_commands(&self) -> bool {
        !self.context_requests.is_empty()
            || !self.doc_requests.is_empty()
            || !self.repo_searches.is_empty()
            || !self.repo_maps().is_empty()
            || !self.dir_list_requests.is_empty()
            || self.repo_dirs
    }

    /// Reports whether the round carried nothing the harness can act upon.
    ///
    /// An empty round produced no conversational text, no file edits, no
    /// protocol error, no discovery command, no plan mutation, no verification
    /// request, and no terminal outcome. Such a round is a transport-level
    /// non-answer rather than a validated reply, so the FSM re-asks the model
    /// instead of recording it as an `ModelResult::Answer`. Ephemeral signals
    /// that grant no capability — the scratchpad notes and the detected user
    /// language — never make a round actionable on their own.
    pub(super) fn is_empty_step(&self) -> bool {
        self.text.trim().is_empty()
            && self.edits.is_empty()
            && self.protocol_errors.is_empty()
            && self.plans.is_empty()
            && self.verify_request.is_none()
            && self.terminal_outcome.is_none()
            && self.blocked.is_none()
            && !self.has_discovery_commands()
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
    intent: TurnIntent,
) -> anyhow::Result<ProposalStep> {
    let repo_root = context_manager.repo_root().to_path_buf();

    // Reading the repository state shells out to git; the call runs off the
    // async executor thread. A repository that cannot be inspected degrades to
    // an absent state block instead of failing the turn.
    let root = repo_root.clone();
    let repo_state = crate::blocking::run_blocking_async(move || {
        crate::git::state::get_repository_state(Some(&root))
    })
    .await
    .ok();

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
        // Staged content is merged in place: a file already present in context
        // keeps its position, its neighbors keep their tokens, and the round
        // extends the previous prompt instead of reshaping it. Files created
        // during the turn are appended after the existing block in path order,
        // which keeps the appended tail deterministic across rounds.
        let mut appended = Vec::new();
        for (path, content) in &staged.staged_files {
            if let Some(file) = context_files.iter_mut().find(|file| &file.path == path) {
                file.content = content.clone();
            } else {
                appended.push(ContextFileContent {
                    path: path.clone(),
                    access: ContextAccess::Editable,
                    content: content.clone(),
                });
            }
        }
        appended.sort_by(|a, b| a.path.cmp(&b.path));
        context_files.extend(appended);
        context_files.retain(|file| !staged.deleted_paths.contains(&file.path));
    }

    let targetable_paths: Vec<String> =
        context_files.iter().map(|file| file.path.clone()).collect();

    // The cache freezes the map on the first round of the turn, so later rounds
    // receive the identical string even after discovery widened the context.
    // The eager symbol map and the compact directory outline are mutually
    // exclusive: the model reads either symbol contours or the folder layout,
    // never both, so exactly one outline enters the prompt.
    let (repo_map, repo_dirs) = if options.include_repo_map {
        let map = cache
            .repo_map(&repo_root, &targetable_paths, options.repomap_token_budget)
            .await;
        (map, None)
    } else {
        (
            None,
            cache
                .repo_dirs(&repo_root, context_manager.pinned_patterns())
                .await,
        )
    };

    let repomap_tokens = repo_map
        .as_deref()
        .or(repo_dirs.as_deref())
        .map(crate::prompt::estimate_tokens)
        .unwrap_or(0);
    tracing::info!(
        workflow = workflow_name,
        model = %model,
        isolated_step = intent.is_isolated_step(),
        repo_map_tokens = repomap_tokens,
        "proposal round prepared"
    );
    let _ = stream_tx
        .send(StreamEvent::RepoMapTokens(repomap_tokens))
        .await;

    let history_tag = cache.history_tag(history_manager, intent).await;

    let mut assembly = PromptAssembly::new(
        repo_state,
        ctx_state.revision,
        context_files,
        workflow_name,
        protocol.name(),
        history_tag,
        repo_map,
    )
    .with_repo_dirs(repo_dirs);
    assembly.target_language = target_language.map(|language| language.to_string());
    assembly.intent = intent;
    assembly.turn_scratchpad = turn_scratchpad.map(|notes| notes.to_string());

    // The declared turn intent narrows the protocol once, so the rendered
    // instructions, the structured response schema, the stream filter, and the
    // decoder all observe the same capability surface: nothing is advertised
    // without being decodable, and nothing is accepted without being advertised.
    let scoped_protocol = protocol.with_capability_set(assembly.effective_capabilities(protocol));
    let protocol: &dyn PromptProtocol = scoped_protocol.as_ref();

    let messages = assembly.assemble_chat_messages(prompt, protocol);
    let response_format = protocol.response_format(&targetable_paths);

    let (llm_tx, mut llm_rx) = mpsc::channel::<StreamEvent>(100);
    // A session identifier pins the proposal round to one provider backend and
    // keeps its prompt-prefix cache warm across the discovery, patch-retry, and
    // healing rounds of the same turn. Without one the provider serves the
    // request statelessly.
    let client_call = provider.stream_chat_with_session(
        &model.name,
        messages,
        response_format,
        options.reasoning_effort,
        options.session_id.as_deref(),
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
                    let _ = forward_stream_tx
                        .send(StreamEvent::Error(err.clone()))
                        .await;
                }
                // A retried attempt starts from a clean slate: the transport
                // decorator discarded the fragments of the interrupted attempt
                // and re-issued the request, so the text accumulated for
                // decoding is discarded with them. Otherwise the partial output
                // of a broken attempt would be decoded together with the
                // complete answer of its retry.
                StreamEvent::ResetStream => {
                    assistant_text.clear();
                    let _ = forward_stream_tx.send(StreamEvent::ResetStream).await;
                }
                StreamEvent::Done => {
                    completed = true;
                }
                // Events the view does not model are forwarded as-is. A client
                // that stopped listening only silences the forwarding: the
                // round keeps draining the provider, so its output is still
                // decoded and the turn reaches a deterministic outcome instead
                // of being reported as an interrupted stream.
                other => {
                    let _ = forward_stream_tx.send(other).await;
                }
            }
        }
        for forwarded in stream_filter.finish() {
            let _ = forward_stream_tx.send(forwarded).await;
        }
        (assistant_text, cancelled, stream_error, completed)
    });

    let client_result = client_call.await;
    let (assistant_text, cancelled, stream_error, completed) =
        forward_task.await.map_err(|join_error| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_commands_preserves_the_decoded_command_vector() {
        let step = ProposalStep::from_commands(vec![
            LlmCommand::Text {
                content: "hello".to_string(),
            },
            LlmCommand::ContextRequest {
                path: "src/a.rs".to_string(),
                access: ContextAccess::ReadOnly,
            },
        ]);

        assert_eq!(step.commands().len(), 2);
        assert!(matches!(
            &step.commands()[0],
            LlmCommand::Text { content } if content == "hello"
        ));
        assert!(matches!(
            &step.commands()[1],
            LlmCommand::ContextRequest { path, .. } if path == "src/a.rs"
        ));
    }

    #[test]
    fn has_discovery_commands_detects_each_discovery_capability() {
        let text_only = ProposalStep::from_commands(vec![LlmCommand::Text {
            content: "hello".to_string(),
        }]);
        assert!(!text_only.has_discovery_commands());

        let context = ProposalStep::from_commands(vec![LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }]);
        assert!(context.has_discovery_commands());

        let search = ProposalStep::from_commands(vec![LlmCommand::RepoSearch {
            path: "src".to_string(),
            regex: "fn main".to_string(),
        }]);
        assert!(search.has_discovery_commands());

        let doc = ProposalStep::from_commands(vec![LlmCommand::DocRequest {
            topic: DocTopic::Git,
        }]);
        assert!(doc.has_discovery_commands());

        let dirs = ProposalStep::from_commands(vec![LlmCommand::RepoDirs]);
        assert!(dirs.repo_dirs());
        assert!(dirs.has_discovery_commands());

        let maps = ProposalStep::from_commands(vec![LlmCommand::RepoMap {
            path: "crates/core".to_string(),
        }]);
        assert_eq!(maps.repo_maps().len(), 1);
        assert_eq!(maps.repo_maps()[0], "crates/core");
        assert!(maps.has_discovery_commands());

        let dir_list = ProposalStep::from_commands(vec![LlmCommand::DirList {
            path: "crates/core".to_string(),
            recursive: false,
        }]);
        assert!(dir_list.has_discovery_commands());

        // A listing that would escape the repository is dropped during
        // decoding, so it never counts as discovery progress.
        let escaping = ProposalStep::from_commands(vec![LlmCommand::DirList {
            path: "../escape".to_string(),
            recursive: true,
        }]);
        assert!(!escaping.has_discovery_commands());
    }

    #[test]
    fn is_empty_step_rejects_rounds_without_actionable_output() {
        use tauqe_protocol::{ContextAccess, LlmCommand, LlmOutcome};

        let nothing = ProposalStep::from_commands(Vec::new());
        assert!(nothing.is_empty_step());

        let blank = ProposalStep::from_commands(vec![LlmCommand::Text {
            content: "  \n\t ".to_string(),
        }]);
        assert!(blank.is_empty_step());

        // Ephemeral channels grant no capability, so they cannot stand in for a
        // concrete proposal on their own.
        let ephemeral = ProposalStep::from_commands(vec![
            LlmCommand::Scratchpad {
                notes: "still thinking".to_string(),
            },
            LlmCommand::UserLanguage {
                language: "Russian".to_string(),
            },
        ]);
        assert!(ephemeral.is_empty_step());

        let answer = ProposalStep::from_commands(vec![LlmCommand::Text {
            content: "The function already returns 42.".to_string(),
        }]);
        assert!(!answer.is_empty_step());

        let discovery = ProposalStep::from_commands(vec![LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }]);
        assert!(!discovery.is_empty_step());

        let malformed = ProposalStep::from_commands(vec![LlmCommand::ProtocolError {
            message: "unclosed <edit>".to_string(),
        }]);
        assert!(!malformed.is_empty_step());

        let terminal = ProposalStep::from_commands(vec![LlmCommand::Outcome {
            outcome: LlmOutcome::Success { summary: None },
        }]);
        assert!(!terminal.is_empty_step());
    }
}
