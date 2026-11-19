//! Wire regression tests for Architect stream publication.
#![cfg(test)]
use std::sync::Arc;

use crate::handlers::common::emit;
use crate::state::{AppState, OperationGuard};
#[cfg(test)]
use crate::usage::UsageGuard;
use tauqe_core::config::{AppConfig, ModelsConfig};
use tauqe_core::context::ContextManager;
use tauqe_core::control::refine::StructuredRefineOutcome;
use tauqe_core::models::ArchitectTier;
use tauqe_core::providers::{LlmProvider, StreamEvent};
use tauqe_core::workflow::WorkflowOptions;
use tauqe_protocol::events;
#[cfg(test)]
use tokio::sync::mpsc;
use tokio::sync::watch;

#[cfg(test)]
use tauqe_flow::api::ContourRelaySummary;
#[cfg(test)]
async fn relay_contour_events(
    state: Arc<AppState>,
    op_id: String,
    usage: UsageGuard,
    events_rx: mpsc::Receiver<StreamEvent>,
    cancel: watch::Receiver<bool>,
) -> ContourRelaySummary {
    tauqe_flow::api::test_relay_architect_events(
        Arc::new(crate::publication::Publisher {
            out: state.out.clone(),
        }),
        op_id,
        usage,
        events_rx,
        cancel,
    )
    .await
}
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_contour_synthesis(
    state: &Arc<AppState>,
    operation: &OperationGuard,
    op_id: &str,
    task: String,
    context: ContextManager,
    options: WorkflowOptions,
    models: ModelsConfig,
    tier: ArchitectTier,
    intro_header: String,
    budget_hint: String,
) -> (anyhow::Result<StructuredRefineOutcome>, ContourRelaySummary) {
    state
        .application
        .test_run_architect_contour(
            operation,
            tauqe_flow::api::PlanHorizonContour {
                operation_id: op_id.to_string(),
                task,
                context,
                options,
                models_config: models,
                tier,
                intro_header,
                budget_hint,
            },
            Arc::new(crate::publication::Publisher {
                out: state.out.clone(),
            }),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::context::ContextAccess;

    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tauqe_core::control::refine::StructuredRefineStatus;
    use tauqe_core::models::{ModelRef, ModelRole, ReasoningEffort};
    use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
    use tauqe_core::providers::ModelUsageInfo;
    use tauqe_core::workflow::turn::TurnPhase;

    use crate::handlers::common::test_support::scratch_root;
    use crate::transport::OutChannel;

    /// Builds a daemon state bound to its own scratch repository root.
    fn test_state(
        label: &str,
    ) -> (
        Arc<AppState>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        PathBuf,
    ) {
        let root = scratch_root(&format!("architect-bridge-{label}"));
        let (out, rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
        (state, rx, root)
    }

    /// Decodes the parameters of the events carrying `method`.
    fn collect(raw_events: &[String], method: &str) -> Vec<serde_json::Value> {
        raw_events
            .iter()
            .filter_map(|raw| {
                let value: serde_json::Value = serde_json::from_str(raw).ok()?;
                if value.get("method").and_then(|value| value.as_str()) != Some(method) {
                    return None;
                }
                value.get("params").cloned()
            })
            .collect()
    }

    /// Builds one Architect payload declaring the given status.
    fn success_payload(explanation: &str) -> String {
        serde_json::json!({
            "status": "success",
            "explanation": explanation,
            "reason": null,
            "context_requests": [],
            "repo_searches": [],
            "repo_maps": [],
            "items": []
        })
        .to_string()
    }

    /// The relay is the single translation seam between the contour vocabulary
    /// and the wire: every streamed coordinate reaches the client, a withdrawn
    /// attempt drops the fragments it already streamed, and the usage window is
    /// closed exactly once.
    #[tokio::test]
    async fn relay_translates_contour_events_and_closes_the_usage_window() {
        let (state, mut rx, root) = test_state("relay");
        let usage = state.begin_usage("op-relay").await;

        let (events_tx, events_rx) = mpsc::channel(64);
        let (_cancel_tx, cancel) = watch::channel(false);
        let relay = tokio::spawn(relay_contour_events(
            Arc::clone(&state),
            "op-relay".to_string(),
            usage,
            events_rx,
            cancel,
        ));

        let scripted = [
            StreamEvent::TextDelta("First attempt.".to_string()),
            StreamEvent::ReasoningDelta("Weighing the invariants".to_string()),
            StreamEvent::TierEscalated {
                tier: 1,
                model: ModelRef::openrouter("mock/strong"),
                role: ModelRole::Architect,
                effort: Some(ReasoningEffort::High),
            },
            StreamEvent::TurnPhase {
                phase: TurnPhase::Research,
                round: Some(1),
                max_rounds: Some(3),
                detail: Some("Research round 1".to_string()),
            },
            StreamEvent::RepoMapTokens(512),
            StreamEvent::RetryingConnection {
                attempt: 1,
                max_attempts: 3,
                delay_secs: 2,
                reason: "connection reset mid-stream".to_string(),
            },
            StreamEvent::Usage(ModelUsageInfo {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..ModelUsageInfo::default()
            }),
            StreamEvent::ResetStream,
            StreamEvent::TextDelta("Second attempt.".to_string()),
            StreamEvent::Done,
        ];
        for event in scripted {
            events_tx.send(event).await.expect("the relay is listening");
        }
        drop(events_tx);

        let summary = relay.await.expect("the relay joins");
        assert_eq!(summary.assistant_text, "Second attempt.");
        assert!(!summary.cancelled);
        assert!(summary.error.is_none());
        assert_eq!(
            state.usage_snapshot().await.operations,
            1,
            "the relay must close the usage window of the synthesis"
        );

        let mut raw_events = Vec::new();
        while let Ok(raw) = rx.try_recv() {
            raw_events.push(raw);
        }

        let deltas: Vec<String> = collect(&raw_events, events::MODEL_TEXT_DELTA)
            .into_iter()
            .filter_map(|params| {
                params
                    .get("delta")
                    .and_then(|delta| delta.as_str())
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(
            deltas,
            vec!["First attempt.".to_string(), "Second attempt.".to_string()]
        );
        assert_eq!(
            collect(&raw_events, events::MODEL_STREAM_RESET).len(),
            1,
            "a voided attempt must withdraw the fragments it streamed"
        );

        let reasoning = collect(&raw_events, events::MODEL_REASONING_DELTA);
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0]["delta"], "Weighing the invariants");

        let escalated = collect(&raw_events, events::MODEL_ESCALATED);
        assert_eq!(escalated.len(), 1);
        assert_eq!(escalated[0]["operation_id"], "op-relay");
        assert_eq!(escalated[0]["model"]["name"], "mock/strong");
        assert_eq!(escalated[0]["role"], "architect");
        assert_eq!(escalated[0]["effort"], "high");
        assert_eq!(escalated[0]["tier"], 1);

        let phases = collect(&raw_events, events::TURN_PHASE);
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0]["phase"], "research");
        assert_eq!(phases[0]["round"], 1);
        assert_eq!(phases[0]["max_rounds"], 3);

        let maps = collect(&raw_events, events::TURN_REPOMAP);
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0]["tokens"], 512);

        let retrying = collect(&raw_events, events::MODEL_CONNECTION_RETRYING);
        assert_eq!(retrying.len(), 1);
        assert_eq!(retrying[0]["attempt"], 1);
        assert_eq!(retrying[0]["delay_secs"], 2);
        assert_eq!(retrying[0]["reason"], "connection reset mid-stream");

        // The observed usage is folded into the session totals the final
        // snapshot publishes.
        let snapshots = collect(&raw_events, events::MODEL_USAGE_TOTALS);
        let last = snapshots.last().expect("the relay publishes the snapshot");
        assert_eq!(last["snapshot"]["session"]["total_tokens"], 15);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Cancellation and failure are reported to the client and carried on the
    /// summary, so the adapter can end the operation deterministically.
    #[tokio::test]
    async fn relay_reports_cancellation_and_failure_on_the_summary() {
        let (state, mut rx, root) = test_state("relay-cancel");
        let usage = state.begin_usage("op-cancel").await;

        let (events_tx, events_rx) = mpsc::channel(16);
        let (_cancel_tx, cancel) = watch::channel(false);
        let relay = tokio::spawn(relay_contour_events(
            Arc::clone(&state),
            "op-cancel".to_string(),
            usage,
            events_rx,
            cancel,
        ));
        events_tx
            .send(StreamEvent::Error(
                "provider rejected the request".to_string(),
            ))
            .await
            .expect("the relay is listening");
        events_tx
            .send(StreamEvent::Cancelled)
            .await
            .expect("the relay is listening");
        drop(events_tx);

        let summary = relay.await.expect("the relay joins");
        assert!(summary.cancelled);
        assert_eq!(
            summary.error.as_deref(),
            Some("provider rejected the request")
        );

        let mut raw_events = Vec::new();
        while let Ok(raw) = rx.try_recv() {
            raw_events.push(raw);
        }
        assert_eq!(collect(&raw_events, events::MODEL_ERROR).len(), 1);
        assert_eq!(collect(&raw_events, events::MODEL_CANCELLED).len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The skeleton resolves the initial client through the injected seam, runs
    /// the contour to a typed synthesized outcome, accounts the run as one
    /// operation, and merges the context the contour worked on back into the
    /// shared state.
    ///
    /// The current-thread flavor is the deliberate host: no seam of the launch
    /// skeleton may require the daemon's multi-threaded runtime.
    #[tokio::test]
    async fn contour_skeleton_synthesizes_and_merges_the_context() {
        let (state, _events, root) = test_state("skeleton");
        tauqe_core::git::init_repository(Some(&root), true).expect("init the fixture repository");
        std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").expect("write the fixture");

        let provider = Arc::new(MockLlmProvider::scripted([MockResponse::text(
            success_payload("The horizon follows from the gathered material"),
        )]));
        state
            .application
            .test_set_provider(provider.clone() as Arc<dyn LlmProvider>)
            .await;

        let config = state.application.configuration().await.config.clone();
        let models = config.models.clone();
        let tier = models.architect_tier(0);
        let options = WorkflowOptions::from_app_config(&config);

        // The adapter hands the contour a snapshot of the shared context, so the
        // synthesis works on its own clone and reports the result back.
        let mut snapshot = state.context_snapshot().await;
        snapshot
            .add_file("a.rs", ContextAccess::Editable)
            .expect("admit the fixture into the snapshot");
        assert!(
            !state
                .context_snapshot()
                .await
                .contains("a.rs")
                .expect("the shared context reads"),
            "the snapshot entry must not be visible before the merge"
        );

        let operation = state
            .try_begin_operation()
            .expect("the operation slot is free");
        let (outcome, summary) = run_contour_synthesis(
            &state,
            &operation,
            "op-skeleton",
            "Synthesize the unfinished horizon".to_string(),
            snapshot,
            options,
            models,
            tier,
            String::new(),
            String::new(),
        )
        .await;

        let outcome = outcome.expect("the synthesis runs to completion");
        assert_eq!(outcome.status, StructuredRefineStatus::Synthesized);
        assert!(outcome.output.is_some());
        assert!(!summary.cancelled);
        assert!(summary.error.is_none());
        assert_eq!(
            provider.requests().len(),
            1,
            "one accepted round spends one provider request"
        );
        assert_eq!(
            state.usage_snapshot().await.operations,
            1,
            "the whole synthesis is accounted as one operation"
        );

        assert!(
            state
                .context_snapshot()
                .await
                .contains("a.rs")
                .expect("the shared context reads"),
            "the contour context must be merged back into the shared state"
        );

        drop(operation);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Sink that records everything the outbound writer publishes, so a pacing
    /// test observes the relayed telemetry without touching the real stdout of
    /// the process.
    #[derive(Clone, Default)]
    struct RecordingSink {
        published: Arc<Mutex<Vec<u8>>>,
    }

    impl RecordingSink {
        /// Everything the writer published so far, decoded as UTF-8.
        fn published(&self) -> String {
            self.published
                .lock()
                .map(|published| String::from_utf8_lossy(&published).into_owned())
                .unwrap_or_default()
        }
    }

    impl tokio::io::AsyncWrite for RecordingSink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if let Ok(mut published) = self.published.lock() {
                published.extend_from_slice(buf);
            }
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Pre-fills the outbound queue one line above the transport's high-water
    /// mark, so the relay observes a client that keeps its pipe open but
    /// stopped reading its stream.
    fn stall_outbound_transport(state: &AppState) {
        for _ in 0..=crate::transport::BACKLOG_HIGH_WATER {
            emit(&state.out, "test/filler", None);
        }
    }

    /// The pacing contract covers the whole relay rather than the textual arms
    /// alone: a stalled client holds every forwarded coordinate, and the drain
    /// of the queue releases it.
    ///
    /// The hold is read from the relay itself instead of from the queue depth.
    /// The depth falls only once a writer publishes what it dequeued, so a
    /// report the gate parked leaves the terminal event behind it unread. A
    /// relay that forwarded the report before the gate would consume that
    /// event and finish while the client is still stalled.
    #[tokio::test]
    async fn relay_withholds_telemetry_until_a_writer_drains_the_stalled_queue() {
        let (state, rx, root) = test_state("relay-pacing");
        stall_outbound_transport(&state);

        let usage = state.begin_usage("op-paced").await;
        let (events_tx, events_rx) = mpsc::channel(16);
        let (_cancel_tx, cancel) = watch::channel(false);
        let mut relay = tokio::spawn(relay_contour_events(
            Arc::clone(&state),
            "op-paced".to_string(),
            usage,
            events_rx,
            cancel,
        ));
        // A repo-map report used to travel ungated, so it is the coordinate
        // that proves the loop-level policy instead of the textual arms alone.
        events_tx
            .send(StreamEvent::RepoMapTokens(512))
            .await
            .expect("the relay is listening");
        events_tx
            .send(StreamEvent::Done)
            .await
            .expect("the relay is listening");

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut relay)
                .await
                .is_err(),
            "a stalled client must hold the relayed telemetry in the gate"
        );

        let sink = RecordingSink::default();
        let writing = tokio::spawn(state.out.writer(rx).run_with(sink.clone()));

        let mut published = String::new();
        for _ in 0..200 {
            published = sink.published();
            if published.contains(events::TURN_REPOMAP) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            published.contains(events::TURN_REPOMAP) && published.contains("\"tokens\":512"),
            "the drain of the stalled queue must release the paused relay"
        );

        drop(events_tx);
        let summary = tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .expect("the released relay reaches the terminal event")
            .expect("the relay joins");
        assert!(summary.assistant_text.is_empty());
        assert!(!summary.cancelled);
        assert!(summary.error.is_none());

        writing.abort();
        let _ = writing.await;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A raised cancellation releases a paused relay even while the client is
    /// still stalled, so an interrupted synthesis reaches its terminal events
    /// without waiting for a reader that never returns.
    #[tokio::test]
    async fn a_cancelled_operation_releases_a_paused_relay_without_a_drain() {
        let (state, mut rx, root) = test_state("relay-pacing-cancel");
        stall_outbound_transport(&state);

        let usage = state.begin_usage("op-paced-cancel").await;
        let (events_tx, events_rx) = mpsc::channel(16);
        let (cancel_tx, cancel) = watch::channel(false);
        let relay = tokio::spawn(relay_contour_events(
            Arc::clone(&state),
            "op-paced-cancel".to_string(),
            usage,
            events_rx,
            cancel,
        ));
        events_tx
            .send(StreamEvent::RepoMapTokens(512))
            .await
            .expect("the relay is listening");

        while rx.try_recv().is_ok() {}
        assert!(
            tokio::time::timeout(Duration::from_millis(20), rx.recv())
                .await
                .is_err(),
            "the stalled client must hold the relay"
        );

        let _ = cancel_tx.send(true);
        events_tx
            .send(StreamEvent::Done)
            .await
            .expect("the relay is listening");
        drop(events_tx);

        let summary = tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .expect("a cancelled operation must not wait for the stalled client")
            .expect("the relay joins");
        assert!(!summary.cancelled);

        let mut raw_events = Vec::new();
        while let Ok(raw) = rx.try_recv() {
            raw_events.push(raw);
        }
        let maps = collect(&raw_events, events::TURN_REPOMAP);
        assert_eq!(
            maps.len(),
            1,
            "the released relay must publish the telemetry it held"
        );
        assert_eq!(maps[0]["tokens"], 512);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A terminal notice a provider reports on its own must reach the client
    /// even while the outbound queue is stalled and no hard cancel was raised.
    ///
    /// Gating the notice parked the relay — and with it the joined contour and
    /// the operation slot it owns — until the client drained its pipe, so a
    /// cooperative `Cancelled` or a terminal `Error` could hang the synthesis
    /// indefinitely. The transport exempts every terminal notice from pacing,
    /// which is what this regression pins.
    #[tokio::test]
    async fn terminal_notices_reach_a_stalled_client_without_a_drain() {
        let cancellation = StreamEvent::Cancelled;
        let failure = StreamEvent::Error("provider rejected the request".to_string());
        let cases = [
            ("cancelled", cancellation, events::MODEL_CANCELLED),
            ("error", failure, events::MODEL_ERROR),
        ];

        for (label, notice, method) in cases {
            let cancelled = matches!(notice, StreamEvent::Cancelled);
            let (state, mut rx, root) = test_state(&format!("relay-terminal-{label}"));
            stall_outbound_transport(&state);

            let usage = state.begin_usage(&format!("op-terminal-{label}")).await;
            let (events_tx, events_rx) = mpsc::channel(16);
            // No hard cancel is raised: the provider reports the notice on its
            // own, exactly as a stream that ends in `Cancelled` or `Error` does.
            let (_cancel_tx, cancel) = watch::channel(false);
            let relay = tokio::spawn(relay_contour_events(
                Arc::clone(&state),
                format!("op-terminal-{label}"),
                usage,
                events_rx,
                cancel,
            ));
            events_tx
                .send(notice)
                .await
                .expect("the relay is listening");
            events_tx
                .send(StreamEvent::Done)
                .await
                .expect("the relay is listening");
            drop(events_tx);

            let summary = tokio::time::timeout(Duration::from_secs(1), relay)
                .await
                .expect("a stalled client must not park a terminal notice")
                .expect("the relay joins");
            assert_eq!(
                summary.cancelled, cancelled,
                "the relay must classify the terminal notice it relayed"
            );
            if cancelled {
                assert!(summary.error.is_none());
            } else {
                assert_eq!(
                    summary.error.as_deref(),
                    Some("provider rejected the request")
                );
            }

            let mut raw_events = Vec::new();
            while let Ok(raw) = rx.try_recv() {
                raw_events.push(raw);
            }
            assert_eq!(
                collect(&raw_events, method).len(),
                1,
                "the terminal notice must reach the client without a drain"
            );

            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
