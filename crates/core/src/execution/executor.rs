//! Preparation and execution of one coherent request, without turn policy.

use super::cache::TurnInputCache;
use super::outcome::{ExecutionError, PromptOutcome, PromptResult, ResponseEnvelope};
use super::support::{CancellationToken, EventSink};
use crate::config::{project::ProjectConfig, OuterDocEntry};
use crate::context::{ContextFileContent, ContextItem};
use crate::edits::stream::EditStreamFilter;
use crate::history::HistorySnapshot;
use crate::llm::{CapabilityTree, LlmCommand};
use crate::models::ReasoningEffort;
use crate::prompt::{protocol::PromptProtocol, PromptAssembly, TurnBrief};
use crate::providers::{ChatMessage, LlmProvider, LlmRequest, ResponseFormat};
use crate::workflow::PatchStrategy;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Request binding, independent of the executor's input cache.
struct PromptBinding<'a> {
    provider: &'a dyn LlmProvider,
    protocol: &'a dyn PromptProtocol,
    model: &'a str,
    reasoning_effort: Option<ReasoningEffort>,
    session_id: Option<&'a str>,
    patch_strategy: PatchStrategy,
}

/// An explicit projection, not a mutable transcript or a scenario selector.
pub enum PromptSequence {
    /// Render context and optional history. A replacement prefix can retain
    /// the workflow's frozen task and accepted conversation after that prefix.
    Assemble {
        continuation: Option<Vec<ChatMessage>>,
    },
    /// A workflow has already retained the unchanged prefix and conversation.
    Frozen(Vec<ChatMessage>),
}

pub struct PromptInput {
    pub task: String,
    pub originals: Vec<ContextFileContent>,
    pub current_files: Vec<ContextFileContent>,
    pub history: Option<HistorySnapshot>,
    pub capabilities: CapabilityTree,
    pub project: ProjectConfig,
    pub repo_root: PathBuf,
    pub repo_map: Option<String>,
    pub repo_dirs: Option<String>,
    pub outer_docs: Vec<OuterDocEntry>,
    pub brief: Option<TurnBrief>,
    pub sequence: PromptSequence,
    pub tail: Vec<ChatMessage>,
    pub excluded_disk_sources: Vec<String>,
}

/// Fixed request dependencies and technical memoization, without turn policy.
pub struct PromptExecutor {
    provider: Arc<dyn LlmProvider>,
    protocol: Box<dyn PromptProtocol>,
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
    session_id: Option<String>,
    patch_strategy: PatchStrategy,
    cache: TurnInputCache,
}

/// Owns all mutable request inputs and the scoped decoder. No workflow state
/// is borrowed, and execution consumes this value rather than cloning it.
pub struct PreparedPrompt<'a> {
    request: PreparedRequest<'a>,
    strategy: PatchStrategy,
    seed_messages: Option<Vec<ChatMessage>>,
    decoder: Box<dyn PromptProtocol>,
    paths: Vec<String>,
}

struct PreparedRequest<'a> {
    provider: &'a dyn LlmProvider,
    model: String,
    session_id: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    messages: Vec<ChatMessage>,
    response_format: Option<ResponseFormat>,
    filter: Box<dyn EditStreamFilter>,
}

impl PromptExecutor {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        protocol: Box<dyn PromptProtocol>,
        model: String,
        reasoning_effort: Option<ReasoningEffort>,
        session_id: Option<String>,
        patch_strategy: PatchStrategy,
    ) -> Self {
        Self {
            provider,
            protocol,
            model,
            reasoning_effort,
            session_id,
            patch_strategy,
            cache: TurnInputCache::default(),
        }
    }

    /// Changes the identity of subsequent requests without dropping memoized inputs.
    pub fn rebind(
        &mut self,
        provider: Arc<dyn LlmProvider>,
        model: String,
        reasoning_effort: Option<ReasoningEffort>,
        session_id: Option<String>,
    ) {
        self.provider = provider;
        self.model = model;
        self.reasoning_effort = reasoning_effort;
        self.session_id = session_id;
    }

    pub fn patch_strategy(&self) -> PatchStrategy {
        self.patch_strategy
    }

    /// Builds a request without provider I/O, admission or transcript mutation.
    pub fn prepare(&mut self, input: PromptInput) -> Result<PreparedPrompt<'_>, ExecutionError> {
        PromptBinding {
            provider: self.provider.as_ref(),
            protocol: self.protocol.as_ref(),
            model: &self.model,
            reasoning_effort: self.reasoning_effort,
            session_id: self.session_id.as_deref(),
            patch_strategy: self.patch_strategy,
        }
        .prepare(&mut self.cache, input)
    }

    pub async fn context_files(
        &mut self,
        root: &Path,
        items: &[ContextItem],
    ) -> anyhow::Result<Vec<ContextFileContent>> {
        self.cache.context_files(root, items).await
    }

    pub async fn repo_map(&mut self, root: &Path, budget: usize) -> Option<String> {
        self.cache.repo_map(root, budget).await
    }

    pub async fn repo_dirs(&mut self, root: &Path, pinned: &[String]) -> Option<String> {
        self.cache.repo_dirs(root, pinned).await
    }

    pub async fn project(
        &mut self,
        root: &Path,
        cancel: Option<&tokio::sync::watch::Receiver<bool>>,
    ) -> ProjectConfig {
        self.cache.project(root, cancel).await
    }
}

impl<'a> PromptBinding<'a> {
    fn prepare(
        self,
        cache: &mut TurnInputCache,
        input: PromptInput,
    ) -> Result<PreparedPrompt<'a>, ExecutionError> {
        if self.model.trim().is_empty() {
            return Err(ExecutionError::Other("a request requires a model".into()));
        }
        let decoder = self
            .protocol
            .with_capabilities(input.capabilities.clone())
            .with_project(Some(input.project.clone()));
        if decoder.capability_expr() != input.capabilities {
            return Err(ExecutionError::Other(
                "the protocol did not bind the request grant".into(),
            ));
        }
        let paths: Vec<_> = input
            .current_files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        let contents = input
            .current_files
            .iter()
            .map(|file| (file.path.clone(), file.content.clone()))
            .collect();
        let response_format =
            decoder.response_format(&paths, self.patch_strategy, Some(&input.project));
        let mut filter = decoder.create_stream_filter_with_strategy(
            paths.clone(),
            input.repo_root,
            contents,
            self.patch_strategy,
        );
        filter.exclude_disk_sources(input.excluded_disk_sources.into_iter().collect());
        let (mut messages, seed_messages) = match input.sequence {
            PromptSequence::Frozen(messages) => (messages, None),
            PromptSequence::Assemble { continuation } => {
                let mut assembly = PromptAssembly::new(
                    None,
                    0,
                    input.originals,
                    "",
                    self.protocol.kind(),
                    input.history.map(std::sync::Arc::new),
                    input.repo_map,
                )
                .with_repo_dirs(input.repo_dirs)
                .with_project(Some(input.project))
                .with_patch_strategy(self.patch_strategy)
                .with_outer_docs(input.outer_docs);
                assembly.turn_brief = input.brief;
                let seed = assembly.assemble_chat_messages_with_context_cache(
                    &input.task,
                    decoder.as_ref(),
                    cache.context_cache(),
                );
                let mut messages = seed.clone();
                if let Some(continuation) = continuation {
                    messages.pop();
                    messages.extend(continuation);
                }
                (messages, Some(seed))
            }
        };
        messages.extend(input.tail);
        Ok(PreparedPrompt {
            request: PreparedRequest {
                provider: self.provider,
                model: self.model.to_owned(),
                session_id: self.session_id.map(str::to_owned),
                reasoning_effort: self.reasoning_effort,
                messages,
                response_format,
                filter,
            },
            strategy: self.patch_strategy,
            seed_messages,
            decoder,
            paths,
        })
    }
}

impl PreparedPrompt<'_> {
    /// Freshly rendered prefix and task, for a caller to retain in its own
    /// transcript. Reading this never registers a round or changes the request.
    pub fn seed_messages(&self) -> Option<&[ChatMessage]> {
        self.seed_messages.as_deref()
    }

    pub async fn execute(
        self,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<PromptResult, ExecutionError> {
        let stream = self.request.receive(events, cancel).await?;
        let outcome = if stream.text.trim().is_empty() {
            PromptOutcome::Empty
        } else {
            let commands =
                self.decoder
                    .decode_with_strategy(&stream.text, &self.paths, self.strategy);
            let defective = commands
                .iter()
                .any(|command| matches!(command, LlmCommand::ProtocolError { .. }));
            if defective {
                PromptOutcome::ProtocolError(ResponseEnvelope::from_commands(&commands))
            } else if commands.is_empty() && !self.decoder.accepts_empty_response(&stream.text) {
                PromptOutcome::Empty
            } else {
                PromptOutcome::Completed(ResponseEnvelope::from_commands(&commands))
            }
        };
        Ok(PromptResult {
            outcome,
            previewed_message: stream.previewed_message,
        })
    }

    /// Runs the prepared request with its contour's typed decoder. A completed
    /// blank stream has no payload; decoding and execution failures stay separate.
    pub async fn execute_decoded<T>(
        self,
        events: &EventSink,
        cancel: &CancellationToken,
        decode: fn(&str) -> Result<T, String>,
    ) -> Result<Option<Result<T, String>>, ExecutionError> {
        let stream = self.request.receive(events, cancel).await?;
        Ok((!stream.text.trim().is_empty()).then(|| decode(&stream.text)))
    }
}

impl PreparedRequest<'_> {
    async fn receive(
        self,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<super::request::RequestStream, ExecutionError> {
        if cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        let stream = super::request::execute(
            self.provider,
            LlmRequest {
                model: &self.model,
                messages: self.messages,
                response_format: self.response_format,
                reasoning_effort: self.reasoning_effort,
                session_id: self.session_id.as_deref(),
            },
            self.filter,
            events,
            cancel.receiver(),
        )
        .await;
        if cancel.is_cancelled() {
            return Err(ExecutionError::Cancelled);
        }
        let stream = stream?;
        if stream.cancelled {
            return Err(ExecutionError::Cancelled);
        }
        if let Some(error) = stream.stream_error {
            return Err(ExecutionError::Provider(error));
        }
        if !stream.completed {
            return Err(ExecutionError::Incomplete);
        }
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmCapability;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};

    fn input() -> PromptInput {
        PromptInput {
            task: "original task".into(),
            originals: vec![],
            current_files: vec![],
            history: None,
            capabilities: CapabilityTree::leaf(LlmCapability::TextMessage),
            project: ProjectConfig::default(),
            repo_root: PathBuf::new(),
            repo_map: None,
            repo_dirs: None,
            outer_docs: vec![],
            brief: None,
            sequence: PromptSequence::Assemble { continuation: None },
            tail: vec![],
            excluded_disk_sources: vec![],
        }
    }

    fn executor(provider: Arc<dyn LlmProvider>, model: &str) -> PromptExecutor {
        PromptExecutor::new(
            provider,
            Box::new(JsonPromptProtocol::new(input().capabilities)),
            model.into(),
            Some(ReasoningEffort::High),
            Some("executor-test-session".into()),
            PatchStrategy::SearchReplace,
        )
    }

    #[test]
    fn preparation_failure_never_calls_the_provider() {
        let provider = Arc::new(MockLlmProvider::new());
        assert!(executor(provider.clone(), "").prepare(input()).is_err());
        assert!(provider.requests().is_empty());
    }

    #[tokio::test]
    async fn bound_identity_survives_context_grant_and_history_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::init_repository(Some(root), true).unwrap();
        let marker = "newly admitted context marker 7093";
        std::fs::write(root.join("helper.txt"), marker).unwrap();
        let mut context = crate::context::ContextManager::new(root.to_path_buf());
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text(r#"{"message":"first"}"#),
            MockResponse::text("second"),
        ]));
        let mut first = input();
        first.repo_root = root.to_path_buf();
        first.capabilities = CapabilityTree::from_leaves([
            LlmCapability::TextMessage,
            LlmCapability::ContextRequest,
        ]);
        first.history = Some(HistorySnapshot::new(vec![
            crate::history::HistoryEntry::Summary {
                id: None,
                text: "private history marker".into(),
            },
        ]));
        let mut executor = executor(provider.clone(), "mock/model");
        let prepared = executor.prepare(first).unwrap();
        assert!(provider.requests().is_empty());
        let events = EventSink::noop();
        let cancel = CancellationToken::new();
        prepared.execute(&events, &cancel).await.unwrap();
        context
            .add_file("helper.txt", crate::context::ContextAccess::ReadOnly)
            .unwrap();
        let mut next = input();
        next.task = "next task".into();
        next.repo_root = root.to_path_buf();
        next.current_files = executor
            .context_files(root, &context.get_state().items)
            .await
            .unwrap();
        next.originals = next.current_files.clone();
        let second = executor.prepare(next).unwrap();
        assert_eq!(provider.requests().len(), 1);
        second.execute(&events, &cancel).await.unwrap();
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(request.model, "mock/model");
            assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
            assert_eq!(request.session_id.as_deref(), Some("executor-test-session"));
            assert_eq!(
                request.messages.iter().any(|m| m.content.contains(marker)),
                index == 1
            );
        }
        assert_eq!(
            requests[0].messages.last().unwrap().content,
            "original task"
        );
        assert_eq!(requests[1].messages.last().unwrap().content, "next task");
        assert!(requests[0]
            .messages
            .iter()
            .any(|m| m.content.contains("private history marker")));
        assert!(!requests[1]
            .messages
            .iter()
            .any(|m| m.content.contains("private history marker")));
        assert!(serde_json::to_string(&requests[0].response_format)
            .unwrap()
            .contains("context_requests"));
        assert!(!serde_json::to_string(&requests[1].response_format)
            .unwrap()
            .contains("context_requests"));
    }

    #[tokio::test]
    async fn empty_message_is_not_an_absent_response_and_decoder_errors_are_typed() {
        let provider = Arc::new(MockLlmProvider::scripted([
            MockResponse::text("  "),
            MockResponse::text(r#"{"message":"","changes":[]}"#),
            MockResponse::text(r#"{"message":"bad","changes":[{"op":"unknown"}]}"#),
        ]));
        let capabilities =
            CapabilityTree::from_leaves([LlmCapability::TextMessage, LlmCapability::EditReplace]);
        let mut executor = executor(provider, "mock/model");
        for expected in ["empty", "completed", "defect"] {
            let mut input = input();
            input.capabilities = capabilities.clone();
            let result = executor
                .prepare(input)
                .unwrap()
                .execute(&EventSink::noop(), &CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(
                match result.outcome {
                    PromptOutcome::Empty => "empty",
                    PromptOutcome::Completed(response) => {
                        assert!(response.commands().is_empty());
                        "completed"
                    }
                    PromptOutcome::ProtocolError(_) => "defect",
                },
                expected
            );
        }
    }

    struct Interrupted;
    #[async_trait::async_trait]
    impl LlmProvider for Interrupted {
        fn kind(&self) -> crate::models::ProviderKind {
            crate::models::ProviderKind::OpenRouter
        }
        async fn stream(
            &self,
            _: LlmRequest<'_>,
            tx: tokio::sync::mpsc::Sender<crate::providers::StreamEvent>,
            _: tokio::sync::watch::Receiver<bool>,
        ) -> crate::providers::ProviderResult {
            tx.send(crate::providers::StreamEvent::TextDelta("partial".into()))
                .await
                .unwrap();
            Ok(())
        }
    }

    #[tokio::test]
    async fn incomplete_stream_is_never_decoded_as_a_completed_response() {
        let result = executor(Arc::new(Interrupted), "mock/model")
            .prepare(input())
            .unwrap()
            .execute(&EventSink::noop(), &CancellationToken::new())
            .await;
        assert!(matches!(result, Err(ExecutionError::Incomplete)));
    }
}
