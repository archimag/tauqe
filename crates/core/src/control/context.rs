//! Ambient execution context for hierarchical control loops.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::llm::CapabilityTree;
#[cfg(test)]
use crate::llm::LlmCapability;
use tokio::sync::{mpsc, watch};

use super::outcome::{ResponseEnvelope, SystemError};
use super::prompt::PromptBuilder;
use crate::context::ContextManager;
use crate::edits::StagedEditsState;
#[cfg(test)]
use crate::history::HistorySnapshot;
use crate::prompt::TurnBrief;
use crate::providers::types::{ChatMessage, StreamEvent};
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
    pub(crate) fn emit(&self, event: StreamEvent) {
        if let Some(ref sender) = self.sender {
            let _ = sender.send(event);
        }
    }

    /// Convenience helper to emit textual response deltas.
    pub(crate) fn emit_text_delta(&self, text: impl Into<String>) {
        self.emit(StreamEvent::TextDelta(text.into()));
    }

    /// Convenience helper to emit reasoning deltas.
    #[cfg(test)]
    fn emit_reasoning_delta(&self, text: impl Into<String>) {
        self.emit(StreamEvent::ReasoningDelta(text.into()));
    }
}

/// Round transcript of one bounded turn.
///
/// The transcript freezes the byte-stable head of the turn and lets every later
/// round append to what the model has already seen instead of rebuilding the
/// whole message sequence. It is shared through [`LoopContext`], so a round a
/// reborrowed child context appends stays visible to the context it came from.
///
/// The prefix is frozen inside a phase, not for the whole turn: a change set
/// that churned the context — the files it converged on and the files the
/// substrate refused — marks the churn epoch, and the next round reseeds the
/// head once from the current context, so the model reads the bytes it is
/// about to change instead of the rendering the turn opened with.
#[derive(Debug)]
pub struct TurnTranscript {
    /// System head of the turn, rendered when the transcript is seeded.
    prefix: Vec<ChatMessage>,
    /// Task message of the turn, frozen for the whole turn.
    task: ChatMessage,
    /// Assistant and user message pairs of the rounds already exchanged.
    rounds: Vec<ChatMessage>,
    /// Feedback the next round appends as its user message.
    pending_feedback: Option<String>,
    /// Serialized capability tree the prefix was seeded with.
    seed_signature: Option<String>,
    /// Churn epoch the rendered prefix was stamped with.
    ///
    /// Seeding and reseeding stamp the epoch the context had reached, so the
    /// prefix is stale exactly when the context churned after it was
    /// rendered.
    seed_churn: u64,
    /// Churn epoch the context has reached.
    ///
    /// A change set that rewrote context files marks the churn, and the next
    /// round reseeds the prefix so the model reads the bytes it is about to
    /// address instead of the bytes the turn opened with.
    context_churn: u64,
}

impl Default for TurnTranscript {
    fn default() -> Self {
        Self {
            prefix: Vec::new(),
            task: ChatMessage::user(String::new()),
            rounds: Vec::new(),
            pending_feedback: None,
            seed_signature: None,
            seed_churn: 0,
            context_churn: 0,
        }
    }
}

impl TurnTranscript {
    /// Seeds the transcript with the rendered head of the turn and its task message.
    /// Seeds the transcript with the rendered head of the turn and its task message.
    ///
    /// The first round of a turn seeds a snapshot of the assembly it was built from,
    /// so the prefix and the task message stay frozen for every round that follows.
    pub(crate) fn seed(
        &mut self,
        prefix: Vec<ChatMessage>,
        task: ChatMessage,
        capabilities: &CapabilityTree,
    ) {
        self.prefix = prefix;
        self.task = task;
        self.seed_signature = Some(serde_json::to_string(capabilities).unwrap_or_default());
        self.seed_churn = self.context_churn;
    }

    /// Replaces the rendered head of the turn and keeps the task and every round.
    ///
    /// A phase transition re-renders the capabilities the turn advertises, so the
    /// prefix changes while the material already exchanged stays byte-for-byte
    /// identical.
    pub(crate) fn reseed_prefix(&mut self, prefix: Vec<ChatMessage>) {
        self.prefix = prefix;
        self.seed_churn = self.context_churn;
    }

    /// Appends one round: the assistant message and the feedback of the next request.
    ///
    /// A round adds exactly two messages to the transcript and never rewrites a
    /// message an earlier round produced.
    #[cfg(test)]
    fn append_round(&mut self, assistant: impl Into<String>, user_feedback: impl Into<String>) {
        self.rounds.push(ChatMessage::assistant(assistant));
        self.rounds.push(ChatMessage::user(user_feedback));
    }
    /// Assembles the message sequence of the next round.
    ///
    /// The sequence is the prefix, the frozen task message, and every round already
    /// exchanged, so round `N + 1` sends exactly the messages of round `N` plus the
    /// pair the last round appended. A transcript that has not been seeded carries
    /// no message at all.
    pub(crate) fn messages(&self) -> Vec<ChatMessage> {
        if !self.is_seeded() {
            return Vec::new();
        }
        let mut messages = Vec::with_capacity(self.prefix.len() + 1 + self.rounds.len());
        messages.extend(self.prefix.iter().cloned());
        messages.push(self.task.clone());
        messages.extend(self.rounds.iter().cloned());
        messages
    }

    /// Reports whether the transcript has been seeded with a prefix and a task.
    pub(crate) fn is_seeded(&self) -> bool {
        self.seed_signature.is_some()
    }

    /// Reports whether the transcript advertises exactly this capability tree.
    ///
    /// A different tree is the phase boundary of the turn: the head the turn renders
    /// is rewritten once, while the task message and the rounds already exchanged keep
    /// their exact bytes.
    pub(crate) fn advertises(&self, capabilities: &CapabilityTree) -> bool {
        let signature = serde_json::to_string(capabilities).unwrap_or_default();
        self.seed_signature.as_deref() == Some(signature.as_str())
    }

    /// Reports whether the context churned after the prefix was rendered.
    ///
    /// A change set that rewrote context files — the members it converged on
    /// and the members the substrate refused — marks the churn, and the round
    /// that follows reseeds the prefix once so the model addresses the bytes
    /// the change set produced instead of the rendering the turn opened
    /// with.
    pub(crate) fn prefix_is_stale(&self) -> bool {
        self.context_churn != self.seed_churn
    }

    /// Marks the context as churned by the change set the turn just staged.
    ///
    /// The mark carries no payload of its own: relocating the named paths
    /// already moved the entries, and the next round that renders a request
    /// compares the epochs and reseeds the prefix once.
    fn mark_context_churn(&mut self) {
        self.context_churn += 1;
    }

    /// Appends the answer of the round that just finished.
    ///
    /// The answer is recorded as soon as it arrives, before the feedback of the round
    /// that follows exists, so a round a later contour requests replays it in order. A
    /// blank answer carries nothing the model can read back and is not recorded.
    pub(crate) fn push_assistant(&mut self, answer: impl Into<String>) {
        let answer = answer.into();
        if answer.trim().is_empty() {
            return;
        }
        self.rounds.push(ChatMessage::assistant(answer));
    }

    /// Appends the feedback the next round carries as its user message.
    ///
    /// The slot [`Self::set_pending_feedback`] fills travels here exactly once, so the
    /// sequence of a round is always its predecessor plus the pair it exchanged.
    pub(crate) fn push_feedback(&mut self, feedback: impl Into<String>) {
        let feedback = feedback.into();
        if feedback.trim().is_empty() {
            return;
        }
        self.rounds.push(ChatMessage::user(feedback));
    }

    /// Takes the feedback the next round appends as its user message.
    pub(crate) fn take_pending_feedback(&mut self) -> Option<String> {
        self.pending_feedback.take()
    }

    /// Records the feedback the next round appends as its user message.
    ///
    /// The slot is written by every producer of an additional round — the
    /// research journal, verification healing, patch retry, and the empty-round
    /// reminder — and taken exactly once by the round that appends the pair, so
    /// feedback never reaches the provider twice. Blank feedback carries no
    /// knowledge and leaves the slot untouched.
    pub(crate) fn set_pending_feedback(&mut self, feedback: impl Into<String>) {
        let feedback = feedback.into();
        if feedback.trim().is_empty() {
            return;
        }
        self.pending_feedback = Some(feedback);
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

    /// Sets the introductory brief of the turn on the underlying prompt builder.
    ///
    /// The brief belongs to the loop that drives the turn, so the contour and the
    /// opening text travel as typed values instead of a rendering flag.
    pub(crate) fn set_turn_brief(&mut self, brief: Option<TurnBrief>) -> &mut Self {
        self.prompt_builder.set_turn_brief(brief);
        self
    }

    /// Opens a research round in the journal of the turn.
    ///
    /// The boundary narrows what the round reports to the records that follow
    /// it, and the patch strategy of the turn travels with the boundary, so the
    /// files a round delivers render with the very anchors the context block
    /// carries.
    pub(crate) fn begin_research_round(&mut self) {
        let patch_strategy = self.prompt_builder.patch_strategy;
        self.journal.set_patch_strategy(patch_strategy);
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

    /// Marks the context as churned by the change set the turn just staged.
    ///
    /// The change set is the churn of the working set the model must read
    /// next: the members that converged relocate to the tail of their context
    /// layers first, the members the substrate refused follow them, and the
    /// relocation re-issues their arrival watermarks so the next assembly
    /// renders the hot group behind the last recorded turn. The churn mark
    /// also stales the rendered prefix, so the round that follows reseeds it
    /// and the model addresses the bytes it is about to change instead of the
    /// bytes the turn opened with.
    ///
    /// The mark is recorded whenever a path was named, even when no context
    /// layer holds it: a change set that creates a file outside the context
    /// still changes what the repair round must see, and the reseed the mark
    /// triggers costs no provider round of its own.
    pub(crate) fn note_context_churn(&mut self, converged: &[String], refused: &[String]) {
        let named: Vec<String> = converged.iter().chain(refused).cloned().collect();
        if named.is_empty() {
            return;
        }
        if self.context_manager.move_files_to_tail(&named) {
            self.events.emit(StreamEvent::ContextChanged(
                self.context_manager.get_state(),
            ));
        }
        self.transcript().mark_context_churn();
    }

    /// Marks the context as churned so the transcript prefix is reseeded.
    pub(crate) fn mark_context_churn(&mut self) {
        self.transcript().mark_context_churn();
    }

    /// Sets the history snapshot on the underlying prompt builder.
    #[cfg(test)]
    pub(crate) fn set_history(&mut self, history: Option<&'a HistorySnapshot>) -> &mut Self {
        self.prompt_builder.set_history(history);
        self
    }
    /// Requests a standard model round without repair diagnostics.
    pub(crate) async fn request_round(&mut self) -> Result<ResponseEnvelope, SystemError> {
        super::prompt::execute_round(self, None).await
    }

    /// Requests a verification healing round with compiler/linter error report.
    ///
    /// The report enters the single feedback slot of the transcript, so it reaches the
    /// model as the user message that follows the answer of the failed round instead of
    /// reshaping the request the turn already sent.
    pub(crate) async fn request_healing(
        &mut self,
        healing_prompt: &str,
    ) -> Result<ResponseEnvelope, SystemError> {
        self.transcript().set_pending_feedback(healing_prompt);
        super::prompt::execute_round(self, None).await
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
        super::prompt::execute_round(self, staged_state).await
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
        let protocol = JsonPromptProtocol::default();
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

    #[test]
    fn test_turn_transcript_appends_exactly_one_round() {
        let mut transcript = seeded_transcript();
        let seeded = transcript.messages();

        transcript.append_round("first answer", "search results");

        let mut expected = seeded;
        expected.push(ChatMessage::assistant("first answer"));
        expected.push(ChatMessage::user("search results"));
        assert_eq!(transcript.messages(), expected);
    }

    #[test]
    fn test_turn_transcript_reseed_replaces_prefix_only() {
        let mut transcript = seeded_transcript();
        transcript.append_round("first answer", "search results");

        transcript.reseed_prefix(vec![
            ChatMessage::system("contract"),
            ChatMessage::user("context"),
        ]);

        assert_eq!(
            transcript.messages(),
            vec![
                ChatMessage::system("contract"),
                ChatMessage::user("context"),
                sample_task_message(),
                ChatMessage::assistant("first answer"),
                ChatMessage::user("search results"),
            ]
        );
    }

    #[test]
    fn test_turn_transcript_is_shared_across_reborrows() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let mut context = ContextManager::new(dir.path().to_path_buf());
        let mut journal = TurnResearchJournal::default();
        let provider = MockLlmProvider::new();
        let model = ModelRef::openrouter("mock/model");
        let protocol = JsonPromptProtocol::default();
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
            );
            child
                .transcript()
                .append_round("child answer", "child feedback");
        }

        let transcript = ctx.transcript();
        assert!(transcript.is_seeded());
        let messages = transcript.messages();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2], ChatMessage::assistant("child answer"));
        assert_eq!(messages[3], ChatMessage::user("child feedback"));
    }

    #[test]
    fn a_context_churn_marks_the_rendered_prefix_stale_until_reseeded() {
        let mut transcript = seeded_transcript();
        assert!(!transcript.prefix_is_stale());

        transcript.mark_context_churn();
        assert!(transcript.prefix_is_stale());

        // The reseed stamps the epoch the context reached, so the prefix the
        // next round sends is current again.
        transcript.reseed_prefix(vec![ChatMessage::system("contract, revised")]);
        assert!(!transcript.prefix_is_stale());
    }

    #[test]
    fn a_churn_before_the_first_seed_is_absorbed_by_the_seed() {
        let mut transcript = TurnTranscript::default();
        transcript.mark_context_churn();
        assert!(!transcript.is_seeded());

        transcript.seed(
            vec![ChatMessage::system("contract")],
            sample_task_message(),
            &CapabilityTree::from_leaves([LlmCapability::EditReplace]),
        );
        assert!(!transcript.prefix_is_stale());
    }

    #[test]
    fn note_context_churn_relocates_the_named_files_in_the_given_order() {
        let dir = tempfile::tempdir().expect("scratch directory");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();

        let mut context = ContextManager::new(root);
        context
            .add_file("b.rs", crate::context::ContextAccess::ReadOnly)
            .unwrap();
        context
            .add_file("a.rs", crate::context::ContextAccess::Editable)
            .unwrap();

        let mut journal = TurnResearchJournal::default();
        let provider = MockLlmProvider::new();
        let model = ModelRef::openrouter("mock/model");
        let protocol = JsonPromptProtocol::default();
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
            CapabilityTree::from_leaves([LlmCapability::TextMessage]),
            prompt_builder,
        );

        // The transcript seeds a prefix first, so the churn that follows is
        // observable as staleness instead of being absorbed by the seed.
        ctx.transcript().seed(
            vec![ChatMessage::system("contract")],
            sample_task_message(),
            &ctx.capabilities,
        );
        assert!(!ctx.transcript().prefix_is_stale());

        // `a.rs` converged and `b.rs` was refused: both relocate, the
        // converged member ahead of the refused one.
        ctx.note_context_churn(&["a.rs".to_string()], &["b.rs".to_string()]);
        assert!(ctx.transcript().prefix_is_stale());
        let state = ctx.context_manager.get_state();
        let paths: Vec<&str> = state.items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "b.rs"]);

        // A churn that names no path marks nothing.
        let churn_epoch = ctx.transcript().context_churn;
        ctx.note_context_churn(&[], &[]);
        assert_eq!(ctx.transcript().context_churn, churn_epoch);
    }
}
