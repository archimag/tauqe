//! Atomic, task-owned execution of a semantic model call.

use super::*;
use crate::execution::request;
use crate::providers::StreamEvent;

impl PromptExecutor {
    /// Admit, stream and decode one request without exposing its prepared transport.
    pub async fn execute(
        &mut self,
        input: ExecuteInput<'_>,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<Vec<LlmCommand>, ExecutionError> {
        self.last_accepted_round = None;
        self.last_response_had_scratchpad = false;
        if cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        let state = Arc::clone(&self.transcript);
        let accepted = self.accepted.clone();
        let ExecuteInput {
            message,
            context_items,
            history,
            capabilities,
            brief,
            feedback,
            directives,
            staged,
            created_paths,
            round,
            journal,
        } = input;
        let (prepared, projection, evidence) = match message {
            PromptMessage::Continuation => {
                let mut projection = state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                projection.remember_created_paths(&created_paths);
                let mut tail = Vec::new();
                if let Some(text) = feedback.as_ref().filter(|text| !text.trim().is_empty()) {
                    tail.push(ChatMessage::user(text.clone()));
                }
                let prepared = self
                    .prepare_round(
                        &projection,
                        RoundPreparation {
                            task: directives.unwrap_or_default(),
                            context_items,
                            history,
                            capabilities,
                            brief,
                            tail,
                            staged,
                            sequence: RoundSequence::Transcript,
                        },
                    )
                    .await?;
                (prepared.prompt, Some(prepared.transcript), None)
            }
            PromptMessage::Task(mut task) => {
                if let Some(directives) = directives.filter(|text| !text.trim().is_empty()) {
                    task.push_str("\n\n");
                    task.push_str(&directives);
                }
                if let (Some(evidence), Some(journal)) = (self.evidence.clone(), journal) {
                    let prepared = self
                        .prepare_evidence_continuation(
                            task,
                            capabilities,
                            brief,
                            &evidence,
                            &context_items,
                            journal,
                        )
                        .await?;
                    (prepared.prompt, None, Some(prepared.evidence))
                } else {
                    if let Some(extra) =
                        journal.and_then(TurnResearchJournal::non_evidence_feedback)
                    {
                        task.push_str("\n\n");
                        task.push_str(&extra);
                    }
                    let mut projection = TurnTranscript::default();
                    projection.remember_created_paths(&created_paths);
                    let prepared = self
                        .prepare_round(
                            &projection,
                            RoundPreparation {
                                task,
                                context_items,
                                history,
                                capabilities,
                                brief,
                                tail: feedback
                                    .as_ref()
                                    .filter(|s| !s.trim().is_empty())
                                    .map(|s| vec![ChatMessage::user(s.clone())])
                                    .unwrap_or_default(),
                                staged,
                                sequence: if journal.is_some() {
                                    RoundSequence::Fresh
                                } else {
                                    RoundSequence::Transcript
                                },
                            },
                        )
                        .await?;
                    let evidence = journal.map(|journal| prepared.evidence(journal));
                    (
                        prepared.prompt,
                        journal.is_none().then_some(prepared.transcript),
                        evidence,
                    )
                }
            }
        };

        let PreparedPrompt {
            request: prepared,
            strategy,
            decoder,
            paths,
            ..
        } = prepared;
        let PreparedRequest {
            provider,
            model,
            session_id,
            reasoning_effort,
            messages,
            response_format,
            filter,
            max_input_tokens,
            usage,
        } = prepared;
        let request = LlmRequest {
            model: &model,
            messages,
            response_format,
            reasoning_effort,
            session_id: session_id.as_deref(),
        };
        let admitted = request::admit(&request, provider.kind(), cancel, max_input_tokens, &usage)?;
        if cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        self.admitted_requests += 1;
        if let Some(evidence) = evidence {
            self.evidence = Some(evidence);
        }
        if let Some(mut projection) = projection {
            if let Some(text) = feedback.as_ref().filter(|text| !text.trim().is_empty()) {
                projection.record_feedback(text);
                accepted.feedback(text);
            }
            *state.lock().unwrap_or_else(|e| e.into_inner()) = projection;
        }
        if let Some(round) = round.as_ref() {
            events.emit(StreamEvent::TurnRoundStarted {
                round: round.round,
                max_rounds: Some(round.max_rounds),
                reason: round.reason,
            });
        }
        let stream = request::execute_admitted(
            provider.as_ref(),
            request,
            filter,
            events,
            cancel.clone(),
            admitted,
        )
        .await?;
        if cancel.is_cancelled() || stream.cancelled {
            return Err(ExecutionError::Cancelled);
        }
        if let Some(error) = stream.stream_error {
            return Err(ExecutionError::Provider(error));
        }
        if !stream.completed {
            return Err(ExecutionError::Incomplete);
        }
        if stream.text.trim().is_empty() {
            return Err(ExecutionError::EmptyResponse);
        }
        let mut commands = decoder.decode_with_strategy(&stream.text, &paths, strategy);
        if let Some(error) = commands.iter().find_map(|cmd| match cmd {
            LlmCommand::ProtocolError { message } => Some(message.clone()),
            _ => None,
        }) {
            return Err(ExecutionError::Protocol(error));
        }
        if commands.is_empty() && !decoder.accepts_empty_response(&stream.text) {
            return Err(ExecutionError::EmptyResponse);
        }
        self.last_response_had_scratchpad = commands
            .iter()
            .any(|cmd| matches!(cmd, LlmCommand::Scratchpad(_)));
        if let Some(round) = round {
            let message = commands
                .iter()
                .find_map(|cmd| match cmd {
                    LlmCommand::Text(text) => Some(text.content.as_str()),
                    _ => None,
                })
                .unwrap_or_default();
            let notes = commands
                .iter()
                .find_map(|cmd| match cmd {
                    LlmCommand::Scratchpad(note) => Some(note.notes()),
                    _ => None,
                })
                .unwrap_or_default();
            let mut transcript = state.lock().unwrap_or_else(|e| e.into_inner());
            transcript.record_answer(message, notes);
            self.last_accepted_round = Some(accepted.append(round.round as u32, message));
            if !stream.previewed_message && !message.trim().is_empty() {
                events.emit_text_delta(crate::history::entry::sanitize_round_message(message));
            }
        }
        commands.retain(|cmd| !matches!(cmd, LlmCommand::Scratchpad(_)));
        Ok(commands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::executor::PromptIdentity;
    use crate::execution::usage::{UsageBinding, UsageScope, UsageSink};
    use crate::llm::{CapabilityTree, LlmCapability};
    use crate::models::ModelRef;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::providers::ModelUsageInfo;
    use crate::workflow::PatchStrategy;

    struct NoopUsage;
    impl UsageSink for NoopUsage {
        fn record(&self, _: &ModelUsageInfo) {}
    }

    async fn executor(root: &std::path::Path, provider: Arc<MockLlmProvider>) -> PromptExecutor {
        let scope = UsageScope::new(Arc::new(NoopUsage));
        scope.activate();
        let mut executor = PromptExecutor::new(
            provider,
            Box::new(JsonPromptProtocol::new(CapabilityTree::from_leaves([
                LlmCapability::TextMessage,
                LlmCapability::Scratchpad,
                LlmCapability::EditReplace,
            ]))),
            PromptIdentity::new(ModelRef::openrouter("mock/model"), None, None, None),
            None,
            PatchStrategy::SearchReplace,
        )
        .with_usage(UsageBinding::Bound(scope));
        executor
            .bind_environment(root, &Default::default(), None)
            .await
            .unwrap();
        executor
    }

    fn input(feedback: Option<&str>) -> ExecuteInput<'static> {
        ExecuteInput {
            message: PromptMessage::Task("opening task".into()),
            context_items: Vec::new(),
            history: None,
            capabilities: CapabilityTree::from_leaves([
                LlmCapability::TextMessage,
                LlmCapability::Scratchpad,
                LlmCapability::EditReplace,
            ]),
            brief: None,
            feedback: feedback.map(str::to_owned),
            directives: None,
            staged: None,
            created_paths: Vec::new(),
            round: Some(RequestRound {
                round: 1,
                max_rounds: 4,
                reason: None,
            }),
            journal: None,
        }
    }

    #[tokio::test]
    async fn admission_and_cancellation_leave_feedback_and_recovery_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"accepted"}"#,
        )]));
        let mut limited = executor(dir.path(), provider.clone())
            .await
            .with_max_input_tokens(1);
        let reader = limited.history_reader();
        assert!(matches!(
            limited
                .execute(
                    input(Some("rejected feedback")),
                    &EventSink::noop(),
                    &CancellationToken::never()
                )
                .await,
            Err(ExecutionError::InputLimit(_))
        ));
        assert_eq!(limited.admitted_requests(), 0);
        let stop = crate::stop::StopSource::new();
        stop.request_hard();
        assert!(matches!(
            limited
                .execute(
                    input(Some("cancelled feedback")),
                    &EventSink::noop(),
                    &stop.token().hard_cancellation()
                )
                .await,
            Err(ExecutionError::Cancelled)
        ));
        assert_eq!(limited.admitted_requests(), 0);
        assert!(reader().is_empty());
        assert!(provider.requests().is_empty());
        let mut allowed = limited.with_max_input_tokens(usize::MAX);
        let commands = allowed
            .execute(input(None), &EventSink::noop(), &CancellationToken::never())
            .await
            .unwrap();
        assert!(!commands.is_empty());
        assert!(allowed.last_accepted_round().is_some());
        assert!(provider.requests()[0].messages.iter().all(|message| {
            !message.content.contains("rejected feedback")
                && !message.content.contains("cancelled feedback")
        }));
        assert_eq!(allowed.history_reader()().len(), 1);
    }

    #[tokio::test]
    async fn only_valid_completed_commands_enter_sanitized_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text("  "),
            MockResponse::text(r#"{"message":"bad","changes":[{"op":"unknown"}]}"#),
            MockResponse::text(r#"{"message":"","scratchpad":"","changes":[]}"#),
            MockResponse::text(r#"{"message":"  visible  ","scratchpad":"private"}"#),
        ]));
        let mut executor = executor(dir.path(), provider.clone()).await;
        let reader = executor.history_reader();
        let (events, mut rx) = EventSink::channel();
        assert!(matches!(
            executor
                .execute(input(None), &events, &CancellationToken::never())
                .await,
            Err(ExecutionError::EmptyResponse)
        ));
        assert!(executor.last_accepted_round().is_none());
        assert!(matches!(
            executor
                .execute(input(None), &events, &CancellationToken::never())
                .await,
            Err(ExecutionError::Protocol(_))
        ));
        assert!(executor.last_accepted_round().is_none());
        let mut third = input(None);
        third.message = PromptMessage::Continuation;
        third.round = Some(RequestRound {
            round: 3,
            max_rounds: 4,
            reason: None,
        });
        assert!(executor
            .execute(third, &events, &CancellationToken::never())
            .await
            .unwrap()
            .is_empty());
        assert!(!executor.last_response_had_scratchpad());
        assert!(executor.last_accepted_round().is_some());
        let mut fourth = input(Some("  feedback for previous  "));
        fourth.message = PromptMessage::Continuation;
        fourth.round = Some(RequestRound {
            round: 4,
            max_rounds: 4,
            reason: None,
        });
        executor
            .execute(fourth, &events, &CancellationToken::never())
            .await
            .unwrap();
        assert!(executor.last_response_had_scratchpad());
        assert!(provider.requests()[3]
            .messages
            .iter()
            .any(|m| m.content == "  feedback for previous  "));
        drop(executor);
        let records = reader();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].index, 3);
        assert!(records[0].message.is_empty());
        assert_eq!(records[0].feedback, "feedback for previous");
        assert_eq!(records[1].message, "visible");
        assert!(records.iter().all(|r| !r.message.contains("private")));
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|event| {
            matches!(event, StreamEvent::TextDelta(text) if text.contains("visible"))
        }));
    }
}
