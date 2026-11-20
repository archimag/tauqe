//! Ambient execution context for hierarchical control loops.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::llm::CapabilityTree;
#[cfg(test)]
use crate::llm::LlmCapability;
use tokio::sync::{mpsc, watch};

use super::outcome::{ResponseEnvelope, SystemError};
use super::prompt::PromptBuilder;
pub use super::transcript::TurnTranscript;
use crate::context::ContextManager;
use crate::edits::StagedEditsState;
use crate::providers::types::{StreamEvent, TurnRequestReason};
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::research::journal::TurnResearchJournal;

/// Cooperative cancellation token raced across asynchronous control loops.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    rx: watch::Receiver<bool>,
    #[cfg(test)]
    tx: Option<Arc<watch::Sender<bool>>>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// Creates a new standalone cancellation token.
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        #[cfg(not(test))]
        let _ = tx;
        Self {
            rx,
            #[cfg(test)]
            tx: Some(Arc::new(tx)),
        }
    }

    /// Wraps an existing `watch::Receiver<bool>` channel from parent infrastructure.
    pub fn from_receiver(rx: watch::Receiver<bool>) -> Self {
        Self {
            rx,
            #[cfg(test)]
            tx: None,
        }
    }

    /// Returns whether cancellation has been requested.
    pub(crate) fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Signals cancellation on this token if it owns the sender.
    #[cfg(test)]
    pub(crate) fn cancel(&self) {
        if let Some(ref tx) = self.tx {
            let _ = tx.send(true);
        }
    }

    /// Asynchronously waits until cancellation is signalled.
    #[cfg(test)]
    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let mut rx = self.rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
    /// Clones the underlying `watch::Receiver<bool>` channel.
    pub(crate) fn receiver(&self) -> watch::Receiver<bool> {
        self.rx.clone()
    }
}

/// Typed event emitter channel isolating control loops from concrete transport.
#[derive(Clone)]
pub struct EventSink {
    sender: Option<mpsc::UnboundedSender<StreamEvent>>,
}

impl Default for EventSink {
    fn default() -> Self {
        Self::noop()
    }
}

impl EventSink {
    /// Creates a no-op event sink that discards all emitted events.
    pub fn noop() -> Self {
        Self { sender: None }
    }

    /// Wraps an unbounded MPSC sender.
    pub(crate) fn from_sender(sender: mpsc::UnboundedSender<StreamEvent>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    /// Creates an event sink paired with an unbounded receiver (ideal for testing).
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<StreamEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self::from_sender(tx), rx)
    }

    /// Emits a single domain stream event.
    pub fn emit(&self, event: StreamEvent) {
        if let Some(ref sender) = self.sender {
            let _ = sender.send(event);
        }
    }

    /// Convenience helper to emit textual response deltas.
    pub fn emit_text_delta(&self, text: impl Into<String>) {
        self.emit(StreamEvent::TextDelta(text.into()));
    }

    /// Convenience helper to emit reasoning deltas.
    #[cfg(test)]
    fn emit_reasoning_delta(&self, text: impl Into<String>) {
        self.emit(StreamEvent::ReasoningDelta(text.into()));
    }
}

/// Ambient execution context passed by reference through hierarchical control loops.
pub struct LoopContext<'a> {
    /// Cooperative cancellation token.
    pub(crate) cancel: &'a CancellationToken,
    /// Domain stream event sink.
    pub(crate) events: &'a EventSink,
    /// Root of the working tree every loop addresses.
    pub(crate) repo_root: &'a Path,
    /// Live context a loop resolves paths and snapshots through.
    pub(crate) context_manager: &'a mut ContextManager,
    pub(crate) journal: &'a mut TurnResearchJournal,
    /// Transactional apply policy governing checkpoints, step commits, and rollback.
    pub(crate) policy: &'a mut dyn ApplyPolicy,
    /// Active model capability surface for the current execution branch.
    pub(crate) capabilities: CapabilityTree,
    /// Model round assembly and execution builder.
    pub(crate) prompt_builder: PromptBuilder<'a>,
    /// Round transcript of the turn, shared with every reborrowed child context.
    pub(crate) transcript: Arc<Mutex<TurnTranscript>>,
    /// Monotonic turn round counter shared across all reborrows of the turn.
    pub(crate) rounds_spent: Arc<std::sync::atomic::AtomicUsize>,
}

impl<'a> LoopContext<'a> {
    /// Binds a temporary context to the transcript and call budget of its unit.
    pub(super) fn with_unit_state(
        mut self,
        transcript: Arc<Mutex<TurnTranscript>>,
        rounds_spent: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        self.transcript = transcript;
        self.rounds_spent = rounds_spent;
        self
    }

    /// Creates a new execution context.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cancel: &'a CancellationToken,
        events: &'a EventSink,
        repo_root: &'a Path,
        context_manager: &'a mut ContextManager,
        journal: &'a mut TurnResearchJournal,
        policy: &'a mut dyn ApplyPolicy,
        capabilities: CapabilityTree,
        prompt_builder: PromptBuilder<'a>,
    ) -> Self {
        Self {
            cancel,
            events,
            repo_root,
            context_manager,
            journal,
            policy,
            capabilities,
            prompt_builder,
            transcript: Arc::new(Mutex::new(TurnTranscript::default())),
            rounds_spent: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Derives a child execution context with a replaced capability surface via reborrow.
    pub(crate) fn reborrow_with_capabilities<'b>(
        &'b mut self,
        capabilities: CapabilityTree,
    ) -> LoopContext<'b> {
        // The next request binds this grant to every protocol projection.
        LoopContext {
            cancel: self.cancel,
            events: self.events,
            repo_root: self.repo_root,
            context_manager: self.context_manager,
            journal: self.journal,
            policy: self.policy,
            capabilities,
            prompt_builder: self.prompt_builder.reborrow(),
            transcript: Arc::clone(&self.transcript),
            rounds_spent: Arc::clone(&self.rounds_spent),
        }
    }

    /// Derives a child execution context extending capabilities via reborrow.
    #[cfg(test)]
    fn reborrow_extending_capabilities<'b>(
        &'b mut self,
        extra: impl IntoIterator<Item = LlmCapability>,
    ) -> LoopContext<'b> {
        let mut expr = self.capabilities.clone();
        for capability in extra {
            if !expr.contains(capability) {
                expr = CapabilityTree::set([expr, CapabilityTree::leaf(capability)]);
            }
        }
        self.reborrow_with_capabilities(expr)
    }
    /// Locks the round transcript shared with every reborrowed child context.
    ///
    /// The guard releases the lock when it is dropped, so a caller reads or appends
    /// a round without holding it across an await point.
    pub(crate) fn transcript(&self) -> MutexGuard<'_, TurnTranscript> {
        self.transcript
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns the number of physical LLM calls spent so far in this turn.
    pub(crate) fn rounds_spent(&self) -> usize {
        self.rounds_spent.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Sets the maximum turn round limit on the underlying prompt builder.
    pub(crate) fn set_turn_round_limit(&mut self, limit: Option<usize>) -> &mut Self {
        self.prompt_builder.set_turn_round_limit(limit);
        self
    }

    /// Opens a research round in the journal of the turn.
    ///
    /// The boundary narrows feedback to the findings of this round.
    pub(crate) fn begin_research_round(&mut self) {
        let next_index = self.journal.round_index() + 1;
        self.journal.begin_round(next_index);
    }

    /// Publishes the feedback of the round the journal currently holds.
    ///
    /// The feedback enters the transcript slot the next round appends as its
    /// user message, so the model reads what its own round delivered and never
    /// the investigation of an earlier one. A round that produced no record
    /// publishes nothing.
    pub(crate) fn publish_round_feedback(&mut self) {
        let feedback = self.journal.render_round_feedback();
        if feedback.is_empty() {
            return;
        }
        self.transcript().set_pending_feedback(feedback);
    }

    /// Requests a standard model round without repair diagnostics.
    pub(crate) async fn request_round(&mut self) -> Result<ResponseEnvelope, SystemError> {
        super::prompt::execute_round(self, None, None).await
    }

    /// Requests a verification healing round with compiler/linter error report.
    ///
    /// The report enters the single feedback slot of the transcript, so it reaches the
    /// model as the user message that follows the answer of the failed round instead of
    /// reshaping the request the turn already sent.
    pub(crate) async fn request_healing(
        &mut self,
        healing_prompt: &str,
        reason: TurnRequestReason,
    ) -> Result<ResponseEnvelope, SystemError> {
        self.transcript().set_pending_feedback(healing_prompt);
        super::prompt::execute_round(self, None, Some(reason)).await
    }

    /// Requests an in-memory patch retry round with virtual staging failure report.
    ///
    /// The report enters the single feedback slot of the transcript, exactly as a
    /// verification report does, so a retry round appends its pair to the turn instead
    /// of re-rendering the material the model already read.
    pub(crate) async fn request_repair(
        &mut self,
        repair_prompt: &str,
        staged_state: Option<&StagedEditsState>,
    ) -> Result<ResponseEnvelope, SystemError> {
        self.transcript().set_pending_feedback(repair_prompt);
        super::prompt::execute_round(self, staged_state, Some(TurnRequestReason::PatchRepair)).await
    }

    /// Checks whether cancellation was requested, returning [`SystemError::Cancelled`] if so.
    pub(crate) fn check_cancelled(&self) -> Result<(), SystemError> {
        if self.cancel.is_cancelled() {
            Err(SystemError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelRef;
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::MockLlmProvider;
    use crate::providers::types::ChatMessage;

    #[test]
    fn test_cancellation_token_signalling() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancellation_token_async_wait() {
        let token = CancellationToken::new();
        let token_clone = token.clone();

        let handle = tokio::spawn(async move {
            token_clone.cancelled().await;
        });

        assert!(!token.is_cancelled());
        token.cancel();
        handle.await.expect("task finished");
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_event_sink_channel() {
        let (sink, mut rx) = EventSink::channel();
        sink.emit_text_delta("hello");
        sink.emit_reasoning_delta("thinking");

        match rx.try_recv() {
            Ok(StreamEvent::TextDelta(t)) => assert_eq!(t, "hello"),
            other => panic!("unexpected event: {other:?}"),
        }
        match rx.try_recv() {
            Ok(StreamEvent::ReasoningDelta(r)) => assert_eq!(r, "thinking"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn test_capability_derivation() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let mut context = ContextManager::new(dir.path().to_path_buf());
        let mut journal = TurnResearchJournal::default();
        let provider = MockLlmProvider::new();
        let model = ModelRef::openrouter("mock/model");
        let protocol =
            JsonPromptProtocol::new(CapabilityTree::from_leaves([LlmCapability::TextMessage]));
        let cancel = CancellationToken::new();
        let sink = EventSink::noop();
        let base_caps = CapabilityTree::set([CapabilityTree::leaf(LlmCapability::TextMessage)]);
        let mut cache = crate::control::cache::TurnInputCache::default();
        let prompt_builder = PromptBuilder::new(&provider, &model, &protocol, &mut cache);
        let mut policy = crate::workflow::policy::DirectApplyPolicy::new();
        let mut ctx = LoopContext::new(
            &cancel,
            &sink,
            dir.path(),
            &mut context,
            &mut journal,
            &mut policy,
            base_caps,
            prompt_builder,
        );

        assert!(ctx.capabilities.contains(LlmCapability::TextMessage));
        assert!(!ctx.capabilities.contains(LlmCapability::EditReplace));
        let mut derived = ctx.reborrow_extending_capabilities([
            LlmCapability::EditReplace,
            LlmCapability::ContextRequest,
        ]);
        assert!(derived.capabilities.contains(LlmCapability::TextMessage));
        assert!(derived.capabilities.contains(LlmCapability::EditReplace));
        assert!(derived.capabilities.contains(LlmCapability::ContextRequest));

        let replaced =
            derived.reborrow_with_capabilities(CapabilityTree::set([CapabilityTree::leaf(
                LlmCapability::ContextRequest,
            )]));
        assert!(replaced
            .capabilities
            .contains(LlmCapability::ContextRequest));
        assert!(!replaced.capabilities.contains(LlmCapability::EditReplace));
        assert_eq!(replaced.repo_root, dir.path());
    }

    fn sample_task_message() -> ChatMessage {
        ChatMessage::user("implement the step")
    }

    fn seeded_transcript() -> TurnTranscript {
        let mut transcript = TurnTranscript::default();
        transcript.seed(
            vec![ChatMessage::system("contract")],
            sample_task_message(),
            &CapabilityTree::from_leaves([LlmCapability::EditReplace]),
            None,
        );
        transcript
    }

    #[test]
    fn test_turn_transcript_seed_freezes_prefix_and_task() {
        let mut transcript = TurnTranscript::default();
        assert!(!transcript.is_seeded());
        assert!(transcript.messages().is_empty());
        assert_eq!(transcript.take_pending_feedback(), None);

        transcript.seed(
            vec![
                ChatMessage::system("contract"),
                ChatMessage::user("context"),
            ],
            sample_task_message(),
            &CapabilityTree::from_leaves([LlmCapability::EditReplace]),
            None,
        );

        assert!(transcript.is_seeded());
        assert_eq!(
            transcript.messages(),
            vec![
                ChatMessage::system("contract"),
                ChatMessage::user("context"),
                sample_task_message(),
            ]
        );
    }

    fn discover(transcript: &mut TurnTranscript, path: &str) {
        transcript.remember_files(&[crate::context::ContextFileContent {
            path: path.into(),
            content: "discovered".into(),
            access: crate::context::ContextAccess::ReadOnly,
            insertion_seq: 0,
        }]);
        transcript.append_discoveries(crate::workflow::PatchStrategy::ApplyPatch);
    }

    #[test]
    fn discovered_files_are_delivered_once_across_reseeding() {
        let mut transcript = seeded_transcript();
        discover(&mut transcript, "helper.rs");
        discover(&mut transcript, "helper.rs");
        let messages = transcript.messages();
        assert_eq!(messages.len(), 3);
        assert!(messages[2].content.contains("helper.rs"));

        let prefix = vec![ChatMessage::system("new contract"), messages[2].clone()];
        transcript.reseed_prefix(
            prefix.clone(),
            &CapabilityTree::from_leaves([LlmCapability::EditReplace]),
            None,
        );
        discover(&mut transcript, "helper.rs");
        let mut expected = prefix;
        expected.push(sample_task_message());
        assert_eq!(transcript.messages(), expected);
    }

    #[test]
    fn test_turn_transcript_is_shared_across_reborrows() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let mut context = ContextManager::new(dir.path().to_path_buf());
        let mut journal = TurnResearchJournal::default();
        let provider = MockLlmProvider::new();
        let model = ModelRef::openrouter("mock/model");
        let protocol =
            JsonPromptProtocol::new(CapabilityTree::from_leaves([LlmCapability::TextMessage]));
        let cancel = CancellationToken::new();
        let sink = EventSink::noop();
        let mut cache = crate::control::cache::TurnInputCache::default();
        let prompt_builder = PromptBuilder::new(&provider, &model, &protocol, &mut cache);
        let mut policy = crate::workflow::policy::DirectApplyPolicy::new();
        let mut ctx = LoopContext::new(
            &cancel,
            &sink,
            dir.path(),
            &mut context,
            &mut journal,
            &mut policy,
            CapabilityTree::leaf(LlmCapability::TextMessage),
            prompt_builder,
        );

        {
            let child = ctx.reborrow_extending_capabilities([LlmCapability::ContextRequest]);
            child.transcript().seed(
                vec![ChatMessage::system("contract")],
                sample_task_message(),
                &child.capabilities,
                None,
            );
            discover(&mut child.transcript(), "child.rs");
        }

        let transcript = ctx.transcript();
        assert!(transcript.is_seeded());
        let messages = transcript.messages();
        assert_eq!(messages.len(), 3);
        assert!(messages[2].content.contains("child.rs"));
    }
}
