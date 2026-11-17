//! Autonomous bounded turn control loop and submodules.
//!
//! [`run_turn_loop`] is the single execution loop of a turn. The model alternates
//! between discovery, virtual staging, application, and verification inside one
//! bounded turn, and every round spends one call of the shared round budget: a
//! discovery round, a patch retry, a verification healing round, and the corrective
//! round of a degenerate answer all draw from the same pool. No phase owns a nested
//! loop of its own, so a turn is never terminated by the exhaustion of a local retry
//! counter instead of by the budget the developer configured.

pub mod edit;
pub mod research;
pub mod staging;

use tauqe_protocol::{
    CapabilityTree, LlmCapability, LlmCommand, ProjectConfig, TextCommand, TurnPhase,
    VerificationVerdict,
};

use super::context::LoopContext;
use super::outcome::{FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use crate::git::state::snapshot_worktree;
use crate::prompt::ContourKind;
use crate::providers::StreamEvent;
use crate::workflow::retry::build_patch_retry_prompt;

use edit::{
    declared_commit_message, execute_edit_step, prepare_healing, EditState, EditStepVerdict,
    TurnTransaction,
};
use research::{
    envelope_has_text, execute_research_step, holds_a_higher_contour_command, ResearchStepKind,
    MAX_EMPTY_RESEARCH_ROUNDS, MAX_SATISFIED_RESEARCH_ROUNDS,
};
use staging::{execute_staging_step, render_failures, StagingState};

pub(crate) use edit::EDIT_CAPABILITIES;
pub use research::ResearchLoopConfig;
#[cfg(test)]
pub(crate) use research::RESEARCH_CAPABILITIES;

/// Builds the algebraic capability expression for an autonomous bounded turn.
///
/// The surface is the one an isolated step owns: the turn executes an approved
/// design inside the codebase, so it carries the ambient channels of the
/// dialogue head — the scratchpad and the conversational message — beside the
/// branches that act, minus plan authoring and the internal documentation of
/// the harness. The answer branch is withheld on purpose: a step reports
/// through the ambient message while its regime obliges it to change the tree,
/// so a conversational-only round is not an outcome the contour offers.
pub fn turn_capability_expr() -> CapabilityTree {
    let ambient = CapabilityTree::root([
        CapabilityTree::leaf(LlmCapability::Scratchpad),
        CapabilityTree::leaf(LlmCapability::TextMessage),
    ]);
    let branches = vec![
        super::repl::research_branch(false),
        super::repl::edit_branch(),
        super::repl::blocked_branch(),
        super::repl::abort_branch(),
    ];
    CapabilityTree::root([ambient, CapabilityTree::any_of("action", branches)])
}

/// Configuration for the bounded turn loop.
///
/// The value carries the one budget the turn spends and the material budgets a
/// discovery round reads. Nothing here is a per-phase retry counter: a patch retry
/// and a healing round draw from `max_turn_rounds` exactly as a discovery round
/// does.
#[derive(Debug, Clone)]
pub struct TurnLoopConfig {
    /// Total budget of physical provider calls across all phases of the turn.
    pub(crate) max_turn_rounds: usize,
    /// Material budgets of a discovery round: context tokens, repo map tokens, and
    /// the operational caps of one round.
    pub(crate) research: ResearchLoopConfig,
}

impl Default for TurnLoopConfig {
    fn default() -> Self {
        Self {
            max_turn_rounds: crate::config::DEFAULT_MAX_TURN_ROUNDS,
            research: ResearchLoopConfig::default(),
        }
    }
}

impl TurnLoopConfig {
    /// Creates turn loop configuration from authoritative project configuration.
    pub fn from_app_config(config: &crate::config::AppConfig) -> Self {
        Self {
            max_turn_rounds: config.repl.max_turn_rounds,
            research: ResearchLoopConfig::from_context(&config.context),
        }
    }
}

/// Artifact produced by a successful turn execution cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnArtifact {
    /// Repository-relative paths touched by code edits during this turn.
    pub modified_paths: Vec<String>,
    /// Git commit hash if changes were committed and squashed.
    pub commit_hash: Option<String>,
    /// Diagnostic of a Git transaction that could not be finalized.
    pub commit_error: Option<String>,
    /// Final toolchain verification verdict if edits were performed.
    pub verification: Option<VerificationVerdict>,
    /// Number of physical provider calls the turn spent.
    rounds_spent: usize,
    /// Remaining response envelope with companion capabilities.
    envelope: ResponseEnvelope,
}

impl TurnArtifact {
    /// Reports a turn that closed without touching the working tree.
    fn without_change_set(rounds_spent: usize, envelope: ResponseEnvelope) -> Self {
        Self {
            modified_paths: Vec::new(),
            commit_hash: None,
            commit_error: None,
            verification: None,
            rounds_spent,
            envelope,
        }
    }
}

/// Concludes a turn that produced no change set.
fn completed_without_change_set(
    rounds_spent: usize,
    envelope: ResponseEnvelope,
) -> LoopResult<TurnArtifact> {
    LoopResult::Success(LoopSuccess::new(
        TurnArtifact::without_change_set(rounds_spent, envelope.clone()),
        envelope,
    ))
}

/// Announces a phase transition of the bounded turn.
///
/// The interface event and the journal record are emitted together, so a phase
/// the turn enters is never visible on one surface and missing from the other.
fn announce_turn_phase(
    ctx: &LoopContext<'_>,
    phase: TurnPhase,
    round: usize,
    max_rounds: Option<usize>,
    detail: impl Into<String>,
) {
    tracing::info!(
        repo_root = %ctx.repo_root.display(),
        phase = %phase,
        round,
        max_rounds,
        "bounded turn entered a phase"
    );
    ctx.events.emit(StreamEvent::TurnPhase {
        phase,
        round: Some(round),
        max_rounds,
        detail: Some(detail.into()),
    });
}

/// Reports whether the turn executes one isolated step.
///
/// The regime of a turn is declared by the brief the loop that drives it handed to
/// the assembly, so no contour is ever inferred from the shape of a response.
fn is_step_contour(ctx: &LoopContext<'_>) -> bool {
    ctx.prompt_builder
        .turn_brief
        .as_ref()
        .is_some_and(|brief| brief.contour == ContourKind::Step)
}

/// Reports whether the round declared how the task ends.
fn declares_terminal(envelope: &ResponseEnvelope) -> bool {
    envelope.has(LlmCapability::CommitMessage)
        || envelope.has(LlmCapability::TurnError)
        || envelope.has(LlmCapability::TurnSkip)
}

/// Resolves the project configuration the turn executes against.
///
async fn resolve_project_config(ctx: &mut LoopContext<'_>) -> ProjectConfig {
    if let Some(project) = ctx.prompt_builder.project_config.clone() {
        return project;
    }
    ctx.prompt_builder
        .input_cache
        .project(ctx.repo_root, Some(&ctx.cancel.receiver()))
        .await
}

/// Returns the last answer the model produced in this turn.
fn last_answer(ctx: &LoopContext<'_>) -> String {
    ctx.transcript()
        .messages()
        .into_iter()
        .rfind(|message| message.role == "assistant")
        .map(|message| message.content)
        .unwrap_or_else(|| {
            "The turn reached its round budget before producing an answer.".to_string()
        })
}

/// Maps the exhausted round budget to the outcome the contour declares.
///
/// An isolated step reports the stop as a domain failure, because the harness that
/// owns it escalates the execution instead of answering the developer. A dialogue is
/// closed by the answer it was asked for, so it carries the last answer the model
/// wrote beside the notice that names the limit it reached.
fn budget_outcome(ctx: &mut LoopContext<'_>) -> LoopResult<TurnArtifact> {
    let spent = ctx.rounds_spent();
    if is_step_contour(ctx) {
        return LoopResult::Exhausted(FailureReport::new(
            "Turn round budget exhausted",
            spent,
            Vec::new(),
        ));
    }
    let answer = last_answer(ctx);
    let text = format!(
        "{answer}\n\n---\n**[Round Budget Reached]** The turn spent its whole budget of {spent} provider call(s). The answer above is the last one the model produced; ask again to continue from the material the turn already gathered."
    );
    let envelope =
        ResponseEnvelope::from_commands(&[LlmCommand::Text(TextCommand { content: text })]);
    completed_without_change_set(spent, envelope)
}

/// Runs the bounded turn lifecycle.
///
/// The loop is the single place a turn is driven: the model alternates between
/// discovery, virtual staging, application, and verification, and the round budget
/// bounds the whole cycle. The turn is transactional — a change set is committed only
/// after it converged in virtual staging and the toolchain accepted it, and every
/// other exit restores the working tree to its pre-turn revision.
pub async fn run_turn_loop(
    ctx: &mut LoopContext<'_>,
    initial_envelope: Option<ResponseEnvelope>,
    config: &TurnLoopConfig,
) -> Result<LoopResult<TurnArtifact>, SystemError> {
    ctx.check_cancelled()?;
    ctx.set_turn_round_limit(Some(config.max_turn_rounds));

    let mut transaction = TurnTransaction::new(ctx.repo_root);
    let outcome = run_turn_loop_inner(ctx, initial_envelope, config, &mut transaction).await;
    // Every exit that did not settle the transaction unwinds it here, so a
    // cancellation, a healing round whose request failed, an exhausted budget, and a
    // change set the substrate refused all leave the repository at its pre-turn
    // revision.
    transaction.rollback(ctx).await;
    match outcome {
        Err(SystemError::RoundBudgetExhausted) => Ok(budget_outcome(ctx)),
        other => other,
    }
}

async fn run_turn_loop_inner(
    ctx: &mut LoopContext<'_>,
    initial_envelope: Option<ResponseEnvelope>,
    config: &TurnLoopConfig,
    transaction: &mut TurnTransaction,
) -> Result<LoopResult<TurnArtifact>, SystemError> {
    let mut envelope = match initial_envelope {
        Some(envelope) => envelope,
        None => {
            announce_turn_phase(
                ctx,
                TurnPhase::Proposal,
                1,
                Some(config.max_turn_rounds),
                "Awaiting initial response",
            );
            ctx.request_round().await?
        }
    };

    let mut staging = StagingState::default();
    let mut edit: Option<EditState> = None;
    let mut declared_summary: Option<String> = None;
    let mut healing_rounds = 0usize;
    let mut satisfied_rounds = 0usize;
    let mut empty_rounds = 0usize;

    loop {
        ctx.check_cancelled()?;

        // 1. Discovery: the round the model produced may widen the context it reads.
        let discovery = execute_research_step(ctx, &mut envelope, &config.research).await?;
        let mut request_next_round = match discovery.kind {
            ResearchStepKind::Idle => false,
            ResearchStepKind::Widened => {
                satisfied_rounds = 0;
                empty_rounds = 0;
                true
            }
            ResearchStepKind::AlreadySatisfied => {
                satisfied_rounds += 1;
                satisfied_rounds <= MAX_SATISFIED_RESEARCH_ROUNDS
            }
            ResearchStepKind::DeadEnd => {
                empty_rounds += 1;
                empty_rounds <= MAX_EMPTY_RESEARCH_ROUNDS
            }
        };
        if request_next_round && holds_a_higher_contour_command(&envelope) {
            request_next_round = false;
        }

        // 2. A change set the round proposed: stage it, then apply and verify it.
        if EDIT_CAPABILITIES
            .iter()
            .any(|capability| envelope.has(*capability))
        {
            if declared_summary.is_none() {
                declared_summary = declared_commit_message(&envelope);
            }
            if edit.is_none() {
                let project = resolve_project_config(ctx).await;
                let initial_worktree =
                    crate::blocking::run_blocking(|| snapshot_worktree(ctx.repo_root));
                edit = Some(EditState::new(project, initial_worktree));
            }
            let state = match edit.as_mut() {
                Some(state) => state,
                None => {
                    return Err(SystemError::Other(
                        "the edit state was not opened before the change set was staged"
                            .to_string(),
                    ))
                }
            };

            // 2a. Virtual staging, repaired from the same budget the turn draws on.
            let mut attempt = execute_staging_step(ctx, &mut envelope, &mut staging);
            while !staging.failed.is_empty() {
                announce_turn_phase(
                    ctx,
                    TurnPhase::Staging,
                    staging.attempts,
                    Some(config.max_turn_rounds),
                    format!("Patch retry {}", staging.attempts),
                );
                for (path, reason) in &attempt.refusals {
                    tracing::warn!(
                        repo_root = %ctx.repo_root.display(),
                        attempt = staging.attempts,
                        path = path.as_str(),
                        reason = reason.as_str(),
                        "patch retry requested"
                    );
                    ctx.events.emit(StreamEvent::EditFileRetrying {
                        path: path.clone(),
                        attempt: staging.attempts,
                        max_retries: 0,
                        reason: reason.clone(),
                    });
                }
                let repair_prompt = build_patch_retry_prompt(
                    &staging.succeeded,
                    &staging.failed,
                    &[],
                    ctx.prompt_builder.patch_strategy,
                );
                let mut reply = ctx
                    .request_repair(&repair_prompt, staging.staged_state.as_ref())
                    .await?;
                attempt = execute_staging_step(ctx, &mut reply, &mut staging);
                if !attempt.addressed {
                    // The repair round answered with something other than a change set,
                    // so its answer is not staging's to interpret and travels upward.
                    return Ok(LoopResult::Diverged(reply));
                }
            }

            let abandoned = render_failures(&staging.abandoned, ctx.prompt_builder.patch_strategy);
            if !abandoned.is_empty() {
                let mut lines = Vec::new();
                for (path, reason) in &abandoned {
                    tracing::warn!(
                        repo_root = %ctx.repo_root.display(),
                        path = path.as_str(),
                        reason = reason.as_str(),
                        "a refused file was not retried and stays out of the change set"
                    );
                    lines.push(format!("- `{path}`: {reason}"));
                }
                let notice = format!(
                    "\n\n---\n⚠️ **[Change Set Incomplete]** The corrected edits do not retry the following failed file(s), so they stay out of this change set:\n{}\n\n",
                    lines.join("\n")
                );
                ctx.events.emit_text_delta(notice);
            }
            state.adopt_staging(&staging);

            // 2b. One application and verification pass over the converged change set.
            announce_turn_phase(
                ctx,
                TurnPhase::Verification,
                healing_rounds + 1,
                Some(config.max_turn_rounds),
                "Verifying project toolchain",
            );

            match execute_edit_step(ctx, &mut *state, transaction).await? {
                EditStepVerdict::Passed(verification) => {
                    let summary = declared_summary.as_deref().unwrap_or("REPL changes");
                    // The change set is already on disk and verified, so a transaction
                    // that could not be finalized must not hide behind a silent `None`
                    // hash: the reason travels with the artifact.
                    let (commit_hash, commit_error) = match ctx
                        .policy
                        .apply_final(ctx.repo_root, &state.modified_paths, summary)
                        .await
                    {
                        Ok(commit_hash) => (commit_hash, None),
                        Err(err) => {
                            let message = format!(
                                "Changes were applied but the Git transaction could not be finalized: {err}"
                            );
                            tracing::error!(error = %err, "{}", message);
                            (None, Some(message))
                        }
                    };
                    transaction.settle();

                    // A committed change set releases the whole transient context: the
                    // files discovery gathered and the external documents the registry
                    // materialized share one lifecycle, so neither outlives the edits it
                    // served. A failed finalization keeps the material, because the
                    // turn's decision is not durable yet.
                    if commit_error.is_none() {
                        crate::blocking::run_blocking(|| ctx.context_manager.clear_transient());
                        ctx.events
                            .emit(StreamEvent::ContextChanged(ctx.context_manager.get_state()));
                    }

                    return Ok(LoopResult::Success(LoopSuccess::new(
                        TurnArtifact {
                            modified_paths: state.modified_paths.clone(),
                            commit_hash,
                            commit_error,
                            verification: Some(verification),
                            rounds_spent: ctx.rounds_spent(),
                            envelope: envelope.clone(),
                        },
                        envelope,
                    )));
                }
                EditStepVerdict::ApplyFailed { message } => {
                    return Ok(LoopResult::Exhausted(FailureReport::new(
                        format!("Failed to commit staged edits to disk: {message}"),
                        healing_rounds,
                        vec![message],
                    )));
                }
                EditStepVerdict::HookFailed { report } => {
                    let retry_prompt = prepare_healing(ctx, &report, &mut healing_rounds).await;
                    announce_turn_phase(
                        ctx,
                        TurnPhase::Healing,
                        healing_rounds,
                        Some(config.max_turn_rounds),
                        format!("Post-hook healing round {healing_rounds}"),
                    );
                    envelope = ctx.request_healing(&retry_prompt).await?;
                    continue;
                }
                EditStepVerdict::VerificationFailed { report } => {
                    let retry_prompt = prepare_healing(ctx, &report, &mut healing_rounds).await;
                    announce_turn_phase(
                        ctx,
                        TurnPhase::Healing,
                        healing_rounds,
                        Some(config.max_turn_rounds),
                        format!("Healing round {healing_rounds}"),
                    );
                    // The healing round re-enters the loop, so the model may widen the
                    // context before it proposes the corrected change set.
                    envelope = ctx.request_healing(&retry_prompt).await?;
                    continue;
                }
            }
        }

        if request_next_round {
            // The findings of the round are the feedback of the round that follows.
            ctx.publish_round_feedback();
            envelope = ctx.request_round().await?;
            continue;
        }

        // A round that declared how the task ends, or a dialogue that answered in
        // prose, concludes the turn without touching the working tree.
        if declares_terminal(&envelope) || (!is_step_contour(ctx) && envelope_has_text(&envelope)) {
            return Ok(completed_without_change_set(ctx.rounds_spent(), envelope));
        }

        return Ok(LoopResult::Diverged(envelope));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink};
    use crate::prompt::json::JsonPromptProtocol;
    use crate::prompt::TurnBrief;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;
    use tauqe_protocol::{CapabilityTree, ContextAccess, LlmCapability, LlmCommand, ModelRef};

    struct Fixture {
        dir: tempfile::TempDir,
        context: ContextManager,
        journal: TurnResearchJournal,
        provider: MockLlmProvider,
        model: ModelRef,
        protocol: JsonPromptProtocol,
        cache: crate::control::cache::TurnInputCache,
        cancel: CancellationToken,
        events: EventSink,
        policy: crate::workflow::git_policy::GitTransactionPolicy,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)], responses: Vec<MockResponse>) -> Self {
            let dir = tempfile::tempdir().expect("scratch directory");
            let root = dir.path().to_path_buf();
            crate::git::init_repository(Some(&root), true).expect("repository");
            for (path, content) in files {
                let full = root.join(path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).expect("dir");
                }
                std::fs::write(full, content).expect("file");
            }
            let owned: Vec<String> = files.iter().map(|(path, _)| path.to_string()).collect();
            if !owned.is_empty() {
                crate::git::cmd::stage_files(&root, &owned).expect("stage");
                crate::git::cmd::commit_with_developer_author(&root, "Initial commit", false)
                    .expect("commit");
            }
            let mut context = ContextManager::new(root);
            for (path, _) in files {
                context
                    .add_file(path, ContextAccess::Editable)
                    .expect("context file");
            }
            Self {
                dir,
                context,
                journal: TurnResearchJournal::default(),
                provider: MockLlmProvider::scripted(responses),
                model: ModelRef::openrouter("mock/model"),
                protocol: JsonPromptProtocol::edits(),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::git_policy::GitTransactionPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            let caps = turn_capability_expr();
            let prompt_builder = crate::control::prompt::PromptBuilder::new(
                &self.provider,
                &self.model,
                &self.protocol,
                &mut self.cache,
            );
            LoopContext::new(
                &self.cancel,
                &self.events,
                self.dir.path(),
                &mut self.context,
                &mut self.journal,
                &mut self.policy,
                caps,
                prompt_builder,
            )
        }
    }

    /// Builds the change set of a replacement of `hello.rs`.
    fn change_set(search: &str, replace: &str) -> String {
        serde_json::json!({
            "message": "Update hello",
            "action": {
                "kind": "edit",
                "changes": [{
                    "op": "replace",
                    "path": "hello.rs",
                    "to": "",
                    "start": "",
                    "end": "",
                    "content": "",
                    "old_text": search,
                    "new_text": replace
                }],
                "commit_message": "Update hello",
                "reason": ""
            }
        })
        .to_string()
    }

    /// Builds a discovery round that lists the repository root.
    fn discovery(message: &str) -> String {
        serde_json::json!({
            "message": message,
            "action": {
                "kind": "research",
                "dir_lists": [{ "path": "", "recursive": false }]
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn turn_loop_returns_pure_text_response() {
        let mut fixture = Fixture::new(&[], vec![MockResponse::text("Turn text result")]);
        let mut ctx = fixture.context();

        let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert!(success.value.modified_paths.is_empty());
        assert!(success.response.has(LlmCapability::TextMessage));
    }

    #[tokio::test]
    async fn turn_loop_executes_edits_and_commits() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![MockResponse::text(change_set(
                "fn hello() {}\n",
                "fn hello() -> i32 { 1 }\n",
            ))],
        );
        let mut ctx = fixture.context();

        let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        let success = outcome.into_success().expect("success");
        assert_eq!(success.value.modified_paths, vec!["hello.rs"]);
        assert!(success.value.commit_hash.is_some());
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("hello.rs")).unwrap(),
            "fn hello() -> i32 { 1 }\n"
        );
    }

    #[test]
    fn turn_capability_expr_declares_the_terminal_branches() {
        let expr = turn_capability_expr();
        let leaves = expr.leaves();
        assert!(leaves.contains(&LlmCapability::TextMessage));
        assert!(leaves.contains(&LlmCapability::EditReplace));
        assert!(leaves.contains(&LlmCapability::ContextRequest));
        assert!(leaves.contains(&LlmCapability::Blocked));
        assert!(leaves.contains(&LlmCapability::Abort));
        assert!(!leaves.contains(&LlmCapability::PlanCreation));
        assert!(!leaves.contains(&LlmCapability::PlanUpdate));
        assert!(!leaves.contains(&LlmCapability::ReviewUpdate));
        assert!(
            !leaves.contains(&LlmCapability::DocRequest),
            "a step never requests the internal documentation of the harness"
        );
        let CapabilityTree::Set(_, items) = expr else {
            panic!("expected Set at root");
        };
        let CapabilityTree::AnyOf(label, branches) = &items[1] else {
            panic!("expected AnyOf for action branches");
        };
        assert_eq!(label, "action");
        let kinds: Vec<&str> = branches
            .iter()
            .filter_map(|branch| branch.label())
            .collect();
        assert_eq!(kinds, vec!["research", "edit", "blocked", "abort"]);
        assert!(branches[1].contains(LlmCapability::CommitMessage));
        assert!(!branches[1].contains(LlmCapability::Blocked));
    }

    /// Sink that collects the records a test emits on its own thread.
    #[derive(Clone, Default)]
    struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLog {
        fn contents(&self) -> String {
            let bytes = match self.0.lock() {
                Ok(captured) => captured.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self.0.lock() {
                Ok(mut captured) => captured.extend_from_slice(buf),
                Err(poisoned) => poisoned.into_inner().extend_from_slice(buf),
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// A phase the turn enters is journaled, not only streamed to the interface.
    #[tokio::test]
    async fn turn_loop_journals_the_phase_it_enters() {
        let mut fixture = Fixture::new(&[], vec![MockResponse::text("Turn text result")]);
        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(captured.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        let mut ctx = fixture.context();
        let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
            .await
            .expect("runs loop");
        drop(guard);

        assert!(outcome.is_success());
        let logged = captured.contents();
        assert!(
            logged.contains("bounded turn entered a phase"),
            "the phase transition must reach the journal: {logged}"
        );
        assert!(
            logged.to_lowercase().contains("proposal"),
            "the journal names the phase it entered: {logged}"
        );
    }

    /// A round that only narrates its next step is a preamble: the turn continues
    /// against the material the round widened instead of closing on the narration.
    #[tokio::test]
    async fn a_narration_round_continues_the_turn() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![
                MockResponse::text(discovery("Let me list the tree first.")),
                MockResponse::text("The file defines one function."),
                MockResponse::text("The file defines one function."),
            ],
        );
        let mut ctx = fixture.context();

        let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());
        assert!(
            fixture.provider.requests().len() >= 2,
            "a narration round never closes the turn"
        );
        let success = outcome.into_success().expect("success");
        let texts: Vec<String> = success
            .response
            .commands()
            .iter()
            .filter_map(|command| match command {
                LlmCommand::Text(command) => Some(command.content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["The file defines one function.".to_string()]);
        assert_eq!(texts, vec!["The file defines one function.".to_string()]);
    }

    /// A file delivered by a discovery round is reseeded into the prefix of the next round.
    #[tokio::test]
    async fn a_delivered_file_reaches_the_request_of_the_next_round() {
        let mut fixture = Fixture::new(
            &[("init.rs", "fn init() {}\n")],
            vec![
                MockResponse::text(
                    serde_json::json!({
                        "message": "Let me read helper.rs first.",
                        "action": {
                            "kind": "research",
                            "context_requests": [
                                { "path": "helper.rs", "access": "read_only" }
                            ]
                        }
                    })
                    .to_string(),
                ),
                MockResponse::text("I have read helper.rs."),
            ],
        );
        let root = fixture.dir.path();
        std::fs::write(root.join("helper.rs"), "pub const ANSWER: i32 = 42;\n").unwrap();
        crate::git::cmd::stage_files(root, &["helper.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(root, "add helper", false).unwrap();

        let mut ctx = fixture.context();
        let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success());

        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 2, "discovery round followed by conclusion");
        let second_context = requests[1]
            .messages
            .iter()
            .find(|m| m.content.starts_with("<context>"))
            .expect("context block present");
        assert!(
            second_context
                .content
                .contains("pub const ANSWER: i32 = 42;"),
            "the delivered file must be present in the second round context"
        );
    }
    /// A healing round re-enters the loop, so the model may widen the context before
    /// it proposes the corrected change set. The round that follows a healing round is
    /// an ordinary round of the same turn and draws from the same budget.
    #[tokio::test]
    async fn a_healing_round_re_enters_the_loop_with_the_report() {
        let verify = concat!(
            "[project.verify]\n",
            "command = \"grep\"\n",
            "args = [\"-q\", \"fn hello() -> i32 { 2 }\", \"hello.rs\"]\n",
        );
        let mut responses = vec![MockResponse::text(change_set(
            "fn hello() {}\n",
            "fn hello() -> i32 { 1 }\n",
        ))];
        // Every later round answers in prose: a dialogue closes on the first answer
        // it receives, so the script can never be drained whatever the budget grants.
        for _ in 0..4 {
            responses.push(MockResponse::text("The verification report was read."));
        }
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n"), ("tauqe.toml", verify)],
            responses,
        );
        let mut ctx = fixture.context();
        let config = TurnLoopConfig {
            max_turn_rounds: 3,
            ..TurnLoopConfig::default()
        };

        let outcome = run_turn_loop(&mut ctx, None, &config)
            .await
            .expect("runs loop");
        assert!(
            outcome.is_success(),
            "a dialogue closes on the answer it was asked for"
        );
        let requests = fixture.provider.requests();
        assert!(
            requests.len() >= 2,
            "the failed verification opens a healing round"
        );
        let healing_request = requests[1]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            healing_request.contains("Code verification failed"),
            "the healing round carries the verification report"
        );
    }

    /// A refused member is repaired by one patch-retry round drawn from the same
    /// budget, and the repaired change set then reaches the working tree.
    #[tokio::test]
    async fn a_refused_change_set_is_repaired_by_one_patch_retry_round() {
        let mut responses = vec![
            MockResponse::text(change_set("nonexistent text", "irrelevant")),
            MockResponse::text(change_set("fn hello() {}\n", "fn hello() -> i32 { 1 }\n")),
        ];
        for _ in 0..4 {
            responses.push(MockResponse::text("The repair was applied."));
        }
        let mut fixture = Fixture::new(&[("hello.rs", "fn hello() {}\n")], responses);
        let mut ctx = fixture.context();

        let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
            .await
            .expect("runs loop");
        assert!(outcome.is_success(), "the repaired change set must commit");
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("hello.rs")).unwrap(),
            "fn hello() -> i32 { 1 }\n"
        );
    }

    /// A step whose round only discovers reports the exhausted budget as a domain
    /// failure, because the harness that owns the step escalates the execution.
    #[tokio::test]
    async fn the_round_budget_bounds_a_step_turn() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![
                MockResponse::text(discovery("Let me list the tree")),
                MockResponse::text(discovery("Let me list the tree again")),
            ],
        );
        let mut ctx = fixture.context();
        ctx.set_turn_brief(Some(TurnBrief::step()));
        let config = TurnLoopConfig {
            max_turn_rounds: 1,
            ..TurnLoopConfig::default()
        };

        let outcome = run_turn_loop(&mut ctx, None, &config)
            .await
            .expect("runs loop");
        assert!(outcome.is_exhausted());
        assert_eq!(
            fixture.provider.requests().len(),
            1,
            "the budget bounds the physical calls of the turn"
        );
    }

    /// A dialogue is closed by the answer it was asked for, so an exhausted budget
    /// carries the last answer the model wrote beside the notice of the limit.
    #[tokio::test]
    async fn the_round_budget_closes_a_dialogue_with_its_last_answer() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![
                MockResponse::text(discovery("Let me list the tree")),
                MockResponse::text(discovery("Let me list the tree again")),
            ],
        );
        let mut ctx = fixture.context();
        let config = TurnLoopConfig {
            max_turn_rounds: 1,
            ..TurnLoopConfig::default()
        };

        let outcome = run_turn_loop(&mut ctx, None, &config)
            .await
            .expect("runs loop");
        assert_eq!(
            fixture.provider.requests().len(),
            1,
            "the budget bounds the physical calls of the turn"
        );
        let success = outcome.into_success().expect("the dialogue still answers");
        let text = success
            .response
            .commands()
            .iter()
            .find_map(|command| match command {
                LlmCommand::Text(command) => Some(command.content.clone()),
                _ => None,
            })
            .expect("the answer travels upward");
        assert!(text.contains("Let me list the tree"));
        assert!(text.contains("Round Budget Reached"));
    }

    /// A contour that advertises no edit branch answers in prose: the edit path is
    /// never entered, because the narrowed schema cannot produce a change set.
    #[tokio::test]
    async fn a_contour_without_edits_answers_in_text() {
        let mut fixture = Fixture::new(&[], vec![MockResponse::text("Nothing to change.")]);
        let mut ctx = fixture.context();
        let narrowed = CapabilityTree::root(
            RESEARCH_CAPABILITIES
                .iter()
                .copied()
                .chain([LlmCapability::TextMessage])
                .map(CapabilityTree::leaf),
        );
        let mut ctx = ctx.reborrow_with_capabilities(narrowed);

        let outcome = run_turn_loop(&mut ctx, None, &TurnLoopConfig::default())
            .await
            .expect("runs loop");
        let success = outcome.into_success().expect("success");
        assert!(success.value.modified_paths.is_empty());
        assert!(success.response.has(LlmCapability::TextMessage));
    }
}
