//! Session-wide usage accounting for every operation kind.
//!
//! Token, cost, and duration totals previously lived wherever an operation
//! happened to need them: the session cost was a bare `f64` on the state, and
//! every handler derived its own view of the tokens it had spent. That made the
//! interface unable to answer a simple question — *what has this session cost
//! so far, and what is the current operation spending right now?* — without
//! replaying per-operation events.
//!
//! This module owns the single authoritative [`UsageTotalsSnapshot`] behind
//! [`AppState::usage`]. An operation opens a [`UsageGuard`], reports the usage
//! each provider call returns through [`UsageGuard::observe`], and closes the
//! window with [`UsageGuard::flush`], which folds the operation into the
//! session totals together with the duration the harness measured itself.
//! Every transition publishes a `model/usageTotals` event, so a client renders
//! one consistent snapshot instead of stitching counters together.

use std::sync::Arc;
use std::time::Instant;

use tauqe_protocol::model::{
    ModelUsageInfo, ModelUsageTotalsEvent, UsageTotals,
};
use tauqe_protocol::{events, Event};

use crate::state::{next_operation_id, AppState};

/// Open accounting window for one operation.
///
/// The window starts when the operation begins and closes on
/// [`UsageGuard::flush`]. Dropping it without flushing abandons the window; the
/// next [`AppState::begin_usage`] discards the abandoned totals, so a failed
/// operation can never leave a permanently in-flight window behind.
pub struct UsageGuard {
    state: Arc<AppState>,
    operation_id: String,
    started: Instant,
}

impl AppState {
    /// Opens a usage accounting window for one operation.
    ///
    /// A window left behind by an operation that failed before flushing is
    /// discarded here, so one abandoned turn never inflates the in-flight
    /// totals of the next one.
    pub async fn begin_usage(self: &Arc<Self>) -> UsageGuard {
        let operation_id = format!("op-{}", next_operation_id());
        {
            let mut snapshot = self.usage.lock().await;
            snapshot.cur = Some(UsageTotals::default());
        }
        let guard = UsageGuard {
            state: Arc::clone(self),
            operation_id,
            started: Instant::now(),
        };
        guard.publish().await;
        guard
    }
}

impl UsageGuard {
    /// Adds the usage reported by one provider call to the in-flight totals.
    ///
    /// A provider may answer with token counts only, a cost only, or nothing at
    /// all; the totals accumulate whatever was reported and stay additive
    /// across the several calls a multi-round operation makes.
    pub async fn observe(&self, usage: &ModelUsageInfo) {
        {
            let mut snapshot = self.state.usage.lock().await;
            snapshot
                .cur
                .get_or_insert_with(UsageTotals::default)
                .accumulate(&UsageTotals::from_usage(usage));
        }
        self.publish().await;
    }

    /// Closes the window, folding the operation into the session totals.
    ///
    /// The duration is measured by the harness rather than reported by the
    /// provider, so it reflects how long the operation actually held the
    /// developer's turn.
    pub async fn flush(self) {
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        {
            let mut snapshot = self.state.usage.lock().await;
            let mut completed = snapshot.cur.take().unwrap_or_default();
            completed.duration_ms = completed.duration_ms.saturating_add(elapsed_ms);
            snapshot.session.accumulate(&completed);
            snapshot.prev = completed;
            snapshot.operations = snapshot.operations.saturating_add(1);
        }
        self.publish().await;
    }

    /// Publishes the current snapshot to the client.
    async fn publish(&self) {
        let snapshot = self.state.usage.lock().await.clone();
        self.state.out.send_event(&Event {
            method: events::MODEL_USAGE_TOTALS.to_string(),
            params: serde_json::to_value(ModelUsageTotalsEvent {
                operation_id: self.operation_id.clone(),
                snapshot,
            })
            .ok(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::OutChannel;
    use tauqe_core::config::AppConfig;
    use std::path::PathBuf;

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
        let root = std::env::temp_dir().join(format!("tauqe-usage-{}", next_operation_id()));
        let _ = std::fs::create_dir_all(&root);
        let (out, rx) = OutChannel::new();
        let (history_sender, _history_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(AppConfig::default(), root.clone(), history_sender, out);
        (state, rx, root)
    }

    /// Every flushed operation folds into the session totals, and the previous
    /// operation keeps its own totals so the footer can show both.
    #[tokio::test]
    async fn flushes_operations_into_the_session_totals() {
        let (state, mut rx, root) = test_state();

        let first = state.begin_usage().await;
        first.observe(&usage(100, 40)).await;
        first.flush().await;

        let second = state.begin_usage().await;
        second.observe(&usage(10, 5)).await;
        second.observe(&usage(2, 3)).await;
        second.flush().await;

        let snapshot = state.usage.lock().await.clone();
        assert_eq!(snapshot.operations, 2);
        assert_eq!(snapshot.session.prompt_tokens, 112);
        assert_eq!(snapshot.session.completion_tokens, 48);
        assert_eq!(snapshot.session.total_tokens, 160);
        assert!((snapshot.session.cost - 0.75).abs() < f64::EPSILON);
        assert_eq!(snapshot.prev.total_tokens, 20);
        assert!(snapshot.cur.is_none(), "a flushed window leaves no residue");

        // Each begin, observe, and flush publishes the snapshot: 3 + 4.
        let mut published = 0;
        while rx.try_recv().is_ok() {
            published += 1;
        }
        assert_eq!(published, 7, "every transition publishes the snapshot");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The in-flight operation stays visible before it is flushed.
    #[tokio::test]
    async fn keeps_the_in_flight_operation_visible_until_flush() {
        let (state, _rx, root) = test_state();

        let guard = state.begin_usage().await;
        guard.observe(&usage(3, 2)).await;

        let in_flight = state.usage.lock().await.clone();
        assert_eq!(in_flight.session.total_tokens, 0);
        assert_eq!(in_flight.operation_totals().total_tokens, 5);

        guard.flush().await;
        let flushed = state.usage.lock().await.clone();
        assert!(flushed.cur.is_none());
        assert_eq!(flushed.session.total_tokens, 5);
        assert_eq!(flushed.prev.total_tokens, 5);

        let _ = std::fs::remove_dir_all(&root);
    }
}
