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
        match serde_json::to_string(resp) {
            Ok(line) => {
                let _ = self.tx.send(line + "\n");
            }
            Err(err) => tracing::error!("failed to serialize response {:?}: {}", resp.id, err),
        }
    }

    pub fn send_event(&self, event: &Event) {
        match serde_json::to_string(event) {
            Ok(line) => {
                let _ = self.tx.send(line + "\n");
            }
            Err(err) => tracing::error!("failed to serialize event '{}': {}", event.method, err),
        }
    }
}
