//! One admission and one request; continuation decisions belong to the caller.

use super::state::TurnState;
use std::path::Path;
use std::sync::Arc;
use tauqe_core::config::{project::ProjectConfig, OuterDocEntry};
use tauqe_core::edits::StagedEditsState;
use tauqe_core::execution::transcript::AcceptedRoundId;
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, PromptExecutor, PromptInput, PromptOutcome,
    PromptResult, PromptSequence,
};
use tauqe_core::llm::LlmCommand;
use tauqe_core::prompt::TurnBrief;
use tauqe_core::providers::{ChatMessage, StreamEvent, TurnRequestReason};

const FINAL_ROUND_DIRECTIVE: &str = "<response_directive>\nThis is your final allowed round for this turn: the turn round budget is reaching its limit. Conclude the task using a terminal branch of the active response schema. Do not issue further discovery requests.\n</response_directive>";

/// Resolved request material and explicit feedback from the owning workflow.
/// The live context, grant and optional history come only from TurnState.
pub(crate) struct RoundInput<'a> {
    pub task: &'a str,
    pub repo_root: &'a Path,
    pub project: &'a ProjectConfig,
    pub repo_map: Option<&'a str>,
    pub repo_dirs: Option<&'a str>,
    pub outer_docs: &'a [OuterDocEntry],
    pub brief: Option<&'a TurnBrief>,
    pub feedback: Option<&'a str>,
    pub reason: Option<TurnRequestReason>,
    pub staged: Option<&'a StagedEditsState>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RoundError {
    #[error("turn round budget exhausted after {spent} admitted requests")]
    BudgetExhausted { spent: usize },
    #[error(transparent)]
    Execution(#[from] ExecutionError),
}

pub(crate) struct RoundWorkflow<'a> {
    pub executor: &'a mut PromptExecutor,
    pub events: &'a EventSink,
    pub cancel: &'a CancellationToken,
}

pub(crate) struct RoundReply {
    pub result: PromptResult,
    pub accepted_round: Option<AcceptedRoundId>,
}

impl RoundWorkflow<'_> {
    pub async fn run(
        &mut self,
        state: &mut TurnState,
        input: RoundInput<'_>,
    ) -> Result<RoundReply, RoundError> {
        self.check_cancelled()?;
        if state.budget.exhausted() {
            return Err(RoundError::BudgetExhausted {
                spent: state.budget.spent,
            });
        }

        let files = self
            .executor
            .context_files(input.repo_root, &state.context.get_state().items)
            .await
            .map_err(|error| ExecutionError::Other(error.to_string()))?;
        let strategy = self.executor.patch_strategy();
        let transcript = Arc::clone(&state.transcript);
        let root = input.repo_root.to_path_buf();
        let staged = input.staged.cloned();
        // File projection checks whether absent originals still exist on disk.
        // Keep that check, as well as file loading, off the async worker.
        let files = tauqe_core::blocking::run_blocking_async(move || {
            transcript
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .project_files(files, staged.as_ref(), &root, strategy)
        })
        .await
        .map_err(|error| ExecutionError::Other(error.to_string()))?;

        let sequence = {
            let mut transcript = state.transcript.lock().unwrap_or_else(|e| e.into_inner());
            if !transcript.is_seeded() {
                PromptSequence::Assemble { continuation: None }
            } else if !transcript.advertises(&state.capabilities, state.history.as_ref()) {
                PromptSequence::Assemble {
                    continuation: Some(transcript.reseed_continuation()),
                }
            } else {
                transcript.append_discoveries(strategy);
                PromptSequence::Frozen(transcript.messages())
            }
        };
        let mut tail = Vec::new();
        if let Some(feedback) = input.feedback.filter(|text| !text.trim().is_empty()) {
            tail.push(ChatMessage::user(feedback));
        }
        if state.budget.is_last() {
            tail.push(ChatMessage::user(FINAL_ROUND_DIRECTIVE));
        }
        if let Some(versions) = files.current_versions {
            tail.push(ChatMessage::user(versions));
        }
        let excluded_disk_sources = input
            .staged
            .map(|staged| {
                staged
                    .deleted_paths
                    .iter()
                    .chain(
                        staged
                            .staged_files
                            .keys()
                            .filter(|path| !files.current.iter().any(|file| &file.path == *path)),
                    )
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let prepared = self.executor.prepare(PromptInput {
            task: input.task.to_owned(),
            originals: files.originals,
            current_files: files.current,
            history: state.history.clone(),
            capabilities: state.capabilities.clone(),
            project: input.project.clone(),
            repo_root: input.repo_root.to_path_buf(),
            repo_map: input.repo_map.map(str::to_owned),
            repo_dirs: input.repo_dirs.map(str::to_owned),
            outer_docs: input.outer_docs.to_vec(),
            brief: input.brief.cloned(),
            sequence,
            tail,
            excluded_disk_sources,
        })?;

        if self.cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled.into());
        }
        {
            let mut transcript = state.transcript.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((task, prefix)) = prepared.seed_messages().and_then(|m| m.split_last()) {
                if transcript.is_seeded() {
                    transcript.reseed_prefix(
                        prefix.to_vec(),
                        &state.capabilities,
                        state.history.as_ref(),
                    );
                } else {
                    transcript.seed(
                        prefix.to_vec(),
                        task.clone(),
                        &state.capabilities,
                        state.history.as_ref(),
                    );
                }
            }
            if let Some(feedback) = input.feedback.filter(|text| !text.trim().is_empty()) {
                transcript.remember_feedback(feedback);
            }
        }
        state.budget.spent += 1;
        let round = state.budget.spent;
        self.events.emit(StreamEvent::TurnRoundStarted {
            round,
            max_rounds: state.budget.max_rounds,
            reason: input.reason,
        });
        let result = prepared.execute(self.events, self.cancel).await?;
        self.check_cancelled()?;
        let accepted_round = if let PromptOutcome::Completed(response) = &result.outcome {
            let message = response
                .commands()
                .iter()
                .find_map(|command| match command {
                    LlmCommand::Text(text) => Some(text.content.as_str()),
                    _ => None,
                })
                .unwrap_or_default();
            let notes = response
                .commands()
                .iter()
                .find_map(|command| match command {
                    LlmCommand::Scratchpad(note) => Some(note.notes()),
                    _ => None,
                })
                .unwrap_or_default();
            let id = state
                .transcript
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .accept_round(round, message, notes, state.max_scratchpad_chars);
            if !result.previewed_message && !message.trim().is_empty() {
                self.events
                    .emit_text_delta(tauqe_core::history::entry::sanitize_round_message(message));
            }
            Some(id)
        } else {
            None
        };
        Ok(RoundReply {
            result,
            accepted_round,
        })
    }

    fn check_cancelled(&self) -> Result<(), ExecutionError> {
        if self.cancel.is_cancelled() {
            Err(ExecutionError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::context::{ContextAccess, ContextManager};
    use tauqe_core::history::HistoryManager;
    use tauqe_core::llm::{CapabilityTree, LlmCapability};
    use tauqe_core::prompt::json::JsonPromptProtocol;
    use tauqe_core::providers::{
        mock::{MockLlmProvider, MockResponse},
        LlmProvider, LlmRequest,
    };
    use tauqe_core::workflow::PatchStrategy;
    use tokio::sync::{mpsc, watch, Notify};

    fn grant() -> CapabilityTree {
        CapabilityTree::set([
            CapabilityTree::leaf(LlmCapability::TextMessage),
            CapabilityTree::leaf(LlmCapability::Scratchpad),
        ])
    }

    fn state(root: &Path, limit: usize) -> TurnState {
        TurnState::new(
            ContextManager::new(root.to_path_buf()),
            grant(),
            None,
            Some(limit),
            3,
        )
    }

    fn input<'a>(root: &'a Path, project: &'a ProjectConfig) -> RoundInput<'a> {
        RoundInput {
            task: "original task",
            repo_root: root,
            project,
            repo_map: None,
            repo_dirs: None,
            outer_docs: &[],
            brief: None,
            feedback: None,
            reason: None,
            staged: None,
        }
    }

    fn executor(provider: Arc<dyn LlmProvider>, model: &str) -> PromptExecutor {
        PromptExecutor::new(
            provider,
            Box::new(JsonPromptProtocol::new(grant())),
            model.into(),
            Some(tauqe_core::models::ReasoningEffort::High),
            Some("round-test".into()),
            PatchStrategy::SearchReplace,
        )
    }

    #[tokio::test]
    async fn preparation_failure_and_cancellation_do_not_spend_or_publish() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 1);
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"done"}"#,
        )]));
        let project = ProjectConfig::default();
        let mut invalid = executor(provider.clone(), "");
        let (events, mut rx) = EventSink::channel();
        let (tx, cancel_rx) = watch::channel(false);
        let cancel = CancellationToken::from_receiver(cancel_rx);
        let mut workflow = RoundWorkflow {
            executor: &mut invalid,
            events: &events,
            cancel: &cancel,
        };
        assert!(matches!(
            workflow.run(&mut state, input(dir.path(), &project)).await,
            Err(RoundError::Execution(ExecutionError::Other(_)))
        ));
        assert_eq!(state.budget.spent, 0);
        assert!(provider.requests().is_empty());
        assert!(rx.try_recv().is_err());
        let mut bound = executor(provider.clone(), "mock/model");
        workflow.executor = &mut bound;
        tx.send(true).unwrap();
        assert!(matches!(
            workflow.run(&mut state, input(dir.path(), &project)).await,
            Err(RoundError::Execution(ExecutionError::Cancelled))
        ));
        assert_eq!(state.budget.spent, 0);
        assert!(rx.try_recv().is_err());
        tx.send(false).unwrap();
        workflow
            .run(&mut state, input(dir.path(), &project))
            .await
            .unwrap();
        assert_eq!(state.budget.spent, 1);
        assert!(provider.requests()[0]
            .messages
            .iter()
            .any(|m| m.content == FINAL_ROUND_DIRECTIVE));
        assert!(matches!(
            workflow.run(&mut state, input(dir.path(), &project)).await,
            Err(RoundError::BudgetExhausted { spent: 1 })
        ));
        assert_eq!(provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn empty_and_defective_attempts_are_not_accepted_or_retried() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 5);
        state.capabilities = CapabilityTree::set([
            CapabilityTree::leaf(LlmCapability::TextMessage),
            CapabilityTree::leaf(LlmCapability::Scratchpad),
            CapabilityTree::leaf(LlmCapability::EditReplace),
        ]);
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(" \n\t"),
            MockResponse::text(r#"{"message":"","scratchpad":"","changes":[]}"#),
            MockResponse::text(r#"{"message":"bad","changes":[{"op":"unknown"}]}"#),
            MockResponse::error("transport failed"),
        ]));
        let project = ProjectConfig::default();
        let mut executor = executor(provider.clone(), "mock/model");
        let events = EventSink::noop();
        let cancel = CancellationToken::new();
        let mut workflow = RoundWorkflow {
            executor: &mut executor,
            events: &events,
            cancel: &cancel,
        };
        let first = workflow
            .run(&mut state, input(dir.path(), &project))
            .await
            .unwrap();
        assert!(matches!(first.result.outcome, PromptOutcome::Empty));
        assert!(first.accepted_round.is_none());
        assert!(state.history_reader()().is_empty());
        assert_eq!(provider.requests().len(), 1);
        let second = workflow
            .run(&mut state, input(dir.path(), &project))
            .await
            .unwrap();
        assert!(matches!(second.result.outcome, PromptOutcome::Completed(_)));
        let third = workflow
            .run(&mut state, input(dir.path(), &project))
            .await
            .unwrap();
        assert!(matches!(
            third.result.outcome,
            PromptOutcome::ProtocolError(_)
        ));
        assert!(third.accepted_round.is_none());
        assert!(matches!(
            workflow.run(&mut state, input(dir.path(), &project)).await,
            Err(RoundError::Execution(ExecutionError::Provider(_)))
        ));
        assert_eq!(state.budget.spent, 4);
        let rounds = state.history_reader()();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].index, 2);
        assert!(second.accepted_round.unwrap().identifies(&rounds[0]));
        assert!(rounds[0].message.is_empty());
        assert!(provider
            .requests()
            .iter()
            .all(|request| request.messages.iter().all(|m| m.role != "assistant")));
    }

    #[tokio::test]
    async fn changed_grant_and_removed_history_reseed_the_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 4);
        state.capabilities = CapabilityTree::set([
            CapabilityTree::leaf(LlmCapability::TextMessage),
            CapabilityTree::leaf(LlmCapability::Scratchpad),
            CapabilityTree::leaf(LlmCapability::ContextRequest),
        ]);
        let mut history = HistoryManager::new(dir.path().to_path_buf());
        history
            .apply_compaction("persisted history marker", Vec::new())
            .unwrap();
        state.history = Some(history.snapshot().unwrap());
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(r#"{"message":"first accepted","scratchpad":"abc"}"#),
            MockResponse::text(r#"{"message":"second accepted"}"#),
        ]));
        let project = ProjectConfig::default();
        let mut executor = executor(provider.clone(), "mock/model");
        let events = EventSink::noop();
        let cancel = CancellationToken::new();
        let mut workflow = RoundWorkflow {
            executor: &mut executor,
            events: &events,
            cancel: &cancel,
        };
        workflow
            .run(&mut state, input(dir.path(), &project))
            .await
            .unwrap();
        state.history = None;
        state.capabilities = grant();
        let mut next = input(dir.path(), &project);
        next.task = "must not replace frozen task";
        workflow.run(&mut state, next).await.unwrap();
        let requests = provider.requests();
        assert!(requests[0]
            .messages
            .iter()
            .any(|m| m.content.contains("persisted history marker")));
        assert!(!requests[1]
            .messages
            .iter()
            .any(|m| m.content.contains("persisted history marker")));
        assert!(serde_json::to_string(&requests[0].response_format)
            .unwrap()
            .contains("context_requests"));
        assert!(!serde_json::to_string(&requests[1].response_format)
            .unwrap()
            .contains("context_requests"));
        assert!(requests[1]
            .messages
            .iter()
            .any(|m| m.content == "original task"));
        assert!(!requests[1]
            .messages
            .iter()
            .any(|m| m.content.contains("must not replace")));
        assert_eq!(
            requests[1].messages.last().unwrap().content,
            "first accepted\n\n<scratchpad>abc</scratchpad>"
        );
    }

    #[tokio::test]
    async fn transcript_orders_discoveries_feedback_and_versions_and_isolates_turns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "original alpha\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "discovered beta\n").unwrap();
        let mut turn = state(dir.path(), 10);
        turn.context
            .add_file("a.rs", ContextAccess::Editable)
            .unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(r#"{"message":"first answer","scratchpad":"界<ёoverflow"}"#),
            MockResponse::text(r#"{"message":"second answer","scratchpad":"discarded"}"#),
            MockResponse::text(r#"{"message":"third answer"}"#),
            MockResponse::text(r#"{"message":"new turn"}"#),
        ]));
        let project = ProjectConfig::default();
        let mut bound = executor(provider.clone(), "mock/model");
        let (events, mut rx) = EventSink::channel();
        let cancel = CancellationToken::new();
        let mut workflow = RoundWorkflow {
            executor: &mut bound,
            events: &events,
            cancel: &cancel,
        };
        workflow
            .run(&mut turn, input(dir.path(), &project))
            .await
            .unwrap();
        turn.context
            .add_auto_file("b.rs", ContextAccess::ReadOnly)
            .unwrap();
        turn.journal.begin_round(1);
        turn.journal.push_block("research feedback marker");
        let feedback = turn.journal.render_round_feedback();
        let mut staged = StagedEditsState::default();
        staged
            .staged_files
            .insert("a.rs".into(), b"staged alpha\n".to_vec());
        let mut next = input(dir.path(), &project);
        next.feedback = Some(&feedback);
        next.reason = Some(TurnRequestReason::PatchRepair);
        next.staged = Some(&staged);
        workflow.run(&mut turn, next).await.unwrap();
        workflow
            .run(&mut turn, input(dir.path(), &project))
            .await
            .unwrap();
        let reader = turn.history_reader();
        drop(turn);
        let rounds = reader();
        assert_eq!(rounds.len(), 3);
        assert_eq!(rounds[0].feedback, feedback);
        assert!(rounds
            .iter()
            .all(|r| !r.message.contains("scratchpad") && !r.message.contains("overflow")));
        let mut fresh = state(dir.path(), 10);
        assert!(fresh.journal.rendered().is_none());
        let mut fresh_executor = executor(provider.clone(), "mock/model");
        workflow.executor = &mut fresh_executor;
        workflow
            .run(&mut fresh, input(dir.path(), &project))
            .await
            .unwrap();
        assert_eq!(fresh.budget.spent, 1);
        let requests = provider.requests();
        assert!(requests[1].messages.starts_with(&requests[0].messages));
        let tail = &requests[1].messages[requests[0].messages.len()..];
        assert_eq!(tail.len(), 4);
        assert_eq!(tail[0].role, "assistant");
        assert_eq!(
            tail[0].content,
            "first answer\n\n<scratchpad>界&lt;ё</scratchpad>"
        );
        assert!(tail[1].content.starts_with("<context>"));
        assert!(tail[1].content.contains("discovered beta"));
        assert_eq!(tail[2].content, feedback);
        assert!(tail[3].content.starts_with("<current_versions>"));
        assert!(tail[3].content.contains("staged alpha"));
        assert!(!requests[2]
            .messages
            .iter()
            .any(|m| m.content.contains("staged alpha")));
        assert_eq!(
            requests[2]
                .messages
                .iter()
                .filter(|m| m.content == feedback)
                .count(),
            1
        );
        assert_eq!(
            requests[2].messages.last().unwrap().content,
            "second answer"
        );
        assert!(requests[3].messages.iter().all(|m| m.role != "assistant"));
        assert!(!requests[3]
            .messages
            .iter()
            .any(|m| m.content.contains("research feedback marker")));
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::TextDelta(text) = event {
                assert!(
                    !text.contains("scratchpad")
                        && !text.contains("overflow")
                        && !text.contains("discarded")
                );
            }
        }
    }

    struct ResettingProvider(MockLlmProvider);

    #[async_trait::async_trait]
    impl LlmProvider for ResettingProvider {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            self.0.kind()
        }

        async fn stream(
            &self,
            request: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            cancel: watch::Receiver<bool>,
        ) -> tauqe_core::providers::ProviderResult {
            tx.send(StreamEvent::ReasoningDelta("withdrawn attempt".into()))
                .await
                .unwrap();
            tx.send(StreamEvent::ResetStream).await.unwrap();
            self.0.stream(request, tx, cancel).await
        }
    }

    #[tokio::test]
    async fn transport_reset_is_inside_one_admission_and_started_precedes_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 1);
        let provider = Arc::new(ResettingProvider(MockLlmProvider::scripted([
            MockResponse::text(r#"{"message":"accepted"}"#),
        ])));
        let project = ProjectConfig::default();
        let mut executor = executor(provider.clone(), "mock/model");
        let (events, mut rx) = EventSink::channel();
        let cancel = CancellationToken::new();
        RoundWorkflow {
            executor: &mut executor,
            events: &events,
            cancel: &cancel,
        }
        .run(&mut state, input(dir.path(), &project))
        .await
        .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            StreamEvent::TurnRoundStarted { round: 1, .. }
        ));
        let remaining: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(remaining
            .iter()
            .any(|e| matches!(e, StreamEvent::ResetStream)));
        assert!(!remaining
            .iter()
            .any(|e| matches!(e, StreamEvent::TurnRoundStarted { .. })));
        assert_eq!(state.budget.spent, 1);
        assert_eq!(provider.0.requests().len(), 1);
        assert_eq!(state.history_reader()().len(), 1);
    }

    struct UnfinishedProvider {
        started: Arc<Notify>,
        park: bool,
    }

    #[async_trait::async_trait]
    impl LlmProvider for UnfinishedProvider {
        fn kind(&self) -> tauqe_core::models::ProviderKind {
            tauqe_core::models::ProviderKind::OpenRouter
        }

        async fn stream(
            &self,
            _: LlmRequest<'_>,
            tx: mpsc::Sender<StreamEvent>,
            mut cancel: watch::Receiver<bool>,
        ) -> tauqe_core::providers::ProviderResult {
            tx.send(StreamEvent::TextDelta(
                r#"{"message":"not accepted"}"#.into(),
            ))
            .await
            .unwrap();
            self.started.notify_one();
            if self.park {
                let _ = cancel.changed().await;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn incomplete_and_cancelled_requests_never_register_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 3);
        let project = ProjectConfig::default();
        let mut incomplete = executor(
            Arc::new(UnfinishedProvider {
                started: Arc::new(Notify::new()),
                park: false,
            }),
            "mock/model",
        );
        let events = EventSink::noop();
        let (tx, rx) = watch::channel(false);
        let cancel = CancellationToken::from_receiver(rx);
        let mut workflow = RoundWorkflow {
            executor: &mut incomplete,
            events: &events,
            cancel: &cancel,
        };
        assert!(matches!(
            workflow.run(&mut state, input(dir.path(), &project)).await,
            Err(RoundError::Execution(ExecutionError::Incomplete))
        ));
        let started = Arc::new(Notify::new());
        let mut parked = executor(
            Arc::new(UnfinishedProvider {
                started: started.clone(),
                park: true,
            }),
            "mock/model",
        );
        workflow.executor = &mut parked;
        let cancellation = async {
            started.notified().await;
            tx.send(true).unwrap();
        };
        let (result, ()) = tokio::join!(
            workflow.run(&mut state, input(dir.path(), &project)),
            cancellation,
        );
        assert!(matches!(
            result,
            Err(RoundError::Execution(ExecutionError::Cancelled))
        ));
        assert_eq!(state.budget.spent, 2);
        assert!(state.history_reader()().is_empty());
    }

    #[tokio::test]
    async fn accepted_rounds_remain_readable_after_forced_task_abort() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 4);
        let reader = state.history_reader();
        let started = Arc::new(Notify::new());
        let signal = Arc::clone(&started);
        let root = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            let project = ProjectConfig::default();
            let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
                r#"{"message":"durable attempt","scratchpad":"abc"}"#,
            )]));
            let mut bound = executor(provider, "mock/model");
            let events = EventSink::noop();
            let (_tx, rx) = watch::channel(false);
            let cancel = CancellationToken::from_receiver(rx);
            let mut workflow = RoundWorkflow {
                executor: &mut bound,
                events: &events,
                cancel: &cancel,
            };
            workflow
                .run(&mut state, input(&root, &project))
                .await
                .unwrap();
            let mut parked = executor(
                Arc::new(UnfinishedProvider {
                    started: signal,
                    park: true,
                }),
                "mock/model",
            );
            workflow.executor = &mut parked;
            let _ = workflow.run(&mut state, input(&root, &project)).await;
        });
        started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let rounds = reader();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].message, "durable attempt");
        assert_eq!(rounds[0].index, 1);
    }
}
