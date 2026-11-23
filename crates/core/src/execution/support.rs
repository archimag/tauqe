//! Cancellation, event delivery and existing read-only request contracts.

use crate::providers::{ResponseFormat, StreamEvent};
use tokio::sync::{mpsc, watch};

/// Cooperative cancellation signal, independent of a workflow lifecycle.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    rx: watch::Receiver<bool>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        let (_, rx) = watch::channel(false);
        Self { rx }
    }

    pub fn from_receiver(rx: watch::Receiver<bool>) -> Self {
        Self { rx }
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    pub fn receiver(&self) -> watch::Receiver<bool> {
        self.rx.clone()
    }
}

/// Semantic event delivery without a transport or presentation dependency.
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
    pub fn noop() -> Self {
        Self { sender: None }
    }

    pub fn channel() -> (Self, mpsc::UnboundedReceiver<StreamEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { sender: Some(tx) }, rx)
    }

    pub fn emit(&self, event: StreamEvent) {
        if let Some(ref sender) = self.sender {
            let _ = sender.send(event);
        }
    }

    pub fn emit_text_delta(&self, text: impl Into<String>) {
        self.emit(StreamEvent::TextDelta(text.into()));
    }
}

/// Existing typed read-only contract; its scenario runner remains separate.
pub struct StructuredContract<T> {
    pub(crate) instructions: String,
    pub(crate) format: ResponseFormat,
    pub(crate) decode: fn(&str) -> Result<T, String>,
    pub(crate) visibility: StructuredVisibility,
    pub(crate) require_done: bool,
}

#[derive(Clone, Copy)]
pub enum StructuredVisibility {
    Silent,
    /// Publish only the decoded contents of this JSON string field.
    TextField(&'static str),
}
