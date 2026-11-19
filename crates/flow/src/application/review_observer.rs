//! Transport-owned review event pacing and publication.

use crate::api::UsageTotalsSnapshot;
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::stream::ReviewStreamUpdate;
use tokio::sync::watch;

/// Pacing is asynchronous. Publication callbacks must not re-enter accounting,
/// including when called synchronously by the usage heartbeat.
#[async_trait::async_trait]
pub trait ReviewObserver: Send + Sync {
    async fn pace(&self, event: &StreamEvent, cancel: &mut watch::Receiver<bool>);
    fn publish(&self, update: ReviewStreamUpdate);
    fn publish_usage_totals(&self, operation_id: &str, snapshot: &UsageTotalsSnapshot);
    fn publish_usage(&self, snapshot: &UsageTotalsSnapshot);
}
