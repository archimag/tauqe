use tauqe_protocol::{Event, Response};
use tokio::sync::mpsc;

/// Safe thread-safe event and response sink that routes output lines sequentially to stdout.
#[derive(Clone)]
pub struct OutChannel {
    tx: mpsc::UnboundedSender<String>,
}

impl OutChannel {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    pub fn send_response(&self, resp: &Response) {
        if let Ok(line) = serde_json::to_string(resp) {
            let _ = self.tx.send(line + "\n");
        }
    }

    pub fn send_event(&self, event: &Event) {
        if let Ok(line) = serde_json::to_string(event) {
            let _ = self.tx.send(line + "\n");
        }
    }
}
