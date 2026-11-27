//! Technical turn transitions. Document interpretation belongs to the caller.
//!
//! The policy retains file state while the technical computation runs.

use super::round::{run_round, EditServices, RoundServices};
use super::state::TurnState;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::config::project::ProjectConfig;
use tauqe_core::context::ContextAccess;
use tauqe_core::edits::execution::{
    declared_commit_message, diagnostic_file_candidates, EditState, EditStepVerdict,
};
use tauqe_core::edits::staging::StagingState;
use tauqe_core::edits::transaction::RollbackOutcome;
use tauqe_core::edits::EditOutcome;
use tauqe_core::execution::executor::{ExecuteInput, PromptMessage, RequestRound};
use tauqe_core::execution::AcceptedRoundId;
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, PromptExecutor, ResponseEnvelope,
};
use tauqe_core::llm::LlmCapability;
use tauqe_core::prompt::TurnBrief;
use tauqe_core::providers::{StreamEvent, TurnRequestReason};
use tauqe_core::workflow::policy::owned::{OwnedApplyPolicy, Settlement};
use tauqe_core::workflow::research::step::ResearchStepKind;
mod execution;
pub(crate) use execution::{run_technical_turn, TechnicalObservation, TechnicalTurnReport};

// Preserve the existing correction policy; all requests also spend TurnState's
// shared budget, including requests following an empty response.
const MAX_EMPTY_RETRIES: usize = 2;
const MAX_SATISFIED_ROUNDS: usize = 1;
const MAX_DEAD_END_ROUNDS: usize = 1;
const MAX_HEALING_AUTO_FILES: usize = 4;
const EMPTY_REMINDER: &str = "Your previous reply produced no actionable result. Provide a concrete result that conforms to the active response schema.";
const DEAD_END_DIRECTIVE: &str = "<response_directive>Discovery did not widen the context. Do not repeat unresolved requests. Use exact repository paths for other material, or conclude using a terminal branch of the active response schema.</response_directive>";
const SATISFIED_DIRECTIVE: &str = "<response_directive>Every requested file is already in context. Do not request those paths again. Use the material already supplied and respond through the active response schema.</response_directive>";

/// Initial semantic request for a technical turn; per-round execution details
/// belong to the owning loop.
pub(crate) struct TurnInput<'a> {
    pub task: &'a str,
    pub repo_root: &'a Path,
    pub brief: Option<&'a TurnBrief>,
    pub feedback: Option<&'a str>,
    pub reason: Option<TurnRequestReason>,
}

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

#[derive(Debug, thiserror::Error)]
pub(crate) enum RoundError {
    #[error("turn round budget exhausted after {spent} admitted requests")]
    BudgetExhausted { spent: usize },
    #[error(transparent)]
    Execution(#[from] ExecutionError),
}

/// Recovery facts used to classify the terminal result of an isolated step.
#[derive(Debug, Clone)]
pub(crate) enum StepRecovery {
    RolledBack(Arc<RollbackOutcome>),
    FinalizationAttempted,
}

impl StepRecovery {
    pub(crate) fn diagnostic(&self) -> Option<String> {
        match self {
            Self::RolledBack(outcome) => outcome.diagnostic(),
            Self::FinalizationAttempted => None,
        }
    }

    pub(crate) fn unconfirmed(&self) -> bool {
        match self {
            Self::RolledBack(outcome) => matches!(
                outcome.as_ref(),
                RollbackOutcome::RestoredFromBackup {
                    policy_error: Some(_)
                } | RollbackOutcome::Incomplete { .. }
            ),
            Self::FinalizationAttempted => false,
        }
    }
}

struct TurnEngine<'a> {
    executor: &'a mut PromptExecutor,
    events: &'a EventSink,
    cancel: &'a CancellationToken,
    project: &'a ProjectConfig,
}

pub(crate) struct TurnWorkflow;

impl TurnWorkflow {
    /// The direct entry point does not settle the policy: its task supervisor
    /// retains that obligation even if this future is forcibly stopped.
    pub(crate) async fn run_direct(
        executor: &mut PromptExecutor,
        events: &EventSink,
        cancel: &CancellationToken,
        project: &ProjectConfig,
        state: &mut TurnState,
        input: TurnInput<'_>,
        policy: &OwnedApplyPolicy,
    ) -> Result<TurnOutcome, RoundError> {
        TurnEngine {
            executor,
            events,
            cancel,
            project,
        }
        .run_inner(state, &input, policy)
        .await
    }
}

impl TurnEngine<'_> {
    async fn run_inner(
        &mut self,
        state: &mut TurnState,
        input: &TurnInput<'_>,
        policy: &OwnedApplyPolicy,
    ) -> Result<TurnOutcome, RoundError> {
        let mut staging = StagingState::default();
        let mut edit_state = EditState::new(self.project.clone());
        let mut summary = None;
        let mut pending_response = None;
        let mut repair = String::new();
        let mut healing = String::new();
        let mut feedback = input.feedback.unwrap_or_default().to_owned();
        let mut reason = input.reason;
        let mut empty_retries = 0;
        let mut satisfied = 0;
        let mut dead_ends = 0;
        let patch_strategy = self.executor.patch_strategy();

        loop {
            self.check_cancelled()?;
            if state.budget.exhausted() {
                return Err(RoundError::BudgetExhausted {
                    spent: state.budget.spent,
                });
            }
            let admitted_before = self.executor.admitted_requests();
            let directives = state.budget.final_directive();
            let request_round = RequestRound {
                round: state.budget.spent + 1,
                max_rounds: state.budget.max_rounds.unwrap_or(usize::MAX),
                reason,
            };
            let staged = staging.staged_state().cloned();
            let reply = run_round(
                self.executor,
                ExecuteInput {
                    message: if admitted_before == 0 {
                        PromptMessage::Task(input.task.to_owned())
                    } else {
                        PromptMessage::Continuation
                    },
                    directives: directives.map(str::to_owned),
                    round: Some(request_round),
                    context_items: state.context.get_state().items,
                    history: state.history.clone(),
                    capabilities: state.capabilities.clone(),
                    brief: input.brief.cloned(),
                    feedback: Some(feedback.clone()),
                    staged: staged.as_ref(),
                    created_paths: state.created_paths.clone(),
                    journal: None,
                },
                RoundServices {
                    context: &mut state.context,
                    journal: &mut state.journal,
                    repairing: !staging.failures().is_empty(),
                    edits: Some(EditServices {
                        staging: &mut staging,
                        edit: &mut edit_state,
                        policy,
                    }),
                },
                self.events,
                self.cancel,
            )
            .await;
            state
                .budget
                .charge(admitted_before, self.executor.admitted_requests());
            let mut response_round;
            let discovery;
            let edited = staging.last_step().addressed;
            state
                .created_paths
                .extend(staging.last_step().new_paths.iter().cloned());
            let mut response = match reply {
                Ok(commands) => {
                    response_round = self.executor.last_accepted_round();
                    discovery = state.journal.last_step().clone();
                    ResponseEnvelope::from_commands(&commands)
                }
                Err(ExecutionError::Protocol(message)) => {
                    return Ok(TurnOutcome::ProtocolDefect(
                        ResponseEnvelope::from_commands(&[
                            tauqe_core::llm::LlmCommand::ProtocolError { message },
                        ]),
                    ));
                }
                Err(ExecutionError::EmptyResponse) => {
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
                Err(error) => return Err(error.into()),
            };
            empty_retries = 0;
            feedback.clear();
            reason = None;
            let repairing = !staging.failures().is_empty();
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
                self.events
                    .emit_text_delta(format!("\n\n{}\n\n", discovery.summary()));
            }

            if edited {
                if summary.is_none() {
                    summary = declared_commit_message(&response);
                }
                self.check_cancelled()?;
                if !staging.failures().is_empty() {
                    for (path, diagnostic) in &staging.last_step().refusals {
                        self.events.emit(StreamEvent::EditFileRetrying {
                            path: path.clone(),
                            attempt: staging.attempts(),
                            max_retries: 0,
                            reason: diagnostic.clone(),
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
                let verdict = edit_state.last_verdict().ok_or_else(|| {
                    ExecutionError::Other("Converged edit has no verification verdict".into())
                })?;
                match verdict {
                    EditStepVerdict::Passed(verification) => {
                        let verification = *verification;
                        let summary = summary.unwrap_or_else(|| "Apply verified changes".into());
                        let edit = match policy.finalize(&summary, verification).await {
                            Ok(Settlement::Finalized { edit, .. }) => edit,
                            Ok(Settlement::NoChanges { .. }) => EditOutcome {
                                applied: true,
                                commit_hash: None,
                                error: None,
                                changed_files: Vec::new(),
                                verification: Some(verification),
                            },
                            Ok(Settlement::RolledBack(_)) => {
                                return Err(ExecutionError::Other(
                                    "Finalization unexpectedly rolled back".into(),
                                )
                                .into());
                            }
                            Err(error) => {
                                return Err(ExecutionError::Other(error.to_string()).into());
                            }
                        };
                        return Ok(TurnOutcome::Settled(TurnArtifact {
                            response,
                            response_round,
                            edit: Some(edit),
                            rounds_spent: state.budget.spent,
                        }));
                    }
                    EditStepVerdict::ApplyFailed { message } => {
                        return Err(ExecutionError::Other(message.clone()).into());
                    }
                    EditStepVerdict::HookFailed { report } => {
                        healing = self.prepare_healing(state, input, report).await;
                        reason = Some(TurnRequestReason::PostHookRepair);
                    }
                    EditStepVerdict::VerificationFailed { report } => {
                        healing = self.prepare_healing(state, input, report).await;
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
        if self.cancel.is_cancelled() {
            Err(ExecutionError::Cancelled.into())
        } else {
            Ok(())
        }
    }

    async fn prepare_healing(
        &self,
        state: &mut TurnState,
        input: &TurnInput<'_>,
        report: &str,
    ) -> String {
        let candidates = diagnostic_file_candidates(input.repo_root, report).await;
        tauqe_core::blocking::run_blocking(|| {
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
        });
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
    use super::execution::supervise_technical_task;
    use super::*;
    use crate::application::operation::OperationRegistry;
    use std::path::Path;
    use std::sync::Arc;
    use tauqe_core::config::project::ProjectConfig;
    use tauqe_core::context::ContextManager;
    use tauqe_core::edits::VerificationVerdict;
    use tauqe_core::execution::executor::PromptIdentity;
    use tauqe_core::llm::CapabilityTree;
    use tauqe_core::models::ModelRef;
    use tauqe_core::prompt::json::JsonPromptProtocol;
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::providers::{LlmProvider, LlmRequest};
    use tauqe_core::stop::StopSource;
    use tauqe_core::workflow::policy::ApplyPolicy;
    use tauqe_core::workflow::{GitTransactionPolicy, PatchStrategy};
    use tokio::sync::{mpsc, oneshot};

    fn executor(provider: Arc<dyn LlmProvider>) -> PromptExecutor {
        struct Recorder(std::sync::Mutex<Vec<tauqe_core::providers::ModelUsageInfo>>);
        impl tauqe_core::execution::usage::UsageSink for Recorder {
            fn record(&self, report: &tauqe_core::providers::ModelUsageInfo) {
                self.0.lock().unwrap().push(report.clone());
            }
        }
        let scope = tauqe_core::execution::usage::UsageScope::new(Arc::new(Recorder(
            std::sync::Mutex::new(Vec::new()),
        )));
        scope.activate();
        PromptExecutor::new(
            provider,
            Box::new(JsonPromptProtocol::new(grant())),
            PromptIdentity::new(ModelRef::openrouter("mock/model"), None, None, None),
            None,
            PatchStrategy::ApplyPatch,
        )
        .with_usage(tauqe_core::execution::usage::UsageBinding::Bound(scope))
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
        read_rounds: Vec<tauqe_core::history::TurnRoundRecord>,
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
            context
                .add_auto_file("a.txt", ContextAccess::Editable)
                .unwrap();
            let state = TurnState::new(context, grant(), None, Some(limit));
            Self {
                dir,
                state,
                read_rounds: Vec::new(),
                project: ProjectConfig::default(),
                cancel: CancellationToken::never(),
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
            let report = self.settle(provider, policy).await;
            if report.cancelled {
                return Err(ExecutionError::Cancelled.into());
            }
            report.outcome
        }

        async fn settle(
            &mut self,
            provider: Arc<dyn LlmProvider>,
            policy: Box<dyn ApplyPolicy>,
        ) -> TechnicalTurnReport {
            let root = self.dir.path().to_path_buf();
            let executor = executor(provider)
                .bind_ready(&root, &Default::default(), None)
                .await
                .unwrap();
            let reader = executor.history_reader();
            let state = std::mem::replace(
                &mut self.state,
                TurnState::new(ContextManager::new(root.clone()), grant(), None, Some(1)),
            );
            let events = self.events.clone();
            let cancel = self.cancel.clone();
            let project = self.project.clone();
            let registry = Arc::new(OperationRegistry::default());
            let operation = registry.begin().unwrap();
            let hold = operation.retain_supervision().unwrap();
            let fallback = state.context.clone();
            let policy = OwnedApplyPolicy::bind(policy, &root, cancel.clone());
            let work_policy = policy.clone();
            let work_cancel = cancel.clone();
            let computation = tokio::spawn(async move {
                let mut state = state;
                let mut executor = executor;
                let outcome = TurnWorkflow::run_direct(
                    &mut executor,
                    &events,
                    &work_cancel,
                    &project,
                    &mut state,
                    TurnInput {
                        task: "Update the file",
                        repo_root: &root,
                        brief: None,
                        feedback: None,
                        reason: None,
                    },
                    &work_policy,
                )
                .await;
                (outcome, state, executor)
            });
            let mut report =
                supervise_technical_task(computation, policy, cancel, None, fallback, hold)
                    .await
                    .unwrap();
            self.read_rounds = reader();
            if let Some(continuation) = report.continuation.take() {
                self.state = continuation.state;
            }
            report
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
    async fn concrete_turn_entry_runs_and_releases_its_task_registration() {
        let Fixture {
            dir, state, events, ..
        } = Fixture::new(5);
        let root = dir.path().to_path_buf();
        let provider = Arc::new(MockLlmProvider::scripted([answer()]));
        let executor = executor(provider.clone())
            .bind_ready(&root, &Default::default(), None)
            .await
            .unwrap();
        let registry = Arc::new(OperationRegistry::default());
        let operation = registry.begin().unwrap();
        let report = run_technical_turn(
            &operation,
            root,
            state,
            executor,
            Box::new(GitTransactionPolicy::new()),
            "Answer without editing".into(),
            None,
            events,
            None,
        )
        .await
        .unwrap();
        assert_eq!(settled(report.outcome.unwrap()).rounds_spent, 1);
        assert!(report.continuation.is_some());
        assert!(!report.cancelled);
        assert!(report.diagnostic.is_none());
        assert_eq!(provider.requests().len(), 1);
        assert!(!registry.abort(operation.id()));
        assert_eq!(registry.current_id(), Some(operation.id()));
        drop(operation);
        assert_eq!(registry.current_id(), None);
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
        assert_eq!(f.read_rounds.len(), 2);
        f.assert_restored();
    }

    #[tokio::test]
    async fn zero_budget_refuses_before_transport() {
        let mut f = Fixture::new(0);
        let provider = Arc::new(MockLlmProvider::scripted([answer()]));
        assert!(matches!(
            f.run(provider.clone(), Box::new(GitTransactionPolicy::new()))
                .await,
            Err(RoundError::BudgetExhausted { spent: 0 })
        ));
        assert!(provider.requests().is_empty());
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
        let rounds = &f.read_rounds;
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
                assert_eq!(f.read_rounds.len(), 1);
            } else {
                let TurnOutcome::Exhausted { reason } = outcome else {
                    panic!("expected exhaustion")
                };
                assert!(reason.contains("2 additional"));
                assert!(f.read_rounds.is_empty());
            }
            assert_eq!(provider.requests().len(), 3);
        }
    }

    #[tokio::test]
    async fn repeated_satisfied_research_stops_without_spending_the_whole_budget() {
        for dead_end in [false, true] {
            let mut f = Fixture::new(10);
            let path = if dead_end { "missing.txt" } else { "a.txt" };
            let provider = Arc::new(MockLlmProvider::scripted([
                research(path),
                if dead_end { answer() } else { research(path) },
            ]));
            let report = f
                .settle(provider.clone(), Box::new(GitTransactionPolicy::new()))
                .await;
            assert_eq!(settled(report.outcome.unwrap()).rounds_spent, 2);
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
        assert!(f.read_rounds.is_empty());
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
            cancel: tauqe_core::stop::HardCancellation,
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
                .add_auto_file("helper.txt", ContextAccess::Editable)
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
            match &settlement.settlement {
                Settlement::RolledBack(RollbackOutcome::RestoredByPolicy) if !refuse_rollback => {
                    f.assert_restored()
                }
                Settlement::RolledBack(RollbackOutcome::RestoredFromBackup {
                    policy_error: Some(error),
                }) if !conflict && refuse_rollback => {
                    assert!(error.contains("index.lock"), "{error}");
                    assert_eq!(f.bytes(), "developer work\n");
                    assert!(root.join(".tauqe/active-transaction.json").exists());
                }
                Settlement::RolledBack(RollbackOutcome::Incomplete {
                    policy_error: Some(error),
                    backup_error,
                }) if conflict => {
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
        cancel: StopSource,
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
            cancel: tauqe_core::stop::HardCancellation,
        ) -> tauqe_core::providers::ProviderResult {
            if self.first.requests().is_empty() {
                self.first.stream(request, tx, cancel).await
            } else {
                assert_eq!(
                    std::fs::read_to_string(self.root.join("a.txt")).unwrap(),
                    "broken\n"
                );
                self.cancel.request_hard();
                Ok(())
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_after_temporary_application_restores_before_returning() {
        let mut f = Fixture::new(5);
        f.verification();
        let tx = StopSource::new();
        f.cancel = tx.token().hard_cancellation();
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
        assert_eq!(f.read_rounds.len(), 1);
    }

    type WriteGate = (
        Arc<tokio::sync::Notify>,
        Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
    );

    #[derive(Default)]
    struct FinalizationFailure {
        direct: tauqe_core::workflow::DirectApplyPolicy,
        rollbacks: Arc<std::sync::atomic::AtomicUsize>,
        write_gate: Option<WriteGate>,
        writes: tauqe_core::blocking::StartedOperations,
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
            cancel: Option<&tauqe_core::stop::HardCancellation>,
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
            if let Some((started, release)) = self.write_gate.take() {
                let path = root.join("a.txt");
                let write = self
                    .writes
                    .start(
                        move || {
                            std::fs::write(path, b"broken\n").unwrap();
                            started.notify_one();
                            release.lock().unwrap().recv().unwrap();
                        },
                        None,
                    )
                    .map_err(anyhow::Error::msg)?;
                write.take_result().await.unwrap()?;
                return Ok(vec!["a.txt".into()]);
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
        async fn stop_and_wait(&mut self) -> anyhow::Result<()> {
            self.writes.close();
            anyhow::ensure!(
                self.writes.wait().await.unconfirmed == 0,
                "write was not confirmed"
            );
            self.direct.stop_and_wait().await
        }
        async fn finalize_resumable(
            &mut self,
            root: &Path,
            changed_files: &[String],
            summary: &str,
            cancel: Option<&tauqe_core::stop::HardCancellation>,
        ) -> anyhow::Result<tauqe_core::workflow::policy::FinalizationStatus> {
            use tauqe_core::workflow::policy::FinalizationStatus;

            if cancel.is_some_and(tauqe_core::stop::HardCancellation::is_cancelled) {
                return Ok(FinalizationStatus::NotAdmitted);
            }
            self.stop_and_wait().await?;
            Ok(FinalizationStatus::Completed {
                result: self
                    .apply_final(root, changed_files, summary)
                    .await
                    .map_err(|error| error.to_string()),
                cancelled: cancel.is_some_and(tauqe_core::stop::HardCancellation::is_cancelled),
            })
        }
        async fn finalization_result(
            &mut self,
            _: &Path,
            _: &[String],
            _: &str,
            _: Option<&tauqe_core::stop::HardCancellation>,
        ) -> anyhow::Result<tauqe_core::workflow::policy::FinalizationStatus> {
            Ok(tauqe_core::workflow::policy::FinalizationStatus::NotAdmitted)
        }
        async fn rollback(
            &mut self,
            _: &Path,
        ) -> anyhow::Result<tauqe_core::workflow::policy::PolicyRollback> {
            self.rollbacks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(tauqe_core::workflow::policy::PolicyRollback::NoCheckpoint)
        }
    }

    #[tokio::test]
    async fn finalization_error_is_explicit_and_does_not_claim_rollback() {
        let mut f = Fixture::new(5);
        let provider = MockLlmProvider::scripted([patch("developer work", "accepted")]);
        let policy = FinalizationFailure::default();
        let rollbacks = policy.rollbacks.clone();
        let settlement = f.settle(Arc::new(provider), Box::new(policy)).await;
        assert!(matches!(
            settlement.settlement,
            Settlement::Finalized { .. }
        ));
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
            cancel: tauqe_core::stop::HardCancellation,
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
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        use tokio::sync::Notify;

        for (during_write, drop_waiter) in [(true, false), (false, false), (true, true)] {
            let mut f = Fixture::new(5);
            f.verification();
            let root = f.dir.path().to_path_buf();
            let reached = Arc::new(Notify::new());
            let (release, held) = std::sync::mpsc::channel();
            let provider = Arc::new(ParkDuringRepair {
                first: MockLlmProvider::scripted([patch("developer work", "broken")]),
                repairing: reached.clone(),
            });
            let policy = FinalizationFailure {
                write_gate: during_write
                    .then(|| (reached.clone(), Arc::new(std::sync::Mutex::new(held)))),
                ..Default::default()
            };
            let rollbacks = policy.rollbacks.clone();
            let state = std::mem::replace(
                &mut f.state,
                TurnState::new(ContextManager::new(root.clone()), grant(), None, Some(1)),
            );
            let project = f.project.clone();
            let cancel = f.cancel.clone();
            let executor = executor(provider)
                .bind_ready(&root, &Default::default(), None)
                .await
                .unwrap();
            let read_rounds = executor.history_reader();
            let registry = Arc::new(OperationRegistry::default());
            let operation = registry.begin().unwrap();
            let operation_id = operation.id();
            let hold = operation.retain_supervision().unwrap();
            let fallback = state.context.clone();
            let policy = OwnedApplyPolicy::bind(Box::new(policy), &root, cancel.clone());
            let work_policy = policy.clone();
            let work_cancel = cancel.clone();
            let task = tokio::spawn(async move {
                let computation = tokio::spawn(async move {
                    let mut state = state;
                    let mut executor = executor;
                    let outcome = TurnWorkflow::run_direct(
                        &mut executor,
                        &EventSink::noop(),
                        &work_cancel,
                        &project,
                        &mut state,
                        TurnInput {
                            task: "Update the file",
                            repo_root: &root,
                            brief: None,
                            feedback: None,
                            reason: None,
                        },
                        &work_policy,
                    )
                    .await;
                    (outcome, state, executor)
                });
                supervise_technical_task(computation, policy, cancel, None, fallback, hold)
                    .await
                    .unwrap()
            });
            tokio::time::timeout(Duration::from_secs(10), reached.notified())
                .await
                .unwrap();
            let file = f.dir.path().join("a.txt");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "broken\n");
            assert!(registry.abort(operation_id));
            if drop_waiter {
                task.abort();
                assert!(task.await.err().unwrap().is_cancelled());
                drop(operation);
                assert_eq!(registry.current_id(), Some(operation_id));
                assert!(registry.begin().is_err());
                assert_eq!(rollbacks.load(Ordering::SeqCst), 0);
                release.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(10), async {
                    while registry.current_id().is_some() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(read_rounds().len(), 1);
                f.assert_restored();
                assert_eq!(rollbacks.load(Ordering::SeqCst), 1);
                continue;
            }
            let mut joined = Box::pin(task);
            if during_write {
                assert!(tokio::time::timeout(Duration::from_millis(20), &mut joined)
                    .await
                    .is_err());
                assert_eq!(rollbacks.load(Ordering::SeqCst), 0);
                release.send(()).unwrap();
            }
            let result = tokio::time::timeout(Duration::from_secs(10), joined)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                result.outcome,
                Err(RoundError::Execution(ExecutionError::Cancelled))
            ));
            assert!(result.continuation.is_none());
            assert!(matches!(
                result.settlement,
                Settlement::RolledBack(RollbackOutcome::RestoredFromBackup { policy_error: None })
            ));
            assert_eq!(read_rounds().len(), 1);
            f.assert_restored();
            assert_eq!(rollbacks.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn finalized_disk_result_is_independent_of_task_interruption() {
        use std::time::Duration;

        for interrupt in [false, true] {
            let Fixture {
                dir,
                state,
                project,
                events,
                ..
            } = Fixture::new(5);
            let root = dir.path().to_path_buf();
            let cancel = CancellationToken::never();
            let (finalized, ready) = oneshot::channel();
            let executor = executor(Arc::new(MockLlmProvider::scripted([patch(
                "developer work",
                "accepted",
            )])))
            .bind_ready(&root, &Default::default(), None)
            .await
            .unwrap();
            let read_rounds = executor.history_reader();
            let registry = Arc::new(OperationRegistry::default());
            let operation = registry.begin().unwrap();
            let hold = operation.retain_supervision().unwrap();
            let fallback = state.context.clone();
            let policy = OwnedApplyPolicy::bind(
                Box::new(GitTransactionPolicy::new()),
                &root,
                cancel.clone(),
            );
            let work_policy = policy.clone();
            let work_cancel = cancel.clone();
            let task = tokio::spawn(async move {
                let computation = tokio::spawn(async move {
                    let mut state = state;
                    let mut executor = executor;
                    let outcome = TurnWorkflow::run_direct(
                        &mut executor,
                        &events,
                        &work_cancel,
                        &project,
                        &mut state,
                        TurnInput {
                            task: "Update the file",
                            repo_root: &root,
                            brief: None,
                            feedback: None,
                            reason: None,
                        },
                        &work_policy,
                    )
                    .await;
                    let TurnOutcome::Settled(artifact) = outcome.as_ref().unwrap() else {
                        panic!("expected a finalized edit")
                    };
                    let hash = artifact.edit.as_ref().unwrap().commit_hash.clone().unwrap();
                    finalized.send(hash).unwrap();
                    if interrupt {
                        std::future::pending::<()>().await;
                    }
                    (outcome, state, executor)
                });
                supervise_technical_task(computation, policy, cancel, None, fallback, hold)
                    .await
                    .unwrap()
            });
            let hash = tokio::time::timeout(Duration::from_secs(10), ready)
                .await
                .unwrap()
                .unwrap();
            if interrupt {
                assert!(registry.abort(operation.id()));
            }
            let report = task.await.unwrap();
            if interrupt {
                assert!(matches!(
                    report.outcome,
                    Err(RoundError::Execution(ExecutionError::Cancelled))
                ));
                assert!(report.continuation.is_none());
            } else {
                let artifact = settled(report.outcome.unwrap());
                assert_eq!(
                    artifact.edit.unwrap().commit_hash.as_deref(),
                    Some(hash.as_str())
                );
            }
            let edit = match &report.settlement {
                Settlement::Finalized { edit, .. } => edit.clone(),
                other => panic!("expected finalized edit: {other:?}"),
            };
            assert_eq!(read_rounds().len(), 1);
            assert!(edit.applied);
            assert_eq!(edit.commit_hash.as_deref(), Some(hash.as_str()));
            assert_eq!(
                std::fs::read(dir.path().join("a.txt")).unwrap(),
                b"accepted\n"
            );
        }
    }
}
