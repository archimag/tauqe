use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tauqe_core::providers::StreamEvent;
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
///
/// The value is crate-visible because a handler test reproduces a stalled
/// client by pre-filling the queue to exactly this depth before it drives the
/// relay the depth is supposed to pace.
pub(crate) const BACKLOG_HIGH_WATER: usize = 8192;

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

/// Liveness, depth, and headroom pacing of the outbound transport, shared by
/// every clone of [`OutChannel`] and by the writer that owns the output sink.
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
    /// Releases the producers paused at [`BACKLOG_HIGH_WATER`] once the writer
    /// drained the queue back below [`BACKLOG_LOW_WATER`].
    ///
    /// The value counts headroom recoveries, so the advance is a durable version
    /// stamp instead of a permit handed to one waiter: every producer holding a
    /// subscription observes it, however many of them are parked.
    headroom: watch::Sender<u64>,
    /// Set once no further line will be produced, so the writer may finish after
    /// it published everything still queued.
    shutdown: AtomicBool,
    /// Wakes the writer parked on the queue when the shutdown is requested.
    shutdown_wakeup: Notify,
}

impl OutboundState {
    fn new() -> Self {
        // No receiver is kept for the headroom signal: a producer subscribes
        // when it reaches the gate, and the writer advances the generation
        // whether or not one is subscribed at this instant.
        let (headroom, _) = watch::channel(0u64);
        Self {
            sink_lost: AtomicBool::new(false),
            loss_reported: AtomicBool::new(false),
            backlog_reported: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            lost: Notify::new(),
            headroom,
            shutdown: AtomicBool::new(false),
            shutdown_wakeup: Notify::new(),
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

    /// Reports whether the transport was asked to finish after its queue drains.
    fn is_shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Records the shutdown request and wakes the writer parked on the queue.
    ///
    /// The flag is stored before the wake-up is published, so a writer that read
    /// it just before this store cannot sleep through the request: it finds the
    /// stored permit and re-checks the flag immediately.
    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.shutdown_wakeup.notify_one();
    }

    /// Resolves once the shutdown was requested.
    async fn await_shutdown(&self) {
        loop {
            if self.is_shutdown_requested() {
                return;
            }
            self.shutdown_wakeup.notified().await;
        }
    }

    /// Accounts `lines` leaving the queue, re-arms the stall report once the
    /// queue has drained, and advances the headroom generation that releases
    /// every producer paused above the high-water mark.
    ///
    /// The depth falls before the generation advances, so a producer released by
    /// this batch re-checks the depth and finds the queue already drained. The
    /// advance is a version stamp every subscription observes instead of a
    /// permit handed to one waiter, so one drain releases however many producers
    /// are parked, and none of them can miss it by having observed the
    /// high-water mark before it started waiting.
    fn release_pending(&self, lines: usize) {
        let remaining = self
            .pending
            .fetch_sub(lines, Ordering::Relaxed)
            .saturating_sub(lines);
        if remaining < BACKLOG_LOW_WATER {
            self.backlog_reported.store(false, Ordering::Relaxed);
            self.headroom.send_modify(|generation| {
                *generation = generation.wrapping_add(1);
            });
        }
    }
}

/// Classifies a stream event for outbound headroom pacing.
///
/// The pacing contract has exactly one owner: terminal notices are exempt
/// because a stall must never delay the completion, cancellation, or failure a
/// contour reports — the client that stopped reading would otherwise park the
/// very event that ends the operation. Every other variant is telemetry a
/// stalled client may hold back, so it waits for the writer to catch up.
///
/// The match is exhaustive on purpose and carries no wildcard arm, so a new
/// [`StreamEvent`] variant fails compilation until it is classified here
/// instead of inheriting an unreviewed default in whichever contour relays it
/// next.
fn is_pacing_exempt(event: &StreamEvent) -> bool {
    match event {
        StreamEvent::Done | StreamEvent::Cancelled | StreamEvent::Error(_) => true,
        StreamEvent::ReasoningDelta(_)
        | StreamEvent::TextDelta(_)
        | StreamEvent::ContextChanged(_)
        | StreamEvent::EditStarted
        | StreamEvent::EditFileStarted { .. }
        | StreamEvent::EditHunk { .. }
        | StreamEvent::EditFileDone { .. }
        | StreamEvent::EditFileRetrying { .. }
        | StreamEvent::TurnPhase { .. }
        | StreamEvent::RepoMapTokens(_)
        | StreamEvent::TierEscalated { .. }
        | StreamEvent::ToolchainStarted { .. }
        | StreamEvent::ToolchainFinished { .. }
        | StreamEvent::RetryingConnection { .. }
        | StreamEvent::ResetStream
        | StreamEvent::Usage(_) => false,
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

    /// Applies the transport's pacing policy to one streamed event.
    ///
    /// A relay contour calls this instead of restating an exemption set of its
    /// own, so the classification declared in [`is_pacing_exempt`] is applied
    /// identically across every contour: a terminal notice publishes
    /// immediately, and every other variant waits for the writer to catch up
    /// through [`OutChannel::wait_for_headroom`].
    pub async fn pace_event(&self, event: &StreamEvent, cancel: &mut watch::Receiver<bool>) {
        if is_pacing_exempt(event) {
            return;
        }
        self.wait_for_headroom(cancel).await;
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
    /// The gate is safe for any number of simultaneously paused producers. Each
    /// producer subscribes to the headroom generation of the transport, and one
    /// drain releases all of them: the writer records the recovery in the
    /// generation instead of handing a permit to a single waiter, so a producer
    /// that observed the high-water mark before it started waiting still
    /// observes that drain. A queue that refills above the high-water mark parks
    /// the producer again until the writer drains it once more.
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
        // Subscribing before the first depth check stamps this receiver with the
        // generation of that instant, so a drain the check itself still observes
        // as missing is recorded on the receiver instead of being lost.
        let mut generation = self.state.headroom.subscribe();
        loop {
            if self.state.pending.load(Ordering::Acquire) < BACKLOG_HIGH_WATER {
                return;
            }
            tokio::select! {
                advanced = generation.changed() => {
                    if advanced.is_err() {
                        // The generation signal is gone, so no drain can release
                        // this producer any more; returning keeps it from parking
                        // on a wakeup that can never arrive.
                        return;
                    }
                }
                changed = cancel.changed() => {
                    // A raised signal ends the wait even while the client is
                    // stalled, and so does an operation whose signal is gone.
                    if changed.is_err() || *cancel.borrow() {
                        return;
                    }
                }
            }
        }
    }

    /// Asks the writer to finish once it published every queued line.
    ///
    /// Closing the queue is not an option: the daemon state and the history
    /// forwarder hold senders for the whole session, so the queue never reaches
    /// its end. An explicit request is the flush-or-close handshake instead —
    /// the caller requests it and then joins the writer's handle, so the
    /// terminal events of a cancelled operation reach a client that is still
    /// reading. Returning from `main` without that join drops the runtime and
    /// cancels the writer wherever it stands, truncating the stream.
    pub fn shutdown(&self) {
        self.state.request_shutdown();
    }
    /// Enqueues one serialized line, recording a dead sink instead of
    /// discarding the send result.
    fn enqueue(&self, line: String) {
        // The request loop has not observed the loss yet, so a producer may
        // still reach this point; its line is dropped, and the loss itself was
        // reported when it happened. A requested shutdown closes the transport
        // the same way: the writer may already be gone, and a line published
        // after the final flush cannot reach the client any more.
        if self.state.is_sink_lost() || self.state.is_shutdown_requested() {
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

    /// Awaits the next line to publish, or `None` once the queue ended or the
    /// shutdown was requested and nothing is left to write.
    ///
    /// Everything already queued is written before the request is honoured, and
    /// a line that arrives while the writer waits still wins the race, so a
    /// shutdown never truncates a line a producer already handed over.
    async fn next_line(&mut self) -> Option<String> {
        loop {
            if let Ok(line) = self.rx.try_recv() {
                return Some(line);
            }
            if self.state.is_shutdown_requested() {
                return None;
            }

            tokio::select! {
                line = self.rx.recv() => return line,
                _ = self.state.await_shutdown() => {}
            }
        }
    }
    /// Implements [`OutboundWriter::run`] against an explicit sink, so a
    /// deterministic test can drive a real drain — or exercise the failure
    /// path — without touching the real stdout of the process.
    ///
    /// Whatever is already queued when the writer wakes is coalesced into one
    /// sink write and one flush. A turn relays its deltas at token
    /// granularity, so a per-line write would pay a blocking-pool dispatch and
    /// a line flush per delta, while a batch pays them once per burst. The loop
    /// ends when the queue closes or when a requested shutdown finds it empty.
    pub(crate) async fn run_with<W>(mut self, mut sink: W)
    where
        W: AsyncWrite + Unpin,
    {
        let mut batch = String::new();
        while let Some(line) = self.next_line().await {
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

    /// A shutdown request ends the writer and still publishes every line queued
    /// before it: the drain of a cancelled operation ends with this handshake,
    /// so a writer that stopped at the request would truncate the stream of the
    /// client that is still reading the terminal events.
    #[tokio::test]
    async fn a_shutdown_request_flushes_the_queue_and_finishes_the_writer() {
        let (out, rx) = OutChannel::new();
        let names = [
            "test/model/cancelled",
            "test/plan/batch_finished",
            "test/git/rolled_back",
        ];
        for name in names {
            out.send_event(&event(name));
        }

        let sink = RecordingSink::default();
        let writing = tokio::spawn(out.writer(rx).run_with(sink.clone()));
        wait_for_drain(&out).await;

        // An open queue keeps the writer alive for the whole session: only the
        // shutdown request may end it, or the daemon would go deaf mid-session.
        assert!(
            !writing.is_finished(),
            "a writer parked on an open queue must not finish on its own"
        );

        out.shutdown();
        tokio::time::timeout(Duration::from_secs(1), writing)
            .await
            .expect("a shutdown request must let the writer finish")
            .expect("the writer task joins");

        let published = sink.writes().concat();
        let text = String::from_utf8_lossy(&published);
        for name in names {
            assert!(text.contains(name), "the shutdown flush dropped '{name}'");
        }
        assert_eq!(
            out.state.pending.load(Ordering::Relaxed),
            0,
            "the flush must release every queued line"
        );
    }
    /// Every producer paused at the high-water mark must be released by the
    /// drain itself, even when the writer falls idle afterwards with the channel
    /// still open: the recovery is a generation the transport records, not a
    /// permit a single waiter consumes.
    #[tokio::test]
    async fn one_full_drain_releases_every_paused_producer() {
        let (out, rx) = OutChannel::new();
        for _ in 0..BACKLOG_HIGH_WATER {
            out.send_event(&event("test/delta"));
        }
        let (cancel_tx, _cancel_rx) = watch::channel(false);

        let spawn_paused = |channel: &OutChannel, cancel: &watch::Sender<bool>| {
            let producer = channel.clone();
            let mut cancel = cancel.subscribe();
            tokio::spawn(async move {
                producer.wait_for_headroom(&mut cancel).await;
                producer.state.pending.load(Ordering::Relaxed)
            })
        };
        let first = spawn_paused(&out, &cancel_tx);
        let second = spawn_paused(&out, &cancel_tx);

        // Both producers reach the gate before a single line leaves the queue,
        // so the drain below is the only wakeup either of them can observe.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !first.is_finished() && !second.is_finished(),
            "every producer must pause while the backlog sits at the high-water mark"
        );

        let writing = tokio::spawn(out.writer(rx).run_with(tokio::io::sink()));
        let paused = async move { (first.await, second.await) };
        let released = tokio::time::timeout(Duration::from_secs(1), paused).await;
        let depths = released.expect("the drain must release every paused producer");
        let first_depth = depths.0.expect("the first producer task joins");
        let second_depth = depths.1.expect("the second producer task joins");
        for depth in [first_depth, second_depth] {
            assert!(
                depth < BACKLOG_LOW_WATER,
                "a released producer must observe the drained depth, saw {depth}"
            );
        }
        assert!(
            !writing.is_finished(),
            "the writer must stay idle with the channel open instead of ending"
        );

        // A producer whose first depth check happens after that drain reads the
        // drained depth and never parks at all.
        let mut late = cancel_tx.subscribe();
        tokio::time::timeout(Duration::from_millis(20), out.wait_for_headroom(&mut late))
            .await
            .expect("a producer that starts after the drain must not park");

        writing.abort();
        let _ = writing.await;
    }

    /// The transport owns the pacing classification, so a terminal notice
    /// passes a stalled queue immediately: a stall must never delay the
    /// completion, cancellation, or failure it reports.
    #[tokio::test]
    async fn terminal_notices_pass_a_stalled_queue_without_waiting() {
        let (out, _rx) = OutChannel::new();
        for _ in 0..=BACKLOG_HIGH_WATER {
            out.send_event(&event("test/delta"));
        }
        let (_cancel_tx, mut cancel) = watch::channel(false);

        for notice in [
            StreamEvent::Done,
            StreamEvent::Cancelled,
            StreamEvent::Error("provider rejected the request".to_string()),
        ] {
            tokio::time::timeout(
                Duration::from_millis(20),
                out.pace_event(&notice, &mut cancel),
            )
            .await
            .expect("a terminal notice must never wait for a stalled client");
        }
    }

    /// A token-granularity event stays gated: the classification exempts the
    /// terminal notices and nothing else, so the event parks at the high-water
    /// mark and resumes once the writer drained the queue.
    #[tokio::test]
    async fn a_gated_event_parks_until_the_writer_drains_the_queue() {
        let (out, rx) = OutChannel::new();
        for _ in 0..=BACKLOG_HIGH_WATER {
            out.send_event(&event("test/delta"));
        }
        let (_cancel_tx, mut cancel) = watch::channel(false);
        let gated_event = StreamEvent::TextDelta("delta".to_string());

        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                out.pace_event(&gated_event, &mut cancel)
            )
            .await
            .is_err(),
            "a gated event must park while the backlog sits above the high-water mark"
        );

        let writing = tokio::spawn(out.writer(rx).run_with(tokio::io::sink()));
        tokio::time::timeout(
            Duration::from_secs(1),
            out.pace_event(&gated_event, &mut cancel),
        )
        .await
        .expect("the writer must drain the queue below the low-water mark");

        writing.abort();
        let _ = writing.await;
    }

    /// A raised cancellation releases a gated event even while the queue stays
    /// stalled, so an interrupted contour never waits for a reader that never
    /// returns.
    #[tokio::test]
    async fn a_cancelled_operation_releases_a_gated_event() {
        let (out, _rx) = OutChannel::new();
        for _ in 0..=BACKLOG_HIGH_WATER {
            out.send_event(&event("test/delta"));
        }
        let (cancel_tx, mut cancel) = watch::channel(false);
        let gated_event = StreamEvent::TextDelta("delta".to_string());

        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                out.pace_event(&gated_event, &mut cancel)
            )
            .await
            .is_err(),
            "the stalled client must hold the gated event"
        );

        let _ = cancel_tx.send(true);
        tokio::time::timeout(
            Duration::from_millis(20),
            out.pace_event(&gated_event, &mut cancel),
        )
        .await
        .expect("a cancelled operation must not wait for the stalled client");
    }
}
