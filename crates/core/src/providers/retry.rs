//! Universal retry decorator for [`LlmProvider`] transports.
//!
//! A generation occasionally fails on the transport layer alone: the connection
//! times out before the response headers arrive, the provider answers
//! `429 Too Many Requests` or a `5xx` status, or the server-sent event stream
//! breaks before its terminator. None of these outcomes says anything about the
//! request itself, so re-issuing it usually succeeds.
//!
//! [`RetryingProvider`] adds that behaviour to any provider without the wrapped
//! provider knowing about it:
//!
//! - Transient failures are retried with an exponential, jittered backoff, while
//!   fatal failures (authentication, malformed request, context overflow) are
//!   reported immediately instead of burning the retry budget.
//! - A structured response cut short at the provider's token limit is transient
//!   too: its partial document is unparseable, so the attempt is re-issued
//!   instead of being parsed.
//! - The backoff pause races the turn cancellation signal, so interrupting a
//!   turn aborts the wait at once instead of leaving a dead pause.
//! - Every second of the pause is reported as
//!   [`StreamEvent::RetryingConnection`], letting a client render a countdown.
//! - A retry that follows partially streamed output emits
//!   [`StreamEvent::ResetStream`] first, so neither the server nor the client
//!   concatenates the fragments of two different attempts.
//! - Model output is metered: an attempt that streams more than
//!   [`DEFAULT_MAX_STREAM_BYTES`] is aborted with a terminal error, so a model
//!   stuck in a reasoning loop cannot grow memory without bound.
//!
//! The decorator owns failure reporting as well: an error raised by an attempt
//! that will be retried never reaches the client, and the terminal failure is
//! reported exactly once, after the retry budget is exhausted.

use std::time::Duration;

use async_trait::async_trait;
use tauqe_protocol::ProviderKind;
use tokio::sync::{mpsc, watch};

use super::types::{ChatMessage, ReasoningEffort, ResponseFormat, StreamEvent};
use super::LlmProvider;

/// Total attempts made for one request, including the first one.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 4;
/// Pause before the second attempt; every later pause doubles it.
pub const DEFAULT_BASE_DELAY_MS: u64 = 500;
/// Upper bound of a single backoff pause.
pub const DEFAULT_MAX_DELAY_MS: u64 = 8_000;
/// Cumulative model output tolerated for a single request attempt.
///
/// A model caught in a reasoning loop keeps streaming deltas forever: the relay
/// forwards them, the client keeps appending, and the process grows until the
/// system kills it. The ceiling turns that into a deterministic, terminal
/// failure while the transport itself is still healthy.
pub const DEFAULT_MAX_STREAM_BYTES: usize = 2 * 1024 * 1024;

/// Marker embedded in the transport error raised when a provider cuts a
/// structured JSON generation short at its token limit.
///
/// The partial document cannot be parsed, so the attempt produced nothing
/// usable; re-issuing the request re-samples the model and can still yield a
/// complete document. [`RetryingProvider`] therefore classifies this marker as
/// a transient failure and tells the client to drop the fragments with
/// [`StreamEvent::ResetStream`].
pub const JSON_TRUNCATION_MARKER: &str = "truncated by finish_reason: length";

/// Largest exponent the doubling is allowed to reach before it is clamped.
const MAX_BACKOFF_EXPONENT: u32 = 16;
/// Longest error snippet reported to the client and the journal.
const REASON_SUMMARY_CHARS: usize = 200;
/// Capacity of the channel relaying the wrapped provider's events.
///
/// The channel is drained while the provider call is polled in the same task,
/// so the capacity only bounds the memory held by the relay itself.
const RELAY_CAPACITY: usize = 64;

/// Retry policy of [`RetryingProvider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryConfig {
    /// Attempts per request, including the first one. Clamped to at least one.
    pub max_attempts: u32,
    /// Backoff pause before the second attempt.
    pub base_delay_ms: u64,
    /// Upper bound of any single backoff pause.
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            base_delay_ms: DEFAULT_BASE_DELAY_MS,
            max_delay_ms: DEFAULT_MAX_DELAY_MS,
        }
    }
}

impl RetryConfig {
    /// Backoff pause after the given attempt.
    ///
    /// The pause doubles with every attempt up to [`Self::max_delay_ms`] and
    /// carries equal jitter: half of it is fixed, the other half is random, so
    /// concurrent clients never retry in lockstep against a struggling
    /// upstream.
    fn delay_for(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(MAX_BACKOFF_EXPONENT);
        let ceiling = self.max_delay_ms.max(1);
        let raw = self
            .base_delay_ms
            .saturating_mul(1u64 << exponent)
            .min(ceiling);
        if raw <= 1 {
            return Duration::from_millis(raw);
        }
        let half = raw / 2;
        Duration::from_millis(half + random_u64() % (half + 1))
    }
}

/// Provider decorator that retries transient transport failures and meters the
/// volume of streamed model output.
pub struct RetryingProvider {
    inner: Box<dyn LlmProvider>,
    config: RetryConfig,
    /// Cumulative output tolerated for one attempt.
    max_stream_bytes: usize,
}

impl RetryingProvider {
    /// Wraps `inner` with the given retry policy.
    pub fn new(inner: Box<dyn LlmProvider>, config: RetryConfig) -> Self {
        Self {
            inner,
            config,
            max_stream_bytes: DEFAULT_MAX_STREAM_BYTES,
        }
    }

    /// Overrides the cumulative output ceiling of a single attempt.
    pub fn with_max_stream_bytes(mut self, max_stream_bytes: usize) -> Self {
        self.max_stream_bytes = max_stream_bytes.max(1);
        self
    }

    /// Runs one request to completion, re-issuing it while the failure is
    /// transient and the retry budget lasts.
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        reasoning_effort: Option<ReasoningEffort>,
        session_id: Option<&str>,
        tx: mpsc::Sender<StreamEvent>,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let max_attempts = self.config.max_attempts.max(1);
        let mut attempt: u32 = 0;

        loop {
            attempt += 1;
            if *cancel_rx.borrow() {
                let _ = tx.send(StreamEvent::Cancelled).await;
                return Ok(());
            }

            let outcome = self
                .attempt(
                    model,
                    messages.clone(),
                    response_format.clone(),
                    reasoning_effort,
                    session_id,
                    &tx,
                    &cancel_rx,
                )
                .await;

            if outcome.cancelled {
                return Ok(());
            }

            let Some((reason, retryable)) = outcome.failure() else {
                return Ok(());
            };

            if !retryable || attempt >= max_attempts {
                // The partial answer of a failed attempt is void, so the client
                // discards it before the terminal failure is reported.
                if outcome.emitted_output {
                    let _ = tx.send(StreamEvent::ResetStream).await;
                }
                let _ = tx.send(StreamEvent::Error(reason.clone())).await;
                tracing::warn!(
                    model = %model,
                    attempt,
                    max_attempts,
                    retryable,
                    reason = %summarize(&reason),
                    "provider attempt failed"
                );
                return Err(match outcome.transport_error {
                    Some(err) => err,
                    None => anyhow::anyhow!(reason),
                });
            }

            // Fragments of two different attempts must never be stitched
            // together, so the client is told to drop what it accumulated.
            if outcome.emitted_output {
                let _ = tx.send(StreamEvent::ResetStream).await;
            }

            let delay = self.config.delay_for(attempt);
            tracing::warn!(
                model = %model,
                attempt,
                max_attempts,
                delay_ms = %delay.as_millis(),
                reason = %summarize(&reason),
                "retrying provider request after a transient failure"
            );

            let reason = summarize(&reason);
            if !wait_with_countdown(delay, attempt, max_attempts, &reason, &tx, &mut cancel_rx)
                .await
            {
                return Ok(());
            }
        }
    }

    /// Performs one provider call, relaying its events to `tx` while recording
    /// what the attempt produced.
    ///
    /// The provider future is polled next to the relay rather than joined with
    /// it, so a runaway generation or a vanished client drops the future at once
    /// and aborts the downstream response instead of draining an unbounded
    /// stream.
    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        reasoning_effort: Option<ReasoningEffort>,
        session_id: Option<&str>,
        tx: &mpsc::Sender<StreamEvent>,
        cancel_rx: &watch::Receiver<bool>,
    ) -> AttemptOutcome {
        let ceiling = self.max_stream_bytes;
        let (inner_tx, mut inner_rx) = mpsc::channel::<StreamEvent>(RELAY_CAPACITY);
        let mut stream = self.inner.stream_chat_with_session(
            model,
            messages,
            response_format,
            reasoning_effort,
            session_id,
            inner_tx,
            cancel_rx.clone(),
        );

        let mut observed = AttemptOutcome::default();

        let transport_error = loop {
            tokio::select! {
                biased;
                result = &mut stream => break result.err(),
                Some(event) = inner_rx.recv() => {
                    if relay_event(&mut observed, event, ceiling, tx).await {
                        return observed;
                    }
                }
            }
        };

        // The provider may have queued events just before it returned, so they
        // are relayed before the attempt is judged.
        while let Some(event) = inner_rx.recv().await {
            if relay_event(&mut observed, event, ceiling, tx).await {
                break;
            }
        }

        observed.transport_error = transport_error;
        observed
    }
}

/// What a single provider attempt produced.
#[derive(Debug, Default)]
struct AttemptOutcome {
    /// Output already streamed to the client during this attempt.
    emitted_output: bool,
    /// Cumulative size in bytes of the deltas streamed during this attempt.
    stream_bytes: usize,
    /// Reason recorded when the cumulative output crossed the safety ceiling.
    runaway: Option<String>,
    /// Error reported on the stream channel by the provider.
    stream_error: Option<String>,
    /// Error returned by the provider call itself.
    transport_error: Option<anyhow::Error>,
    /// The attempt observed a cancellation signal.
    cancelled: bool,
}

impl AttemptOutcome {
    /// Folds one relayed event into the observation.
    ///
    /// Returns `true` when the attempt must stop immediately: the cumulative
    /// output crossed `ceiling`, so the generation is treated as runaway. Both
    /// answer text and reasoning text are metered, because either can grow
    /// without bound.
    fn observe(&mut self, event: &StreamEvent, ceiling: usize) -> bool {
        match event {
            StreamEvent::TextDelta(delta) | StreamEvent::ReasoningDelta(delta) => {
                if !delta.is_empty() {
                    self.emitted_output = true;
                    self.stream_bytes = self.stream_bytes.saturating_add(delta.len());
                    if self.stream_bytes > ceiling {
                        self.runaway = Some(format!(
                            "Model output exceeded the safety ceiling of {ceiling} bytes in a \
                             single response; generation aborted to prevent a runaway stream"
                        ));
                        return true;
                    }
                }
            }
            StreamEvent::Error(message) => self.stream_error = Some(message.clone()),
            StreamEvent::Cancelled => self.cancelled = true,
            _ => {}
        }
        false
    }

    /// Returns the failure reason and whether it is transient.
    fn failure(&self) -> Option<(String, bool)> {
        // A runaway generation is a property of the request itself: re-issuing
        // it would only reproduce the loop, so it is never retried.
        if let Some(reason) = self.runaway.as_ref() {
            return Some((reason.clone(), false));
        }

        let stream_retryable = self
            .stream_error
            .as_deref()
            .is_some_and(is_retryable_message);

        match self.transport_error.as_ref() {
            Some(err) => Some((err.to_string(), is_retryable_error(err) || stream_retryable)),
            None => self
                .stream_error
                .as_deref()
                .map(|message| (message.to_string(), stream_retryable)),
        }
    }
}

/// Folds one provider event into the observation and forwards it to the client.
///
/// Returns `true` when the attempt must end immediately: the cumulative output
/// crossed the ceiling, or the client stopped listening. A provider error is
/// withheld, because an attempt that may still be retried must not terminate
/// the client's stream; the terminal failure is re-emitted once, if needed,
/// after the attempt ends.
async fn relay_event(
    observed: &mut AttemptOutcome,
    event: StreamEvent,
    ceiling: usize,
    tx: &mpsc::Sender<StreamEvent>,
) -> bool {
    if observed.observe(&event, ceiling) {
        return true;
    }
    if matches!(event, StreamEvent::Error(_)) {
        return false;
    }
    tx.send(event).await.is_err()
}

#[async_trait]
impl LlmProvider for RetryingProvider {
    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }

    async fn stream_chat(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        self.run(model, messages, response_format, None, None, tx, cancel_rx)
            .await
    }

    async fn stream_chat_with_effort(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        reasoning_effort: Option<ReasoningEffort>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        self.run(
            model,
            messages,
            response_format,
            reasoning_effort,
            None,
            tx,
            cancel_rx,
        )
        .await
    }

    async fn stream_chat_with_session(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        reasoning_effort: Option<ReasoningEffort>,
        session_id: Option<&str>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        self.run(
            model,
            messages,
            response_format,
            reasoning_effort,
            session_id,
            tx,
            cancel_rx,
        )
        .await
    }

    async fn stream_text(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        self.run(model, messages, None, None, None, tx, cancel_rx)
            .await
    }
}

/// Sleeps for `delay`, reporting the remaining seconds once per second and
/// abandoning the pause the instant the turn is cancelled.
///
/// Returns `false` when the client went away or the turn was cancelled; the
/// cancellation signal itself has already been forwarded in that case.
async fn wait_with_countdown(
    delay: Duration,
    attempt: u32,
    max_attempts: u32,
    reason: &str,
    tx: &mpsc::Sender<StreamEvent>,
    cancel_rx: &mut watch::Receiver<bool>,
) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    let mut announced = false;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if announced && remaining.is_zero() {
            return true;
        }

        let next = StreamEvent::RetryingConnection {
            attempt,
            max_attempts,
            delay_secs: seconds_ceil(remaining).max(1),
            reason: reason.to_string(),
        };
        if tx.send(next).await.is_err() {
            return false;
        }
        announced = true;

        let step = remaining.min(Duration::from_secs(1));
        if step.is_zero() {
            return true;
        }

        tokio::select! {
            _ = tokio::time::sleep(step) => {}
            changed = cancel_rx.changed() => {
                match changed {
                    Ok(()) => {
                        if *cancel_rx.borrow() {
                            let _ = tx.send(StreamEvent::Cancelled).await;
                            return false;
                        }
                    }
                    // The cancellation sender is gone, so no further signal can
                    // arrive: fall back to a plain sleep instead of spinning.
                    Err(_) => tokio::time::sleep(step).await,
                }
            }
        }
    }
}

/// Rounds a duration up to whole seconds for a countdown display.
fn seconds_ceil(duration: Duration) -> u64 {
    let seconds = duration.as_secs();
    if duration.subsec_nanos() > 0 {
        seconds.saturating_add(1)
    } else {
        seconds
    }
}

/// Returns whether a failure message describes a transient transport condition.
fn is_retryable_message(message: &str) -> bool {
    if let Some(status) = http_status(message) {
        return is_transient_status(status);
    }

    // Markers emitted by the transport layer itself for interrupted streams and
    // connections that never reached the response headers.
    const TRANSIENT_MARKERS: [&str; 9] = [
        "stream ended prematurely",
        "stream read error",
        "failed to connect",
        "connection closed",
        "connection reset",
        "broken pipe",
        "timed out",
        "timeout",
        JSON_TRUNCATION_MARKER,
    ];

    let lowered = message.to_ascii_lowercase();
    TRANSIENT_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// Returns whether an error chain carries a transient transport failure.
fn is_retryable_error(err: &anyhow::Error) -> bool {
    if err.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_timeout() || error.is_connect() || error.is_body())
    }) {
        return true;
    }

    err.chain()
        .any(|cause| is_retryable_message(&cause.to_string()))
}

/// Returns whether an HTTP status describes a transient condition.
fn is_transient_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Extracts the status code out of an `... HTTP error <code>: ...` message.
fn http_status(message: &str) -> Option<u16> {
    let rest = message.split_once("HTTP error ")?.1;
    let digits = rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|digits| !digits.is_empty())?;
    digits.parse().ok()
}

/// Returns a uniformly distributed `u64` without pulling in an entropy crate.
///
/// [`std::collections::hash_map::RandomState`] is seeded from the operating
/// system once per process, so a fresh hasher yields a value that concurrent
/// clients do not share.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};

    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0x9E37_79B9_7F4A_7C15);
    hasher.finish()
}

/// Truncates a failure description for the client and the journal.
fn summarize(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.chars().count() <= REASON_SUMMARY_CHARS {
        return trimmed.to_string();
    }
    let mut snippet: String = trimmed.chars().take(REASON_SUMMARY_CHARS).collect();
    snippet.push('…');
    snippet
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::providers::mock::{MockLlmProvider, MockResponse};

    use super::*;

    /// One scripted provider call: the deltas it streams and the failure it
    /// ends with.
    #[derive(Clone, Debug, Default)]
    struct ScriptedAttempt {
        deltas: Vec<String>,
        reasoning: Vec<String>,
        stream_error: Option<&'static str>,
        transport_error: Option<&'static str>,
    }

    impl ScriptedAttempt {
        fn text(delta: &'static str) -> Self {
            Self {
                deltas: vec![delta.to_string()],
                ..Self::default()
            }
        }

        fn transient(message: &'static str) -> Self {
            Self {
                transport_error: Some(message),
                ..Self::default()
            }
        }

        /// Streams `chunks` copies of `chunk` as answer text.
        fn text_chunks(chunk: &str, chunks: usize) -> Self {
            Self {
                deltas: vec![chunk.to_string(); chunks],
                ..Self::default()
            }
        }

        /// Streams `chunks` copies of `chunk` as reasoning text.
        fn reasoning_chunks(chunk: &str, chunks: usize) -> Self {
            Self {
                reasoning: vec![chunk.to_string(); chunks],
                ..Self::default()
            }
        }
    }

    #[derive(Default)]
    struct ScriptedState {
        attempts: Mutex<VecDeque<ScriptedAttempt>>,
        calls: AtomicUsize,
        sessions: Mutex<Vec<Option<String>>>,
        efforts: Mutex<Vec<Option<ReasoningEffort>>>,
    }

    struct ScriptedProvider {
        state: Arc<ScriptedState>,
    }

    impl ScriptedProvider {
        fn new(attempts: Vec<ScriptedAttempt>) -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(ScriptedState {
                    attempts: Mutex::new(attempts.into()),
                    ..ScriptedState::default()
                }),
            })
        }

        fn calls(&self) -> usize {
            self.state.calls.load(Ordering::SeqCst)
        }

        fn sessions(&self) -> Vec<Option<String>> {
            self.state.sessions.lock().expect("session log").clone()
        }

        fn efforts(&self) -> Vec<Option<ReasoningEffort>> {
            self.state.efforts.lock().expect("effort log").clone()
        }
    }

    #[async_trait]
    impl LlmProvider for Arc<ScriptedProvider> {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenRouter
        }

        async fn stream_chat(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            _response_format: Option<ResponseFormat>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }

        async fn stream_chat_with_session(
            &self,
            _model: &str,
            _messages: Vec<ChatMessage>,
            _response_format: Option<ResponseFormat>,
            reasoning_effort: Option<ReasoningEffort>,
            session_id: Option<&str>,
            tx: mpsc::Sender<StreamEvent>,
            _cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            self.state.calls.fetch_add(1, Ordering::SeqCst);
            self.state
                .sessions
                .lock()
                .expect("session log")
                .push(session_id.map(str::to_string));
            self.state
                .efforts
                .lock()
                .expect("effort log")
                .push(reasoning_effort);

            let attempt = self
                .state
                .attempts
                .lock()
                .expect("script")
                .pop_front()
                .expect("script exhausted");

            for delta in attempt.deltas {
                if tx.send(StreamEvent::TextDelta(delta)).await.is_err() {
                    return Ok(());
                }
            }

            for delta in attempt.reasoning {
                if tx.send(StreamEvent::ReasoningDelta(delta)).await.is_err() {
                    return Ok(());
                }
            }

            if let Some(message) = attempt.stream_error {
                let _ = tx.send(StreamEvent::Error(message.to_string())).await;
                return Ok(());
            }
            if let Some(message) = attempt.transport_error {
                return Err(anyhow::anyhow!(message));
            }

            let _ = tx.send(StreamEvent::Done).await;
            Ok(())
        }
    }

    /// Retry policy whose pauses are effectively instantaneous.
    fn fast_config(max_attempts: u32) -> RetryConfig {
        RetryConfig {
            max_attempts,
            base_delay_ms: 1,
            max_delay_ms: 1,
        }
    }

    /// Joins the collected event stream of a single decorated request.
    async fn drive(
        provider: &Arc<ScriptedProvider>,
        config: RetryConfig,
        session_id: Option<&str>,
    ) -> (anyhow::Result<()>, Vec<StreamEvent>) {
        drive_with_ceiling(provider, config, session_id, DEFAULT_MAX_STREAM_BYTES).await
    }

    /// Joins the collected event stream of a single decorated request carrying
    /// an explicit runaway ceiling.
    async fn drive_with_ceiling(
        provider: &Arc<ScriptedProvider>,
        config: RetryConfig,
        session_id: Option<&str>,
        max_stream_bytes: usize,
    ) -> (anyhow::Result<()>, Vec<StreamEvent>) {
        let (tx, mut rx) = mpsc::channel(RELAY_CAPACITY);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let retrying = RetryingProvider::new(Box::new(Arc::clone(provider)), config)
            .with_max_stream_bytes(max_stream_bytes);

        let collector = async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        };

        let stream = retrying.stream_chat_with_session(
            "test-model",
            vec![ChatMessage::user("hi")],
            None,
            Some(ReasoningEffort::High),
            session_id,
            tx,
            cancel_rx,
        );

        tokio::join!(stream, collector)
    }

    fn streamed_text(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Adapter that lets the decorator wrap a scripted [`MockLlmProvider`]
    /// while the recorded requests stay observable from the test.
    struct SharedMock(Arc<MockLlmProvider>);

    #[async_trait]
    impl LlmProvider for SharedMock {
        fn kind(&self) -> ProviderKind {
            self.0.kind()
        }

        async fn stream_chat(
            &self,
            model: &str,
            messages: Vec<ChatMessage>,
            response_format: Option<ResponseFormat>,
            tx: mpsc::Sender<StreamEvent>,
            cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            self.0
                .stream_chat(model, messages, response_format, tx, cancel_rx)
                .await
        }

        async fn stream_chat_with_session(
            &self,
            model: &str,
            messages: Vec<ChatMessage>,
            response_format: Option<ResponseFormat>,
            reasoning_effort: Option<ReasoningEffort>,
            session_id: Option<&str>,
            tx: mpsc::Sender<StreamEvent>,
            cancel_rx: watch::Receiver<bool>,
        ) -> anyhow::Result<()> {
            self.0
                .stream_chat_with_session(
                    model,
                    messages,
                    response_format,
                    reasoning_effort,
                    session_id,
                    tx,
                    cancel_rx,
                )
                .await
        }
    }

    /// Joins the collected event stream of a single decorated mock request.
    async fn drive_mock(
        mock: &Arc<MockLlmProvider>,
        config: RetryConfig,
    ) -> (anyhow::Result<()>, Vec<StreamEvent>) {
        let (tx, mut rx) = mpsc::channel(RELAY_CAPACITY);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let retrying = RetryingProvider::new(Box::new(SharedMock(Arc::clone(mock))), config);

        let collector = async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        };

        let stream = retrying.stream_chat_with_session(
            "mock-model",
            vec![ChatMessage::user("hi")],
            None,
            None,
            Some("tauqe-plan-step-1"),
            tx,
            cancel_rx,
        );

        tokio::join!(stream, collector)
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_transparently() {
        let provider = ScriptedProvider::new(vec![
            ScriptedAttempt::transient("OpenRouter stream ended prematurely without [DONE] marker"),
            ScriptedAttempt::text("Hello"),
        ]);

        let (result, events) = drive(&provider, fast_config(3), None).await;

        assert!(result.is_ok(), "the retry must succeed: {:?}", result.err());
        assert_eq!(provider.calls(), 2);
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::RetryingConnection {
                attempt: 1,
                max_attempts: 3,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::ResetStream)),
            "a failure before any output needs no reset"
        );
        assert_eq!(streamed_text(&events), "Hello");
    }

    #[tokio::test]
    async fn a_mid_stream_failure_resets_the_accumulated_output() {
        let provider = ScriptedProvider::new(vec![
            ScriptedAttempt {
                deltas: vec!["Hel".to_string()],
                stream_error: Some("OpenRouter stream read error: connection reset by peer"),
                transport_error: None,
                ..ScriptedAttempt::default()
            },
            ScriptedAttempt::text("Hello"),
        ]);

        let (result, events) = drive(&provider, fast_config(3), None).await;

        assert!(result.is_ok(), "the retry must succeed: {:?}", result.err());
        assert_eq!(provider.calls(), 2);

        let reset_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::ResetStream))
            .expect("a mid-stream failure must reset the stream");
        let first_delta = events
            .iter()
            .position(|event| matches!(event, StreamEvent::TextDelta(delta) if delta == "Hel"))
            .expect("the first attempt streamed a delta");
        assert!(
            first_delta < reset_at,
            "the reset marks the seam after the partial output"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Error(_))),
            "the error of a retried attempt is withheld from the client"
        );
        assert_eq!(streamed_text(&events), "HelHello");
    }

    #[tokio::test]
    async fn a_fatal_status_is_reported_without_a_retry() {
        let provider = ScriptedProvider::new(vec![ScriptedAttempt::transient(
            "OpenRouter HTTP error 401 Unauthorized: invalid api key",
        )]);

        let (result, events) = drive(&provider, fast_config(3), None).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("401"));
        assert_eq!(provider.calls(), 1, "a fatal failure is never retried");
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::RetryingConnection { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Error(message) if message.contains("401"))));
    }

    #[tokio::test]
    async fn exhausted_attempts_report_the_last_failure_once() {
        let provider = ScriptedProvider::new(vec![
            ScriptedAttempt::transient("OpenRouter HTTP error 503: upstream unavailable"),
            ScriptedAttempt::transient("OpenRouter HTTP error 503: upstream unavailable"),
            ScriptedAttempt::transient("OpenRouter HTTP error 503: upstream unavailable"),
        ]);

        let (result, events) = drive(&provider, fast_config(2), None).await;

        assert!(result.is_err());
        assert_eq!(provider.calls(), 2, "the policy bounds the attempts");
        let errors = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Error(_)))
            .count();
        assert_eq!(errors, 1, "only the terminal failure reaches the client");
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Error(message) if message.contains("503"))));
    }

    #[tokio::test]
    async fn an_exhausted_stream_break_discards_the_partial_output_before_the_error() {
        let break_message = "OpenRouter stream read error: connection reset by peer";
        let provider = ScriptedProvider::new(vec![
            ScriptedAttempt {
                deltas: vec!["partial".to_string()],
                stream_error: Some(break_message),
                ..ScriptedAttempt::default()
            },
            ScriptedAttempt {
                deltas: vec!["partial".to_string()],
                stream_error: Some(break_message),
                ..ScriptedAttempt::default()
            },
        ]);

        let (result, events) = drive(&provider, fast_config(2), None).await;

        assert!(
            result.is_err(),
            "a stream that never completes must terminate the request"
        );
        assert_eq!(provider.calls(), 2, "the policy bounds the attempts");

        // Every interrupted attempt discards what it streamed before the next
        // one starts, so no partial answer is left in front of the client.
        let resets: Vec<usize> = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| matches!(event, StreamEvent::ResetStream).then_some(index))
            .collect();
        assert_eq!(
            resets.len(),
            2,
            "each interrupted attempt resets the stream"
        );

        let error_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::Error(_)))
            .expect("the terminal failure must be reported");
        assert!(
            resets[1] < error_at,
            "the last reset precedes the terminal error"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::Error(_)))
                .count(),
            1,
            "only the terminal failure reaches the client"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Error(message) if message.contains("connection reset")
        )));
    }

    #[tokio::test]
    async fn a_retry_keeps_the_session_and_the_reasoning_effort() {
        let provider = ScriptedProvider::new(vec![
            ScriptedAttempt::transient("OpenRouter HTTP error 429: Too Many Requests"),
            ScriptedAttempt::text("ok"),
        ]);

        let (result, _events) = drive(&provider, fast_config(3), Some("tauqe-plan-step-1")).await;

        assert!(result.is_ok(), "the retry must succeed: {:?}", result.err());
        assert_eq!(
            provider.sessions(),
            vec![
                Some("tauqe-plan-step-1".to_string()),
                Some("tauqe-plan-step-1".to_string())
            ],
            "every attempt carries the same provider session"
        );
        assert_eq!(
            provider.efforts(),
            vec![Some(ReasoningEffort::High), Some(ReasoningEffort::High)],
            "every attempt carries the same reasoning effort"
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_the_backoff_pause() {
        let provider = ScriptedProvider::new(vec![
            ScriptedAttempt::transient("OpenRouter stream read error: connection closed"),
            ScriptedAttempt::text("never"),
        ]);
        let config = RetryConfig {
            max_attempts: 4,
            base_delay_ms: 60_000,
            max_delay_ms: 60_000,
        };

        let (tx, mut rx) = mpsc::channel(RELAY_CAPACITY);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let retrying = RetryingProvider::new(Box::new(Arc::clone(&provider)), config);

        let stream = tokio::spawn(async move {
            retrying
                .stream_chat_with_session(
                    "test-model",
                    vec![ChatMessage::user("hi")],
                    None,
                    None,
                    None,
                    tx,
                    cancel_rx,
                )
                .await
        });
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });

        // Let the first attempt fail and the long pause begin.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cancelled_at = tokio::time::Instant::now();
        let _ = cancel_tx.send(true);

        let result = tokio::time::timeout(Duration::from_secs(2), stream)
            .await
            .expect("the pause must be interrupted promptly")
            .expect("the stream task joins");
        let events = collector.await.expect("the collector joins");

        assert!(result.is_ok());
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Cancelled)));
        assert_eq!(
            provider.calls(),
            1,
            "the cancelled pause never launches another attempt"
        );
    }

    #[tokio::test]
    async fn a_runaway_text_stream_is_aborted_at_the_ceiling() {
        let provider = ScriptedProvider::new(vec![ScriptedAttempt::text_chunks("0123456789", 64)]);

        let (result, events) = drive_with_ceiling(&provider, fast_config(3), None, 16).await;

        let error = result.expect_err("the ceiling must terminate the request");
        assert!(
            error.to_string().contains("safety ceiling"),
            "unexpected error: {error}"
        );
        assert_eq!(provider.calls(), 1, "a runaway generation is never retried");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::RetryingConnection { .. })),
            "a runaway generation never enters a backoff pause"
        );

        let forwarded: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            forwarded,
            vec!["0123456789"],
            "the delta that crosses the ceiling is never forwarded"
        );

        let reset_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::ResetStream))
            .expect("the partial output must be discarded");
        let error_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::Error(_)))
            .expect("the ceiling must be reported");
        assert!(reset_at < error_at, "the reset precedes the terminal error");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::Error(_)))
                .count(),
            1,
            "the terminal failure reaches the client exactly once"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Error(message) if message.contains("16 bytes")
        )));
    }

    #[tokio::test]
    async fn a_runaway_reasoning_stream_is_aborted_at_the_ceiling() {
        let provider =
            ScriptedProvider::new(vec![ScriptedAttempt::reasoning_chunks("thinking...", 64)]);

        let (result, events) = drive_with_ceiling(&provider, fast_config(3), None, 16).await;

        let error = result.expect_err("the ceiling must terminate the request");
        assert!(error.to_string().contains("safety ceiling"));
        assert_eq!(provider.calls(), 1, "a runaway generation is never retried");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::ReasoningDelta(_)))
                .count(),
            1,
            "reasoning tokens count against the ceiling too"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Error(message) if message.contains("safety ceiling")
        )));
    }

    #[tokio::test]
    async fn a_retry_restarts_the_ceiling_with_the_discarded_output() {
        let provider = ScriptedProvider::new(vec![
            ScriptedAttempt {
                deltas: vec!["0123456789".to_string()],
                transport_error: Some("OpenRouter stream read error: connection reset by peer"),
                ..ScriptedAttempt::default()
            },
            ScriptedAttempt::text("0123456789"),
        ]);

        let (result, events) = drive_with_ceiling(&provider, fast_config(3), None, 16).await;

        assert!(
            result.is_ok(),
            "each attempt is measured on its own: {:?}",
            result.err()
        );
        assert_eq!(provider.calls(), 2);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Error(_))),
            "a retried attempt never reports its failure"
        );
        assert_eq!(streamed_text(&events), "01234567890123456789");
    }

    #[tokio::test]
    async fn output_exactly_at_the_ceiling_is_accepted() {
        let provider = ScriptedProvider::new(vec![ScriptedAttempt::text_chunks("0123456789", 2)]);

        let (result, events) = drive_with_ceiling(&provider, fast_config(3), None, 20).await;

        assert!(
            result.is_ok(),
            "a stream that stops at the ceiling is valid: {:?}",
            result.err()
        );
        assert_eq!(streamed_text(&events), "01234567890123456789");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Error(_))),
            "the ceiling itself is not a failure"
        );
    }

    #[tokio::test]
    async fn one_byte_over_the_ceiling_aborts_the_attempt() {
        let provider = ScriptedProvider::new(vec![ScriptedAttempt::text_chunks("0123456789", 2)]);

        let (result, events) = drive_with_ceiling(&provider, fast_config(3), None, 19).await;

        let error = result.expect_err("one byte past the ceiling must abort");
        assert!(
            error.to_string().contains("safety ceiling"),
            "unexpected error: {error}"
        );
        assert_eq!(provider.calls(), 1, "a runaway generation is never retried");
        assert_eq!(
            streamed_text(&events),
            "0123456789",
            "the delta that crosses the ceiling is never forwarded"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Error(message) if message.contains("19 bytes")
        )));
    }

    #[tokio::test]
    async fn reasoning_and_answer_share_one_output_ceiling() {
        let provider = ScriptedProvider::new(vec![ScriptedAttempt {
            deltas: vec!["0123456789".to_string()],
            reasoning: vec!["0123456789".to_string()],
            ..ScriptedAttempt::default()
        }]);

        let (result, events) = drive_with_ceiling(&provider, fast_config(3), None, 16).await;

        let error = result.expect_err("the ceiling covers both channels");
        assert!(
            error.to_string().contains("safety ceiling"),
            "unexpected error: {error}"
        );
        assert_eq!(provider.calls(), 1, "a runaway generation is never retried");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "0123456789")),
            "the answer delta below the ceiling is forwarded"
        );
        assert!(
            !events.iter().any(
                |event| matches!(event, StreamEvent::ReasoningDelta(text) if text == "0123456789")
            ),
            "the delta that crosses the cumulative ceiling is never forwarded"
        );
    }

    #[tokio::test]
    async fn a_mock_failure_before_any_token_is_retried_transparently() {
        let mock = Arc::new(MockLlmProvider::scripted([
            MockResponse::error("OpenRouter HTTP error 429: Too Many Requests"),
            MockResponse::text("Hello"),
        ]));

        let (result, events) = drive_mock(&mock, fast_config(3)).await;

        assert!(result.is_ok(), "the retry must succeed: {:?}", result.err());
        assert_eq!(mock.requests().len(), 2, "the request is re-issued once");
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::RetryingConnection {
                attempt: 1,
                max_attempts: 3,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::ResetStream)),
            "an attempt that streamed nothing needs no reset"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Error(_))),
            "the error of a retried attempt is withheld from the client"
        );
        assert_eq!(streamed_text(&events), "Hello");
    }

    #[tokio::test]
    async fn a_mock_mid_stream_break_resets_the_partial_output() {
        let mock = Arc::new(MockLlmProvider::scripted([
            MockResponse::error("OpenRouter stream read error: connection reset by peer")
                .with_reasoning("partial plan"),
            MockResponse::text("Hello"),
        ]));

        let (result, events) = drive_mock(&mock, fast_config(3)).await;

        assert!(result.is_ok(), "the retry must succeed: {:?}", result.err());
        assert_eq!(mock.requests().len(), 2);

        let partial_at = events
            .iter()
            .position(
                |event| matches!(event, StreamEvent::ReasoningDelta(text) if text == "partial plan"),
            )
            .expect("the first attempt streamed partial output");
        let reset_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::ResetStream))
            .expect("a mid-stream break must reset the stream");
        assert!(
            partial_at < reset_at,
            "the reset marks the seam after the partial output"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Error(_))),
            "the intermediate failure never reaches the client"
        );
        assert_eq!(streamed_text(&events), "Hello");
    }

    #[tokio::test]
    async fn a_mock_fatal_error_is_reported_without_another_attempt() {
        let mock = Arc::new(MockLlmProvider::scripted([
            MockResponse::error("OpenRouter HTTP error 401 Unauthorized: invalid api key"),
            MockResponse::text("never"),
        ]));

        let (result, events) = drive_mock(&mock, fast_config(3)).await;

        assert!(result.is_err(), "a rejected key is not recoverable");
        assert_eq!(mock.requests().len(), 1, "a fatal failure is never retried");
        assert_eq!(
            mock.remaining(),
            1,
            "the unused script stays untouched for a later turn"
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::RetryingConnection { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Error(message) if message.contains("401"))));
    }

    #[tokio::test]
    async fn a_mock_backoff_pause_yields_to_cancellation() {
        let mock = Arc::new(MockLlmProvider::scripted([
            MockResponse::error("OpenRouter HTTP error 503: upstream unavailable"),
            MockResponse::text("never"),
        ]));
        let config = RetryConfig {
            max_attempts: 4,
            base_delay_ms: 60_000,
            max_delay_ms: 60_000,
        };

        let (tx, mut rx) = mpsc::channel(RELAY_CAPACITY);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let retrying = RetryingProvider::new(Box::new(SharedMock(Arc::clone(&mock))), config);

        let stream = tokio::spawn(async move {
            retrying
                .stream_chat_with_session(
                    "mock-model",
                    vec![ChatMessage::user("hi")],
                    None,
                    None,
                    None,
                    tx,
                    cancel_rx,
                )
                .await
        });
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });

        // Let the first attempt fail and the long pause begin.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cancelled_at = tokio::time::Instant::now();
        let _ = cancel_tx.send(true);

        let result = tokio::time::timeout(Duration::from_secs(2), stream)
            .await
            .expect("the pause must be interrupted promptly")
            .expect("the stream task joins");
        let events = collector.await.expect("the collector joins");

        assert!(result.is_ok());
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Cancelled)));
        assert_eq!(
            mock.requests().len(),
            1,
            "the cancelled pause never launches another attempt"
        );
        assert_eq!(
            mock.remaining(),
            1,
            "the queued answer survives the interrupted turn"
        );
    }

    #[tokio::test]
    async fn a_json_truncated_at_the_token_limit_is_retried_and_resets_the_stream() {
        let provider = ScriptedProvider::new(vec![
            ScriptedAttempt {
                deltas: vec!["{\"message\":\"par".to_string()],
                stream_error: Some(
                    "OpenRouter structured JSON response truncated by finish_reason: length",
                ),
                ..ScriptedAttempt::default()
            },
            ScriptedAttempt::text("{\"message\":\"partial\"}"),
        ]);

        let (result, events) = drive(&provider, fast_config(3), None).await;

        assert!(result.is_ok(), "the retry must succeed: {:?}", result.err());
        assert_eq!(provider.calls(), 2);
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::RetryingConnection { attempt: 1, .. })));

        let partial_at = events
            .iter()
            .position(|event| {
                matches!(event, StreamEvent::TextDelta(delta) if delta == "{\"message\":\"par")
            })
            .expect("the truncated attempt streamed a fragment");
        let reset_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::ResetStream))
            .expect("the partial document must be discarded");
        assert!(partial_at < reset_at, "the reset marks the seam");

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Error(_))),
            "the truncation of a retried attempt is withheld from the client"
        );
        assert!(streamed_text(&events).ends_with("{\"message\":\"partial\"}"));
    }

    #[test]
    fn transient_statuses_and_markers_are_retryable() {
        for status in [408, 425, 429, 500, 502, 503, 504] {
            assert!(is_retryable_message(&format!(
                "OpenRouter HTTP error {status}: upstream"
            )));
        }
        for status in [400, 401, 403, 404, 413, 422] {
            assert!(!is_retryable_message(&format!(
                "OpenRouter HTTP error {status}: rejected"
            )));
        }
        assert!(is_retryable_message(
            "OpenRouter stream ended prematurely without [DONE] marker"
        ));
        assert!(is_retryable_message(&format!(
            "OpenRouter structured JSON response {JSON_TRUNCATION_MARKER}"
        )));
        assert!(is_retryable_message(
            "Failed to connect to OpenRouter API: connection refused"
        ));
        assert!(!is_retryable_message(
            "OpenRouter streaming choice error: content filter"
        ));
    }

    #[test]
    fn backoff_doubles_with_bounded_jitter() {
        let config = RetryConfig {
            max_attempts: 6,
            base_delay_ms: 100,
            max_delay_ms: 1_000,
        };

        for attempt in 1..=5 {
            let ceiling = (100u64 << (attempt - 1)).min(1_000);
            let delay = config.delay_for(attempt).as_millis() as u64;
            assert!(
                delay >= ceiling / 2 && delay <= ceiling,
                "attempt {attempt} produced an out-of-range pause of {delay} ms"
            );
        }
    }
}
