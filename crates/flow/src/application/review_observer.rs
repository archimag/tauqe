//! Review event pacing and publication shared by audit and post-analysis.

use crate::api::UsageTotalsSnapshot;
use tauqe_core::providers::StreamEvent;
use tauqe_core::review::stream::ReviewStreamUpdate;
use tauqe_core::stop::HardCancellation;

/// Pacing is asynchronous. Publication callbacks must not re-enter accounting,
/// including when called synchronously by the usage heartbeat.
#[async_trait::async_trait]
pub trait ReviewObserver: Send + Sync {
    async fn pace(&self, event: &StreamEvent, cancel: &HardCancellation);
    fn publish(&self, update: ReviewStreamUpdate);
    fn publish_usage_totals(&self, operation_id: &str, snapshot: &UsageTotalsSnapshot);
    fn publish_usage(&self, snapshot: &UsageTotalsSnapshot);
}
