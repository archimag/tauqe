//! Preparation and execution of one coherent request, without turn policy.

use super::cache::{PromptEnvironment, TurnInputCache};
use super::outcome::ExecutionError;
use super::support::{CancellationToken, EventSink};
use super::transcript::{ResearchEvidence, TurnTranscript};
use super::usage::UsageBinding;
use crate::config::project::ProjectConfig;
use crate::context::{ContextFileContent, ContextItem};
use crate::edits::stream::EditStreamFilter;
use crate::edits::StagedEditsState;
use crate::history::HistorySnapshot;
use crate::llm::{CapabilityTree, LlmCommand};
use crate::models::{ModelRef, ModelRole, ReasoningEffort};
use crate::prompt::{protocol::PromptProtocol, PromptAssembly, TurnBrief};
use crate::providers::{ChatMessage, LlmProvider, LlmRequest, ResponseFormat};
use crate::workflow::research::journal::TurnResearchJournal;
use crate::workflow::PatchStrategy;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use tauqe_tools::stop::HardCancellation;

mod execute;

/// Semantic inputs to one model call. The executor constructs the transport request.
pub struct ExecuteInput<'a> {
    pub message: PromptMessage,
    pub context_items: Vec<ContextItem>,
    pub history: Option<HistorySnapshot>,
    pub capabilities: CapabilityTree,
    pub brief: Option<TurnBrief>,
    pub feedback: Option<String>,
    pub directives: Option<String>,
    pub staged: Option<&'a StagedEditsState>,
    pub created_paths: Vec<String>,
    pub round: Option<RequestRound>,
    pub journal: Option<&'a TurnResearchJournal>,
}

/// A new independent task or a continuation of the accepted conversation.
pub enum PromptMessage {
    Task(String),
    Continuation,
}

/// Optional round metadata for events and accepted-answer registration only.
pub struct RequestRound {
    pub round: usize,
    pub max_rounds: usize,
    pub reason: Option<crate::providers::TurnRequestReason>,
}

/// An executor whose repository materials have been frozen before a workflow starts.
pub struct ReadyPromptExecutor(PromptExecutor, PromptEnvironment);

impl std::ops::Deref for ReadyPromptExecutor {
    type Target = PromptExecutor;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ReadyPromptExecutor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl ReadyPromptExecutor {
    pub fn project(&self) -> &ProjectConfig {
        self.1.project()
    }

    /// Bind accounting before the first request without rebuilding the frozen
    /// environment or dropping cached context. Tier rebinding retains this scope.
    pub fn bind_usage(&mut self, usage: UsageBinding) -> Result<(), ExecutionError> {
        if self.0.usage.is_some() {
            return Err(ExecutionError::Other(
                "prompt executor already has a usage binding".into(),
            ));
        }
        self.0.usage = Some(usage);
        Ok(())
    }

    /// Retain the frozen environment and input cache across completed phases.
    pub fn rebind_usage(&mut self, usage: UsageBinding) -> Result<(), ExecutionError> {
        if !self.0.usage.as_ref().is_some_and(UsageBinding::is_closed) {
            return Err(ExecutionError::Other(
                "the previous usage window has not been closed".into(),
            ));
        }
        self.0.usage = Some(usage);
        Ok(())
    }

    pub fn into_inner(self) -> PromptExecutor {
        self.0
    }
}

/// Request binding, independent of the executor's input cache.
struct PromptBinding<'a> {
    provider: Arc<dyn LlmProvider>,
    protocol: &'a dyn PromptProtocol,
    identity: &'a PromptIdentity,
    session_id: Option<&'a str>,
    patch_strategy: PatchStrategy,
    max_input_tokens: usize,
    usage: UsageBinding,
}

/// Identity frozen with a provider binding, independent of selection policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptIdentity {
    model: ModelRef,
    effort: Option<ReasoningEffort>,
    role: Option<ModelRole>,
    alias: Option<String>,
}

impl PromptIdentity {
    pub fn new(
        model: ModelRef,
        effort: Option<ReasoningEffort>,
        role: Option<ModelRole>,
        alias: Option<String>,
    ) -> Self {
        Self {
            model,
            effort,
            role,
            alias,
        }
    }

    pub fn model(&self) -> &ModelRef {
        &self.model
    }

    pub fn effort(&self) -> Option<ReasoningEffort> {
        self.effort
    }

    pub fn role(&self) -> Option<ModelRole> {
        self.role
    }

    pub fn alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }
}

/// An explicit projection, not a mutable transcript or a scenario selector.
enum PromptSequence {
    /// Render context and optional history. A replacement prefix can retain
    /// the workflow's frozen task and accepted conversation after that prefix.
    Assemble {
        continuation: Option<Vec<ChatMessage>>,
    },
    /// A workflow has already retained the unchanged prefix and conversation.
    Frozen(Vec<ChatMessage>),
}

/// Scenario intent without technical catalogs. Only admitted context items
/// are read; the caller can provide a staged current view for an edit retry.
struct PromptInput {
    pub task: String,
    pub context_items: Vec<ContextItem>,
    pub originals: Vec<ContextFileContent>,
    pub current_files: Option<Vec<ContextFileContent>>,
    pub history: Option<HistorySnapshot>,
    pub capabilities: CapabilityTree,
    pub brief: Option<TurnBrief>,
    pub sequence: PromptSequence,
    pub tail: Vec<ChatMessage>,
    pub excluded_disk_sources: Vec<String>,
}

/// The owning workflow chooses whether to replay the turn or assemble a fresh task.
#[derive(Clone, Copy)]
enum RoundSequence {
    Transcript,
    Fresh,
}

struct RoundPreparation<'a> {
    pub task: String,
    pub context_items: Vec<ContextItem>,
    pub history: Option<HistorySnapshot>,
    pub capabilities: CapabilityTree,
    pub brief: Option<TurnBrief>,
    pub tail: Vec<ChatMessage>,
    pub staged: Option<&'a StagedEditsState>,
    pub sequence: RoundSequence,
}

/// The transcript projection remains provisional until the workflow admits the round.
struct PreparedRound {
    pub(crate) prompt: PreparedPrompt,
    pub(crate) transcript: TurnTranscript,
    evidence_files: Vec<ContextFileContent>,
    evidence_map: Option<String>,
}

/// A prepared continuation and the exact evidence view used for its request.
struct PreparedEvidenceRound {
    prompt: PreparedPrompt,
    evidence: ResearchEvidence,
}

impl PreparedRound {
    /// Snapshot the effective files admitted with this request and the results
    /// already delivered to its architect; no disk reads occur at this boundary.
    pub(crate) fn evidence(&self, journal: &TurnResearchJournal) -> ResearchEvidence {
        ResearchEvidence::new(
            self.evidence_files.clone(),
            self.evidence_map.clone(),
            journal,
        )
    }
}

struct RequestMaterials {
    task: String,
    originals: Vec<ContextFileContent>,
    current_files: Vec<ContextFileContent>,
    history: Option<HistorySnapshot>,
    capabilities: CapabilityTree,
    project: ProjectConfig,
    repo_root: PathBuf,
    repo_map: Option<String>,
    outer_docs: Vec<crate::config::OuterDocEntry>,
    brief: Option<TurnBrief>,
    sequence: PromptSequence,
    tail: Vec<ChatMessage>,
    excluded_disk_sources: Vec<String>,
}

/// Fixed request dependencies and technical memoization, without turn policy.
pub struct PromptExecutor {
    provider: Arc<dyn LlmProvider>,
    protocol: Box<dyn PromptProtocol>,
    identity: PromptIdentity,
    session_id: Option<String>,
    patch_strategy: PatchStrategy,
    max_input_tokens: usize,
    cache: TurnInputCache,
    environment: Option<PromptEnvironment>,
    usage: Option<UsageBinding>,
    transcript: Arc<Mutex<TurnTranscript>>,
    accepted: crate::execution::accepted_rounds::AcceptedRounds,
    evidence: Option<ResearchEvidence>,
    admitted_requests: usize,
    last_accepted_round: Option<crate::execution::AcceptedRoundId>,
    last_response_had_scratchpad: bool,
}

/// Owns all mutable request inputs and the scoped decoder. No workflow state
/// is borrowed, and execution consumes this value rather than cloning it.
struct PreparedPrompt {
    request: PreparedRequest,
    strategy: PatchStrategy,
    seed_messages: Option<Vec<ChatMessage>>,
    decoder: Box<dyn PromptProtocol>,
    paths: Vec<String>,
}

struct PreparedRequest {
    provider: Arc<dyn LlmProvider>,
    model: String,
    session_id: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    messages: Vec<ChatMessage>,
    response_format: Option<ResponseFormat>,
    filter: Box<dyn EditStreamFilter>,
    max_input_tokens: usize,
    usage: UsageBinding,
}

impl PromptExecutor {
    async fn prepare_round(
        &mut self,
        previous: &TurnTranscript,
        input: RoundPreparation<'_>,
    ) -> Result<PreparedRound, ExecutionError> {
        let root = self.repo_root()?.to_path_buf();
        let loaded = self
            .cache
            .context_files(&root, &input.context_items)
            .await
            .map_err(|error| ExecutionError::Other(error.to_string()))?;
        let strategy = self.patch_strategy;
        let staged = input.staged.cloned();
        let mut transcript = previous.clone();
        let (files, projected_transcript) = crate::blocking::run_blocking_async(move || {
            let files = transcript.project_files(loaded, staged.as_ref(), &root, strategy);
            (files, transcript)
        })
        .await
        .map_err(|error| ExecutionError::Other(error.to_string()))?;
        transcript = projected_transcript;
        let mut task = input.task;
        let mut tail = input.tail;
        let sequence = match input.sequence {
            RoundSequence::Fresh => {
                if let Some(versions) = &files.current_versions {
                    task.push_str("\n\n");
                    task.push_str(versions);
                }
                PromptSequence::Assemble { continuation: None }
            }
            RoundSequence::Transcript => {
                if let Some(versions) = files.current_versions {
                    tail.push(ChatMessage::user(versions));
                }
                if !transcript.is_seeded() {
                    PromptSequence::Assemble { continuation: None }
                } else if !transcript.advertises(&input.capabilities, input.history.as_ref()) {
                    PromptSequence::Assemble {
                        continuation: Some(transcript.reseed_continuation()),
                    }
                } else {
                    transcript.append_discoveries(strategy);
                    PromptSequence::Frozen(transcript.messages())
                }
            }
        };
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
        let capabilities = input.capabilities;
        let history = input.history;
        let evidence_files = files.current.clone();
        let evidence_map = self
            .environment
            .as_ref()
            .and_then(|env| env.repo_map.clone());
        let prompt = self
            .prepare(PromptInput {
                task,
                context_items: input.context_items,
                originals: files.originals,
                current_files: Some(files.current),
                history: history.clone(),
                capabilities: capabilities.clone(),
                brief: input.brief,
                sequence,
                tail,
                excluded_disk_sources,
            })
            .await?;
        if matches!(input.sequence, RoundSequence::Transcript) {
            if let Some((task, prefix)) = prompt.seed_messages().and_then(|m| m.split_last()) {
                if transcript.is_seeded() {
                    transcript.reseed_prefix(prefix.to_vec(), &capabilities, history.as_ref());
                } else {
                    transcript.seed(
                        prefix.to_vec(),
                        task.clone(),
                        &capabilities,
                        history.as_ref(),
                    );
                }
            }
        }
        Ok(PreparedRound {
            prompt,
            transcript,
            evidence_files,
            evidence_map,
        })
    }

    /// Continue from frozen evidence; only newly admitted paths are read from
    /// disk. The resulting snapshot and request share the same file view.
    async fn prepare_evidence_continuation(
        &mut self,
        mut task: String,
        capabilities: CapabilityTree,
        brief: Option<TurnBrief>,
        evidence: &ResearchEvidence,
        context_items: &[ContextItem],
        journal: &TurnResearchJournal,
    ) -> Result<PreparedEvidenceRound, ExecutionError> {
        let root = self.repo_root()?.to_path_buf();
        let new_items: Vec<_> = context_items
            .iter()
            .filter(|item| !evidence.files().iter().any(|file| file.path == item.path))
            .cloned()
            .collect();
        let new_files = self
            .cache
            .context_files(&root, &new_items)
            .await
            .map_err(|error| ExecutionError::Other(error.to_string()))?;
        let extended = evidence.extend(new_files, journal);
        if let Some(feedback) = journal.non_evidence_feedback() {
            task.push_str("\n\n");
            task.push_str(&feedback);
        }
        let projection = extended.read_only_projection(self.patch_strategy);
        let prompt = self
            .prepare_with_repo_map(
                PromptInput {
                    task,
                    context_items: Vec::new(),
                    originals: Vec::new(),
                    current_files: Some(extended.files().to_vec()),
                    history: None,
                    capabilities,
                    brief,
                    sequence: PromptSequence::Assemble { continuation: None },
                    tail: vec![ChatMessage::user(projection)],
                    excluded_disk_sources: Vec::new(),
                },
                None,
            )
            .await?;
        Ok(PreparedEvidenceRound {
            prompt,
            evidence: extended,
        })
    }

    pub async fn bind_ready(
        mut self,
        root: &Path,
        snapshot: &crate::config::AppConfig,
        cancel: Option<&HardCancellation>,
    ) -> Result<ReadyPromptExecutor, ExecutionError> {
        let environment = self.bind_environment(root, snapshot, cancel).await?.clone();
        Ok(ReadyPromptExecutor(self, environment))
    }

    pub fn repo_root(&self) -> Result<&Path, ExecutionError> {
        self.environment
            .as_ref()
            .map(|environment| environment.root.as_path())
            .ok_or_else(|| ExecutionError::Other("prompt executor has no bound environment".into()))
    }

    pub fn new(
        provider: Arc<dyn LlmProvider>,
        protocol: Box<dyn PromptProtocol>,
        identity: PromptIdentity,
        session_id: Option<String>,
        patch_strategy: PatchStrategy,
    ) -> Self {
        Self {
            provider,
            protocol,
            identity,
            session_id,
            patch_strategy,
            max_input_tokens: crate::config::RequestConfig::default().max_input_tokens,
            cache: TurnInputCache::default(),
            environment: None,
            usage: None,
            transcript: Arc::new(Mutex::new(TurnTranscript::default())),
            accepted: crate::execution::accepted_rounds::AcceptedRounds::default(),
            evidence: None,
            admitted_requests: 0,
            last_accepted_round: None,
            last_response_had_scratchpad: false,
        }
    }

    pub fn admitted_requests(&self) -> usize {
        self.admitted_requests
    }

    pub fn last_accepted_round(&self) -> Option<crate::execution::AcceptedRoundId> {
        self.last_accepted_round
    }

    pub fn last_response_had_scratchpad(&self) -> bool {
        self.last_response_had_scratchpad
    }

    pub fn evidence_snapshot(&self) -> Option<ResearchEvidence> {
        self.evidence.clone()
    }

    /// Recovery exposes sanitized records only, never the private conversation.
    pub fn history_reader(
        &self,
    ) -> impl FnOnce() -> Vec<crate::history::TurnRoundRecord> + Send + 'static {
        self.accepted.reader()
    }

    /// Bind accounting without invalidating cached inputs or model rebinding.
    pub fn with_usage(mut self, usage: UsageBinding) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Changes the identity of subsequent requests without dropping memoized inputs.
    pub fn rebind(
        &mut self,
        provider: Arc<dyn LlmProvider>,
        identity: PromptIdentity,
        session_id: Option<String>,
    ) {
        self.provider = provider;
        self.identity = identity;
        self.session_id = session_id;
    }

    pub fn identity(&self) -> &PromptIdentity {
        &self.identity
    }

    pub fn patch_strategy(&self) -> PatchStrategy {
        self.patch_strategy
    }

    /// Bind once to the repository and operation snapshot. Model rebinding
    /// does not invalidate the frozen materials or the per-turn cache.
    pub async fn bind_environment(
        &mut self,
        root: &Path,
        snapshot: &crate::config::AppConfig,
        cancel: Option<&HardCancellation>,
    ) -> Result<&PromptEnvironment, ExecutionError> {
        if cancel.is_some_and(HardCancellation::is_cancelled) {
            return Err(ExecutionError::Cancelled);
        }
        if let Some(environment) = &self.environment {
            if environment.root != root {
                return Err(ExecutionError::Other(
                    "executor is already bound to another repository".into(),
                ));
            }
        } else {
            let environment = self
                .cache
                .bind_environment(root, snapshot, cancel)
                .await
                .map_err(|error| ExecutionError::Other(error.to_string()))?;
            if cancel.is_some_and(HardCancellation::is_cancelled) {
                return Err(ExecutionError::Cancelled);
            }
            self.environment = Some(environment);
        }
        if cancel.is_some_and(HardCancellation::is_cancelled) {
            return Err(ExecutionError::Cancelled);
        }
        self.environment
            .as_ref()
            .ok_or_else(|| ExecutionError::Other("prompt executor has no bound environment".into()))
    }

    pub fn environment(&self) -> Option<&PromptEnvironment> {
        self.environment.as_ref()
    }

    /// Fetch admitted files asynchronously, then use the same synchronous
    /// assembly and full-request admission as the legacy entry point.
    async fn prepare(&mut self, input: PromptInput) -> Result<PreparedPrompt, ExecutionError> {
        let repo_map = self
            .environment
            .as_ref()
            .ok_or_else(|| {
                ExecutionError::Other("prompt executor has no bound environment".into())
            })?
            .repo_map
            .clone();
        self.prepare_with_repo_map(input, repo_map).await
    }

    async fn prepare_with_repo_map(
        &mut self,
        input: PromptInput,
        repo_map: Option<String>,
    ) -> Result<PreparedPrompt, ExecutionError> {
        let environment = self.environment.as_ref().ok_or_else(|| {
            ExecutionError::Other("prompt executor has no bound environment".into())
        })?;
        let root = environment.root.clone();
        let project = environment.project.clone();
        let outer_docs = environment.outer_docs.clone();
        let current_files = match input.current_files {
            Some(files) => files,
            None => self
                .cache
                .context_files(&root, &input.context_items)
                .await
                .map_err(|error| ExecutionError::Other(error.to_string()))?,
        };
        PromptBinding {
            provider: Arc::clone(&self.provider),
            protocol: self.protocol.as_ref(),
            identity: &self.identity,
            session_id: self.session_id.as_deref(),
            patch_strategy: self.patch_strategy,
            max_input_tokens: self.max_input_tokens,
            usage: self.usage.clone().ok_or_else(|| {
                ExecutionError::Other("prompt executor has no usage binding".into())
            })?,
        }
        .prepare(
            &mut self.cache,
            RequestMaterials {
                task: input.task,
                originals: input.originals,
                current_files,
                history: input.history,
                capabilities: input.capabilities,
                project,
                repo_root: root,
                repo_map,
                outer_docs,
                brief: input.brief,
                sequence: input.sequence,
                tail: input.tail,
                excluded_disk_sources: input.excluded_disk_sources,
            },
        )
    }

    /// Bind the frozen operation's input ceiling without dropping cached inputs.
    pub fn with_max_input_tokens(mut self, limit: usize) -> Self {
        self.max_input_tokens = limit;
        self
    }

    /// Report the frozen outline's estimated size without starting a turn round.
    pub fn emit_repo_map_tokens(&self, events: &EventSink) {
        let tokens = self
            .environment
            .as_ref()
            .and_then(|environment| environment.repo_map.as_deref())
            .map(crate::prompt::estimate_tokens)
            .unwrap_or(0);
        events.emit(crate::providers::StreamEvent::RepoMapTokens(tokens));
    }
}

impl<'a> PromptBinding<'a> {
    fn prepare(
        self,
        cache: &mut TurnInputCache,
        input: RequestMaterials,
    ) -> Result<PreparedPrompt, ExecutionError> {
        if self.identity.model.name.trim().is_empty() {
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
        super::budget::admit(
            &LlmRequest {
                model: &self.identity.model.name,
                messages: messages.clone(),
                response_format: response_format.clone(),
                reasoning_effort: self.identity.effort,
                session_id: self.session_id,
            },
            self.provider.kind(),
            self.max_input_tokens,
        )?;
        Ok(PreparedPrompt {
            request: PreparedRequest {
                provider: self.provider,
                model: self.identity.model.name.clone(),
                session_id: self.session_id.map(str::to_owned),
                reasoning_effort: self.identity.effort,
                messages,
                response_format,
                filter,
                max_input_tokens: self.max_input_tokens,
                usage: self.usage,
            },
            strategy: self.patch_strategy,
            seed_messages,
            decoder,
            paths,
        })
    }
}

impl PreparedPrompt {
    /// Freshly rendered prefix and task, for a caller to retain in its own
    /// transcript. Reading this never registers a round or changes the request.
    fn seed_messages(&self) -> Option<&[ChatMessage]> {
        self.seed_messages.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmCapability;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::{MockLlmProvider, MockResponse};
    use crate::stop::StopSource;

    #[tokio::test]
    async fn hard_stop_refuses_environment_binding_before_request_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(MockLlmProvider::new());
        let stop = StopSource::new();
        stop.request_hard();
        let hard = stop.token().hard_cancellation();
        let mut executor = executor(provider.clone(), "mock/model");

        assert!(matches!(
            executor
                .bind_environment(dir.path(), &Default::default(), Some(&hard))
                .await,
            Err(ExecutionError::Cancelled)
        ));
        assert!(executor.environment().is_none());
        assert!(provider.requests().is_empty());
    }

    struct RecordingUsage(std::sync::Mutex<Vec<crate::providers::ModelUsageInfo>>);

    impl super::super::usage::UsageSink for RecordingUsage {
        fn record(&self, report: &crate::providers::ModelUsageInfo) {
            self.0.lock().unwrap().push(report.clone());
        }
    }

    fn input() -> ExecuteInput<'static> {
        ExecuteInput {
            message: PromptMessage::Task("original task".into()),
            context_items: vec![],
            history: None,
            capabilities: CapabilityTree::leaf(LlmCapability::TextMessage),
            brief: None,
            feedback: None,
            directives: None,
            staged: None,
            created_paths: vec![],
            round: Some(RequestRound {
                round: 1,
                max_rounds: 4,
                reason: None,
            }),
            journal: None,
        }
    }

    fn preparation_input() -> PromptInput {
        PromptInput {
            task: "original task".into(),
            context_items: vec![],
            originals: vec![],
            current_files: Some(vec![]),
            history: None,
            capabilities: input().capabilities,
            brief: None,
            sequence: PromptSequence::Assemble { continuation: None },
            tail: vec![],
            excluded_disk_sources: vec![],
        }
    }

    fn executor(provider: Arc<dyn LlmProvider>, model: &str) -> PromptExecutor {
        let scope = super::super::usage::UsageScope::new(Arc::new(RecordingUsage(
            std::sync::Mutex::new(Vec::new()),
        )));
        scope.activate();
        PromptExecutor::new(
            provider,
            Box::new(JsonPromptProtocol::new(input().capabilities)),
            PromptIdentity::new(
                ModelRef::openrouter(model),
                Some(ReasoningEffort::High),
                None,
                None,
            ),
            Some("executor-test-session".into()),
            PatchStrategy::SearchReplace,
        )
        .with_usage(UsageBinding::Bound(scope))
    }

    #[tokio::test]
    async fn unbound_executor_refuses_before_provider_io() {
        let provider = Arc::new(MockLlmProvider::new());
        let mut executor = PromptExecutor::new(
            provider.clone(),
            Box::new(JsonPromptProtocol::new(input().capabilities)),
            PromptIdentity::new(ModelRef::openrouter("mock/model"), None, None, None),
            None,
            PatchStrategy::SearchReplace,
        );
        let dir = tempfile::tempdir().unwrap();
        executor
            .bind_environment(dir.path(), &Default::default(), None)
            .await
            .unwrap();
        assert!(
            matches!(executor.execute(input(), &EventSink::noop(), &CancellationToken::never()).await, Err(ExecutionError::Other(message)) if message.contains("usage binding"))
        );
        assert!(provider.requests().is_empty());
    }

    #[tokio::test]
    async fn preparation_failure_never_calls_the_provider() {
        let provider = Arc::new(MockLlmProvider::new());
        let dir = tempfile::tempdir().unwrap();
        let mut executor = executor(provider.clone(), "");
        executor
            .bind_environment(dir.path(), &Default::default(), None)
            .await
            .unwrap();
        assert!(executor
            .execute(input(), &EventSink::noop(), &CancellationToken::never())
            .await
            .is_err());
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
            MockResponse::text(r#"{"message":"second"}"#),
        ]));
        let mut first = input();
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
        executor
            .bind_environment(root, &Default::default(), None)
            .await
            .unwrap();
        assert!(provider.requests().is_empty());
        let events = EventSink::noop();
        let cancel = CancellationToken::never();
        executor.execute(first, &events, &cancel).await.unwrap();
        context
            .add_file("helper.txt", crate::context::ContextAccess::ReadOnly)
            .unwrap();
        let mut next = input();
        next.message = PromptMessage::Continuation;
        next.context_items = context.get_state().items.clone();
        next.round = Some(RequestRound {
            round: 2,
            max_rounds: 4,
            reason: None,
        });
        assert_eq!(provider.requests().len(), 1);
        executor.execute(next, &events, &cancel).await.unwrap();
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
        assert!(requests[1]
            .messages
            .iter()
            .any(|m| m.content == "original task"));
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
    async fn identity_rebinding_updates_requests_without_rebuilding_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::init_repository(Some(root), true).unwrap();
        let first_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"first"}"#,
        )]));
        let second_provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            r#"{"message":"second"}"#,
        )]));
        let mut executor = executor(first_provider.clone(), "mock/model");
        executor
            .bind_environment(root, &Default::default(), None)
            .await
            .unwrap();
        let first = executor.identity().clone();
        let events = EventSink::noop();
        let cancel = CancellationToken::never();
        executor.execute(input(), &events, &cancel).await.unwrap();
        assert_eq!(
            first_provider.last_request().unwrap().model,
            first.model().name
        );
        assert_eq!(
            first_provider.last_request().unwrap().reasoning_effort,
            first.effort()
        );

        let next = PromptIdentity::new(
            ModelRef::openrouter("mock/model"),
            Some(ReasoningEffort::Low),
            Some(ModelRole::Architect),
            Some("New alias".into()),
        );
        executor.rebind(
            second_provider.clone(),
            next.clone(),
            Some("next-session".into()),
        );
        assert_eq!(executor.identity(), &next);
        assert_eq!(executor.repo_root().unwrap(), root);
        let mut second_input = input();
        second_input.message = PromptMessage::Continuation;
        second_input.round = Some(RequestRound {
            round: 2,
            max_rounds: 4,
            reason: None,
        });
        executor
            .execute(second_input, &events, &cancel)
            .await
            .unwrap();
        let request = second_provider.last_request().unwrap();
        assert_eq!(request.model, next.model().name);
        assert_eq!(request.reasoning_effort, next.effort());
        assert_eq!(request.session_id.as_deref(), Some("next-session"));
        assert_eq!(first_provider.requests().len(), 1);
        assert_eq!(second_provider.requests().len(), 1);
        assert_eq!(executor.identity().role(), Some(ModelRole::Architect));
        assert_eq!(executor.identity().alias(), Some("New alias"));
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
        let dir = tempfile::tempdir().unwrap();
        executor
            .bind_environment(dir.path(), &Default::default(), None)
            .await
            .unwrap();
        for (index, expected) in ["empty", "completed", "defect"].into_iter().enumerate() {
            let mut input = input();
            input.capabilities = capabilities.clone();
            input.round = Some(RequestRound {
                round: index + 1,
                max_rounds: 4,
                reason: None,
            });
            let result = executor
                .execute(input, &EventSink::noop(), &CancellationToken::never())
                .await;
            assert_eq!(
                match result {
                    Err(ExecutionError::EmptyResponse) => "empty",
                    Ok(commands) => {
                        assert!(commands.is_empty());
                        "completed"
                    }
                    Err(ExecutionError::Protocol(_)) => "defect",
                    Err(error) => panic!("unexpected error: {error}"),
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
            _: HardCancellation,
        ) -> crate::providers::ProviderResult {
            tx.send(crate::providers::StreamEvent::TextDelta("partial".into()))
                .await
                .unwrap();
            Ok(())
        }
    }

    #[tokio::test]
    async fn incomplete_stream_is_never_decoded_as_a_completed_response() {
        let dir = tempfile::tempdir().unwrap();
        let mut executor = executor(Arc::new(Interrupted), "mock/model");
        executor
            .bind_environment(dir.path(), &Default::default(), None)
            .await
            .unwrap();
        let result = executor
            .execute(input(), &EventSink::noop(), &CancellationToken::never())
            .await;
        assert!(matches!(result, Err(ExecutionError::Incomplete)));
    }

    #[tokio::test]
    async fn oversized_prompt_never_calls_provider() {
        let provider = Arc::new(MockLlmProvider::new());
        let mut executor = executor(provider.clone(), "mock/model").with_max_input_tokens(1);
        let dir = tempfile::tempdir().unwrap();
        executor
            .bind_environment(dir.path(), &Default::default(), None)
            .await
            .unwrap();
        let result = executor
            .execute(input(), &EventSink::noop(), &CancellationToken::never())
            .await;
        assert!(matches!(result, Err(ExecutionError::InputLimit(_))));
        assert!(provider.requests().is_empty());
    }

    #[tokio::test]
    async fn preparation_admits_response_schema_with_the_complete_request() {
        let provider = Arc::new(MockLlmProvider::new());
        let mut executor = executor(provider.clone(), "mock/model");
        let dir = tempfile::tempdir().unwrap();
        executor
            .bind_environment(dir.path(), &Default::default(), None)
            .await
            .unwrap();
        let prepared = executor.prepare(preparation_input()).await.unwrap();
        let request = &prepared.request;
        let without_schema = super::super::budget::measure(
            &LlmRequest {
                model: &request.model,
                messages: request.messages.clone(),
                response_format: None,
                reasoning_effort: request.reasoning_effort,
                session_id: request.session_id.as_deref(),
            },
            provider.kind(),
        )
        .unwrap()
        .total;
        let complete = super::super::budget::measure(
            &LlmRequest {
                model: &request.model,
                messages: request.messages.clone(),
                response_format: request.response_format.clone(),
                reasoning_effort: request.reasoning_effort,
                session_id: request.session_id.as_deref(),
            },
            provider.kind(),
        )
        .unwrap()
        .total;
        assert!(complete > without_schema);
        drop(prepared);
        let mut limited = executor.with_max_input_tokens(without_schema);
        assert!(matches!(
            limited.execute(input(), &EventSink::noop(), &CancellationToken::never()).await,
            Err(ExecutionError::InputLimit(error)) if error.size == complete
        ));
        assert!(provider.requests().is_empty());
    }
}
