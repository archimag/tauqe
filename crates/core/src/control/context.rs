//! Ambient execution context for hierarchical control loops.

use std::path::Path;
use std::sync::Arc;

use tauqe_protocol::{CapabilityExpr, LlmCapability};
use tokio::sync::{mpsc, watch};

use super::outcome::{ResponseEnvelope, SystemError};
use super::prompt::PromptBuilder;
use crate::context::ContextManager;
use crate::edits::StagedEditsState;
use crate::history::HistorySnapshot;
use crate::prompt::TurnBrief;
use crate::providers::types::StreamEvent;
use crate::workflow::policy::ApplyPolicy;
use crate::workflow::research::journal::TurnResearchJournal;

/// Cooperative cancellation token raced across asynchronous control loops.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    rx: watch::Receiver<bool>,
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
        Self {
            rx,
            tx: Some(Arc::new(tx)),
        }
    }

    /// Wraps an existing `watch::Receiver<bool>` channel from parent infrastructure.
    pub fn from_receiver(rx: watch::Receiver<bool>) -> Self {
        Self { rx, tx: None }
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Signals cancellation on this token if it owns the sender.
    pub fn cancel(&self) {
        if let Some(ref tx) = self.tx {
            let _ = tx.send(true);
        }
    }

    /// Asynchronously waits until cancellation is signalled.
    pub async fn cancelled(&self) {
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

    /// Clones the underlying `watch::Receiver<bool>`.
    pub fn receiver(&self) -> watch::Receiver<bool> {
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
    pub fn from_sender(sender: mpsc::UnboundedSender<StreamEvent>) -> Self {
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
    pub fn emit_reasoning_delta(&self, text: impl Into<String>) {
        self.emit(StreamEvent::ReasoningDelta(text.into()));
    }
}

/// Ambient execution context passed by reference through hierarchical control loops.
pub struct LoopContext<'a> {
    /// Cooperative cancellation token.
    pub cancel: &'a CancellationToken,
    /// Domain stream event sink.
    pub events: &'a EventSink,
    /// Root of the working tree every loop addresses.
    pub repo_root: &'a Path,
    /// Live context a loop resolves paths and snapshots through.
    pub context_manager: &'a mut ContextManager,
    pub journal: &'a mut TurnResearchJournal,
    /// Transactional apply policy governing checkpoints, step commits, and rollback.
    pub policy: &'a mut dyn ApplyPolicy,
    /// Active model capability surface for the current execution branch.
    pub capabilities: CapabilityExpr,
    /// Model round assembly and execution builder.
    pub(crate) prompt_builder: PromptBuilder<'a>,
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
        capabilities: CapabilityExpr,
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
        }
    }

    /// Derives a child execution context with a replaced capability surface via reborrow.
    pub fn reborrow_with_capabilities<'b>(
        &'b mut self,
        capabilities: CapabilityExpr,
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
        }
    }

    /// Derives a child execution context extending capabilities via reborrow.
    pub fn reborrow_extending_capabilities<'b>(
        &'b mut self,
        extra: impl IntoIterator<Item = LlmCapability>,
    ) -> LoopContext<'b> {
        let mut expr = self.capabilities.clone();
        for capability in extra {
            if !expr.contains(capability) {
                expr = CapabilityExpr::set([expr, CapabilityExpr::leaf(capability)]);
            }
        }
        self.reborrow_with_capabilities(expr)
    }

    /// Sets turn scratchpad notes on the underlying prompt builder.
    pub fn set_scratchpad(&mut self, scratchpad: Option<String>) -> &mut Self {
        self.prompt_builder.set_scratchpad(scratchpad);
        self
    }

    /// Sets the introductory brief of the turn on the underlying prompt builder.
    ///
    /// The brief belongs to the loop that drives the turn, so the contour and the
    /// opening text travel as typed values instead of a rendering flag.
    pub fn set_turn_brief(&mut self, brief: Option<TurnBrief>) -> &mut Self {
        self.prompt_builder.set_turn_brief(brief);
        self
    }

    /// Sets the history snapshot on the underlying prompt builder.
    pub fn set_history(&mut self, history: Option<&'a HistorySnapshot>) -> &mut Self {
        self.prompt_builder.set_history(history);
        self
    }

    /// Sets repo map configuration parameters on the underlying prompt builder.
    pub fn set_repo_map_config(
        &mut self,
        include_repo_map: bool,
        token_budget: usize,
    ) -> &mut Self {
        self.prompt_builder
            .set_repo_map_config(include_repo_map, token_budget);
        self
    }

    /// Requests a standard model round without repair diagnostics.
    pub async fn request_round(&mut self) -> Result<ResponseEnvelope, SystemError> {
        super::prompt::execute_round(self, None, None).await
    }

    /// Requests a verification healing round with compiler/linter error report.
    pub async fn request_healing(
        &mut self,
        healing_prompt: &str,
    ) -> Result<ResponseEnvelope, SystemError> {
        super::prompt::execute_round(self, Some(healing_prompt), None).await
    }

    /// Requests an in-memory patch retry round with virtual staging failure report.
    pub async fn request_repair(
        &mut self,
        repair_prompt: &str,
        staged_state: Option<&StagedEditsState>,
    ) -> Result<ResponseEnvelope, SystemError> {
        super::prompt::execute_round(self, Some(repair_prompt), staged_state).await
    }

    /// Checks whether cancellation was requested, returning [`SystemError::Cancelled`] if so.
    pub fn check_cancelled(&self) -> Result<(), SystemError> {
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
    use crate::prompt::json::JsonPromptProtocol;
    use crate::providers::mock::MockLlmProvider;
    use tauqe_protocol::ModelRef;

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
        let base_caps = CapabilityExpr::set([CapabilityExpr::leaf(LlmCapability::TextMessage)]);
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
            LlmCapability::ToolchainVerify,
        ]);
        assert!(derived.capabilities.contains(LlmCapability::TextMessage));
        assert!(derived.capabilities.contains(LlmCapability::EditReplace));
        assert!(derived
            .capabilities
            .contains(LlmCapability::ToolchainVerify));

        let replaced =
            derived.reborrow_with_capabilities(CapabilityExpr::set([CapabilityExpr::leaf(
                LlmCapability::ContextRequest,
            )]));
        assert!(replaced
            .capabilities
            .contains(LlmCapability::ContextRequest));
        assert!(!replaced.capabilities.contains(LlmCapability::EditReplace));
        assert_eq!(replaced.repo_root, dir.path());
    }
}
