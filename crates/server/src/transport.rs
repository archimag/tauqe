use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tauqe_protocol::{Event, Response};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Notify};

/// Queue depth at which the transport reports that the client stopped draining
/// its stream.
///
/// The queue stays unbounded on purpose: dropping a line would corrupt the wire
/// stream, because a client cannot tell a discarded event from one that was
/// never produced. A dead sink now stops the daemon instead, so unbounded
/// growth is confined to a client that merely reads slower than the daemon
/// produces, and this threshold makes exactly that condition visible.
const BACKLOG_WARN_THRESHOLD: usize = 8192;

/// Liveness and depth of the outbound transport, shared by every clone of
/// [`OutChannel`] and by the writer that owns the output sink.
struct OutboundState {
    /// Set once the sink can no longer publish a line.
    sink_lost: AtomicBool,
    /// Latched so the loss of the sink is reported once instead of on every
    /// line a producer still enqueues before the request loop notices.
    loss_reported: AtomicBool,
    /// Latched while the backlog stays above the threshold, so a slow client is
    /// reported once per sustained episode.
    backlog_reported: AtomicBool,
    /// Lines enqueued and not yet written, the observable depth of the queue.
    pending: AtomicUsize,
    /// Wakes the request loop when the sink dies.
    lost: Notify,
}

impl OutboundState {
    fn new() -> Self {
        Self {
            sink_lost: AtomicBool::new(false),
            loss_reported: AtomicBool::new(false),
            backlog_reported: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            lost: Notify::new(),
        }
    }

    /// Reports whether the sink can no longer publish a line.
    fn is_sink_lost(&self) -> bool {
        self.sink_lost.load(Ordering::SeqCst)
    }

    /// Records the loss of the sink, reports its cause once, and wakes the
    /// request loop that awaits [`OutChannel::sink_lost`].
    fn mark_sink_lost(&self, cause: &str) {
        self.sink_lost.store(true, Ordering::SeqCst);
        if !self.loss_reported.swap(true, Ordering::SeqCst) {
            tracing::error!(
                error = cause,
                "Outbound transport sink is gone; no response or event can reach the client"
            );
        }
        self.lost.notify_one();
    }

    /// Accounts one line leaving the queue and re-arms the backlog report once
    /// the queue has drained.
    fn release_pending(&self) {
        let remaining = self
            .pending
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        if remaining < BACKLOG_WARN_THRESHOLD {
            self.backlog_reported.store(false, Ordering::Relaxed);
        }
    }
}

/// Safe thread-safe event and response sink that routes output lines
/// sequentially to stdout.
#[derive(Clone)]
pub struct OutChannel {
    tx: mpsc::UnboundedSender<String>,
    state: Arc<OutboundState>,
}

impl OutChannel {
    /// Creates the channel together with the queue its writer drains.
    ///
    /// The receiver is returned rather than spawned here, so the daemon hands it
    /// to [`OutChannel::writer`] while a deterministic test can read the queued
    /// lines directly.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(OutboundState::new());
        let channel = Self {
            tx,
            state: Arc::clone(&state),
        };
        (channel, rx)
    }

    /// Wraps the queue receiver into the writer that owns the output sink.
    ///
    /// The writer shares this channel's liveness state, so a failed write is
    /// recorded where [`OutChannel::sink_lost`] observes it instead of ending
    /// the task silently.
    pub fn writer(&self, rx: mpsc::UnboundedReceiver<String>) -> OutboundWriter {
        OutboundWriter {
            rx,
            state: Arc::clone(&self.state),
        }
    }

    pub fn send_response(&self, resp: &Response) {
        match serde_json::to_string(resp) {
            Ok(line) => self.enqueue(line + "\n"),
            Err(err) => tracing::error!("failed to serialize response {:?}: {}", resp.id, err),
        }
    }

    pub fn send_event(&self, event: &Event) {
        match serde_json::to_string(event) {
            Ok(line) => self.enqueue(line + "\n"),
            Err(err) => tracing::error!("failed to serialize event '{}': {}", event.method, err),
        }
    }

    /// Resolves once the writer can no longer publish to the client.
    ///
    /// The request loop races it against stdin, so a client that closed its
    /// read end stops the daemon through the ordinary cancel-and-drain path
    /// instead of leaving it turning blind.
    pub async fn sink_lost(&self) {
        loop {
            if self.state.is_sink_lost() {
                return;
            }
            self.state.lost.notified().await;
        }
    }

    /// Enqueues one serialized line, recording a dead sink instead of
    /// discarding the send result.
    fn enqueue(&self, line: String) {
        // The request loop has not observed the loss yet, so a producer may
        // still reach this point; its line is dropped, and the loss itself was
        // reported when it happened.
        if self.state.is_sink_lost() {
            return;
        }

        let pending = self.state.pending.fetch_add(1, Ordering::Relaxed) + 1;
        if pending >= BACKLOG_WARN_THRESHOLD
            && !self.state.backlog_reported.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                pending,
                threshold = BACKLOG_WARN_THRESHOLD,
                "Outbound backlog is growing; the client is not draining its stream"
            );
        }

        if let Err(err) = self.tx.send(line) {
            self.state.release_pending();
            self.state.mark_sink_lost(&err.to_string());
        }
    }
}

/// Owner of the outbound queue and of the sink it drains into.
///
/// Only the writer can tell whether the client is still reachable, so a failed
/// write is recorded on the shared state instead of ending the task silently;
/// [`OutChannel::sink_lost`] is what the request loop observes.
pub struct OutboundWriter {
    rx: mpsc::UnboundedReceiver<String>,
    state: Arc<OutboundState>,
}

impl OutboundWriter {
    /// Drains every queued line into the daemon's stdout.
    pub async fn run(self) {
        self.run_with(tokio::io::stdout()).await;
    }

    /// Implements [`OutboundWriter::run`] against an explicit sink, so a
    /// deterministic test can exercise the failure path without touching the
    /// real stdout of the process.
    async fn run_with<W>(mut self, mut sink: W)
    where
        W: AsyncWrite + Unpin,
    {
        while let Some(line) = self.rx.recv().await {
            let written = match sink.write_all(line.as_bytes()).await {
                Ok(()) => sink.flush().await,
                Err(err) => Err(err),
            };
            self.state.release_pending();
            if let Err(err) = written {
                self.state.mark_sink_lost(&err.to_string());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::Ordering;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::*;

    /// Sink that fails exactly like a client pipe whose read end was closed, so
    /// the writer's failure path is exercised without the real stdout.
    struct ClosedPipe;

    impl AsyncWrite for ClosedPipe {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "client closed its output pipe",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn event(name: &str) -> Event {
        Event {
            method: name.to_string(),
            params: None,
        }
    }

    /// A write failure must be recorded and surfaced instead of swallowed, and
    /// every later send must be contained.
    #[tokio::test]
    async fn a_dead_sink_is_reported_and_later_sends_are_contained() {
        let (out, rx) = OutChannel::new();
        let writing = tokio::spawn(out.writer(rx).run_with(ClosedPipe));
        let lost = out.sink_lost();

        out.send_event(&event("test/event"));

        tokio::time::timeout(Duration::from_secs(1), lost)
            .await
            .expect("a dead sink must be reported instead of swallowed");
        writing.await.expect("the writer task exits on its own");

        out.send_event(&event("test/event"));
        assert_eq!(
            out.state.pending.load(Ordering::Relaxed),
            0,
            "a lost sink must not accumulate queued lines"
        );
    }

    /// A writer that drains its queue normally is not a lost transport, and the
    /// queue depth returns to zero.
    #[tokio::test]
    async fn a_drained_queue_is_not_a_lost_transport() {
        let (out, rx) = OutChannel::new();
        out.send_event(&event("test/event"));
        let writing = tokio::spawn(out.writer(rx).run_with(tokio::io::sink()));

        let mut drained = false;
        for _ in 0..200 {
            if out.state.pending.load(Ordering::Relaxed) == 0 {
                drained = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(drained, "the writer must drain the queued line");

        let loss = tokio::time::timeout(Duration::from_millis(20), out.sink_lost()).await;
        assert!(
            loss.is_err(),
            "a healthy sink must not report transport loss"
        );

        writing.abort();
        let _ = writing.await;
    }

    /// A client that reads slower than the daemon produces must be visible
    /// through the queue depth instead of growing the queue unobserved.
    #[tokio::test]
    async fn a_growing_backlog_arms_the_depth_report() {
        let (out, _rx) = OutChannel::new();
        for _ in 0..=BACKLOG_WARN_THRESHOLD {
            out.send_event(&event("test/delta"));
        }

        assert_eq!(
            out.state.pending.load(Ordering::Relaxed),
            BACKLOG_WARN_THRESHOLD + 1,
            "every queued line must be accounted for"
        );
        assert!(
            out.state.backlog_reported.load(Ordering::Relaxed),
            "crossing the depth threshold must arm the backlog report"
        );
    }
}
