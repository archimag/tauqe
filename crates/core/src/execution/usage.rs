//! Request-bound usage receipt, independent of event publication.

use super::outcome::ExecutionError;
use crate::providers::ModelUsageInfo;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// Synchronous receipt of one normalized provider increment.
pub trait UsageSink: Send + Sync {
    fn record(&self, usage: &ModelUsageInfo);
}

/// Shared accounting scope for the requests in one execution window.
#[derive(Clone)]
pub struct UsageScope {
    inner: Arc<ScopeInner>,
}

struct ScopeInner {
    sink: Arc<dyn UsageSink>,
    state: Mutex<ScopeState>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepared,
    Active,
    Closing,
    Closed,
}

struct ScopeState {
    phase: Phase,
    sources: watch::Sender<usize>,
}

impl UsageScope {
    pub fn new(sink: Arc<dyn UsageSink>) -> Self {
        let (sources, _) = watch::channel(0);
        Self {
            inner: Arc::new(ScopeInner {
                sink,
                state: Mutex::new(ScopeState {
                    phase: Phase::Prepared,
                    sources,
                }),
            }),
        }
    }

    /// Admit provider requests for this prepared execution.
    pub fn activate(&self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            state.phase == Phase::Prepared,
            "usage scope already activated"
        );
        state.phase = Phase::Active;
    }

    /// Stop admitting requests while registered sources finish their queues.
    pub fn stop_accepting(&self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(state.phase == Phase::Active, "usage scope is not active");
        state.phase = Phase::Closing;
    }

    /// Wait until all registered forwarders have drained their accepted queues.
    pub async fn drain(&self) {
        let mut rx = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sources
            .subscribe();
        while *rx.borrow_and_update() != 0 {
            if rx.changed().await.is_err() {
                break;
            }
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(state.phase == Phase::Closing, "usage scope is not closing");
        state.phase = Phase::Closed;
    }

    fn register(&self) -> Result<UsageSource, ExecutionError> {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.phase != Phase::Active {
            return Err(ExecutionError::UsageNotActive);
        }
        state.sources.send_modify(|count| *count += 1);
        Ok(UsageSource(self.clone()))
    }
}

/// A request can only register against its supervised accounting scope.
#[derive(Clone)]
pub enum UsageBinding {
    Bound(UsageScope),
}

impl UsageBinding {
    /// A completed phase may hand its executor to a new accounting window only
    /// after every source of the previous window has drained.
    pub fn is_closed(&self) -> bool {
        match self {
            Self::Bound(scope) => {
                scope
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .phase
                    == Phase::Closed
            }
        }
    }

    pub(crate) fn register(&self) -> Result<UsageSource, ExecutionError> {
        match self {
            Self::Bound(scope) => scope.register(),
        }
    }
}

pub(crate) struct UsageSource(UsageScope);

impl UsageSource {
    pub fn record(&self, usage: &ModelUsageInfo) {
        self.0.inner.sink.record(usage);
    }
}

impl Drop for UsageSource {
    fn drop(&mut self) {
        self.0
            .inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sources
            .send_modify(|count| *count -= 1);
    }
}
