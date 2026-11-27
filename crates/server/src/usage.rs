//! Usage RPC publication and adapters for core-owned accounting.

#[cfg(test)]
use crate::state::AppState;
use crate::transport::OutChannel;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
pub use tauqe_flow::api::UsageGuard;
use tauqe_flow::api::{UsageTotals, UsageTotalsSnapshot};
use tauqe_protocol::model::ModelUsageTotalsEvent;
use tauqe_protocol::{events, Event};

#[cfg(test)]
fn publisher(out: OutChannel) -> impl Fn(&str, &UsageTotalsSnapshot) + Send + Sync + 'static {
    move |operation_id, snapshot| publish_snapshot(&out, operation_id, snapshot)
}

pub(crate) fn publish_snapshot(
    out: &OutChannel,
    operation_id: &str,
    snapshot: &UsageTotalsSnapshot,
) {
    out.send_event(&Event {
        method: events::MODEL_USAGE_TOTALS.to_string(),
        params: snapshot_event_params(operation_id, snapshot),
    });
}

/// Encodes one snapshot publication for the wire.
fn snapshot_event_params(
    operation_id: &str,
    snapshot: &UsageTotalsSnapshot,
) -> Option<serde_json::Value> {
    serde_json::to_value(ModelUsageTotalsEvent {
        operation_id: operation_id.to_string(),
        snapshot: wire_snapshot(snapshot),
    })
    .ok()
}

/// Projects application accounting into the stable RPC contract.
pub(crate) fn wire_snapshot(snapshot: &UsageTotalsSnapshot) -> tauqe_protocol::UsageTotalsSnapshot {
    tauqe_protocol::UsageTotalsSnapshot {
        session: wire_totals(&snapshot.session),
        prev: wire_totals(&snapshot.prev),
        cur: snapshot.cur.as_ref().map(wire_totals),
        operations: snapshot.operations,
        lifetime: wire_totals(&snapshot.lifetime),
        lifetime_operations: snapshot.lifetime_operations,
    }
}

/// Projects provider metrics without replacing unknown values with zeros.
pub(crate) fn wire_usage(
    usage: &tauqe_flow::api::types::ModelUsageInfo,
) -> tauqe_protocol::ModelUsageInfo {
    tauqe_protocol::ModelUsageInfo {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cached_tokens: usage.cached_tokens,
        cost: usage.cost,
    }
}

pub(crate) fn publish_legacy_usage(out: &OutChannel, op_id: &str, snapshot: &UsageTotalsSnapshot) {
    let totals = snapshot.operation_totals();
    out.send_event(&Event {
        method: events::MODEL_USAGE.to_string(),
        params: serde_json::to_value(tauqe_protocol::ModelUsageEvent {
            operation_id: op_id.to_string(),
            usage: wire_usage(&totals.to_usage_info()),
            session_total_cost: snapshot.session.cost,
            current_cost: Some(totals.cost),
        })
        .ok(),
    });
}

fn wire_totals(totals: &UsageTotals) -> tauqe_protocol::UsageTotals {
    tauqe_protocol::UsageTotals {
        prompt_tokens: totals.prompt_tokens,
        completion_tokens: totals.completion_tokens,
        total_tokens: totals.total_tokens,
        reasoning_tokens: totals.reasoning_tokens,
        cached_tokens: totals.cached_tokens,
        cost: totals.cost,
        duration_ms: totals.duration_ms,
    }
}

#[cfg(test)]
impl AppState {
    pub async fn begin_usage(self: &Arc<Self>, operation_id: &str) -> UsageGuard {
        self.application
            .test_begin_usage(operation_id, publisher(self.out.clone()))
            .await
    }

    #[cfg(test)]
    async fn begin_usage_with_tick(
        self: &Arc<Self>,
        operation_id: &str,
        tick: std::time::Duration,
    ) -> UsageGuard {
        self.application
            .test_begin_usage_with_tick(operation_id, tick, publisher(self.out.clone()))
            .await
    }

    pub async fn reset_lifetime_usage(self: &Arc<Self>) -> UsageTotalsSnapshot {
        self.application
            .reset_project_usage(Arc::new(crate::publication::Publisher {
                out: self.out.clone(),
            }))
            .await
            .unwrap()
    }

    #[cfg(test)]
    pub async fn reload_usage_lifetime(self: &Arc<Self>) {
        self.application
            .test_reload_usage_lifetime(publisher(self.out.clone()))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
    use crate::transport::OutChannel;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tauqe_core::config::AppConfig;
    use tauqe_core::providers::ModelUsageInfo;

    fn usage(prompt: u32, completion: u32) -> ModelUsageInfo {
        ModelUsageInfo {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cost: Some(0.25),
            ..ModelUsageInfo::default()
        }
    }

    fn test_state() -> (
        Arc<AppState>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        PathBuf,
    ) {
        let root = scratch_root("usage");
        let (out, rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
        (state, rx, root)
    }

    /// Builds a state bound to an existing directory, so two states can share
    /// one telemetry file the way two daemon runs share a repository.
    fn state_at(root: &Path) -> Arc<AppState> {
        let (out, _rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(
            AppConfig::default(),
            root.to_path_buf(),
            history_sender,
            out,
        )
    }

    fn read_stored_lifetime(root: &Path) -> serde_json::Value {
        let raw =
            std::fs::read_to_string(root.join(".tauqe/usage.json")).expect("telemetry file exists");
        serde_json::from_str(&raw).expect("telemetry file is valid JSON")
    }

    /// Decodes a `model/usageTotals` event line from the test transport,
    /// returning `None` for every other event.
    fn decode_usage_event(raw: &str) -> Option<ModelUsageTotalsEvent> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        if value.get("method").and_then(|method| method.as_str())
            != Some(events::MODEL_USAGE_TOTALS)
        {
            return None;
        }
        serde_json::from_value(value.get("params")?.clone()).ok()
    }

    #[test]
    fn provider_usage_publication_preserves_unknown_and_zero_metrics() {
        for (reasoning_tokens, cached_tokens, cost) in [
            (None, None, None),
            (Some(0), Some(0), Some(0.0)),
            (Some(7), Some(11), Some(0.125)),
        ] {
            let report = ModelUsageInfo {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
                reasoning_tokens,
                cached_tokens,
                cost,
            };
            let event = tauqe_protocol::ModelUsageEvent {
                operation_id: "op-provider".into(),
                usage: wire_usage(&report),
                session_total_cost: 0.5,
                current_cost: cost,
            };
            let encoded = serde_json::to_value(event).unwrap();
            let decoded: tauqe_protocol::ModelUsageEvent = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded.usage.reasoning_tokens, reasoning_tokens);
            assert_eq!(decoded.usage.cached_tokens, cached_tokens);
            assert_eq!(decoded.usage.cost, cost);
            assert_eq!(decoded.usage.total_tokens, 120);
            assert_eq!(decoded.current_cost, cost);
        }
    }

    #[test]
    fn accounting_projection_preserves_wire_and_storage_fields() {
        let totals = UsageTotals {
            prompt_tokens: 120,
            completion_tokens: 30,
            total_tokens: 150,
            reasoning_tokens: Some(8),
            cached_tokens: Some(0),
            cost: 0.125,
            duration_ms: 1_234,
        };
        let mut snapshot = UsageTotalsSnapshot {
            session: totals.clone(),
            prev: UsageTotals::default(),
            cur: Some(totals.clone()),
            operations: 3,
            lifetime: totals,
            lifetime_operations: 42,
        };
        for active in [true, false] {
            if !active {
                snapshot.cur = None;
            }
            let wire = wire_snapshot(&snapshot);
            assert_eq!(
                serde_json::to_value(&wire).unwrap(),
                serde_json::to_value(&snapshot).unwrap(),
            );
            // Persisted core totals remain readable as the previous wire type.
            let restored: tauqe_protocol::UsageTotals =
                serde_json::from_value(serde_json::to_value(&snapshot.lifetime).unwrap()).unwrap();
            assert_eq!(restored, wire.lifetime);
            let params = snapshot_event_params("op-projection", &snapshot).unwrap();
            let event: ModelUsageTotalsEvent = serde_json::from_value(params).unwrap();
            assert_eq!(event.operation_id, "op-projection");
            assert_eq!(event.snapshot, wire);
        }
    }

    /// Every flushed operation folds into the session totals, and the previous
    /// operation keeps its own totals so the footer can show both.
    #[tokio::test]
    async fn flushes_operations_into_the_session_totals() {
        let (state, mut rx, root) = test_state();

        let first = state.begin_usage("op-first").await;
        first.observe(&usage(100, 40)).await;
        first.flush().await;

        let second = state.begin_usage("op-second").await;
        second.observe(&usage(10, 5)).await;
        second.observe(&usage(2, 3)).await;
        second.flush().await;

        let snapshot = state.usage_snapshot().await;
        assert_eq!(snapshot.operations, 2);
        assert_eq!(snapshot.session.prompt_tokens, 112);
        assert_eq!(snapshot.session.completion_tokens, 48);
        assert_eq!(snapshot.session.total_tokens, 160);
        assert!((snapshot.session.cost - 0.75).abs() < f64::EPSILON);
        assert_eq!(snapshot.prev.total_tokens, 20);
        assert!(snapshot.cur.is_none(), "a flushed window leaves no residue");

        // Each begin, observe, and flush publishes the snapshot: 3 + 4. The
        // live-duration heartbeat may add its own publications on a slow
        // runner, so the count is restricted to the transition events: their
        // in-flight window still carries the zero stored duration, while a
        // heartbeat always reports a whole tick or more.
        let mut published = 0;
        while let Ok(raw) = rx.try_recv() {
            if let Some(event) = decode_usage_event(&raw) {
                let is_heartbeat = event
                    .snapshot
                    .cur
                    .as_ref()
                    .is_some_and(|cur| cur.duration_ms > 0);
                if !is_heartbeat {
                    published += 1;
                }
            }
        }
        assert_eq!(published, 7, "every transition publishes the snapshot");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The in-flight operation stays visible before it is flushed.
    #[tokio::test]
    async fn keeps_the_in_flight_operation_visible_until_flush() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage("op-visible").await;
        guard.observe(&usage(3, 2)).await;

        let in_flight = state.usage_snapshot().await;
        assert_eq!(in_flight.session.total_tokens, 0);
        assert_eq!(in_flight.operation_totals().total_tokens, 5);

        guard.flush().await;
        let flushed = state.usage_snapshot().await;
        assert!(flushed.cur.is_none());
        assert_eq!(flushed.session.total_tokens, 5);
        assert_eq!(flushed.prev.total_tokens, 5);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// While the window is open, the heartbeat republishes the snapshot once
    /// per tick with the live in-flight duration; flush silences it and folds
    /// the full measured duration into the session and lifetime totals.
    ///
    /// The heartbeat runs on the real clock, so the test opens the window on a
    /// compressed tick and polls the transport until at least one live
    /// publication arrives, instead of relying on a paused tokio clock, which
    /// would require the `test-util` feature the workspace does not enable.
    #[tokio::test]
    async fn live_duration_heartbeat_publishes_and_is_silenced_by_flush() {
        // The window runs on a compressed tick: the first heartbeat arrives in
        // tens of milliseconds instead of a whole production second.
        const TEST_TICK: Duration = Duration::from_millis(50);
        let (state, mut rx, root) = test_state();

        let guard = state.begin_usage_with_tick("op-heartbeat", TEST_TICK).await;
        guard.observe(&usage(3, 2)).await;

        // Poll the transport until a heartbeat publishes the live duration.
        // Each publication re-reports the observed tokens with a growing
        // live duration, while the stored window keeps its zero duration
        // until flush.
        let mut max_live_ms = 0u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while max_live_ms == 0 && std::time::Instant::now() < deadline {
            while let Ok(raw) = rx.try_recv() {
                let Some(event) = decode_usage_event(&raw) else {
                    continue;
                };
                let Some(cur) = event.snapshot.cur.as_ref() else {
                    continue;
                };
                if cur.duration_ms > 0 {
                    assert_eq!(
                        cur.total_tokens, 5,
                        "the heartbeat re-reports the observed tokens"
                    );
                    max_live_ms = max_live_ms.max(cur.duration_ms);
                }
            }
            if max_live_ms == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        assert!(
            max_live_ms > 0,
            "the heartbeat must publish a live in-flight duration"
        );

        // The stored window was never touched by the heartbeat: flush folds
        // the full measured duration, never the heartbeat's derived one on
        // top of it.
        guard.flush().await;
        let snapshot = state.usage_snapshot().await;
        assert!(snapshot.cur.is_none());
        assert_eq!(snapshot.prev.total_tokens, 5);
        assert!(
            snapshot.prev.duration_ms >= max_live_ms,
            "flush records the whole window duration"
        );
        assert_eq!(snapshot.session.duration_ms, snapshot.prev.duration_ms);
        assert_eq!(snapshot.lifetime.duration_ms, snapshot.prev.duration_ms);

        // Drain everything published so far, then let several ticks pass:
        // the queue must stay empty because the heartbeat was silenced with
        // the window and no stale "still running" snapshot can arrive after
        // the final publish.
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(TEST_TICK + Duration::from_millis(200)).await;
        assert!(
            rx.try_recv().is_err(),
            "no heartbeat may outlive the flushed window"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A completed operation is folded into the project lifetime and written to
    /// `.tauqe/usage.json`, so a restarted daemon continues the same totals
    /// while its session scope starts from zero.
    #[tokio::test]
    async fn flush_persists_the_project_lifetime() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage("op-persist").await;
        guard.observe(&usage(100, 40)).await;
        guard.flush().await;

        let stored = read_stored_lifetime(&root);
        assert_eq!(stored["lifetime"]["total_tokens"].as_u64(), Some(140));
        assert_eq!(stored["lifetime_operations"].as_u64(), Some(1));

        let restarted = state_at(&root);
        let snapshot = restarted.usage_snapshot().await;
        assert_eq!(snapshot.lifetime.total_tokens, 140);
        assert_eq!(snapshot.lifetime_operations, 1);
        assert!(
            snapshot.session.is_empty(),
            "the session scope restarts with the process"
        );
        assert_eq!(snapshot.operations, 0);

        drop(restarted);
    }

    /// A successor waits for the final snapshot of its predecessor before it
    /// opens, and a late source cannot charge the successor's window.
    #[tokio::test]
    async fn a_successor_waits_for_final_publication_and_rejects_late_reports() {
        const PARKED_TICK: Duration = Duration::from_secs(3600);
        let (state, mut rx, root) = test_state();
        let first = state.begin_usage_with_tick("op-first", PARKED_TICK).await;
        first.observe(&usage(100, 40)).await;
        let late = first.sink();
        while rx.try_recv().is_ok() {}
        let peer = state.clone();
        let next =
            tokio::spawn(async move { peer.begin_usage_with_tick("op-second", PARKED_TICK).await });
        tokio::task::yield_now().await;
        assert!(!next.is_finished());
        let first_receipt = first.finish().await.unwrap();
        let second = next.await.unwrap();
        let final_event = decode_usage_event(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(final_event.operation_id, "op-first");
        assert_eq!(final_event.snapshot, wire_snapshot(&first_receipt.snapshot));
        let started = decode_usage_event(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(started.operation_id, "op-second");
        assert_eq!(started.snapshot.session.total_tokens, 140);
        late.record(&usage(200, 0));
        assert!(rx.try_recv().is_err(), "late sources do not publish");
        second.observe(&usage(3, 2)).await;
        let second_receipt = second.finish().await.unwrap();
        assert_ne!(first_receipt.window_id, second_receipt.window_id);
        assert_eq!(second_receipt.snapshot.session.total_tokens, 145);
        assert_eq!(second_receipt.snapshot.operations, 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Resetting clears the project lifetime in memory and on disk, while the
    /// session counters keep describing what this process already spent.
    #[tokio::test]
    async fn reset_zeroes_the_project_lifetime_and_keeps_the_session() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage("op-reset").await;
        guard.observe(&usage(10, 5)).await;
        guard.flush().await;

        let snapshot = state.reset_lifetime_usage().await;
        assert!(snapshot.lifetime.is_empty());
        assert_eq!(snapshot.lifetime_operations, 0);
        assert_eq!(snapshot.session.total_tokens, 15);
        assert_eq!(snapshot.operations, 1);

        let stored = read_stored_lifetime(&root);
        assert_eq!(stored["lifetime"]["total_tokens"].as_u64(), Some(0));
        assert_eq!(stored["lifetime_operations"].as_u64(), Some(0));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A corrupt telemetry document never blocks startup: the totals start from
    /// zero and the next flush rewrites the document.
    #[tokio::test]
    async fn unreadable_telemetry_falls_back_to_empty_totals() {
        let root = scratch_root("usage-corrupt");
        let _ = std::fs::create_dir_all(root.join(".tauqe"));
        std::fs::write(root.join(".tauqe/usage.json"), "{ not telemetry").unwrap();

        let snapshot = state_at(&root).usage_snapshot().await;
        assert!(snapshot.lifetime.is_empty());
        assert_eq!(snapshot.lifetime_operations, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rebinding to another repository adopts the lifetime totals persisted
    /// there, while the session totals keep describing what this process has
    /// already spent.
    #[tokio::test]
    async fn rebinding_adopts_the_telemetry_of_the_new_repository() {
        let (state, _rx, first_root) = test_state();
        let second_root = scratch_root("usage-rebind");

        let guard = state.begin_usage("op-first").await;
        guard.observe(&usage(10, 5)).await;
        guard.flush().await;
        assert_eq!(state.usage_snapshot().await.lifetime.total_tokens, 15);

        // The second repository already carries its own, much smaller,
        // telemetry document.
        let second = state_at(&second_root);
        let stored = second.begin_usage("op-second").await;
        stored.observe(&usage(7, 3)).await;
        stored.flush().await;
        drop(second);

        // Rebinding re-anchors the context first and then reloads the
        // telemetry of the newly bound repository.
        let rebind_root = second_root.clone();
        state
            .mutate_context(move |ctx| ctx.set_repo_root(rebind_root))
            .await
            .expect("context mutation runs");
        state.reload_usage_lifetime().await;

        let snapshot = state.usage_snapshot().await;
        assert_eq!(snapshot.lifetime.total_tokens, 10);
        assert_eq!(snapshot.lifetime_operations, 1);
        assert_eq!(
            snapshot.session.total_tokens, 15,
            "the session scope belongs to the process, not to the repository"
        );

        let _ = std::fs::remove_dir_all(&first_root);
        let _ = std::fs::remove_dir_all(&second_root);
    }
}
