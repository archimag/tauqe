//! Autonomous bounded turn control loop and submodules.
//!
//! [`run_turn_loop`] is the single execution loop of a turn. The model alternates
//! between discovery, virtual staging, application, and verification inside one
//! bounded turn, and every round spends one call of the shared round budget: a
//! discovery round, a patch retry, a verification healing round, and the corrective
//! round of a degenerate answer all draw from the same pool. No phase owns a nested
//! loop of its own, so a turn is never terminated by the exhaustion of a local retry
//! counter instead of by the budget the developer configured.

use crate::config::project::ProjectConfig;

pub mod edit;
pub mod research;
pub mod staging;
#[cfg(test)]
mod step_outcome_tests;

use crate::edits::VerificationVerdict;
use crate::llm::CapabilityTree;
use crate::llm::LlmCapability;
use crate::workflow::turn::TurnPhase;

use super::context::LoopContext;
use super::outcome::{FailureReport, LoopResult, LoopSuccess, ResponseEnvelope, SystemError};
use crate::git::state::snapshot_worktree;
use crate::providers::types::TurnRequestReason;
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
    pub rounds_spent: usize,
    /// Remaining response envelope with companion capabilities.
    envelope: ResponseEnvelope,
}

impl TurnArtifact {
    /// Reports a turn that closed without touching the working tree.
    pub fn without_change_set(rounds_spent: usize, envelope: ResponseEnvelope) -> Self {
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
pub(super) fn completed_without_change_set(
    rounds_spent: usize,
    envelope: ResponseEnvelope,
) -> LoopResult<TurnArtifact> {
    LoopResult::Success(LoopSuccess::new(
        TurnArtifact::without_change_set(rounds_spent, envelope.clone()),
        envelope,
    ))
}

/// Announces deterministic work without spending a model round.
fn announce_turn_phase(ctx: &LoopContext<'_>, phase: TurnPhase) {
    tracing::info!(
        repo_root = %ctx.repo_root.display(),
        phase = %phase,
        "bounded turn entered a phase"
    );
    ctx.events.emit(StreamEvent::TurnPhase {
        phase,
        round: None,
        max_rounds: None,
        detail: None,
    });
}

/// Whether a text-only answer can complete the work requested by the caller.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TurnCompletion {
    AnswerAllowed,
    ActionRequired,
}

fn has_edits(envelope: &ResponseEnvelope) -> bool {
    EDIT_CAPABILITIES
        .iter()
        .any(|capability| envelope.has(*capability))
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
    completion: TurnCompletion,
) -> Result<LoopResult<TurnArtifact>, SystemError> {
    ctx.check_cancelled()?;
    ctx.set_turn_round_limit(Some(config.max_turn_rounds));

    let mut transaction = TurnTransaction::new(ctx.repo_root);
    let outcome =
        run_turn_loop_inner(ctx, initial_envelope, config, completion, &mut transaction).await;
    // Every exit that did not settle the transaction unwinds it here, so a
    // cancellation, a healing round whose request failed, an exhausted budget, and a
    // change set the substrate refused all leave the repository at its pre-turn
    // revision.
    transaction.rollback(ctx).await;
    outcome
}

async fn run_turn_loop_inner(
    ctx: &mut LoopContext<'_>,
    initial_envelope: Option<ResponseEnvelope>,
    config: &TurnLoopConfig,
    completion: TurnCompletion,
    transaction: &mut TurnTransaction,
) -> Result<LoopResult<TurnArtifact>, SystemError> {
    let mut envelope = match initial_envelope {
        Some(envelope) => {
            // A caller can provide the opening answer without replaying its
            // request here. Capture that context before its edits reach disk.
            let items = ctx.context_manager.get_state().items;
            let files = ctx
                .prompt_builder
                .input_cache
                .context_files(ctx.repo_root, &items)
                .await
                .map_err(|error| SystemError::Other(error.to_string()))?;
            ctx.transcript().remember_files(&files);
            envelope
        }
        None => ctx.request_round().await?,
    };

    let mut staging = StagingState::default();
    let mut edit: Option<EditState> = None;
    let mut declared_summary: Option<String> = None;
    let mut pending_edit_envelope = None;
    let mut healing_feedback = String::new();
    let mut repair_prompt = String::new();
    let mut healing_rounds = 0usize;
    let mut satisfied_rounds = 0usize;
    let mut empty_rounds = 0usize;

    loop {
        ctx.check_cancelled()?;

        // 1. Discovery: the round the model produced may widen the context it reads.
        let repairing = !staging.failed.is_empty();
        let discovery =
            execute_research_step(ctx, &mut envelope, &config.research, repairing).await?;
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
        if has_edits(&envelope) {
            announce_turn_phase(ctx, TurnPhase::Staging);
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
            let attempt = execute_staging_step(ctx, &mut envelope, &mut staging);
            if !attempt.addressed {
                return Ok(LoopResult::Diverged(envelope));
            }
            if !staging.failed.is_empty() {
                for (path, reason) in &attempt.refusals {
                    tracing::warn!(
                        repo_root = %ctx.repo_root.display(),
                        attempt = staging.attempts,
                        path = path.as_str(),
                        reason = reason.as_str(),
                        "patch retry requested"
                    );
                    if path.is_empty() {
                        ctx.events.emit(StreamEvent::TurnPhase {
                            phase: TurnPhase::Staging,
                            round: None,
                            max_rounds: None,
                            detail: Some(reason.clone()),
                        });
                        continue;
                    }
                    let display_reason = match staging.failed.get(path) {
                        Some(crate::edits::EditError::PatchContext { hunk, count: 0, .. }) => {
                            format!("Context for hunk {hunk} did not match")
                        }
                        Some(crate::edits::EditError::PatchContext { hunk, count, .. }) => {
                            format!("Context for hunk {hunk} matched {count} locations")
                        }
                        Some(crate::edits::EditError::InvalidPatchSection {
                            line, reason, ..
                        }) => {
                            format!("Patch line {line}: {reason}")
                        }
                        _ => reason.clone(),
                    };
                    ctx.events.emit(StreamEvent::EditFileRetrying {
                        path: path.clone(),
                        attempt: staging.attempts,
                        max_retries: 0,
                        reason: display_reason,
                    });
                }
                satisfied_rounds = 0;
                empty_rounds = 0;
                repair_prompt = build_patch_retry_prompt(
                    &staging.succeeded,
                    &staging.failed,
                    &[],
                    ctx.prompt_builder.patch_strategy,
                );
                if !healing_feedback.is_empty() {
                    repair_prompt.push_str("\n\n");
                    repair_prompt.push_str(&healing_feedback);
                }
                if let Some(patch) = staging.refused_patch.take() {
                    repair_prompt
                        .push_str("\n\nRejected patch (line numbers refer to this document):\n");
                    repair_prompt.push_str(&patch);
                }
                pending_edit_envelope.get_or_insert(envelope);
                envelope = ctx
                    .request_repair(&repair_prompt, staging.staged_state.as_ref())
                    .await?;
                continue;
            }
            if let Some(original) = pending_edit_envelope.take() {
                envelope = original;
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
            announce_turn_phase(ctx, TurnPhase::Verification);

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
                    healing_feedback = prepare_healing(ctx, &report, &mut healing_rounds).await;
                    staging = StagingState::default();
                    envelope = ctx
                        .request_healing(&healing_feedback, TurnRequestReason::PostHookRepair)
                        .await?;
                    continue;
                }
                EditStepVerdict::VerificationFailed { report } => {
                    healing_feedback = prepare_healing(ctx, &report, &mut healing_rounds).await;
                    // The healing round re-enters the loop, so the model may widen the
                    // context before it proposes the corrected change set.
                    staging = StagingState::default();
                    envelope = ctx
                        .request_healing(&healing_feedback, TurnRequestReason::VerificationRepair)
                        .await?;
                    continue;
                }
            }
        }

        if repairing {
            if discovery.kind == ResearchStepKind::Idle || holds_a_higher_contour_command(&envelope)
            {
                return Ok(LoopResult::Diverged(envelope));
            }
            let feedback = ctx.journal.render_round_feedback();
            if !request_next_round {
                return Ok(LoopResult::Exhausted(FailureReport::new(
                    "Patch repair research made no further progress".to_string(),
                    staging.attempts,
                    render_failures(&staging.failed, ctx.prompt_builder.patch_strategy)
                        .into_iter()
                        .map(|(_, reason)| reason)
                        .collect(),
                )));
            }
            envelope = ctx
                .request_repair(
                    &format!("{feedback}\n\n{repair_prompt}"),
                    staging.staged_state.as_ref(),
                )
                .await?;
            continue;
        }

        if request_next_round {
            // The findings of the round are the feedback of the round that follows.
            if healing_feedback.is_empty() {
                ctx.publish_round_feedback();
            } else {
                let findings = ctx.journal.render_round_feedback();
                ctx.transcript()
                    .set_pending_feedback(format!("{findings}\n\n{healing_feedback}"));
            }
            envelope = ctx.request_round().await?;
            continue;
        }

        // A round that declared how the task ends, or a dialogue that answered in
        // prose, concludes the turn without touching the working tree.
        if declares_terminal(&envelope)
            || (completion == TurnCompletion::AnswerAllowed && envelope_has_text(&envelope))
        {
            return Ok(completed_without_change_set(ctx.rounds_spent(), envelope));
        }

        return Ok(LoopResult::Diverged(envelope));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;
    use crate::context::ContextManager;
    use crate::control::context::{CancellationToken, EventSink};
    use crate::llm::CapabilityTree;
    use crate::llm::LlmCapability;
    use crate::llm::LlmCommand;
    use crate::models::ModelRef;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::prompt::TurnBrief;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::workflow::research::journal::TurnResearchJournal;

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
                protocol: JsonPromptProtocol::new(turn_capability_expr()),
                cache: crate::control::cache::TurnInputCache::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
                policy: crate::workflow::git_policy::GitTransactionPolicy::new(),
            }
        }

        fn context(&mut self) -> LoopContext<'_> {
            self.context_with_brief(None)
        }

        fn context_with_brief(&mut self, brief: Option<TurnBrief>) -> LoopContext<'_> {
            let caps = turn_capability_expr();
            let prompt_builder = crate::control::prompt::PromptBuilder::new(
                &self.provider,
                &self.model,
                &self.protocol,
                &mut self.cache,
            )
            .with_turn_brief(brief)
            .with_patch_strategy(crate::workflow::PatchStrategy::LineAnchor);
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

    #[tokio::test]
    async fn apply_patch_retries_atomically_and_previews_only_accepted_edits() {
        let invalid = "*** Begin Patch\n*** Update File: hello.rs\n@@\n-original\n+refused\n*** Update File: hidden.rs\n@@\n-hidden\n+refused\n*** End Patch";
        let context_only =
            "*** Begin Patch\n*** Update File: ./hello.rs\n@@\n original\n*** End Patch";
        let valid =
            "*** Begin Patch\n*** Update File: hello.rs\n@@\n-original\n+accepted\n*** End Patch";
        let response = |patch: &str| {
            serde_json::json!({
            "message": "Update hello",
            "action": {"kind": "edit", "patch": patch, "commit_message": "Update hello", "reason": ""}
        }).to_string()
        };
        let mut fixture = Fixture::new(
            &[("hello.rs", "original\n"), ("hidden.rs", "hidden\n")],
            vec![
                MockResponse::text(response(invalid)),
                MockResponse::text(response(context_only)),
                MockResponse::text(response(valid)),
            ],
        );
        fixture.context.remove_file("hidden.rs").unwrap();
        let (events, mut rx) = EventSink::channel();
        fixture.events = events;
        let mut ctx = fixture.context();
        ctx.prompt_builder.patch_strategy = crate::workflow::PatchStrategy::ApplyPatch;
        let outcome = run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
        .await
        .unwrap();
        assert!(outcome.is_success());
        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[1]
            .messages
            .last()
            .unwrap()
            .content
            .ends_with(invalid));
        assert!(requests[2]
            .messages
            .last()
            .unwrap()
            .content
            .ends_with(context_only));
        assert!(!requests[2]
            .messages
            .last()
            .unwrap()
            .content
            .contains(invalid));
        let schema = serde_json::to_value(&requests[0].response_format).unwrap();
        let edit_branch = schema
            .pointer("/json_schema/schema/properties/action/anyOf")
            .and_then(|branches| {
                branches
                    .as_array()?
                    .iter()
                    .find(|branch| branch["properties"]["kind"]["enum"][0] == "edit")
            })
            .unwrap();
        assert_eq!(edit_branch["properties"]["patch"]["type"], "string");
        assert!(edit_branch["properties"].get("changes").is_none());
        assert!(requests[1]
            .messages
            .last()
            .unwrap()
            .content
            .contains("Patch staging failed"));
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("hello.rs")).unwrap(),
            "accepted\n"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("hidden.rs")).unwrap(),
            "hidden\n"
        );
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::EditFileRetrying { path, reason, .. }
                if path == "hello.rs" && reason.contains("context-only hunk")
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            StreamEvent::EditFileRetrying { path, .. } if path == "patch"
        )));
        let hunks: Vec<_> = events
            .into_iter()
            .filter_map(|event| match event {
                StreamEvent::EditHunk { new_text, .. } => Some(new_text),
                _ => None,
            })
            .collect();
        assert!(!hunks.is_empty());
        assert!(hunks.iter().all(|text| text.contains("accepted")));
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

        let outcome = run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
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

        let outcome = run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
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
        let mut fixture = Fixture::new(
            &[("hello.rs", "fn hello() {}\n")],
            vec![MockResponse::text(change_set(
                "fn hello() {}\n",
                "fn hello() -> i32 { 1 }\n",
            ))],
        );
        let captured = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(captured.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        let mut ctx = fixture.context();
        let outcome = run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
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
            logged.contains("staging"),
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

        let outcome = run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
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
    }

    /// A file delivered by a discovery round is appended to the next round.
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
        let outcome = run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
        .await
        .expect("runs loop");
        assert!(outcome.is_success());

        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 2, "discovery round followed by conclusion");
        assert!(requests[1].messages.starts_with(&requests[0].messages));
        assert_eq!(requests[1].response_format, requests[0].response_format);
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .map(|m| m.content.matches("pub const ANSWER: i32 = 42;").count())
                .sum::<usize>(),
            1
        );
        assert!(requests[1]
            .messages
            .last()
            .unwrap()
            .content
            .contains("pub const ANSWER: i32 = 42;"));
    }
    /// A refusal retains discovered material for the scenario after recovery.
    #[tokio::test]
    async fn researched_context_survives_a_blocked_step() {
        let discovery = serde_json::json!({
            "message": "Read the helper",
            "action": {"kind": "research", "context_requests": [
                {"path": "helper.rs", "access": "read_only"}
            ]}
        });
        let refusal = serde_json::json!({
            "message": "Cannot proceed",
            "action": {"kind": "blocked", "blocked": {
                "reason": "missing prerequisite", "details": "", "is_blocked": true
            }}
        });
        let mut fixture = Fixture::new(
            &[("helper.rs", "pub const HELPER: i32 = 1;\n")],
            vec![
                MockResponse::text(discovery.to_string()),
                MockResponse::text(refusal.to_string()),
            ],
        );
        fixture.context.remove_file("helper.rs").unwrap();
        let result =
            crate::control::step::run_step_turn(&mut fixture.context(), &TurnLoopConfig::default())
                .await
                .unwrap();
        assert!(matches!(result, LoopResult::Diverged(_)));
        assert!(fixture.context.contains("helper.rs").unwrap());
        assert_eq!(fixture.provider.requests().len(), 2);
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

        let outcome = run_turn_loop(&mut ctx, None, &config, TurnCompletion::AnswerAllowed)
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
        assert!(requests[1].messages.starts_with(&requests[0].messages));
        assert_eq!(requests[1].response_format, requests[0].response_format);
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

    #[tokio::test]
    async fn healing_restores_the_dirty_original_from_formatted_bytes() {
        let config = r#"
[models]
repl = ["openrouter:a/b"]
[project.format]
command = "sed"
args = ["s/broken/formatted/"]
[project.verify]
command = "grep"
args = ["-qx", "developer work", "hello.rs"]
"#;
        let original = "developer work\n";
        let mut fixture = Fixture::new(
            &[("hello.rs", "committed\n"), ("tauqe.toml", config)],
            vec![
                MockResponse::text(change_set(original, "broken\n")),
                MockResponse::text(change_set("formatted\n", original)),
            ],
        );
        let path = fixture.dir.path().join("hello.rs");
        std::fs::write(&path, original).unwrap();
        let outcome = run_turn_loop(
            &mut fixture.context(),
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
        .await
        .unwrap();
        assert!(outcome.is_success());
        assert_eq!(std::fs::read(path).unwrap(), original.as_bytes());
        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 2);
        let baseline = requests[0]
            .messages
            .iter()
            .find(|m| m.content.starts_with("<context>"))
            .unwrap();
        assert!(baseline.content.contains(original));
        assert!(requests[1].messages.contains(baseline));
        let latest = &requests[1].messages.last().unwrap().content;
        assert!(latest.contains("Code verification failed"));
        assert!(latest.contains("formatted\n"));
        assert!(latest.contains("<current_versions>"));
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Work {
        Call(usize, Option<TurnRequestReason>),
        Phase(TurnPhase),
    }

    fn observed_work(rx: &mut tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) -> Vec<Work> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                StreamEvent::TurnRoundStarted { round, reason, .. } => {
                    Some(Work::Call(round, reason))
                }
                StreamEvent::TurnPhase {
                    phase,
                    round,
                    max_rounds,
                    ..
                } => {
                    assert_eq!((round, max_rounds), (None, None));
                    Some(Work::Phase(phase))
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn verification_repair_can_research_before_staging_and_verifying_again() {
        let config = r#"
[models]
repl = ["openrouter:a/b"]
[project.format]
command = "sed"
args = ["s/broken/formatted/"]
[project.verify]
command = "grep"
args = ["-qx", "developer work", "hello.rs"]
"#;
        let original = "developer work\n";
        let research = serde_json::json!({
            "message": "Read the helper first.",
            "action": {
                "kind": "research",
                "context_requests": [{ "path": "helper.rs", "access": "read_only" }]
            }
        });
        let mut fixture = Fixture::new(
            &[("hello.rs", "committed\n"), ("tauqe.toml", config)],
            vec![
                MockResponse::text(change_set(original, "broken\n")),
                MockResponse::text(research.to_string()),
                MockResponse::text(change_set("formatted\n", original)),
            ],
        );
        let root = fixture.dir.path();
        std::fs::write(root.join("helper.rs"), "pub const HELPER: i32 = 1;\n").unwrap();
        crate::git::cmd::stage_files(root, &["helper.rs".to_string()]).unwrap();
        crate::git::cmd::commit_with_developer_author(root, "add helper", false).unwrap();
        std::fs::write(root.join("hello.rs"), original).unwrap();
        let (events, mut rx) = EventSink::channel();
        fixture.events = events;

        let outcome = run_turn_loop(
            &mut fixture.context(),
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
        .await;
        let work = observed_work(&mut rx);
        let success = outcome
            .unwrap_or_else(|error| panic!("turn failed after {work:?}: {error:?}"))
            .into_success()
            .expect("the corrected change set passed verification");
        assert_eq!(
            success.value.verification,
            Some(VerificationVerdict::Passed)
        );
        assert_eq!(
            work,
            vec![
                Work::Call(1, None),
                Work::Phase(TurnPhase::Staging),
                Work::Phase(TurnPhase::Verification),
                Work::Call(2, Some(TurnRequestReason::VerificationRepair)),
                Work::Phase(TurnPhase::Research),
                Work::Call(3, None),
                Work::Phase(TurnPhase::Staging),
                Work::Phase(TurnPhase::Verification),
            ]
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
        let (events, mut rx) = EventSink::channel();
        fixture.events = events;
        let mut ctx = fixture.context();

        let outcome = run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
        .await
        .expect("runs loop");
        assert!(outcome.is_success(), "the repaired change set must commit");
        assert_eq!(
            observed_work(&mut rx),
            vec![
                Work::Call(1, None),
                Work::Phase(TurnPhase::Staging),
                Work::Call(2, Some(TurnRequestReason::PatchRepair)),
                Work::Phase(TurnPhase::Staging),
                Work::Phase(TurnPhase::Verification),
            ]
        );
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("hello.rs")).unwrap(),
            "fn hello() -> i32 { 1 }\n"
        );
    }

    #[tokio::test]
    async fn verification_and_refused_patch_remain_available_across_research() {
        let config =
            "[models]\nrepl = [\"openrouter:a/b\"]\n[project.verify]\ncommand = \"grep\"\nargs = [\"-qx\", \"accepted\", \"hello.rs\"]\n";
        let patch = |body: &str| {
            serde_json::json!({
                "message": "Update file",
                "action": {"kind": "edit", "patch": body, "commit_message": "Update", "reason": ""}
            })
            .to_string()
        };
        let rejected = "*** Begin Patch\n*** Update File: hello.rs\n@@\n missing\n*** End Patch";
        let mut fixture = Fixture::new(
            &[("hello.rs", "section\noriginal\n"), ("tauqe.toml", config)],
            vec![
                MockResponse::text(patch("*** Begin Patch\n*** Update File: hello.rs\n@@\n-original\n+broken\n*** End Patch")),
                MockResponse::text(reread_files(&["hello.rs"])),
                MockResponse::text(patch(rejected)),
                MockResponse::text(reread_files(&["hello.rs"])),
                MockResponse::text(patch("*** Begin Patch\n*** Update File: hello.rs\n@@ section\n@@\n-broken\n+accepted\n*** End Patch")),
            ],
        );
        let mut ctx = fixture.context();
        ctx.prompt_builder.patch_strategy = crate::workflow::PatchStrategy::ApplyPatch;
        assert!(run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed
        )
        .await
        .unwrap()
        .is_success());
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("hello.rs")).unwrap(),
            "section\naccepted\n"
        );
        let requests = fixture.provider.requests();
        assert_eq!(requests.len(), 5);
        for request in &requests[2..] {
            assert!(request
                .messages
                .last()
                .unwrap()
                .content
                .contains("Code verification failed"));
        }
        for request in &requests[3..] {
            assert!(request.messages.last().unwrap().content.contains(rejected));
        }
    }

    fn reread_files(paths: &[&str]) -> String {
        let requests: Vec<_> = paths
            .iter()
            .map(|path| serde_json::json!({"path": path, "access": "editable"}))
            .collect();
        serde_json::json!({"message":"Checking anchors", "action":{"kind":"research", "context_requests": requests}}).to_string()
    }

    #[tokio::test]
    async fn repair_rereads_current_anchors_and_preserves_partial_staging() {
        let anchor = crate::edits::anchor::encode_anchor("context.rs", 0);
        let other = crate::edits::anchor::encode_anchor("other.rs", 0);
        let edits = |changes: serde_json::Value| {
            serde_json::json!({
            "message":"Edit files", "action":{"kind":"edit", "changes":changes, "commit_message":"Update both files"}
        }).to_string()
        };
        let replace = |path: &str, token: &str, content: &str| {
            serde_json::json!({
                "op":"replace", "path":path, "to":"", "start":token, "end":token, "content":content
            })
        };
        let pending = edits(serde_json::json!([
            replace("context.rs", "RWfQ", "new context"),
            replace("other.rs", &other, "staged other")
        ]));
        let corrected = edits(serde_json::json!([replace(
            "context.rs",
            &anchor,
            "new context"
        )]));
        for step in [false, true] {
            let capabilities = if step {
                turn_capability_expr()
            } else {
                crate::control::repl::repl_capability_expr(None)
            };
            let mut fixture = Fixture::new(
                &[
                    ("context.rs", "original context\n"),
                    ("other.rs", "original other\n"),
                    ("helper.rs", "newly discovered helper\n"),
                ],
                vec![
                    MockResponse::text(pending.clone()),
                    MockResponse::text(reread_files(&["context.rs", "other.rs", "helper.rs"])),
                    MockResponse::text(reread_files(&["context.rs", "other.rs"])),
                    MockResponse::text(corrected.clone()),
                ],
            );
            fixture.protocol = JsonPromptProtocol::new(capabilities.clone());
            fixture.context.remove_file("helper.rs").unwrap();
            let mut config = crate::config::AppConfig::default();
            config.repl.patch_strategy = crate::workflow::PatchStrategy::LineAnchor;
            let turn = crate::control::execution::TurnExecutionConfig {
                repo_root: fixture.dir.path(),
                provider: &fixture.provider,
                model: &fixture.model,
                protocol: &fixture.protocol,
                config: &config,
                session_id: None,
                reasoning_effort: None,
            };
            let prompt = "Update both files".to_owned();
            if step {
                let result = crate::control::step::run_step_execution(
                    &mut fixture.context,
                    &mut fixture.policy,
                    &fixture.cancel,
                    &fixture.events,
                    turn,
                    prompt,
                )
                .await
                .unwrap();
                assert!(result.is_success());
            } else {
                let mut unit = crate::control::execution::ExecutionUnit::default();
                let result = unit
                    .execute_bounded(
                        turn,
                        crate::control::execution::BoundedUnitRequest {
                            context: &mut fixture.context,
                            history: None,
                            capabilities,
                            task: prompt,
                            completion: TurnCompletion::AnswerAllowed,
                            brief: None,
                        },
                        &TurnLoopConfig::from_app_config(&config),
                        &mut fixture.policy,
                        &fixture.cancel,
                        &fixture.events,
                    )
                    .await
                    .unwrap();
                assert!(result.is_success());
            }
            let requests = fixture.provider.requests();
            assert_eq!(requests.len(), 4);
            for pair in requests.windows(2) {
                assert!(pair[1].messages.starts_with(&requests[0].messages));
                assert!(!pair[1]
                    .messages
                    .iter()
                    .any(|message| message.role == "assistant"));
                assert_eq!(pair[1].response_format, pair[0].response_format);
            }
            assert!(requests[2]
                .messages
                .iter()
                .any(|message| message.content.contains("newly discovered helper")));
            assert!(!requests[3]
                .messages
                .last()
                .unwrap()
                .content
                .contains("newly discovered helper"));
            {
                let before = &requests[2].messages;
                let after = &requests[3].messages;
                assert_eq!(&before[..before.len() - 1], &after[..after.len() - 1]);
                assert_eq!(
                    requests[3]
                        .messages
                        .iter()
                        .filter(|message| message.content.contains("staged other"))
                        .count(),
                    1
                );
            }
            for request in &requests[2..] {
                let feedback = &request.messages.last().unwrap().content;
                let original = format!("{anchor} | original context");
                assert!(request.messages[..request.messages.len() - 1]
                    .iter()
                    .any(|message| message.content.contains(&original)));
                assert!(!feedback.contains(&original));
                assert!(feedback.contains(&format!("{other} | staged other")));
                assert!(feedback.contains("RWfQ"));
                assert!(!feedback.contains("Do not request those paths again"));
            }
            for (path, expected) in [("context.rs", "new context"), ("other.rs", "staged other")] {
                assert_eq!(
                    std::fs::read_to_string(fixture.dir.path().join(path))
                        .unwrap()
                        .trim_end(),
                    expected
                );
            }
        }
    }

    #[tokio::test]
    async fn repeated_repair_rereads_stop_before_the_shared_budget() {
        let mut fixture = Fixture::new(
            &[("hello.rs", "original\n")],
            vec![
                MockResponse::text(change_set("missing", "changed")),
                MockResponse::text(reread_files(&["hello.rs"])),
                MockResponse::text(reread_files(&["hello.rs"])),
            ],
        );
        let mut ctx = fixture.context_with_brief(Some(TurnBrief::step()));
        let config = TurnLoopConfig {
            max_turn_rounds: 8,
            ..Default::default()
        };
        assert!(crate::control::step::run_step_turn(&mut ctx, &config)
            .await
            .unwrap()
            .is_exhausted());
        assert_eq!(fixture.provider.requests().len(), 3);
        assert_eq!(
            std::fs::read_to_string(fixture.dir.path().join("hello.rs")).unwrap(),
            "original\n"
        );
    }

    #[tokio::test]
    async fn exhaustion_after_failed_verification_restores_the_dirty_worktree() {
        let verify = "[project.verify]\ncommand = \"false\"\n";
        let mut fixture = Fixture::new(
            &[("hello.rs", "committed\n"), ("tauqe.toml", verify)],
            vec![MockResponse::text(change_set(
                "developer work\n",
                "broken\n",
            ))],
        );
        let file = fixture.dir.path().join("hello.rs");
        std::fs::write(&file, "developer work\n").unwrap();
        let config = TurnLoopConfig {
            max_turn_rounds: 1,
            ..Default::default()
        };
        let result = crate::control::step::run_step_turn(&mut fixture.context(), &config)
            .await
            .unwrap();
        let LoopResult::Exhausted(report) = result else {
            panic!("expected step exhaustion")
        };
        assert_eq!(report.attempts, 1);
        assert_eq!(report.reason, "Turn round budget exhausted");
        assert_eq!(fixture.provider.requests().len(), 1);
        assert_eq!(std::fs::read_to_string(file).unwrap(), "developer work\n");
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
        let mut ctx = fixture.context_with_brief(Some(TurnBrief::step()));
        let config = TurnLoopConfig {
            max_turn_rounds: 1,
            ..TurnLoopConfig::default()
        };

        let outcome = crate::control::step::run_step_turn(&mut ctx, &config)
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
        let mut config = crate::config::AppConfig::default();
        config.repl.max_turn_rounds = 1;
        let outcome = run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::from_app_config(&config),
            TurnCompletion::AnswerAllowed,
        )
        .await
        .expect_err("the technical turn reports budget exhaustion to its caller");
        assert_eq!(
            fixture.provider.requests().len(),
            1,
            "the budget bounds the physical calls of the turn"
        );
        assert!(matches!(
            outcome,
            SystemError::RoundBudgetExhausted { spent: 1, .. }
        ));
    }

    /// A contour that advertises no edit branch answers in prose: the edit path is
    /// never entered, because the narrowed schema cannot produce a change set.
    #[tokio::test]
    async fn a_contour_without_edits_answers_in_text() {
        let mut fixture = Fixture::new(&[], vec![MockResponse::text("Nothing to change.")]);
        let narrowed = CapabilityTree::root(
            RESEARCH_CAPABILITIES
                .iter()
                .copied()
                .chain([LlmCapability::TextMessage])
                .map(CapabilityTree::leaf),
        );
        fixture.protocol = JsonPromptProtocol::new(narrowed.clone());
        let mut ctx = fixture.context();
        let mut ctx = ctx.reborrow_with_capabilities(narrowed);

        let outcome = run_turn_loop(
            &mut ctx,
            None,
            &TurnLoopConfig::default(),
            TurnCompletion::AnswerAllowed,
        )
        .await
        .expect("runs loop");
        let success = outcome.into_success().expect("success");
        assert!(success.value.modified_paths.is_empty());
        assert!(success.response.has(LlmCapability::TextMessage));
    }
}
