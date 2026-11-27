//! One admission and one request; continuation decisions belong to the caller.

use tauqe_core::context::ContextManager;
use tauqe_core::edits::execution::{
    execute_edit_step, EditEnvironment, EditState, EDIT_CAPABILITIES,
};
use tauqe_core::edits::staging::{
    execute_staging_step, render_failures, StagingEnvironment, StagingState,
};
use tauqe_core::execution::executor::ExecuteInput;
use tauqe_core::execution::{
    CancellationToken, EventSink, ExecutionError, PromptExecutor, ResponseEnvelope,
};
use tauqe_core::llm::LlmCommand;
use tauqe_core::providers::StreamEvent;
use tauqe_core::workflow::policy::owned::OwnedApplyPolicy;
use tauqe_core::workflow::research::journal::TurnResearchJournal;
use tauqe_core::workflow::research::step::{
    execute_research_step, ResearchEnvironment, RESEARCH_CAPABILITIES,
};
use tauqe_core::workflow::turn::TurnPhase;

pub(crate) struct EditServices<'a> {
    pub staging: &'a mut StagingState,
    pub edit: &'a mut EditState,
    pub policy: &'a OwnedApplyPolicy,
}

/// Services for the operations permitted during this request.
pub(crate) struct RoundServices<'a> {
    pub context: &'a mut ContextManager,
    pub journal: &'a mut TurnResearchJournal,
    pub repairing: bool,
    pub edits: Option<EditServices<'a>>,
}

pub(crate) async fn run_round(
    executor: &mut PromptExecutor,
    input: ExecuteInput<'_>,
    mut services: RoundServices<'_>,
    events: &EventSink,
    cancel: &CancellationToken,
) -> Result<Vec<LlmCommand>, ExecutionError> {
    if cancel.is_cancelled() {
        return Err(ExecutionError::Cancelled);
    }
    let research_round = input
        .round
        .as_ref()
        .map_or_else(|| services.journal.round_index() + 1, |round| round.round);
    services.journal.reset_step();
    if let Some(edits) = services.edits.as_mut() {
        edits.staging.reset_step();
        edits.edit.reset_verdict();
    }
    let mut response =
        ResponseEnvelope::from_commands(&executor.execute(input, events, cancel).await?);
    if RESEARCH_CAPABILITIES.iter().any(|cap| response.has(*cap)) {
        events.emit(StreamEvent::TurnPhase {
            phase: TurnPhase::Research,
            round: None,
            max_rounds: None,
            detail: None,
        });
    }
    let environment = executor
        .environment()
        .ok_or_else(|| ExecutionError::Other("prompt executor has no bound environment".into()))?;
    execute_research_step(
        &mut ResearchEnvironment {
            repo_root: executor.repo_root()?,
            context_manager: services.context,
            journal: services.journal,
            outer_docs: environment.outer_docs(),
            project: environment.project(),
            cancel,
            events,
            round: research_round,
        },
        &mut response,
        environment.research(),
        services.repairing,
        executor.last_response_had_scratchpad(),
    )
    .await?;
    if let Some(edits) = services.edits.as_mut() {
        if EDIT_CAPABILITIES.iter().any(|cap| response.has(*cap)) {
            events.emit(StreamEvent::TurnPhase {
                phase: TurnPhase::Staging,
                round: None,
                max_rounds: None,
                detail: None,
            });
            let root = executor.repo_root()?;
            let strategy = executor.patch_strategy();
            tauqe_core::blocking::run_blocking(|| {
                execute_staging_step(
                    &mut StagingEnvironment {
                        repo_root: root,
                        context_manager: services.context,
                        patch_strategy: strategy,
                        events,
                    },
                    &mut response,
                    edits.staging,
                );
            });
            if edits.staging.last_step().addressed && edits.staging.failures().is_empty() {
                if !edits.staging.abandoned().is_empty() {
                    let lines = render_failures(edits.staging.abandoned(), strategy)
                        .into_iter()
                        .map(|(path, error)| format!("- `{path}`: {error}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    events.emit_text_delta(format!("\n\n[Change Set Incomplete] Unretried files remain outside the change set:\n{lines}\n\n"));
                }
                edits.edit.adopt_staging(edits.staging);
                edits.staging.clear_accumulated();
                events.emit(StreamEvent::TurnPhase {
                    phase: TurnPhase::Verification,
                    round: None,
                    max_rounds: None,
                    detail: None,
                });
                let result = execute_edit_step(
                    &mut EditEnvironment {
                        repo_root: root,
                        context_manager: services.context,
                        policy: edits.policy,
                        cancel,
                        events,
                    },
                    edits.edit,
                )
                .await;
                if cancel.is_cancelled() {
                    return Err(ExecutionError::Cancelled);
                }
                result?;
            }
        }
    }
    Ok(response.commands().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::state::TurnState;
    use std::path::Path;
    use std::sync::Arc;
    use tauqe_core::config::project::ProjectConfig;
    use tauqe_core::context::{ContextAccess, ContextManager};
    use tauqe_core::edits::StagedEditsState;
    use tauqe_core::execution::executor::{PromptIdentity, PromptMessage, RequestRound};
    use tauqe_core::history::HistoryManager;
    use tauqe_core::llm::{CapabilityTree, LlmCapability};
    use tauqe_core::models::ModelRef;
    use tauqe_core::prompt::json::JsonPromptProtocol;
    use tauqe_core::providers::{
        mock::{MockLlmProvider, MockResponse},
        LlmProvider, LlmRequest, StreamEvent,
    };
    use tauqe_core::stop::StopSource;
    use tauqe_core::workflow::PatchStrategy;
    use tokio::sync::{mpsc, Notify};

    struct TestInput<'a> {
        message: PromptMessage,
        feedback: Option<&'a str>,
        staged: Option<&'a StagedEditsState>,
    }

    async fn run_round(
        executor: &mut PromptExecutor,
        state: &mut TurnState,
        input: TestInput<'_>,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<Vec<LlmCommand>, ExecutionError> {
        let admitted = executor.admitted_requests();
        super::run_round(
            executor,
            ExecuteInput {
                message: if admitted == 0 {
                    input.message
                } else {
                    PromptMessage::Continuation
                },
                context_items: state.context.get_state().items,
                history: state.history.clone(),
                capabilities: state.capabilities.clone(),
                brief: None,
                feedback: input.feedback.map(str::to_owned),
                directives: None,
                staged: input.staged,
                created_paths: state.created_paths.clone(),
                round: Some(RequestRound {
                    round: admitted + 1,
                    max_rounds: usize::MAX,
                    reason: None,
                }),
                journal: None,
            },
            RoundServices {
                context: &mut state.context,
                journal: &mut state.journal,
                repairing: false,
                edits: None,
            },
            events,
            cancel,
        )
        .await
    }

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
        )
    }

    fn input<'a>(root: &Path, project: &ProjectConfig) -> TestInput<'a> {
        let _ = (root, project);
        TestInput {
            message: PromptMessage::Task("original task".into()),
            feedback: None,
            staged: None,
        }
    }

    fn executor(provider: Arc<dyn LlmProvider>, model: &str) -> PromptExecutor {
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
            PromptIdentity::new(
                ModelRef::openrouter(model),
                Some(tauqe_core::models::ReasoningEffort::High),
                None,
                None,
            ),
            Some("round-test".into()),
            PatchStrategy::SearchReplace,
        )
        .with_usage(tauqe_core::execution::usage::UsageBinding::Bound(scope))
    }

    async fn bound_executor(
        root: &Path,
        provider: Arc<dyn LlmProvider>,
        model: &str,
    ) -> PromptExecutor {
        let mut executor = executor(provider, model);
        executor
            .bind_environment(root, &Default::default(), None)
            .await
            .unwrap();
        executor
    }

    #[tokio::test]
    async fn input_limit_precedes_first_round_and_admitted_request_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 2);
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"accepted"}"#,
        )]));
        let project = ProjectConfig::default();
        let mut small = bound_executor(dir.path(), provider.clone(), "mock/model")
            .await
            .with_max_input_tokens(1);
        let (events, mut rx) = EventSink::channel();
        let cancel = CancellationToken::never();
        assert!(matches!(
            run_round(
                &mut small,
                &mut state,
                input(dir.path(), &project),
                &events,
                &cancel
            )
            .await,
            Err(ExecutionError::InputLimit(_))
        ));
        assert_eq!(small.admitted_requests(), 0);
        assert!(provider.requests().is_empty());
        assert!(rx.try_recv().is_err());
        assert!(small.history_reader()().is_empty());

        let mut allowed = small.with_max_input_tokens(usize::MAX);
        run_round(
            &mut allowed,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        assert!(allowed.last_accepted_round().is_some());
        assert_eq!(allowed.admitted_requests(), 1);
        assert_eq!(provider.requests().len(), 1);
        assert!(matches!(
            rx.try_recv(),
            Ok(StreamEvent::TurnRoundStarted { round: 1, .. })
        ));
    }

    #[tokio::test]
    async fn later_input_limit_counts_feedback_and_current_versions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "original\n").unwrap();
        let mut state = state(dir.path(), 4);
        state
            .context
            .add_auto_file("a.rs", ContextAccess::Editable)
            .unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(r#"{"message":"first"}"#),
            MockResponse::text(r#"{"message":"second"}"#),
        ]));
        let project = ProjectConfig::default();
        let (events, mut rx) = EventSink::channel();
        let cancel = CancellationToken::never();
        let mut allowed = bound_executor(dir.path(), provider.clone(), "mock/model").await;
        run_round(
            &mut allowed,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        let mut probe = allowed.with_max_input_tokens(1);
        let baseline = match run_round(
            &mut probe,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        {
            Err(ExecutionError::InputLimit(error)) => error.size,
            _ => panic!("expected plain subsequent request to exceed probe limit"),
        };
        let mut limited = probe.with_max_input_tokens(baseline);
        while rx.try_recv().is_ok() {}

        let feedback = "feedback marker ".repeat(300);
        let mut with_feedback = input(dir.path(), &project);
        with_feedback.feedback = Some(&feedback);
        assert!(matches!(
            run_round(&mut limited, &mut state, with_feedback, &events, &cancel).await,
            Err(ExecutionError::InputLimit(_))
        ));
        let mut staged = StagedEditsState::default();
        staged
            .staged_files
            .insert("a.rs".into(), "version marker ".repeat(300).into_bytes());
        let mut with_versions = input(dir.path(), &project);
        with_versions.staged = Some(&staged);
        assert!(matches!(
            run_round(&mut limited, &mut state, with_versions, &events, &cancel).await,
            Err(ExecutionError::InputLimit(_))
        ));
        assert_eq!(limited.admitted_requests(), 1);
        assert_eq!(provider.requests().len(), 1);
        assert!(rx.try_recv().is_err());
        assert_eq!(limited.history_reader()().len(), 1);

        let mut allowed = limited.with_max_input_tokens(usize::MAX);
        run_round(
            &mut allowed,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(provider.requests().len(), 2);
        assert_eq!(allowed.admitted_requests(), 2);
        assert!(provider.requests()[1].messages.iter().all(|message| {
            !message.content.contains("feedback marker")
                && !message.content.contains("version marker")
        }));
    }

    #[tokio::test]
    async fn full_scratchpad_and_navigation_feedback_are_admitted_or_refused_together() {
        let dir = tempfile::tempdir().unwrap();
        let notes = "界<ё".repeat(2000);
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            serde_json::json!({"message": "research", "scratchpad": notes}).to_string(),
        )]));
        let project = ProjectConfig::default();
        let events = EventSink::noop();
        let cancel = CancellationToken::never();
        let mut state = state(dir.path(), 4);
        let mut allowed = bound_executor(dir.path(), provider.clone(), "mock/model").await;
        run_round(
            &mut allowed,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();

        let mut limited = allowed.with_max_input_tokens(1);
        let navigation = format!(
            "<search_feedback>\n{}\n</search_feedback>",
            (0..150)
                .map(|index| format!("src/file_{index:03}.rs: 1 match"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let mut next = input(dir.path(), &project);
        next.feedback = Some(&navigation);
        assert!(matches!(
            run_round(&mut limited, &mut state, next, &events, &cancel).await,
            Err(ExecutionError::InputLimit(_))
        ));
        assert_eq!(limited.admitted_requests(), 1);
        assert_eq!(provider.requests().len(), 1);
        let requests = provider.requests();
        let messages = &requests[0].messages;
        assert!(messages
            .iter()
            .all(|message| !message.content.contains(&notes)));
        assert!(!messages
            .iter()
            .any(|message| message.content.contains("file_149.rs")));
        assert!(limited.history_reader()()
            .iter()
            .all(|round| !round.message.contains(&notes)));
    }

    #[tokio::test]
    async fn preparation_failure_and_cancellation_do_not_admit_or_publish() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 1);
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"done"}"#,
        )]));
        let project = ProjectConfig::default();
        let mut invalid = bound_executor(dir.path(), provider.clone(), "").await;
        let (events, mut rx) = EventSink::channel();
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        assert!(matches!(
            run_round(
                &mut invalid,
                &mut state,
                input(dir.path(), &project),
                &events,
                &cancel
            )
            .await,
            Err(ExecutionError::Other(_))
        ));
        assert_eq!(invalid.admitted_requests(), 0);
        assert!(provider.requests().is_empty());
        assert!(rx.try_recv().is_err());
        invalid.rebind(
            provider.clone(),
            PromptIdentity::new(ModelRef::openrouter("mock/model"), None, None, None),
            None,
        );
        source.request_hard();
        assert!(matches!(
            run_round(
                &mut invalid,
                &mut state,
                input(dir.path(), &project),
                &events,
                &cancel
            )
            .await,
            Err(ExecutionError::Cancelled)
        ));
        assert_eq!(invalid.admitted_requests(), 0);
        assert!(rx.try_recv().is_err());
        let fresh = StopSource::new();
        let fresh_cancel = fresh.token().hard_cancellation();
        run_round(
            &mut invalid,
            &mut state,
            input(dir.path(), &project),
            &events,
            &fresh_cancel,
        )
        .await
        .unwrap();
        assert_eq!(invalid.admitted_requests(), 1);
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
        let mut executor = bound_executor(dir.path(), provider.clone(), "mock/model").await;
        let events = EventSink::noop();
        let cancel = CancellationToken::never();
        assert!(matches!(
            run_round(
                &mut executor,
                &mut state,
                input(dir.path(), &project),
                &events,
                &cancel,
            )
            .await,
            Err(ExecutionError::EmptyResponse)
        ));
        assert!(executor.history_reader()().is_empty());
        assert_eq!(provider.requests().len(), 1);
        let second = run_round(
            &mut executor,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        assert!(second.is_empty());
        let accepted = executor.last_accepted_round().unwrap();
        assert!(matches!(
            run_round(
                &mut executor,
                &mut state,
                input(dir.path(), &project),
                &events,
                &cancel,
            )
            .await,
            Err(ExecutionError::Protocol(_))
        ));
        assert!(matches!(
            run_round(
                &mut executor,
                &mut state,
                input(dir.path(), &project),
                &events,
                &cancel
            )
            .await,
            Err(ExecutionError::Provider(_))
        ));
        assert_eq!(executor.admitted_requests(), 4);
        let rounds = executor.history_reader()();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].index, 2);
        assert!(accepted.identifies(&rounds[0]));
        assert!(rounds[0].message.is_empty());
        assert!(!provider.requests()[2]
            .messages
            .iter()
            .any(|m| m.role == "assistant"));
    }

    #[tokio::test]
    async fn defective_research_is_not_accepted_or_applied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "original\n").unwrap();
        let mut state = state(dir.path(), 2);
        state.capabilities = tauqe_core::llm::step::turn_capability_expr();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"Researching","action":{"kind":"research","context_requests":[{"path":"a.rs","access":"editable"},{"path":"../escape.rs","access":"read_only"}],"repo_searches":[]}}"#,
        )]));
        let project = ProjectConfig::default();
        let mut executor = bound_executor(dir.path(), provider, "mock/model").await;
        assert!(matches!(
            run_round(
                &mut executor,
                &mut state,
                input(dir.path(), &project),
                &EventSink::noop(),
                &CancellationToken::never(),
            )
            .await,
            Err(ExecutionError::Protocol(_))
        ));
        assert!(executor.history_reader()().is_empty());
        assert!(state.context.get_state().items.is_empty());
        assert!(state.journal.rendered().is_none());
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
        let mut history = HistoryManager::new(
            dir.path().to_path_buf(),
            tauqe_core::config::HistoryConfig::default().budget_tokens,
        );
        history
            .apply_compaction("persisted history marker", Vec::new())
            .unwrap();
        state.history = Some(history.snapshot().unwrap());
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(r#"{"message":"first accepted","scratchpad":"abc"}"#),
            MockResponse::text(r#"{"message":"second accepted"}"#),
        ]));
        let project = ProjectConfig::default();
        let mut executor = bound_executor(dir.path(), provider.clone(), "mock/model").await;
        let events = EventSink::noop();
        let cancel = CancellationToken::never();
        run_round(
            &mut executor,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        state.history = None;
        state.capabilities = grant();
        let mut next = input(dir.path(), &project);
        next.message = PromptMessage::Task("must not replace frozen task".into());
        run_round(&mut executor, &mut state, next, &events, &cancel)
            .await
            .unwrap();
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
    async fn model_rebinding_retains_the_accepted_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 3);
        let first = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"first","scratchpad":"private note"}"#,
        )]));
        let second = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"second"}"#,
        )]));
        let mut executor = bound_executor(dir.path(), first.clone(), "mock/first").await;
        let events = EventSink::noop();
        let cancel = CancellationToken::never();
        let project = ProjectConfig::default();
        run_round(
            &mut executor,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        executor.rebind(
            second.clone(),
            PromptIdentity::new(ModelRef::openrouter("mock/second"), None, None, None),
            Some("new-session".into()),
        );
        run_round(
            &mut executor,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        let requests = second.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, "mock/second");
        assert_eq!(requests[0].session_id.as_deref(), Some("new-session"));
        assert!(requests[0]
            .messages
            .starts_with(&first.requests()[0].messages));
        assert_eq!(
            requests[0].messages.last().unwrap().content,
            "first\n\n<scratchpad>private note</scratchpad>"
        );
        assert_eq!(executor.history_reader()().len(), 2);
    }

    #[tokio::test]
    async fn staged_creates_deletes_and_access_changes_keep_opening_files_stable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.rs"), "opening old\n").unwrap();
        std::fs::write(dir.path().join("access.rs"), "opening access\n").unwrap();
        let mut state = state(dir.path(), 4);
        for path in ["old.rs", "access.rs"] {
            state
                .context
                .add_auto_file(path, ContextAccess::Editable)
                .unwrap();
        }
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(r#"{"message":"first"}"#),
            MockResponse::text(r#"{"message":"second"}"#),
            MockResponse::text(r#"{"message":"third"}"#),
        ]));
        let mut executor = bound_executor(dir.path(), provider.clone(), "mock/model").await;
        let events = EventSink::noop();
        let cancel = CancellationToken::never();
        let project = ProjectConfig::default();
        run_round(
            &mut executor,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        state.created_paths.push("new.rs".into());
        std::fs::write(dir.path().join("new.rs"), "created on disk\n").unwrap();
        state
            .context
            .add_auto_file("new.rs", ContextAccess::Editable)
            .unwrap();
        state.context.remove_auto_file("access.rs").unwrap();
        state
            .context
            .add_auto_file("access.rs", ContextAccess::ReadOnly)
            .unwrap();
        let mut staged = StagedEditsState::default();
        staged.deleted_paths.insert("old.rs".into());
        staged
            .staged_files
            .insert("new.rs".into(), b"staged new\n".to_vec());
        let mut next = input(dir.path(), &project);
        next.staged = Some(&staged);
        run_round(&mut executor, &mut state, next, &events, &cancel)
            .await
            .unwrap();
        let requests = provider.requests();
        let second = &requests[1].messages;
        assert!(second.starts_with(&requests[0].messages));
        let versions = second
            .iter()
            .find(|m| m.content.starts_with("<current_versions>"))
            .expect("changed files are explicit");
        assert!(versions.content.contains("<deleted path=\"old.rs\""));
        assert!(versions
            .content
            .contains("<access path=\"access.rs\" value=\"read_only\""));
        assert!(versions.content.contains("staged new"));
        assert!(!second.iter().any(|m| m.content.contains("created on disk")));
        assert_eq!(
            second
                .iter()
                .filter(|m| m.content.contains("opening old"))
                .count(),
            1
        );
        run_round(
            &mut executor,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        let requests = provider.requests();
        let third = &requests[2].messages;
        assert!(third.iter().all(|m| !m.content.contains("staged new")));
        assert!(third.iter().any(|m| m.content.contains("created on disk")));
    }

    #[tokio::test]
    async fn transcript_orders_discoveries_feedback_and_versions_and_isolates_turns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "original alpha\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "discovered beta\n").unwrap();
        let mut turn = state(dir.path(), 10);
        turn.context
            .add_auto_file("a.rs", ContextAccess::Editable)
            .unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(r#"{"message":"first answer","scratchpad":"界<ёoverflow"}"#),
            MockResponse::text(r#"{"message":"second answer","scratchpad":"discarded"}"#),
            MockResponse::text(r#"{"message":"third answer"}"#),
            MockResponse::text(r#"{"message":"new turn"}"#),
        ]));
        let project = ProjectConfig::default();
        let mut bound = bound_executor(dir.path(), provider.clone(), "mock/model").await;
        let (events, mut rx) = EventSink::channel();
        let cancel = CancellationToken::never();
        run_round(
            &mut bound,
            &mut turn,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
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
        next.staged = Some(&staged);
        run_round(&mut bound, &mut turn, next, &events, &cancel)
            .await
            .unwrap();
        run_round(
            &mut bound,
            &mut turn,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        let reader = bound.history_reader();
        drop(turn);
        let rounds = reader();
        assert_eq!(rounds.len(), 3);
        assert_eq!(rounds[0].feedback, feedback);
        assert!(rounds
            .iter()
            .all(|r| !r.message.contains("scratchpad") && !r.message.contains("overflow")));
        let mut fresh = state(dir.path(), 10);
        assert!(fresh.journal.rendered().is_none());
        let mut fresh_executor = bound_executor(dir.path(), provider.clone(), "mock/model").await;
        run_round(
            &mut fresh_executor,
            &mut fresh,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(fresh_executor.admitted_requests(), 1);
        let requests = provider.requests();
        assert!(requests[1].messages.starts_with(&requests[0].messages));
        let tail = &requests[1].messages[requests[0].messages.len()..];
        assert_eq!(tail.len(), 4);
        assert_eq!(tail[0].role, "assistant");
        assert_eq!(
            tail[0].content,
            "first answer\n\n<scratchpad>界&lt;ёoverflow</scratchpad>"
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
            "second answer\n\n<scratchpad>discarded</scratchpad>"
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
            cancel: tauqe_core::stop::HardCancellation,
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
        let mut executor = bound_executor(dir.path(), provider.clone(), "mock/model").await;
        let (events, mut rx) = EventSink::channel();
        let cancel = CancellationToken::never();
        run_round(
            &mut executor,
            &mut state,
            input(dir.path(), &project),
            &events,
            &cancel,
        )
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
        assert_eq!(executor.admitted_requests(), 1);
        assert_eq!(provider.0.requests().len(), 1);
        assert_eq!(executor.history_reader()().len(), 1);
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
            cancel: tauqe_core::stop::HardCancellation,
        ) -> tauqe_core::providers::ProviderResult {
            tx.send(StreamEvent::TextDelta(
                r#"{"message":"not accepted"}"#.into(),
            ))
            .await
            .unwrap();
            self.started.notify_one();
            if self.park {
                cancel.cancelled().await;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn incomplete_and_cancelled_requests_never_register_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 3);
        let project = ProjectConfig::default();
        let mut incomplete = bound_executor(
            dir.path(),
            Arc::new(UnfinishedProvider {
                started: Arc::new(Notify::new()),
                park: false,
            }),
            "mock/model",
        )
        .await;
        let events = EventSink::noop();
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        assert!(matches!(
            run_round(
                &mut incomplete,
                &mut state,
                input(dir.path(), &project),
                &events,
                &cancel
            )
            .await,
            Err(ExecutionError::Incomplete)
        ));
        let started = Arc::new(Notify::new());
        let mut parked = bound_executor(
            dir.path(),
            Arc::new(UnfinishedProvider {
                started: started.clone(),
                park: true,
            }),
            "mock/model",
        )
        .await;
        let cancellation = async {
            started.notified().await;
            source.request_hard();
        };
        let (result, ()) = tokio::join!(
            run_round(
                &mut parked,
                &mut state,
                input(dir.path(), &project),
                &events,
                &cancel
            ),
            cancellation,
        );
        assert!(matches!(result, Err(ExecutionError::Cancelled)));
        assert_eq!(incomplete.admitted_requests(), 1);
        assert_eq!(parked.admitted_requests(), 1);
        assert!(incomplete.history_reader()().is_empty());
    }

    #[tokio::test]
    async fn accepted_rounds_remain_readable_after_forced_task_abort() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path(), 4);
        let started = Arc::new(Notify::new());
        let signal = Arc::clone(&started);
        let root = dir.path().to_path_buf();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"durable attempt","scratchpad":"abc"}"#,
        )]));
        let mut bound = bound_executor(&root, provider, "mock/model").await;
        let reader = bound.history_reader();
        let task = tokio::spawn(async move {
            let project = ProjectConfig::default();
            let events = EventSink::noop();
            let source = StopSource::new();
            let cancel = source.token().hard_cancellation();
            run_round(
                &mut bound,
                &mut state,
                input(&root, &project),
                &events,
                &cancel,
            )
            .await
            .unwrap();
            bound.rebind(
                Arc::new(UnfinishedProvider {
                    started: signal,
                    park: true,
                }),
                PromptIdentity::new(ModelRef::openrouter("mock/model"), None, None, None),
                None,
            );
            let _ = run_round(
                &mut bound,
                &mut state,
                input(&root, &project),
                &events,
                &cancel,
            )
            .await;
        });
        started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let rounds = reader();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].message, "durable attempt");
        assert_eq!(rounds[0].index, 1);
    }

    #[tokio::test]
    async fn research_uses_the_allowlist_frozen_with_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tauqe_core::git::init_repository(Some(root), true).unwrap();
        let config_path = root.join("tauqe.toml");
        let declaration = |name: &str| {
            format!(
                "[models]\nrepl = ['openrouter:mock/model']\n[project.commands.{name}]\ncommand = 'printf'\nargs = ['frozen']\n"
            )
        };
        std::fs::write(&config_path, declaration("original")).unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(
                r#"{"message":"first","custom_tools":[{"name":"original","args":{}}]}"#,
            ),
            MockResponse::text(
                r#"{"message":"second","custom_tools":[{"name":"replacement","args":{}}]}"#,
            ),
        ]));
        let mut state = state(root, 3);
        state.capabilities = CapabilityTree::set([
            CapabilityTree::leaf(LlmCapability::TextMessage),
            CapabilityTree::leaf(LlmCapability::CustomTools),
        ]);
        let mut executor = bound_executor(root, provider.clone(), "mock/model").await;
        let project = ProjectConfig::default();
        let first = run_round(
            &mut executor,
            &mut state,
            input(root, &project),
            &EventSink::noop(),
            &CancellationToken::never(),
        )
        .await
        .unwrap();
        assert!(first
            .iter()
            .all(|command| !matches!(command, LlmCommand::CustomTool(_))));
        let schema = serde_json::to_string(&provider.requests()[0].response_format).unwrap();
        assert!(schema.contains("original"));
        assert!(!schema.contains("replacement"));
        assert!(state
            .journal
            .rendered()
            .unwrap()
            .contains("name=\"original\" success=\"true\""));

        std::fs::write(&config_path, declaration("replacement")).unwrap();
        run_round(
            &mut executor,
            &mut state,
            input(root, &project),
            &EventSink::noop(),
            &CancellationToken::never(),
        )
        .await
        .unwrap();
        let feedback = state.journal.rendered().unwrap();
        assert!(
            feedback.contains("name=\"replacement\" success=\"false\""),
            "{feedback}"
        );
    }

    fn architect_executor(provider: Arc<dyn LlmProvider>) -> PromptExecutor {
        struct Recorder;
        impl tauqe_core::execution::usage::UsageSink for Recorder {
            fn record(&self, _: &tauqe_core::providers::ModelUsageInfo) {}
        }
        let scope = tauqe_core::execution::usage::UsageScope::new(Arc::new(Recorder));
        scope.activate();
        PromptExecutor::new(
            provider,
            Box::new(tauqe_core::prompt::refine::PlanRefinePromptProtocol::default()),
            PromptIdentity::new(ModelRef::openrouter("mock/model"), None, None, None),
            None,
            PatchStrategy::SearchReplace,
        )
        .with_usage(tauqe_core::execution::usage::UsageBinding::Bound(scope))
    }

    async fn architect_round(
        executor: &mut PromptExecutor,
        context: &mut ContextManager,
        journal: &mut TurnResearchJournal,
        task: &str,
        capabilities: CapabilityTree,
    ) -> Result<Vec<LlmCommand>, ExecutionError> {
        let evidence_journal = journal.clone();
        super::run_round(
            executor,
            ExecuteInput {
                message: PromptMessage::Task(task.into()),
                context_items: context.get_state().items,
                history: None,
                capabilities,
                brief: Some(tauqe_core::prompt::TurnBrief::architect()),
                feedback: None,
                directives: None,
                staged: None,
                created_paths: Vec::new(),
                round: None,
                journal: Some(&evidence_journal),
            },
            RoundServices {
                context,
                journal,
                repairing: false,
                edits: None,
            },
            &EventSink::noop(),
            &CancellationToken::never(),
        )
        .await
    }

    #[tokio::test]
    async fn architect_keeps_complete_round_and_frozen_evidence_without_dialogue_replay() {
        use tauqe_core::plan::prompt::RoundStatus;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tauqe_core::git::init_repository(Some(root), true).unwrap();
        std::fs::write(root.join("source.rs"), "original architect evidence\n").unwrap();
        tauqe_core::git::cmd::stage_files(root, &["source.rs".into()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(root, "initial", false).unwrap();
        let mut context = ContextManager::new(root.to_path_buf());
        context
            .add_auto_file("source.rs", ContextAccess::ReadOnly)
            .unwrap();
        let mut journal = TurnResearchJournal::default();
        let payload = serde_json::json!({
            "status": "success", "explanation": "A complete horizon", "reason": null,
            "context_requests": [], "repo_searches": [], "repo_maps": [],
            "items": [{"id": "1", "title": "Step", "files": ["source.rs"]}]
        });
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(payload.to_string()),
            MockResponse::text(payload.to_string()),
        ]));
        let mut executor = architect_executor(provider.clone());
        executor
            .bind_environment(root, &Default::default(), None)
            .await
            .unwrap();
        let mut limited = executor.with_max_input_tokens(1);
        assert!(matches!(
            architect_round(
                &mut limited,
                &mut context,
                &mut journal,
                "original task",
                tauqe_core::plan::prompt::architect_synthesis_capability_expr()
            )
            .await,
            Err(ExecutionError::InputLimit(_))
        ));
        assert!(limited.evidence_snapshot().is_none());
        assert!(provider.requests().is_empty());
        let mut executor = limited.with_max_input_tokens(usize::MAX);
        let first = architect_round(
            &mut executor,
            &mut context,
            &mut journal,
            "original task",
            tauqe_core::plan::prompt::architect_synthesis_capability_expr(),
        )
        .await
        .unwrap();
        assert!(executor.last_accepted_round().is_none());
        assert!(first.iter().any(|command| matches!(command,
            LlmCommand::ArchitectRound { output, status: RoundStatus::Success }
                if output.items[0].files == ["source.rs"]
        )));
        assert_eq!(
            executor.evidence_snapshot().unwrap().files()[0].content,
            "original architect evidence\n"
        );
        std::fs::remove_file(root.join("source.rs")).unwrap();
        let critique_material = executor
            .evidence_snapshot()
            .unwrap()
            .read_only_projection(PatchStrategy::SearchReplace);
        assert!(critique_material.contains("original architect evidence"));
        assert!(!critique_material.contains("<deleted path=\"source.rs\""));
        architect_round(
            &mut executor,
            &mut context,
            &mut journal,
            "independent architectural request",
            tauqe_core::plan::prompt::architect_synthesis_capability_expr(),
        )
        .await
        .unwrap();
        assert_eq!(
            executor.evidence_snapshot().unwrap().files()[0].content,
            "original architect evidence\n"
        );
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert!(!requests[1]
            .messages
            .iter()
            .any(|message| message.role == "assistant"));
        assert!(requests[1].messages.iter().any(|message| message
            .content
            .contains("independent architectural request")));
        assert!(!requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("<deleted path=\"source.rs\"")));
        assert!(requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("original architect evidence")));
    }

    #[tokio::test]
    async fn architect_terminal_grant_rejects_discovery_before_research() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tauqe_core::git::init_repository(Some(root), true).unwrap();
        std::fs::write(root.join("source.rs"), "source\n").unwrap();
        tauqe_core::git::cmd::stage_files(root, &["source.rs".into()]).unwrap();
        tauqe_core::git::cmd::commit_with_developer_author(root, "initial", false).unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(
                r#"{"status":"needs_context","explanation":"inspect","context_requests":["source.rs"],"items":[]}"#,
            ),
            MockResponse::text(
                r#"{"status":"needs_context","explanation":"bad","context_requests":["source.rs","../escape.rs"],"items":[]}"#,
            ),
        ]));
        let mut executor = architect_executor(provider.clone());
        executor
            .bind_environment(root, &Default::default(), None)
            .await
            .unwrap();
        let mut context = ContextManager::new(root.to_path_buf());
        let mut journal = TurnResearchJournal::default();
        let reply = architect_round(
            &mut executor,
            &mut context,
            &mut journal,
            "original task",
            tauqe_core::plan::prompt::architect_synthesis_terminal_capability_expr(),
        )
        .await;
        assert!(executor.evidence_snapshot().is_some());
        assert!(matches!(reply, Err(ExecutionError::Protocol(_))));
        assert!(context.get_state().items.is_empty());
        assert!(journal.rendered().is_none());
        assert!(matches!(
            architect_round(
                &mut executor,
                &mut context,
                &mut journal,
                "original task",
                tauqe_core::plan::prompt::architect_synthesis_terminal_capability_expr()
            )
            .await,
            Err(ExecutionError::Protocol(_))
        ));
        assert!(context.get_state().items.is_empty());
        assert!(journal.rendered().is_none());
        assert_eq!(provider.requests().len(), 2);
        for request in provider.requests() {
            let schema = serde_json::to_value(&request.response_format).unwrap();
            assert_eq!(
                schema["json_schema"]["schema"]["properties"]["context_requests"]["maxItems"],
                0
            );
            assert_eq!(
                schema["json_schema"]["schema"]["properties"]["status"]["enum"],
                serde_json::json!(["success", "blocked"])
            );
        }
    }
}
