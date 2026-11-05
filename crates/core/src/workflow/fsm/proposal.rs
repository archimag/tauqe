//! Model proposal resolution and normalized command accumulation.
//!
//! A proposal round is one atomic model interaction: the prompt is assembled
//! from the active context, the repo map, and history, sent to the provider, and
//! decoded into a uniform [`LlmCommand`] vocabulary. The round executor itself
//! lives in [`super::kernel`], which owns the frozen turn inputs and the
//! corrective budget of an empty answer. This module owns the per-round proposal
//! value, the turn-wide state accumulated across rounds, and the helpers that
//! normalize commands into verification targets, terminal outcomes, and
//! conversational text.

use tauqe_protocol::{
    DirListRequest, DocTopic, EditOperation, LlmCommand, LlmOutcome, ModelRef, RepoSearchRequest,
    VerifyOnSuccess, VerifyTarget,
};

use crate::edits::protocol::{
    ContextRequest, VerifyOnSuccess as EditVerifyOnSuccess, VerifyRequest as EditVerifyRequest,
    VerifyTarget as EditVerifyTarget,
};
use crate::workflow::WorkflowOptions;

use super::AbortReport;

/// Normalized proposal returned by a single model interaction.
///
/// Decoded commands are partitioned once at construction into typed buckets, so
/// every accessor reads pre-collected data instead of rescanning the raw
/// command list on each call. The explicit buckets also keep the proposal shape
/// self-documenting and make a new command kind impossible to miss silently.
///
/// The kernel assembles a cancelled round through the derived default, so every
/// field stays visible to the bounded-turn module and its phase executors.
#[derive(Default)]
pub(super) struct ProposalStep {
    pub(super) commands: Vec<LlmCommand>,
    pub(super) text: String,
    pub(super) edits: Vec<EditOperation>,
    pub(super) context_requests: Vec<ContextRequest>,
    pub(super) doc_requests: Vec<DocTopic>,
    pub(super) repo_searches: Vec<RepoSearchRequest>,
    pub(super) repo_dirs: bool,
    pub(super) repo_map_requests: Vec<String>,
    pub(super) dir_list_requests: Vec<DirListRequest>,
    /// Declared external documents requested by their mount point, in the order
    /// they were stated and without repetition.
    pub(super) outer_doc_requests: Vec<String>,
    pub(super) verify_request: Option<EditVerifyRequest>,
    pub(super) terminal_outcome: Option<LlmOutcome>,
    pub(super) blocked: Option<String>,
    pub(super) abort: Option<AbortReport>,
    pub(super) scratchpad: Option<String>,
    pub(super) language: Option<String>,
    pub(super) plans: Vec<tauqe_protocol::DiscussionPlanUpdate>,
    /// Decisions recorded for review findings, in the order they were stated.
    pub(super) review_updates: Vec<tauqe_protocol::review::ReviewUpdateProposal>,
    pub(super) custom_tools: Vec<LlmCommand>,
    pub(super) protocol_errors: Vec<String>,
    pub(super) is_cancelled: bool,
    /// Raw assistant text the round was decoded from.
    ///
    /// A contour that owns a typed payload decodes it through the single
    /// deserializer of its own schema, so the round keeps the text it was read
    /// from instead of reconstructing the payload from decoded commands.
    pub(super) raw_text: String,
}

impl ProposalStep {
    /// Partitions decoded commands into typed buckets in a single pass.
    ///
    /// Singleton commands (verification, terminal outcome, block, scratchpad,
    /// language) keep their first occurrence, matching the previous scan order.
    pub(super) fn from_commands(commands: Vec<LlmCommand>) -> Self {
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
                // A request for external project documentation travels the
                // research phase, which owns the catalogue and the cache of the
                // documents the project declares. The bucket keeps the mount
                // points, so the phase materializes each of them once.
                LlmCommand::OuterDocRequest { id } => {
                    let id = id.trim().to_string();
                    if !id.is_empty() && !step.outer_doc_requests.contains(&id) {
                        step.outer_doc_requests.push(id);
                    }
                }
                LlmCommand::RepoSearch { path, regex } => {
                    step.repo_searches
                        .push(RepoSearchRequest::new(path.clone(), regex.clone()));
                }
                LlmCommand::RepoDirs => step.repo_dirs = true,
                LlmCommand::RepoMap { path } => step.repo_map_requests.push(path.clone()),
                // The request is normalized during decoding, so a listing that
                // names a location outside the repository is dropped here
                // instead of reaching the research dispatcher.
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
                LlmCommand::ReviewUpdate { update } => {
                    step.review_updates.push(update.clone());
                }
                // A project command invocation is carried as a typed command and
                // executed by the harness against the declared allowlist before
                // anything is staged.
                LlmCommand::CustomTool { .. } => step.custom_tools.push(command.clone()),
                // An abort is an unconditional stop, so the first one carried by
                // a round is retained and later ones add nothing.
                LlmCommand::Abort {
                    reason,
                    recommendation,
                } => {
                    if step.abort.is_none() {
                        step.abort = Some(AbortReport {
                            reason: reason.clone(),
                            recommendation: recommendation.clone(),
                        });
                    }
                }
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

    /// Raw assistant text the round was decoded from.
    pub(super) fn raw_text(&self) -> &str {
        &self.raw_text
    }

    /// Attaches the raw assistant text the round was decoded from.
    pub(super) fn with_raw_text(mut self, raw_text: String) -> Self {
        self.raw_text = raw_text;
        self
    }

    /// File edit operations carried by the proposal.
    pub(super) fn edits(&self) -> &[EditOperation] {
        &self.edits
    }

    /// Context additions requested by the model.
    pub(super) fn context_requests(&self) -> &[ContextRequest] {
        &self.context_requests
    }

    /// Targeted repository map requests carried by the proposal.
    pub(super) fn repo_maps(&self) -> &[String] {
        &self.repo_map_requests
    }

    /// Declared external documents requested by their mount point.
    ///
    /// Each entry names an `[[outer_docs]]` id rather than a cache path, so the
    /// research phase resolves the location the harness derived for it.
    pub(super) fn outer_doc_requests(&self) -> &[String] {
        &self.outer_doc_requests
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

    /// Unconditional abort report carried by the proposal, if any.
    fn abort(&self) -> Option<&AbortReport> {
        self.abort.as_ref()
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
    pub(super) fn plans(&self) -> &[tauqe_protocol::DiscussionPlanUpdate] {
        &self.plans
    }

    /// Typed project command invocations carried by the proposal.
    ///
    /// Every entry is a `CustomTool` command: the harness validates its name
    /// against the declared allowlist and its arguments against the declaration
    /// before anything runs, so an invocation can never widen into an arbitrary
    /// command line.
    pub(super) fn custom_tools(&self) -> &[LlmCommand] {
        &self.custom_tools
    }

    /// Protocol syntax or structure errors encountered during decoding.
    pub(super) fn protocol_errors(&self) -> &[String] {
        &self.protocol_errors
    }

    /// Reports whether the proposal carries any research capability command.
    ///
    /// Reads the pre-partitioned buckets, so the healing phase can decide
    /// whether to run the research dispatcher without rescanning the raw
    /// command list.
    pub(super) fn has_research_commands(&self) -> bool {
        !self.context_requests.is_empty()
            || !self.doc_requests.is_empty()
            || !self.repo_searches.is_empty()
            || !self.repo_maps().is_empty()
            || !self.dir_list_requests.is_empty()
            || !self.outer_doc_requests.is_empty()
            || self.repo_dirs
    }

    /// Reports whether the round carried nothing the harness can act upon.
    ///
    /// An empty round produced no conversational text, no file edits, no
    /// protocol error, no research command, no plan mutation, no verification
    /// request, no project command invocation, and no terminal outcome that
    /// carries its own result. Such a round is a transport-level non-answer
    /// rather than a validated reply, so the FSM re-asks the model instead of
    /// recording it as an `ModelResult::Answer`. Ephemeral signals that grant no
    /// capability — the scratchpad notes and the detected user language — never
    /// make a round actionable on their own, and neither does a `success` status
    /// the model declared without stating what it accomplished.
    pub(super) fn is_empty_step(&self) -> bool {
        self.text.trim().is_empty()
            && self.edits.is_empty()
            && self.protocol_errors.is_empty()
            && self.plans.is_empty()
            && self.review_updates.is_empty()
            && self.verify_request.is_none()
            && !outcome_carries_result(self.terminal_outcome.as_ref())
            && self.blocked.is_none()
            && self.abort.is_none()
            && self.custom_tools.is_empty()
            && !self.has_research_commands()
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
    pub(super) abort: Option<AbortReport>,
    pub(super) plans: Vec<tauqe_protocol::DiscussionPlanUpdate>,
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
        if let Some(abort) = step.abort() {
            self.abort = Some(abort.clone());
        }
        self.plans.extend(step.plans().iter().cloned());
    }

    /// Drops the plan mutations accumulated so far.
    ///
    /// A corrective round answers for the payload that failed validation, so the
    /// refused mutations leave the turn state before the corrected round is
    /// absorbed: the payload is replaced, never accumulated beside the document it
    /// repairs.
    pub(super) fn clear_plans(&mut self) {
        self.plans.clear();
    }
}

/// Reports whether a success commit message states what the turn accomplished.
///
/// The commit message is the only payload a `success` status carries, so an
/// absent or blank one declares an outcome without stating a result.
fn commit_message_states_result(commit_message: Option<&str>) -> bool {
    match commit_message {
        Some(text) => !text.trim().is_empty(),
        None => false,
    }
}

/// Reports whether a terminal outcome carries the result its status declares.
///
/// `error` and `skip` always qualify: their decoders substitute a deterministic
/// message or reason when the model supplied none, so the status still travels
/// with a payload. `success` cannot be completed that way, because nobody but
/// the model can state what the turn accomplished; a payload-free success is
/// therefore a protocol defect rather than a result.
pub(super) fn outcome_carries_result(outcome: Option<&LlmOutcome>) -> bool {
    match outcome {
        Some(LlmOutcome::Success { commit_message }) => {
            commit_message_states_result(commit_message.as_deref())
        }
        Some(LlmOutcome::Error { .. } | LlmOutcome::Skip { .. }) => true,
        None => false,
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

/// Extracts a non-empty success commit message from a terminal outcome.
pub(super) fn extract_commit_message(outcome: Option<&LlmOutcome>) -> Option<String> {
    match outcome {
        Some(LlmOutcome::Success { commit_message }) => commit_message
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

/// Applies the review decisions a dialogue round recorded.
///
/// A decision is a domain mutation of the review session the turn is anchored
/// on, so it is applied at the round boundary that decoded it: the session
/// identity travels as a typed field of the turn options, and the storage engine
/// applies the amendment under its own lock, so two rounds can never interleave
/// their read-modify-write cycles. A decision that names a review group settles
/// the group itself — it ends the blocker, records the resolution, and moves the
/// whole membership — while a decision that names only a finding updates that
/// finding. A decision that cannot be persisted — an absent session, a finding
/// or a group that no longer exists, or a storage failure — is journaled and
/// never fails the turn: the code the round proposed is still valid, and the
/// developer keeps the diagnostic in the journal.
pub(super) fn record_review_decisions(
    step: &ProposalStep,
    options: &WorkflowOptions,
    repo_root: &std::path::Path,
) {
    let Some(review_id) = options.review_session_id.as_deref() else {
        return;
    };
    for decision in &step.review_updates {
        // A decision that names a group belongs to the group, not to one of its
        // manifestations, so it settles the group as one operational unit.
        match decision
            .group_id
            .as_deref()
            .map(str::trim)
            .filter(|group_id| !group_id.is_empty())
        {
            Some(group_id) => record_group_decision(repo_root, review_id, group_id, decision),
            None => record_finding_decision(repo_root, review_id, decision),
        }
    }
}

/// Applies a decision the round recorded for one finding of the anchored session.
fn record_finding_decision(
    repo_root: &std::path::Path,
    review_id: &str,
    decision: &tauqe_protocol::review::ReviewUpdateProposal,
) {
    let item_id = decision.item_id;
    let root = repo_root.to_path_buf();
    let anchored_review = review_id.to_string();
    let decision = decision.clone();
    let applied = crate::blocking::run_blocking(move || {
        crate::review::ReviewStorage::apply_decision(&root, &anchored_review, &decision)
    });
    match applied {
        Ok(Some(issue)) => tracing::info!(
            review_id = %review_id,
            item_id = issue.id,
            status = issue.status.as_str(),
            "review decision recorded in the dialogue",
        ),
        Ok(None) => tracing::warn!(
            review_id = %review_id,
            item_id,
            "review decision was not recorded: the finding is absent from the session",
        ),
        Err(err) => tracing::warn!(
            review_id = %review_id,
            item_id,
            error = %err,
            "review decision was not persisted",
        ),
    }
}

/// Applies a decision the round recorded for a whole review group.
///
/// A decision taken for a group belongs to the group, not to one of its
/// manifestations: it ends the recorded blocker, stores the resolution once on
/// the group, and stamps the same note onto every member finding in one atomic
/// write, so the group and its findings can never disagree about the decision.
/// The decided status then moves the whole membership together, because one root
/// cause is remediated by one coherent change set.
fn record_group_decision(
    repo_root: &std::path::Path,
    review_id: &str,
    group_id: &str,
    decision: &tauqe_protocol::review::ReviewUpdateProposal,
) {
    let item_id = decision.item_id;
    let root = repo_root.to_path_buf();
    let anchored_review = review_id.to_string();
    let anchored_group = group_id.to_string();
    let decision = decision.clone();
    let applied = crate::blocking::run_blocking(move || {
        // The recorded resolution settles the group itself, and the storage
        // engine rejects a decision that states no reasoning, so nothing is half
        // applied when the note is blank.
        let settled = crate::review::ReviewStorage::apply_group_decision(
            &root,
            &anchored_review,
            &anchored_group,
            &decision.note,
        )?;
        if settled.is_none() {
            return Ok(None);
        }
        let status = tauqe_protocol::IssueStatus::from(decision.status);
        let recommendations = decision.recommendations.clone();
        crate::review::ReviewStorage::update_session_group_findings(
            &root,
            &anchored_review,
            &anchored_group,
            move |issue| {
                issue.status = status;
                for recommendation in &recommendations {
                    let trimmed = recommendation.trim();
                    if !trimmed.is_empty()
                        && !issue
                            .recommendations
                            .iter()
                            .any(|existing| existing == trimmed)
                    {
                        issue.recommendations.push(trimmed.to_string());
                    }
                }
            },
        )
    });
    match applied {
        Ok(Some(findings)) => tracing::info!(
            review_id = %review_id,
            group_id,
            item_id,
            findings,
            "group decision recorded in the dialogue",
        ),
        Ok(None) => tracing::warn!(
            review_id = %review_id,
            group_id,
            item_id,
            "group decision was not recorded: the group is absent from the session",
        ),
        Err(err) => tracing::warn!(
            review_id = %review_id,
            group_id,
            item_id,
            error = %err,
            "group decision was not persisted",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_commands_preserves_the_decoded_command_vector() {
        use tauqe_protocol::ContextAccess;

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
    fn has_research_commands_detects_each_research_capability() {
        use tauqe_protocol::ContextAccess;

        let text_only = ProposalStep::from_commands(vec![LlmCommand::Text {
            content: "hello".to_string(),
        }]);
        assert!(!text_only.has_research_commands());

        let context = ProposalStep::from_commands(vec![LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }]);
        assert!(context.has_research_commands());

        let search = ProposalStep::from_commands(vec![LlmCommand::RepoSearch {
            path: "src".to_string(),
            regex: "fn main".to_string(),
        }]);
        assert!(search.has_research_commands());

        let doc = ProposalStep::from_commands(vec![LlmCommand::DocRequest {
            topic: DocTopic::Git,
        }]);
        assert!(doc.has_research_commands());

        let dirs = ProposalStep::from_commands(vec![LlmCommand::RepoDirs]);
        assert!(dirs.has_research_commands());

        let maps = ProposalStep::from_commands(vec![LlmCommand::RepoMap {
            path: "crates/core".to_string(),
        }]);
        assert_eq!(maps.repo_maps().len(), 1);
        assert_eq!(maps.repo_maps()[0], "crates/core");
        assert!(maps.has_research_commands());

        let dir_list = ProposalStep::from_commands(vec![LlmCommand::DirList {
            path: "crates/core".to_string(),
            recursive: false,
        }]);
        assert!(dir_list.has_research_commands());

        // A listing that would escape the repository is dropped during
        // decoding, so it never counts as research progress.
        let escaping = ProposalStep::from_commands(vec![LlmCommand::DirList {
            path: "../escape".to_string(),
            recursive: true,
        }]);
        assert!(!escaping.has_research_commands());
    }

    /// An outer-document request names a mount point of the project
    /// declaration, so it is research: a round that only asks for external
    /// documentation carries an actionable command instead of being an empty
    /// reply the corrective machine must re-ask.
    #[test]
    fn an_outer_document_request_is_research_that_names_its_mount_point() {
        use tauqe_protocol::LlmCommand;

        let step = ProposalStep::from_commands(vec![LlmCommand::OuterDocRequest {
            id: "mylib".to_string(),
        }]);
        assert_eq!(step.outer_doc_requests().len(), 1);
        assert_eq!(step.outer_doc_requests()[0], "mylib");
        assert!(step.has_research_commands());
        assert!(!step.is_empty_step());

        // A repeated mount point is materialized once, and a blank id grants
        // nothing: the round keeps the declared documents it actually named.
        let repeated = ProposalStep::from_commands(vec![
            LlmCommand::OuterDocRequest {
                id: " mylib ".to_string(),
            },
            LlmCommand::OuterDocRequest {
                id: "   ".to_string(),
            },
        ]);
        assert_eq!(repeated.outer_doc_requests().len(), 1);
        assert_eq!(repeated.outer_doc_requests()[0], "mylib");
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

        let research = ProposalStep::from_commands(vec![LlmCommand::ContextRequest {
            path: "src/a.rs".to_string(),
            access: ContextAccess::ReadOnly,
        }]);
        assert!(!research.is_empty_step());

        let malformed = ProposalStep::from_commands(vec![LlmCommand::ProtocolError {
            message: "unclosed <edit>".to_string(),
        }]);
        assert!(!malformed.is_empty_step());

        // A status without its payload is a protocol defect, not a result, so
        // the round stays empty and the corrective machine re-asks the model.
        let bare_success = ProposalStep::from_commands(vec![LlmCommand::Outcome {
            outcome: LlmOutcome::Success {
                commit_message: None,
            },
        }]);
        assert!(bare_success.is_empty_step());

        let summarized = ProposalStep::from_commands(vec![LlmCommand::Outcome {
            outcome: LlmOutcome::Success {
                commit_message: Some("Confirmed the change".to_string()),
            },
        }]);
        assert!(!summarized.is_empty_step());
    }
    #[test]
    fn a_terminal_success_counts_only_when_it_states_a_result() {
        use tauqe_protocol::{EditOperation, LlmCommand, LlmOutcome};

        // A written conclusion is a result on its own, whatever the status says.
        let explained = ProposalStep::from_commands(vec![
            LlmCommand::Text {
                content: "The helper already returns 42.".to_string(),
            },
            LlmCommand::Outcome {
                outcome: LlmOutcome::Success {
                    commit_message: None,
                },
            },
        ]);
        assert!(!explained.is_empty_step());

        // So is a proposal the harness can stage.
        let edited = ProposalStep::from_commands(vec![
            LlmCommand::Edit {
                operation: EditOperation::Create {
                    path: "src/new.rs".to_string(),
                    content: "fn created() {}\n".to_string(),
                },
            },
            LlmCommand::Outcome {
                outcome: LlmOutcome::Success {
                    commit_message: None,
                },
            },
        ]);
        assert!(!edited.is_empty_step());

        // A blank summary states no result either.
        let blank = ProposalStep::from_commands(vec![LlmCommand::Outcome {
            outcome: LlmOutcome::Success {
                commit_message: Some("  \n\t ".to_string()),
            },
        }]);
        assert!(blank.is_empty_step());

        // `error` and `skip` always carry their result: a decoder substitutes a
        // deterministic payload when the model supplied none.
        for outcome in [
            LlmOutcome::Error {
                message: "Execution error".to_string(),
            },
            LlmOutcome::Skip {
                reason: "Step skipped without a stated reason".to_string(),
            },
        ] {
            let step = ProposalStep::from_commands(vec![LlmCommand::Outcome { outcome }]);
            assert!(!step.is_empty_step());
        }
    }
    /// Builds a scratch session whose architectural group carries a blocker and
    /// whose findings belong to it, returning the root that holds the session.
    ///
    /// One finding is stored without a group, so it belongs to the implicit
    /// default group and must stay untouched by a decision taken for `arch-1`.
    fn blocked_group_fixture(review_id: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use tauqe_protocol::review::{
            IssueStatus, IssueType, ReviewGroup, ReviewItem, ReviewSession, ReviewSeverity,
        };

        let dir = tempfile::tempdir().expect("scratch root");
        let root = dir.path().to_path_buf();
        let finding = |id: u32, group: Option<&str>| ReviewItem {
            id,
            title: format!("Finding {id}"),
            uuid: None,
            commit: None,
            group: group.map(str::to_string),
            model: None,
            issue_type: IssueType::Architecture,
            severity: ReviewSeverity::Warning,
            status: IssueStatus::Todo,
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            resolution_notes: None,
            body: format!("Body {id}"),
        };
        let session = ReviewSession {
            id: review_id.to_string(),
            title: "Grouped Audit".to_string(),
            created_at: 1,
            groups: vec![ReviewGroup {
                id: "arch-1".to_string(),
                title: "Shared root cause".to_string(),
                blocked_reason: Some("The lock ordering must be decided first".to_string()),
                ..Default::default()
            }],
            items: vec![
                finding(1, Some("arch-1")),
                finding(2, Some("arch-1")),
                finding(3, None),
            ],
            ..Default::default()
        };
        crate::review::ReviewStorage::save_session(&root, &session).expect("save the session");
        (dir, root)
    }

    /// Records one group decision over the fixture.
    fn decide_group(root: &std::path::Path, review_id: &str, note: &str) {
        use tauqe_protocol::review::{ReviewDecisionStatus, ReviewUpdateProposal};

        record_group_decision(
            root,
            review_id,
            "arch-1",
            &ReviewUpdateProposal {
                item_id: 1,
                group_id: Some("arch-1".to_string()),
                status: ReviewDecisionStatus::Rejected,
                note: note.to_string(),
                recommendations: vec!["Document the ordering".to_string()],
            },
        );
    }

    /// A decision that names a review group settles the group itself: one atomic
    /// write clears the blocker and stamps the note onto every member finding,
    /// and the whole membership moves to the decided status.
    #[test]
    fn a_group_decision_clears_the_blocker_and_moves_every_member() {
        use tauqe_protocol::review::IssueStatus;

        let (_dir, root) = blocked_group_fixture("rev-group-decision");
        decide_group(&root, "rev-group-decision", "The cache owns the lock");

        let stored = crate::review::ReviewStorage::load_review(&root, "rev-group-decision")
            .expect("load the session")
            .expect("the session is stored");
        let group = &stored.groups[0];
        assert!(
            group.blocked_reason.is_none(),
            "the decision ends the blocker: {:?}",
            group.blocked_reason
        );
        assert_eq!(
            group.resolution_notes.as_deref(),
            Some("The cache owns the lock")
        );

        for item in stored.items.iter().filter(|item| item.id != 3) {
            assert_eq!(item.status, IssueStatus::Rejected);
            assert_eq!(
                item.resolution_notes.as_deref(),
                Some("The cache owns the lock")
            );
            assert_eq!(
                item.recommendations,
                vec!["Document the ordering".to_string()],
                "the agreed remediation is appended once"
            );
        }

        // A finding of another group is never touched.
        let untouched = stored.find_item(3).expect("the third finding is stored");
        assert_eq!(untouched.status, IssueStatus::Todo);
        assert!(untouched.resolution_notes.is_none());
    }

    /// A decision must state its own reasoning: the storage engine rejects one
    /// without a note, so the group keeps its blocker and its membership keeps
    /// its lifecycle status.
    #[test]
    fn a_group_decision_without_its_reasoning_changes_nothing() {
        use tauqe_protocol::review::IssueStatus;

        let (_dir, root) = blocked_group_fixture("rev-group-blank-decision");
        decide_group(&root, "rev-group-blank-decision", "   ");

        let stored = crate::review::ReviewStorage::load_review(&root, "rev-group-blank-decision")
            .expect("load the session")
            .expect("the session is stored");
        assert_eq!(
            stored.groups[0].blocked_reason.as_deref(),
            Some("The lock ordering must be decided first")
        );
        assert!(stored.groups[0].resolution_notes.is_none());
        let member = stored.find_item(1).expect("the finding is stored");
        assert_eq!(member.status, IssueStatus::Todo);
        assert!(member.resolution_notes.is_none());
    }
}
