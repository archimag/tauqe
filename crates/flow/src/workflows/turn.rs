//! Technical turn transitions. Document interpretation belongs to the caller.
//!
//! Disk operations belong to a supervised worker, not to the round task.

use super::round::{RoundError, RoundInput, RoundWorkflow};
use super::state::TurnState;
use std::path::PathBuf;
use std::sync::Arc;
use tauqe_core::config::project::ProjectConfig;
use tauqe_core::context::{ContextAccess, ContextManager};
use tauqe_core::edits::execution::{
    declared_commit_message, diagnostic_file_candidates, execute_edit_step, EditEnvironment,
    EditState, EditStepVerdict, EDIT_CAPABILITIES,
};
use tauqe_core::edits::staging::{
    execute_staging_step, render_failures, StagingEnvironment, StagingState,
};
use tauqe_core::edits::transaction::{RollbackOutcome, TurnTransaction};
use tauqe_core::edits::{EditOutcome, VerificationVerdict};
use tauqe_core::execution::transcript::AcceptedRoundId;
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, PromptOutcome, ResponseEnvelope,
};
use tauqe_core::llm::LlmCapability;
use tauqe_core::providers::{StreamEvent, TurnRequestReason};
use tauqe_core::workflow::policy::ApplyPolicy;
use tauqe_core::workflow::research::step::{
    execute_research_step, ResearchEnvironment, ResearchLoopConfig, ResearchStepKind,
    RESEARCH_CAPABILITIES,
};
use tauqe_core::workflow::turn::TurnPhase;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

// Preserve the existing correction policy; all requests also spend TurnState's
// shared budget, including requests following an empty response.
const MAX_EMPTY_RETRIES: usize = 2;
const MAX_SATISFIED_ROUNDS: usize = 1;
const MAX_DEAD_END_ROUNDS: usize = 1;
const MAX_HEALING_AUTO_FILES: usize = 4;
const EMPTY_REMINDER: &str = "Your previous reply produced no actionable result. Provide a concrete result that conforms to the active response schema.";
const DEAD_END_DIRECTIVE: &str = "<response_directive>Discovery did not widen the context. Do not repeat unresolved requests. Use exact repository paths for other material, or conclude using a terminal branch of the active response schema.</response_directive>";
const SATISFIED_DIRECTIVE: &str = "<response_directive>Every requested file is already in context. Do not request those paths again. Use the material already supplied and respond through the active response schema.</response_directive>";

/// Technical facts, not a declaration that a document operation succeeded.
/// In particular, commit_error means verified bytes were not finalized; the
/// shared transaction deliberately does not claim to roll them back.
#[derive(Debug)]
pub(crate) struct TurnArtifact {
    pub response: ResponseEnvelope,
    pub response_round: Option<AcceptedRoundId>,
    pub edit: Option<EditOutcome>,
    pub rounds_spent: usize,
}

#[derive(Debug)]
pub(crate) enum TurnOutcome {
    Settled(TurnArtifact),
    Exhausted { reason: String },
    ProtocolDefect(ResponseEnvelope),
}

pub(crate) struct TurnSettlement {
    pub outcome: Result<TurnOutcome, RoundError>,
    pub rollback: Arc<RollbackOutcome>,
    pub cancelled: bool,
}

#[derive(Debug)]
pub(crate) struct DiskSettlement(DiskOutcome);

#[derive(Debug)]
enum DiskOutcome {
    RolledBack(Arc<RollbackOutcome>),
    Finalized(EditOutcome),
    WorkerFailed(String),
}

impl DiskSettlement {
    pub(crate) fn worker_failed(&self) -> bool {
        matches!(&self.0, DiskOutcome::WorkerFailed(_))
    }

    pub(crate) fn finalized_edit(&self) -> Option<EditOutcome> {
        let DiskOutcome::Finalized(edit) = &self.0 else {
            return None;
        };
        Some(edit.clone())
    }

    pub(crate) fn notice(&self) -> Option<String> {
        match &self.0 {
            DiskOutcome::WorkerFailed(error) => Some(error.clone()),
            DiskOutcome::RolledBack(outcome) => outcome.diagnostic(),
            DiskOutcome::Finalized(edit) => Some(match (&edit.error, &edit.commit_hash) {
                (Some(error), _) => error.clone(),
                (None, Some(hash)) => format!("The edit transaction finalized as commit {hash}."),
                (None, None) => "The edit transaction finalized without a Git commit.".into(),
            }),
        }
    }
}

struct DiskPass {
    project: ProjectConfig,
    staging: StagingState,
    context: ContextManager,
    cancel: CancellationToken,
    events: EventSink,
}

type PassResult = (ContextManager, Result<EditStepVerdict, ExecutionError>);

enum DiskCommand {
    Apply(Box<DiskPass>, oneshot::Sender<PassResult>),
    Finalize(
        String,
        VerificationVerdict,
        oneshot::Sender<Result<EditOutcome, ExecutionError>>,
    ),
    Settle(
        Box<ContextManager>,
        oneshot::Sender<(ContextManager, Arc<RollbackOutcome>)>,
    ),
}

pub(crate) struct TurnDisk(mpsc::Sender<DiskCommand>);

/// The supervisor holds the last sender until the round task has joined.
/// Closing the inbox drains admitted work and then settles the transaction.
pub(crate) struct DiskWorker {
    keep_open: mpsc::Sender<DiskCommand>,
    task: JoinHandle<DiskSettlement>,
}

impl DiskWorker {
    pub(crate) async fn finish(self) -> DiskSettlement {
        drop(self.keep_open);
        match self.task.await {
            Ok(settlement) => settlement,
            Err(error) => DiskSettlement(DiskOutcome::WorkerFailed(format!(
                "The turn disk worker did not complete: {error}"
            ))),
        }
    }
}

impl TurnDisk {
    pub(crate) fn start(root: PathBuf, policy: Box<dyn ApplyPolicy>) -> (Self, DiskWorker) {
        let (tx, rx) = mpsc::channel(1);
        let worker = DiskWorker {
            keep_open: tx.clone(),
            task: tokio::spawn(run_disk(root, policy, rx)),
        };
        (Self(tx), worker)
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> DiskCommand,
    ) -> Result<T, ExecutionError> {
        let (reply, result) = oneshot::channel();
        let stopped =
            || ExecutionError::Other("The turn disk worker stopped without a result".into());
        self.0.send(command(reply)).await.map_err(|_| stopped())?;
        result.await.map_err(|_| stopped())
    }
}

async fn run_disk(
    root: PathBuf,
    mut policy: Box<dyn ApplyPolicy>,
    mut commands: mpsc::Receiver<DiskCommand>,
) -> DiskSettlement {
    let mut transaction = TurnTransaction::new(&root);
    let mut edit = None;
    let mut last_context = None;
    let mut finalized = None;
    while let Some(command) = commands.recv().await {
        match command {
            DiskCommand::Apply(pass, reply) => {
                let DiskPass {
                    project,
                    staging,
                    mut context,
                    cancel,
                    events,
                } = *pass;
                let edit = match edit {
                    Some(ref mut state) => state,
                    None => {
                        let path = root.clone();
                        match tauqe_core::blocking::run_blocking_async(move || {
                            tauqe_core::git::state::snapshot_worktree(&path)
                        })
                        .await
                        {
                            Ok(baseline) => edit.insert(EditState::new(project, baseline)),
                            Err(error) => {
                                let _ = reply
                                    .send((context, Err(ExecutionError::Other(error.to_string()))));
                                continue;
                            }
                        }
                    }
                };
                edit.adopt_staging(&staging);
                let verdict = execute_edit_step(
                    &mut EditEnvironment {
                        repo_root: &root,
                        context_manager: &mut context,
                        policy: policy.as_mut(),
                        cancel: &cancel,
                        events: &events,
                    },
                    edit,
                    &mut transaction,
                )
                .await;
                last_context = Some(context.clone());
                let _ = reply.send((context, verdict));
            }
            DiskCommand::Finalize(summary, verification, reply) => {
                let Some(edit) = edit.as_ref() else {
                    let _ = reply.send(Err(ExecutionError::Other(
                        "Cannot finalize the edit transaction before a disk pass".into(),
                    )));
                    continue;
                };
                let commit = transaction
                    .finalize(policy.as_mut(), edit.modified_paths(), &summary)
                    .await;
                let outcome = EditOutcome {
                    applied: commit.commit_error.is_none(),
                    commit_hash: commit.commit_hash,
                    error: commit.commit_error,
                    changed_files: edit.modified_paths().to_vec(),
                    verification: Some(verification),
                };
                finalized = Some(outcome.clone());
                let _ = reply.send(Ok(outcome));
            }
            DiskCommand::Settle(context, reply) => {
                let mut context = *context;
                let (outcome, rollback) = match finalized {
                    Some(edit) => (
                        DiskOutcome::Finalized(edit),
                        Arc::new(RollbackOutcome::FinalizationAttempted),
                    ),
                    None => {
                        let rollback =
                            Arc::new(transaction.rollback(policy.as_mut(), &mut context).await);
                        (DiskOutcome::RolledBack(rollback.clone()), rollback)
                    }
                };
                let _ = reply.send((context, rollback));
                return DiskSettlement(outcome);
            }
        }
    }
    if let Some(edit) = finalized {
        return DiskSettlement(DiskOutcome::Finalized(edit));
    }
    let outcome = match last_context {
        Some(mut context) => transaction.rollback(policy.as_mut(), &mut context).await,
        None => RollbackOutcome::NothingToDo,
    };
    DiskSettlement(DiskOutcome::RolledBack(Arc::new(outcome)))
}

pub(crate) struct TurnWorkflow<'a> {
    pub round: RoundWorkflow<'a>,
    pub disk: &'a TurnDisk,
    pub research: &'a ResearchLoopConfig,
}

impl TurnWorkflow<'_> {
    pub async fn run(&mut self, state: &mut TurnState, input: RoundInput<'_>) -> TurnSettlement {
        let outcome = self.run_inner(state, &input).await;
        let rollback = match self
            .disk
            .request(|reply| DiskCommand::Settle(Box::new(state.context.clone()), reply))
            .await
        {
            Ok((context, rollback)) => {
                state.context = context;
                rollback
            }
            Err(error) => Arc::new(RollbackOutcome::Incomplete {
                policy_error: None,
                backup_error: error.to_string(),
            }),
        };
        TurnSettlement {
            outcome,
            rollback,
            cancelled: self.round.cancel.is_cancelled(),
        }
    }

    async fn run_inner(
        &mut self,
        state: &mut TurnState,
        input: &RoundInput<'_>,
    ) -> Result<TurnOutcome, RoundError> {
        let mut staging = StagingState::default();
        let mut summary = None;
        let mut pending_response = None;
        let mut repair = String::new();
        let mut healing = String::new();
        let mut feedback = input.feedback.unwrap_or_default().to_owned();
        let mut reason = input.reason;
        let mut empty_retries = 0;
        let mut satisfied = 0;
        let mut dead_ends = 0;
        let patch_strategy = self.round.executor.patch_strategy();

        loop {
            self.check_cancelled()?;
            let reply = self
                .round
                .run(
                    state,
                    RoundInput {
                        task: input.task,
                        repo_root: input.repo_root,
                        project: input.project,
                        repo_map: input.repo_map,
                        repo_dirs: input.repo_dirs,
                        outer_docs: input.outer_docs,
                        brief: input.brief,
                        feedback: Some(&feedback),
                        reason,
                        staged: staging.staged_state(),
                    },
                )
                .await?;
            let mut response_round = reply.accepted_round;
            let mut response = match reply.result.outcome {
                PromptOutcome::Completed(response) => response,
                PromptOutcome::ProtocolError(response) => {
                    return Ok(TurnOutcome::ProtocolDefect(response));
                }
                PromptOutcome::Empty => {
                    if empty_retries == MAX_EMPTY_RETRIES {
                        return Ok(TurnOutcome::Exhausted {
                            reason: format!("The model produced an empty response after {empty_retries} additional attempt(s)."),
                        });
                    }
                    empty_retries += 1;
                    // Earlier feedback is already in the transcript. Preserve
                    // it there, without duplicating it on each empty retry.
                    feedback = if empty_retries == 1 {
                        EMPTY_REMINDER.into()
                    } else {
                        String::new()
                    };
                    reason = Some(TurnRequestReason::EmptyResponseRetry);
                    continue;
                }
            };
            empty_retries = 0;
            feedback.clear();
            reason = None;
            let repairing = !staging.failures().is_empty();
            if RESEARCH_CAPABILITIES.iter().any(|cap| response.has(*cap)) {
                self.announce(TurnPhase::Research);
            }
            let discovery = execute_research_step(
                &mut ResearchEnvironment {
                    repo_root: input.repo_root,
                    context_manager: &mut state.context,
                    journal: &mut state.journal,
                    outer_docs: input.outer_docs,
                    cancel: self.round.cancel,
                    events: self.round.events,
                    round: state.budget.spent,
                },
                &mut response,
                self.research,
                repairing,
            )
            .await?;
            self.check_cancelled()?;
            let continue_research = match discovery.kind {
                ResearchStepKind::Idle => false,
                ResearchStepKind::Widened => {
                    satisfied = 0;
                    dead_ends = 0;
                    true
                }
                ResearchStepKind::AlreadySatisfied => {
                    satisfied += 1;
                    if !repairing {
                        state.journal.push_block(SATISFIED_DIRECTIVE);
                    }
                    satisfied <= MAX_SATISFIED_ROUNDS
                }
                ResearchStepKind::DeadEnd => {
                    dead_ends += 1;
                    state.journal.push_block(DEAD_END_DIRECTIVE);
                    dead_ends <= MAX_DEAD_END_ROUNDS
                }
            };
            if matches!(
                discovery.kind,
                ResearchStepKind::Widened | ResearchStepKind::AlreadySatisfied
            ) {
                self.round
                    .events
                    .emit_text_delta(format!("\n\n{}\n\n", discovery.summary()));
            }

            if EDIT_CAPABILITIES.iter().any(|cap| response.has(*cap)) {
                self.announce(TurnPhase::Staging);
                if summary.is_none() {
                    summary = declared_commit_message(&response);
                }
                let attempt = tauqe_core::blocking::run_blocking(|| {
                    execute_staging_step(
                        &mut StagingEnvironment {
                            repo_root: input.repo_root,
                            context_manager: &state.context,
                            patch_strategy,
                            events: self.round.events,
                        },
                        &mut response,
                        &mut staging,
                    )
                });
                state
                    .transcript
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remember_created_paths(&attempt.new_paths);
                self.check_cancelled()?;
                if !attempt.addressed {
                    return Ok(TurnOutcome::Exhausted {
                        reason: "The change set could not be staged".into(),
                    });
                }
                if !staging.failures().is_empty() {
                    for (path, diagnostic) in attempt.refusals {
                        self.round.events.emit(StreamEvent::EditFileRetrying {
                            path,
                            attempt: staging.attempts(),
                            max_retries: 0,
                            reason: diagnostic,
                        });
                    }
                    satisfied = 0;
                    dead_ends = 0;
                    repair = join_feedback(&staging.retry_prompt(patch_strategy), &healing);
                    if let Some(patch) = staging.take_refused_patch() {
                        repair.push_str(
                            "\n\nRejected patch (line numbers refer to this document):\n",
                        );
                        repair.push_str(&patch);
                    }
                    pending_response.get_or_insert((response, response_round));
                    feedback.clone_from(&repair);
                    reason = Some(TurnRequestReason::PatchRepair);
                    continue;
                }
                if let Some(original) = pending_response.take() {
                    (response, response_round) = original;
                }
                let abandoned = render_failures(staging.abandoned(), patch_strategy);
                if !abandoned.is_empty() {
                    let lines = abandoned
                        .into_iter()
                        .map(|(path, error)| format!("- `{path}`: {error}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.round.events.emit_text_delta(format!("\n\n[Change Set Incomplete] Unretried files remain outside the change set:\n{lines}\n\n"));
                }
                self.announce(TurnPhase::Verification);
                let pass = DiskPass {
                    project: input.project.clone(),
                    staging: std::mem::take(&mut staging),
                    context: state.context.clone(),
                    cancel: self.round.cancel.clone(),
                    events: self.round.events.clone(),
                };
                let (context, verdict) = self
                    .disk
                    .request(|reply| DiskCommand::Apply(Box::new(pass), reply))
                    .await?;
                state.context = context;
                // Cancellation takes precedence over an apply or hook failure.
                self.check_cancelled()?;
                match verdict? {
                    EditStepVerdict::Passed(verification) => {
                        let edit = self
                            .disk
                            .request(|reply| {
                                DiskCommand::Finalize(
                                    summary.unwrap_or_else(|| "Apply verified changes".into()),
                                    verification,
                                    reply,
                                )
                            })
                            .await??;
                        return Ok(TurnOutcome::Settled(TurnArtifact {
                            response,
                            response_round,
                            edit: Some(edit),
                            rounds_spent: state.budget.spent,
                        }));
                    }
                    EditStepVerdict::ApplyFailed { message } => {
                        return Err(ExecutionError::Other(format!(
                            "Failed to commit staged edits to disk: {message}"
                        ))
                        .into());
                    }
                    EditStepVerdict::HookFailed { report } => {
                        healing = self.prepare_healing(state, input, &report).await;
                        reason = Some(TurnRequestReason::PostHookRepair);
                    }
                    EditStepVerdict::VerificationFailed { report } => {
                        healing = self.prepare_healing(state, input, &report).await;
                        reason = Some(TurnRequestReason::VerificationRepair);
                    }
                }
                repair.clear();
                feedback.clone_from(&healing);
                continue;
            }

            // Research drained only its own commands and scratchpad. Anything
            // else operational belongs to the outer scenario, even in repair.
            let companion = response
                .remaining_capabilities()
                .into_iter()
                .any(|cap| cap != LlmCapability::TextMessage);
            if !companion && discovery.kind != ResearchStepKind::Idle {
                if continue_research {
                    feedback = join_feedback(
                        &state.journal.render_round_feedback(),
                        if repairing { &repair } else { &healing },
                    );
                    if repairing {
                        reason = Some(TurnRequestReason::PatchRepair);
                    }
                    continue;
                }
                if repairing {
                    return Ok(TurnOutcome::Exhausted {
                        reason: "Patch repair research made no further progress".into(),
                    });
                }
            }
            // A terminal answer, blocker or document operation may abandon
            // temporary edits. The outer run boundary restores them first.
            return Ok(TurnOutcome::Settled(TurnArtifact {
                response,
                response_round,
                edit: None,
                rounds_spent: state.budget.spent,
            }));
        }
    }

    fn check_cancelled(&self) -> Result<(), RoundError> {
        if self.round.cancel.is_cancelled() {
            Err(ExecutionError::Cancelled.into())
        } else {
            Ok(())
        }
    }

    fn announce(&self, phase: TurnPhase) {
        tracing::info!(%phase, "technical turn entered a phase");
        self.round.events.emit(StreamEvent::TurnPhase {
            phase,
            round: None,
            max_rounds: None,
            detail: None,
        });
    }

    async fn prepare_healing(
        &self,
        state: &mut TurnState,
        input: &RoundInput<'_>,
        report: &str,
    ) -> String {
        let candidates = diagnostic_file_candidates(input.repo_root, report).await;
        let added = tauqe_core::blocking::run_blocking(|| {
            let mut added = 0;
            for path in candidates {
                if added == MAX_HEALING_AUTO_FILES {
                    break;
                }
                if !state
                    .context
                    .get_state()
                    .items
                    .iter()
                    .any(|item| item.path == path && item.access == ContextAccess::Editable)
                    && state
                        .context
                        .add_auto_file(&path, ContextAccess::Editable)
                        .is_ok()
                {
                    added += 1;
                }
            }
            added
        });
        if added > 0 {
            self.round
                .events
                .emit(StreamEvent::ContextChanged(state.context.get_state()));
        }
        format!("Code verification failed with the following errors:\n\n{report}\n\nPropose corrected edits using the active edit protocol. Request additional material through the active research protocol if needed.")
    }
}

fn join_feedback(first: &str, second: &str) -> String {
    [first, second]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;
    use tauqe_core::config::project::ProjectConfig;
    use tauqe_core::context::ContextManager;
    use tauqe_core::execution::{CancellationToken, EventSink, PromptExecutor};
    use tauqe_core::llm::CapabilityTree;
    use tauqe_core::prompt::json::JsonPromptProtocol;
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::providers::{LlmProvider, LlmRequest};
    use tauqe_core::workflow::{GitTransactionPolicy, PatchStrategy};
    use tokio::sync::{mpsc, watch};

    fn executor(provider: Arc<dyn LlmProvider>) -> PromptExecutor {
        PromptExecutor::new(
            provider,
            Box::new(JsonPromptProtocol::new(grant())),
            "mock/model".into(),
            None,
            None,
            PatchStrategy::ApplyPatch,
        )
    }

    fn grant() -> CapabilityTree {
        let branch = |name: &str, caps: &[LlmCapability]| {
            CapabilityTree::Set(
                name.into(),
                caps.iter().copied().map(CapabilityTree::leaf).collect(),
            )
        };
        CapabilityTree::set([
            CapabilityTree::leaf(LlmCapability::TextMessage),
            CapabilityTree::leaf(LlmCapability::Scratchpad),
            CapabilityTree::AnyOf(
                "action".into(),
                vec![
                    branch("research", &[LlmCapability::ContextRequest]),
                    branch(
                        "edit",
                        &[
                            LlmCapability::EditReplace,
                            LlmCapability::EditCreate,
                            LlmCapability::CommitMessage,
                        ],
                    ),
                    branch("answer", &[]),
                    branch("blocked", &[LlmCapability::Blocked]),
                    branch("abort", &[LlmCapability::Abort]),
                ],
            ),
        ])
    }

    fn answer() -> MockResponse {
        MockResponse::text(r#"{"message":"done","action":{"kind":"answer"}}"#)
    }

    fn research(path: &str) -> MockResponse {
        MockResponse::text(serde_json::json!({"message":"read", "action":{"kind":"research", "context_requests":[{"path":path,"access":"read_only"}]}}).to_string())
    }

    fn patch(old: &str, new: &str) -> MockResponse {
        MockResponse::text(serde_json::json!({"message":"change", "action":{"kind":"edit", "commit_message":"fix: update file", "patch":format!("*** Begin Patch\n*** Update File: a.txt\n@@\n-{old}\n+{new}\n*** End Patch")}}).to_string())
    }

    fn refusal(kind: &str) -> MockResponse {
        let payload = if kind == "blocked" {
            serde_json::json!({"is_blocked":true,"reason":"prerequisite","details":"details"})
        } else {
            serde_json::json!({"is_aborted":true,"reason":"host","recommendation":"install tool"})
        };
        MockResponse::text(
            serde_json::json!({"message":"stop","action":{"kind":kind,kind:payload}}).to_string(),
        )
    }

    struct Fixture {
        dir: tempfile::TempDir,
        state: TurnState,
        project: ProjectConfig,
        cancel: CancellationToken,
        events: EventSink,
    }

    impl Fixture {
        fn new(limit: usize) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            tauqe_core::git::init_repository(Some(root), true).unwrap();
            std::fs::write(root.join("a.txt"), "committed\n").unwrap();
            std::fs::write(root.join("helper.txt"), "helper material\n").unwrap();
            tauqe_core::git::cmd::stage_files(root, &["a.txt".into(), "helper.txt".into()])
                .unwrap();
            tauqe_core::git::cmd::commit_with_developer_author(root, "initial", false).unwrap();
            std::fs::write(root.join("a.txt"), "developer work\n").unwrap();
            let mut context = ContextManager::new(root.to_path_buf());
            context.add_file("a.txt", ContextAccess::Editable).unwrap();
            let state = TurnState::new(context, grant(), None, Some(limit), 32);
            Self {
                dir,
                state,
                project: ProjectConfig::default(),
                cancel: CancellationToken::new(),
                events: EventSink::noop(),
            }
        }

        fn verification(&mut self) {
            self.project =
                toml::from_str("[verify]\ncommand = 'grep'\nargs = ['-qx', 'accepted', 'a.txt']\n")
                    .unwrap();
        }

        async fn run(
            &mut self,
            provider: Arc<dyn LlmProvider>,
            policy: Box<dyn ApplyPolicy>,
        ) -> Result<TurnOutcome, RoundError> {
            let settlement = self.settle(provider, policy).await;
            if settlement.cancelled {
                return Err(ExecutionError::Cancelled.into());
            }
            settlement.outcome
        }

        async fn settle(
            &mut self,
            provider: Arc<dyn LlmProvider>,
            policy: Box<dyn ApplyPolicy>,
        ) -> TurnSettlement {
            let mut executor = executor(provider);
            let (disk, worker) = TurnDisk::start(self.dir.path().to_path_buf(), policy);
            let settlement = TurnWorkflow {
                round: RoundWorkflow {
                    executor: &mut executor,
                    events: &self.events,
                    cancel: &self.cancel,
                },
                disk: &disk,
                research: &ResearchLoopConfig::default(),
            }
            .run(
                &mut self.state,
                RoundInput {
                    task: "Update the file",
                    repo_root: self.dir.path(),
                    project: &self.project,
                    repo_map: None,
                    repo_dirs: None,
                    outer_docs: &[],
                    brief: None,
                    feedback: None,
                    reason: None,
                    staged: None,
                },
            )
            .await;
            drop(disk);
            worker.finish().await;
            settlement
        }

        fn bytes(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("a.txt")).unwrap()
        }

        fn assert_restored(&self) {
            assert_eq!(self.bytes(), "developer work\n");
            assert!(!self
                .dir
                .path()
                .join(".tauqe/active-transaction.json")
                .exists());
            assert_eq!(
                tauqe_core::git::cmd::run_git(Some(self.dir.path()), &["log", "-1", "--format=%s"])
                    .unwrap()
                    .trim(),
                "initial"
            );
        }
    }

    fn settled(outcome: TurnOutcome) -> TurnArtifact {
        let TurnOutcome::Settled(artifact) = outcome else {
            panic!("expected settled response: {outcome:?}")
        };
        artifact
    }

    #[tokio::test]
    async fn research_then_answer_retains_context_and_transcript() {
        let mut f = Fixture::new(5);
        let provider = Arc::new(MockLlmProvider::scripted([
            research("helper.txt"),
            answer(),
        ]));
        let result = settled(
            f.run(provider.clone(), Box::new(GitTransactionPolicy::new()))
                .await
                .unwrap(),
        );
        assert_eq!(result.rounds_spent, 2);
        assert!(result.edit.is_none());
        assert!(f.state.context.contains("helper.txt").unwrap());
        let requests = provider.requests();
        assert!(requests[1].messages.starts_with(&requests[0].messages));
        assert!(requests[1]
            .messages
            .iter()
            .any(|m| m.content.contains("helper material")));
        assert_eq!(f.state.history_reader()().len(), 2);
        f.assert_restored();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repair_and_healing_can_research_before_verifying_and_committing() {
        let mut f = Fixture::new(10);
        f.verification();
        let provider = Arc::new(MockLlmProvider::scripted([
            patch("missing", "broken"),
            research("helper.txt"),
            patch("developer work", "broken"),
            research("a.txt"),
            patch("missing again", "accepted"),
            research("a.txt"),
            patch("broken", "accepted"),
        ]));
        let (events, mut rx) = EventSink::channel();
        f.events = events;
        let result = settled(
            f.run(provider.clone(), Box::new(GitTransactionPolicy::new()))
                .await
                .unwrap(),
        );
        let edit = result.edit.unwrap();
        assert_eq!(edit.verification, Some(VerificationVerdict::Passed));
        assert!(edit.commit_hash.is_some());
        assert!(edit.applied);
        assert!(edit.error.is_none());
        assert_eq!(edit.changed_files, ["a.txt"]);
        assert_eq!(result.rounds_spent, 7);
        let rounds = f.state.history_reader()();
        let source = result.response_round.unwrap();
        assert!(source.identifies(&rounds[4]));
        assert!(!source.identifies(&rounds[6]));
        assert_eq!(f.bytes(), "accepted\n");
        let requests = provider.requests();
        assert!(requests[5]
            .messages
            .iter()
            .any(|m| m.content.contains("Code verification failed")));
        assert!(requests[6]
            .messages
            .iter()
            .any(|m| m.content.contains("missing again")));
        assert!(requests[6]
            .messages
            .iter()
            .any(|m| m.content.contains("<current_versions>")));
        let reasons: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                StreamEvent::TurnRoundStarted { reason, .. } => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasons,
            [
                None,
                Some(TurnRequestReason::PatchRepair),
                Some(TurnRequestReason::PatchRepair),
                Some(TurnRequestReason::VerificationRepair),
                None,
                Some(TurnRequestReason::PatchRepair),
                Some(TurnRequestReason::PatchRepair)
            ]
        );
    }

    #[tokio::test]
    async fn empty_replies_are_bounded_but_empty_messages_are_valid() {
        for count in [2, 3] {
            let mut f = Fixture::new(10);
            let provider = Arc::new(MockLlmProvider::scripted(
                (0..count)
                    .map(|_| MockResponse::text(" "))
                    .chain([MockResponse::text(
                        r#"{"message":"","action":{"kind":"answer"}}"#,
                    )]),
            ));
            let outcome = f
                .run(provider.clone(), Box::new(GitTransactionPolicy::new()))
                .await
                .unwrap();
            if count == 2 {
                assert_eq!(settled(outcome).rounds_spent, 3);
                assert_eq!(f.state.history_reader()().len(), 1);
            } else {
                let TurnOutcome::Exhausted { reason } = outcome else {
                    panic!("expected exhaustion")
                };
                assert!(reason.contains("2 additional"));
                assert!(f.state.history_reader()().is_empty());
            }
            assert_eq!(provider.requests().len(), 3);
        }
    }

    #[tokio::test]
    async fn repeated_satisfied_and_dead_end_research_stop_without_spending_the_whole_budget() {
        for dead_end in [false, true] {
            let mut f = Fixture::new(10);
            let mut context_config = tauqe_core::config::ContextConfig::default();
            context_config.token_budget = 1;
            let research_config = if dead_end {
                ResearchLoopConfig::from_context(&context_config)
            } else {
                ResearchLoopConfig::default()
            };
            let path = if dead_end { "helper.txt" } else { "a.txt" };
            let provider = Arc::new(MockLlmProvider::scripted([research(path), research(path)]));
            let mut executor = executor(provider.clone());
            let (disk, worker) = TurnDisk::start(
                f.dir.path().to_path_buf(),
                Box::new(GitTransactionPolicy::new()),
            );
            let settlement = TurnWorkflow {
                round: RoundWorkflow {
                    executor: &mut executor,
                    events: &f.events,
                    cancel: &f.cancel,
                },
                disk: &disk,
                research: &research_config,
            }
            .run(
                &mut f.state,
                RoundInput {
                    task: "inspect",
                    repo_root: f.dir.path(),
                    project: &f.project,
                    repo_map: None,
                    repo_dirs: None,
                    outer_docs: &[],
                    brief: None,
                    feedback: None,
                    reason: None,
                    staged: None,
                },
            )
            .await;
            drop(disk);
            worker.finish().await;
            assert_eq!(settled(settlement.outcome.unwrap()).rounds_spent, 2);
            assert_eq!(provider.requests().len(), 2);
        }
    }

    #[tokio::test]
    async fn protocol_defect_never_executes_edits_or_enters_history() {
        let mut f = Fixture::new(5);
        let provider = MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"bad","action":{"kind":"edit","patch":42}}"#,
        )]);
        let outcome = f
            .run(Arc::new(provider), Box::new(GitTransactionPolicy::new()))
            .await
            .unwrap();
        let TurnOutcome::ProtocolDefect(response) = outcome else {
            panic!("expected defect")
        };
        assert!(!response.commands().is_empty());
        assert!(f.state.history_reader()().is_empty());
        f.assert_restored();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn budget_provider_failure_and_terminal_refusals_restore_dirty_bytes() {
        for exit in [
            "budget", "provider", "blocked", "abort", "answer", "protocol",
        ] {
            let mut f = Fixture::new(if exit == "budget" { 1 } else { 5 });
            f.verification();
            let next = match exit {
                "provider" => MockResponse::error("network failed"),
                "blocked" | "abort" => refusal(exit),
                "protocol" => MockResponse::text(r#"{"action":{"kind":"invalid"}}"#),
                _ => answer(),
            };
            let provider = Arc::new(MockLlmProvider::scripted([
                patch("developer work", "broken"),
                next,
            ]));
            let outcome = f
                .run(provider.clone(), Box::new(GitTransactionPolicy::new()))
                .await;
            match exit {
                "budget" => assert!(matches!(
                    outcome,
                    Err(RoundError::BudgetExhausted { spent: 1, .. })
                )),
                "provider" => assert!(matches!(
                    outcome,
                    Err(RoundError::Execution(ExecutionError::Provider(_)))
                )),
                "protocol" => assert!(matches!(outcome, Ok(TurnOutcome::ProtocolDefect(_)))),
                _ => {
                    let artifact = settled(outcome.unwrap());
                    assert!(artifact.edit.is_none());
                    assert!(artifact.response.has(match exit {
                        "blocked" => LlmCapability::Blocked,
                        "abort" => LlmCapability::Abort,
                        _ => LlmCapability::TextMessage,
                    }));
                }
            }
            f.assert_restored();
            if exit == "budget" {
                let requests = provider.requests();
                assert_eq!(requests.len(), 1);
                assert!(requests[0]
                    .messages
                    .iter()
                    .any(|m| m.content.contains("final allowed round")
                        && m.content.contains("active response schema")));
            }
        }
    }

    #[cfg(unix)]
    struct StopAfterApplication {
        provider: MockLlmProvider,
        root: std::path::PathBuf,
        refuse_rollback: bool,
        conflict: bool,
    }

    #[cfg(unix)]
    #[async_trait::async_trait]
    impl LlmProvider for StopAfterApplication {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            self.provider.kind()
        }

        async fn stream(
            &self,
            request: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            cancel: watch::Receiver<bool>,
        ) -> tauqe_core::providers::ProviderResult {
            if self.provider.requests().len() == 1 {
                assert_eq!(std::fs::read(self.root.join("a.txt")).unwrap(), b"broken\n");
                assert_eq!(
                    std::fs::read(self.root.join("helper.txt")).unwrap(),
                    b"temporary\n"
                );
                if self.refuse_rollback {
                    std::fs::write(self.root.join(".git/index.lock"), b"held").unwrap();
                }
                if self.conflict {
                    let path = self.root.join("a.txt");
                    std::fs::remove_file(&path).unwrap();
                    std::fs::create_dir(path).unwrap();
                }
            }
            self.provider.stream(request, tx, cancel).await
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rollback_outcome_keeps_the_primary_refusal_and_reports_file_conflicts() {
        for (refuse_rollback, conflict) in [(false, false), (true, false), (true, true)] {
            let mut f = Fixture::new(5);
            f.verification();
            let root = f.dir.path().to_path_buf();
            std::fs::write(root.join("helper.txt"), b"other developer work\n").unwrap();
            f.state
                .context
                .add_file("helper.txt", ContextAccess::Editable)
                .unwrap();
            let change = serde_json::json!({
                "message": "change both files",
                "action": {
                    "kind": "edit",
                    "commit_message": "fix: update both files",
                    "patch": "*** Begin Patch\n*** Update File: a.txt\n@@\n-developer work\n+broken\n*** Update File: helper.txt\n@@\n-other developer work\n+temporary\n*** End Patch"
                }
            });
            let provider = StopAfterApplication {
                provider: MockLlmProvider::scripted([
                    MockResponse::text(change.to_string()),
                    refusal("blocked"),
                ]),
                root: root.clone(),
                refuse_rollback,
                conflict,
            };
            let settlement = f
                .settle(Arc::new(provider), Box::new(GitTransactionPolicy::new()))
                .await;
            let artifact = settled(settlement.outcome.unwrap());
            assert!(artifact.response.commands().iter().any(|command| matches!(
                command,
                tauqe_core::llm::LlmCommand::Blocked(blocked) if blocked.reason() == "prerequisite"
            )));
            assert_eq!(
                std::fs::read(root.join("helper.txt")).unwrap(),
                b"other developer work\n"
            );
            match settlement.rollback.as_ref() {
                RollbackOutcome::RestoredByPolicy if !refuse_rollback => f.assert_restored(),
                RollbackOutcome::RestoredFromBackup {
                    policy_error: Some(error),
                } if !conflict && refuse_rollback => {
                    assert!(error.contains("index.lock"), "{error}");
                    assert_eq!(f.bytes(), "developer work\n");
                    assert!(root.join(".tauqe/active-transaction.json").exists());
                }
                RollbackOutcome::Incomplete {
                    policy_error: Some(error),
                    backup_error,
                } if conflict => {
                    assert!(error.contains("index.lock"), "{error}");
                    assert!(backup_error.contains("`a.txt`"), "{backup_error}");
                    assert!(root.join("a.txt").is_dir());
                }
                other => panic!("unexpected rollback outcome: {other:?}"),
            }
        }
    }

    struct CancelAfterApplication {
        first: MockLlmProvider,
        cancel: watch::Sender<bool>,
        root: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CancelAfterApplication {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            self.first.kind()
        }
        async fn stream(
            &self,
            request: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            cancel: watch::Receiver<bool>,
        ) -> tauqe_core::providers::ProviderResult {
            if self.first.requests().is_empty() {
                self.first.stream(request, tx, cancel).await
            } else {
                assert_eq!(
                    std::fs::read_to_string(self.root.join("a.txt")).unwrap(),
                    "broken\n"
                );
                self.cancel.send(true).unwrap();
                Ok(())
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_after_temporary_application_restores_before_returning() {
        let mut f = Fixture::new(5);
        f.verification();
        let (tx, rx) = watch::channel(false);
        f.cancel = CancellationToken::from_receiver(rx);
        let provider = CancelAfterApplication {
            first: MockLlmProvider::scripted([patch("developer work", "broken")]),
            cancel: tx,
            root: f.dir.path().to_path_buf(),
        };
        assert!(matches!(
            f.run(Arc::new(provider), Box::new(GitTransactionPolicy::new()))
                .await,
            Err(RoundError::Execution(ExecutionError::Cancelled))
        ));
        f.assert_restored();
        assert_eq!(f.state.history_reader()().len(), 1);
    }

    #[derive(Default)]
    struct FinalizationFailure {
        direct: tauqe_core::workflow::DirectApplyPolicy,
        rollbacks: Arc<std::sync::atomic::AtomicUsize>,
        write_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    }

    #[async_trait::async_trait]
    impl ApplyPolicy for FinalizationFailure {
        async fn staging(
            &mut self,
            root: &Path,
            project: &ProjectConfig,
            context: &ContextManager,
            edits: &[tauqe_core::edits::EditOperation],
            staged: Option<&tauqe_core::edits::StagedEditsState>,
            cancel: Option<&watch::Receiver<bool>>,
        ) -> tauqe_core::edits::StageEditsResult {
            self.direct
                .staging(root, project, context, edits, staged, cancel)
                .await
        }
        async fn apply_temporary_batches(
            &mut self,
            root: &Path,
            project: &ProjectConfig,
            context: &mut ContextManager,
            batches: &[&[tauqe_core::edits::EditOperation]],
        ) -> anyhow::Result<Vec<String>> {
            if let Some((started, release)) = &self.write_gate {
                started.notify_one();
                release.notified().await;
            }
            self.direct
                .apply_temporary_batches(root, project, context, batches)
                .await
        }
        async fn apply_final(
            &mut self,
            _: &Path,
            _: &[String],
            _: &str,
        ) -> anyhow::Result<Option<String>> {
            anyhow::bail!("finalization failed")
        }
        async fn rollback(&mut self, _: &Path) -> anyhow::Result<bool> {
            self.rollbacks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(false)
        }
    }

    #[tokio::test]
    async fn finalization_error_is_explicit_and_does_not_claim_rollback() {
        let mut f = Fixture::new(5);
        let provider = MockLlmProvider::scripted([patch("developer work", "accepted")]);
        let policy = FinalizationFailure::default();
        let rollbacks = policy.rollbacks.clone();
        let settlement = f.settle(Arc::new(provider), Box::new(policy)).await;
        assert_eq!(*settlement.rollback, RollbackOutcome::FinalizationAttempted);
        let result = settled(settlement.outcome.unwrap());
        let edit = result.edit.unwrap();
        assert_eq!(edit.verification, Some(VerificationVerdict::NotConfigured));
        assert!(!edit.applied);
        assert!(edit.commit_hash.is_none());
        assert!(edit.error.unwrap().contains("finalization failed"));
        assert_eq!(rollbacks.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(f.bytes(), "accepted\n");
    }

    #[cfg(unix)]
    struct ParkDuringRepair {
        first: MockLlmProvider,
        repairing: Arc<tokio::sync::Notify>,
    }

    #[cfg(unix)]
    #[async_trait::async_trait]
    impl LlmProvider for ParkDuringRepair {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            self.first.kind()
        }

        async fn stream(
            &self,
            request: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            cancel: watch::Receiver<bool>,
        ) -> tauqe_core::providers::ProviderResult {
            if self.first.requests().is_empty() {
                self.first.stream(request, tx, cancel).await
            } else {
                self.repairing.notify_one();
                std::future::pending().await
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forced_abort_waits_for_disk_work_and_restores_once() {
        use crate::api::{Application, InteractiveTaskError, InteractiveTaskOutcome};
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        use tokio::sync::Notify;

        for during_write in [true, false] {
            let mut f = Fixture::new(5);
            f.verification();
            let root = f.dir.path().to_path_buf();
            let (history, _) = mpsc::unbounded_channel();
            let app = Application::for_repository(Default::default(), root.clone(), history);
            let operation = Arc::new(app.try_begin_operation().unwrap());
            let reached = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let provider = Arc::new(ParkDuringRepair {
                first: MockLlmProvider::scripted([patch("developer work", "broken")]),
                repairing: reached.clone(),
            });
            let policy = FinalizationFailure {
                write_gate: during_write.then(|| (reached.clone(), release.clone())),
                ..Default::default()
            };
            let rollbacks = policy.rollbacks.clone();
            let (disk, worker) = TurnDisk::start(root.clone(), Box::new(policy));
            let mut state = f.state;
            let read_rounds = state.history_reader();
            let project = f.project;
            let cancel = CancellationToken::from_receiver(operation.cancel_receiver());
            let task = app
                .start_interactive_task(operation.clone(), move |_| async move {
                    let mut executor = executor(provider);
                    TurnWorkflow {
                        round: RoundWorkflow {
                            executor: &mut executor,
                            events: &EventSink::noop(),
                            cancel: &cancel,
                        },
                        disk: &disk,
                        research: &ResearchLoopConfig::default(),
                    }
                    .run(
                        &mut state,
                        RoundInput {
                            task: "Update the file",
                            repo_root: &root,
                            project: &project,
                            repo_map: None,
                            repo_dirs: None,
                            outer_docs: &[],
                            brief: None,
                            feedback: None,
                            reason: None,
                            staged: None,
                        },
                    )
                    .await;
                    InteractiveTaskOutcome {
                        result: Err(ExecutionError::Other("Expected forced abort".into())),
                        context: state.context,
                    }
                })
                .unwrap()
                .with_disk_worker(worker);
            tokio::time::timeout(Duration::from_secs(10), reached.notified())
                .await
                .unwrap();
            let file = f.dir.path().join("a.txt");
            let temporary = if during_write {
                "developer work\n"
            } else {
                "broken\n"
            };
            assert_eq!(std::fs::read_to_string(&file).unwrap(), temporary);
            assert!(app.abort_operation(operation.id()));
            let mut joined = Box::pin(task.join());
            if during_write {
                assert!(tokio::time::timeout(Duration::from_millis(20), &mut joined)
                    .await
                    .is_err());
                assert_eq!(rollbacks.load(Ordering::SeqCst), 0);
                release.notify_one();
            }
            let result = tokio::time::timeout(Duration::from_secs(10), joined)
                .await
                .unwrap();
            assert!(
                matches!(result.task, Err(InteractiveTaskError::Interrupted(cause)) if cause.is_cancelled())
            );
            assert!(
                matches!(result.disk, Some(DiskSettlement(DiskOutcome::RolledBack(outcome)))
                if *outcome == RollbackOutcome::RestoredFromBackup { policy_error: None })
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "developer work\n");
            assert_eq!(rollbacks.load(Ordering::SeqCst), 1);
            assert_eq!(read_rounds().len(), 1);
            assert!(app.try_begin_operation().is_err());
        }
    }

    #[tokio::test]
    async fn finalized_disk_result_is_independent_of_task_interruption() {
        use crate::api::{Application, InteractiveTaskError, InteractiveTaskOutcome};
        use crate::workflows::repl::{ReplResult, ReplText};
        use std::time::Duration;

        for interrupt in [false, true] {
            let Fixture {
                dir,
                mut state,
                project,
                events,
                ..
            } = Fixture::new(5);
            let root = dir.path().to_path_buf();
            let (history, _) = mpsc::unbounded_channel();
            let app = Application::for_repository(Default::default(), root.clone(), history);
            let operation = Arc::new(app.try_begin_operation().unwrap());
            let cancel = CancellationToken::from_receiver(operation.cancel_receiver());
            let (disk, worker) =
                TurnDisk::start(root.clone(), Box::new(GitTransactionPolicy::new()));
            let (finalized, ready) = oneshot::channel();
            let task = app
                .start_interactive_task(operation.clone(), move |_| async move {
                    let provider = MockLlmProvider::scripted([patch("developer work", "accepted")]);
                    let mut executor = executor(Arc::new(provider));
                    let settlement = TurnWorkflow {
                        round: RoundWorkflow {
                            executor: &mut executor,
                            events: &events,
                            cancel: &cancel,
                        },
                        disk: &disk,
                        research: &ResearchLoopConfig::default(),
                    }
                    .run(
                        &mut state,
                        RoundInput {
                            task: "Update the file",
                            repo_root: &root,
                            project: &project,
                            repo_map: None,
                            repo_dirs: None,
                            outer_docs: &[],
                            brief: None,
                            feedback: None,
                            reason: None,
                            staged: None,
                        },
                    )
                    .await;
                    let edit = settled(settlement.outcome.unwrap()).edit.unwrap();
                    finalized.send(edit.commit_hash.clone().unwrap()).unwrap();
                    if interrupt {
                        std::future::pending::<()>().await;
                    }
                    InteractiveTaskOutcome {
                        result: Ok(ReplResult::Edit {
                            text: ReplText::Environment("done".into()),
                            outcome: edit,
                        }),
                        context: state.context,
                    }
                })
                .unwrap()
                .with_disk_worker(worker);
            let hash = tokio::time::timeout(Duration::from_secs(10), ready)
                .await
                .unwrap()
                .unwrap();
            if interrupt {
                assert_eq!(app.cancel_active_operation(), Some(operation.id()));
                assert!(app.abort_operation(operation.id()));
            }
            let report = task.join().await;
            if interrupt {
                assert!(
                    matches!(report.task, Err(InteractiveTaskError::Interrupted(cause)) if cause.is_cancelled())
                );
            } else {
                assert!(report.task.unwrap().result.is_ok());
            }
            let edit = report.disk.unwrap().finalized_edit().unwrap();
            assert!(edit.applied);
            assert_eq!(edit.commit_hash.as_deref(), Some(hash.as_str()));
            assert_eq!(
                std::fs::read(dir.path().join("a.txt")).unwrap(),
                b"accepted\n"
            );
        }
    }
}
