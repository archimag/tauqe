use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tauqe_protocol::{Event, Response};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch, Notify};

/// Queue depth at which the transport pauses a streaming producer and reports
/// that the client stopped draining its stream.
///
/// The queue stays unbounded on purpose: dropping a line would corrupt the wire
/// stream, because a client cannot tell a discarded event from one that was
/// never produced. The bound therefore comes from the producing side — a contour
/// that relays token-granularity telemetry waits for
/// [`OutChannel::wait_for_headroom`] before it enqueues the next fragment, so the
/// depth follows the writer's progress instead of the length of the operation. A
/// dead sink still stops the daemon through [`OutChannel::sink_lost`].
const BACKLOG_HIGH_WATER: usize = 8192;

/// Queue depth the writer must reach before a paused producer resumes.
///
/// The gap to [`BACKLOG_HIGH_WATER`] is what keeps the gate from oscillating on
/// every drained batch: a paused producer resumes only after the writer caught
/// up by that whole margin, so one stalled episode costs one report and one
/// pause instead of a flip on every write.
const BACKLOG_LOW_WATER: usize = BACKLOG_HIGH_WATER / 8;

/// Upper bound on the number of queued lines one sink write coalesces.
///
/// The writer publishes whatever is already queued in a single write, so the
/// bound keeps that drain finite when a producer outpaces the writer and keeps
/// the coalescing buffer from growing with the queue itself.
const MAX_COALESCED_LINES: usize = 1024;

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
    /// Releases a producer that paused at [`BACKLOG_HIGH_WATER`] once the writer
    /// drained the queue back below [`BACKLOG_LOW_WATER`].
    headroom: Notify,
}

impl OutboundState {
    fn new() -> Self {
        Self {
            sink_lost: AtomicBool::new(false),
            loss_reported: AtomicBool::new(false),
            backlog_reported: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            lost: Notify::new(),
            headroom: Notify::new(),
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

    /// Accounts `lines` leaving the queue, re-arms the stall report once the
    /// queue has drained, and releases the producers that paused above the
    /// high-water mark.
    ///
    /// The depth falls before the wake-up is published, so a producer that read
    /// the depth just before this batch left the queue cannot sleep through the
    /// drain meant to release it: it finds the stored permit and re-checks the
    /// depth immediately.
    fn release_pending(&self, lines: usize) {
        let remaining = self
            .pending
            .fetch_sub(lines, Ordering::Relaxed)
            .saturating_sub(lines);
        if remaining < BACKLOG_LOW_WATER {
            self.backlog_reported.store(false, Ordering::Relaxed);
            // Every producer already waiting re-checks the depth, and the stored
            // permit covers the producer that starts waiting at this instant.
            self.headroom.notify_waiters();
            self.headroom.notify_one();
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
            Ok(mut line) => {
                // The terminator joins the payload inside its own allocation,
                // so a response stays one buffer end to end instead of a
                // payload and a whole-line `line + "\n"` copy of it.
                line.push('\n');
                self.enqueue(line);
            }
            Err(err) => tracing::error!("failed to serialize response {:?}: {}", resp.id, err),
        }
    }

    pub fn send_event(&self, event: &Event) {
        match serde_json::to_string(event) {
            Ok(mut line) => {
                // One buffer carries the payload and its terminator, so an
                // event costs a single allocation on the streaming hot path.
                line.push('\n');
                self.enqueue(line);
            }
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

    /// Awaits until the outbound queue drained below the low-water mark.
    ///
    /// This is the backpressure of the transport. A client that keeps its pipe
    /// open but stops reading cannot make the daemon accumulate every serialized
    /// line of a long streaming turn: the contour that relays those lines pauses
    /// here and resumes only once the writer caught up. No line is ever shed —
    /// the queue keeps every event in order — so the bound applies to the
    /// producer and never to the payload.
    ///
    /// Only a high-rate streaming contour calls this, so responses and control
    /// events keep flowing and the request loop stays responsive while a stream
    /// is throttled. The wait ends with the operation: a raised `cancel` signal
    /// releases it even while the client is still stalled, because a paused
    /// stream must never outlive the turn that produces it. What is left to
    /// enqueue afterwards is bounded by the relay channel, since the producer
    /// stops producing once it observes the cancellation.
    pub async fn wait_for_headroom(&self, cancel: &mut watch::Receiver<bool>) {
        if *cancel.borrow() {
            return;
        }
        let drained = async {
            while self.state.pending.load(Ordering::Acquire) >= BACKLOG_HIGH_WATER {
                self.state.headroom.notified().await;
            }
        };
        tokio::select! {
            _ = drained => {}
            _ = cancel.changed() => {}
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
        if pending >= BACKLOG_HIGH_WATER
            && !self.state.backlog_reported.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                pending,
                threshold = BACKLOG_HIGH_WATER,
                "Outbound backlog reached the high-water mark; the client is not draining its stream"
            );
        }

        if let Err(err) = self.tx.send(line) {
            self.state.release_pending(1);
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
    ///
    /// Whatever is already queued when the writer wakes is coalesced into one
    /// sink write and one flush. A turn relays its deltas at token
    /// granularity, so a per-line write would pay a blocking-pool dispatch and
    /// a line flush per delta, while a batch pays them once per burst.
    async fn run_with<W>(mut self, mut sink: W)
    where
        W: AsyncWrite + Unpin,
    {
        let mut batch = String::new();
        while let Some(line) = self.rx.recv().await {
            batch.clear();
            batch.push_str(&line);
            let mut coalesced = 1;
            while coalesced < MAX_COALESCED_LINES {
                match self.rx.try_recv() {
                    Ok(next) => {
                        batch.push_str(&next);
                        coalesced += 1;
                    }
                    Err(_) => break,
                }
            }

            let written = match sink.write_all(batch.as_bytes()).await {
                Ok(()) => sink.flush().await,
                Err(err) => Err(err),
            };
            self.state.release_pending(coalesced);
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

    /// Waits until the writer released every queued line.
    async fn wait_for_drain(out: &OutChannel) {
        for _ in 0..200 {
            if out.state.pending.load(Ordering::Relaxed) == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            out.state.pending.load(Ordering::Relaxed),
            0,
            "the writer must release every queued line"
        );
    }

    /// Sink that records every write and every flush, so the coalescing
    /// contract is observable without the real stdout of the process.
    #[derive(Clone, Default)]
    struct RecordingSink {
        writes: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        flushes: Arc<AtomicUsize>,
    }

    impl RecordingSink {
        /// Payloads the writer handed to the sink, in write order.
        fn writes(&self) -> Vec<Vec<u8>> {
            self.writes
                .lock()
                .map(|writes| writes.clone())
                .unwrap_or_default()
        }

        /// Number of flushes the writer requested.
        fn flushes(&self) -> usize {
            self.flushes.load(Ordering::SeqCst)
        }
    }

    impl AsyncWrite for RecordingSink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if let Ok(mut writes) = self.writes.lock() {
                writes.push(buf.to_vec());
            }
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A burst of queued lines must cost one sink write and one flush, not one
    /// of each per line: a turn relays its deltas at token granularity, so a
    /// per-line cost would scale with tokens instead of bursts.
    #[tokio::test]
    async fn queued_lines_are_coalesced_into_one_write_and_one_flush() {
        let (out, rx) = OutChannel::new();
        let names = ["test/first", "test/second", "test/third"];
        for name in names {
            out.send_event(&event(name));
        }

        let sink = RecordingSink::default();
        let writing = tokio::spawn(out.writer(rx).run_with(sink.clone()));
        wait_for_drain(&out).await;

        let writes = sink.writes();
        assert_eq!(
            writes.len(),
            1,
            "a queued burst must reach the sink in one write"
        );
        assert_eq!(sink.flushes(), 1, "the burst must cost exactly one flush");

        let payload = &writes[0];
        assert_eq!(
            payload.iter().filter(|byte| **byte == b'\n').count(),
            names.len(),
            "every queued line must keep exactly one terminator"
        );
        assert!(
            payload.ends_with(b"\n"),
            "the batch must end on a line boundary"
        );
        let text = String::from_utf8_lossy(payload);
        for name in names {
            assert!(text.contains(name), "the batch dropped '{name}'");
        }

        writing.abort();
        let _ = writing.await;
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
        for _ in 0..=BACKLOG_HIGH_WATER {
            out.send_event(&event("test/delta"));
        }

        assert_eq!(
            out.state.pending.load(Ordering::Relaxed),
            BACKLOG_HIGH_WATER + 1,
            "every queued line must be accounted for"
        );
        assert!(
            out.state.backlog_reported.load(Ordering::Relaxed),
            "crossing the high-water mark must arm the backlog report"
        );
    }

    /// A client that keeps its pipe open but stops reading must not turn a
    /// streaming turn into an unbounded queue: the producer pauses at the
    /// high-water mark and resumes only after the writer caught up.
    #[tokio::test]
    async fn a_stalled_client_pauses_the_producer_until_the_writer_catches_up() {
        let (out, rx) = OutChannel::new();
        let (_cancel_tx, mut cancel) = watch::channel(false);

        tokio::time::timeout(
            Duration::from_millis(20),
            out.wait_for_headroom(&mut cancel),
        )
        .await
        .expect("an empty queue must never delay a producer");

        for _ in 0..BACKLOG_HIGH_WATER {
            out.send_event(&event("test/delta"));
        }
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                out.wait_for_headroom(&mut cancel)
            )
            .await
            .is_err(),
            "a producer must wait while the backlog sits at the high-water mark"
        );

        let writing = tokio::spawn(out.writer(rx).run_with(tokio::io::sink()));
        tokio::time::timeout(Duration::from_secs(1), out.wait_for_headroom(&mut cancel))
            .await
            .expect("the writer must drain the queue below the low-water mark");
        assert!(
            out.state.pending.load(Ordering::Relaxed) < BACKLOG_LOW_WATER,
            "the released producer must observe the drained depth"
        );

        writing.abort();
        let _ = writing.await;
    }

    /// A cancelled operation must leave the gate even while the client is still
    /// stalled, so an interrupted turn reaches its terminal events and releases
    /// its operation slot instead of waiting for a reader that never returns.
    #[tokio::test]
    async fn a_cancelled_operation_stops_waiting_for_a_stalled_client() {
        let (out, _rx) = OutChannel::new();
        for _ in 0..BACKLOG_HIGH_WATER {
            out.send_event(&event("test/delta"));
        }
        let (cancel_tx, mut cancel) = watch::channel(false);

        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                out.wait_for_headroom(&mut cancel)
            )
            .await
            .is_err(),
            "the stalled client must hold the producer"
        );

        let _ = cancel_tx.send(true);
        tokio::time::timeout(
            Duration::from_millis(20),
            out.wait_for_headroom(&mut cancel),
        )
        .await
        .expect("a cancelled operation must not wait for the stalled client");
    }
}
