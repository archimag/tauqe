//! Domain accounting values, independent of RPC snapshots.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tauqe_core::providers::ModelUsageInfo;
use tauqe_core::workflow::batch::{BatchStepOutcome, BatchUsage};

/// Aggregated usage accounting shared by every operation kind.
///
/// Holds prompt, completion, and total tokens, optional reasoning and cached
/// token breakdowns, the monetary cost, and the harness-measured duration of an
/// operation. It is the single accumulator for token, money, and time totals so
/// a session never keeps three drifting counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub duration_ms: u64,
}

impl UsageTotals {
    /// Reports whether every token counter, the cost, and the duration are zero.
    pub fn is_empty(&self) -> bool {
        self.prompt_tokens == 0
            && self.completion_tokens == 0
            && self.total_tokens == 0
            && self.reasoning_tokens.unwrap_or(0) == 0
            && self.cached_tokens.unwrap_or(0) == 0
            && self.cost == 0.0
            && self.duration_ms == 0
    }

    /// Adds another usage total into this one.
    ///
    /// Token counters saturate instead of wrapping, matching a long-running
    /// session that could otherwise overflow; cost uses ordinary addition and
    /// the duration saturates as well.
    pub(crate) fn accumulate(&mut self, other: &UsageTotals) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.reasoning_tokens = add_optional_tokens(self.reasoning_tokens, other.reasoning_tokens);
        self.cached_tokens = add_optional_tokens(self.cached_tokens, other.cached_tokens);
        self.cost += other.cost;
        self.duration_ms = self.duration_ms.saturating_add(other.duration_ms);
    }

    /// Builds a usage total from the legacy provider usage payload.
    ///
    /// The harness measures duration separately, so the migrated total starts
    /// with a zero duration and a concrete cost (an absent cost becomes zero).
    pub(crate) fn from_usage(usage: &ModelUsageInfo) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cached_tokens: usage.cached_tokens,
            cost: usage.cost.unwrap_or(0.0),
            duration_ms: 0,
        }
    }

    /// Converts back into the legacy provider usage payload.
    ///
    /// A positive cost becomes `Some`, while a zero cost becomes `None`, so the
    /// optional cost field keeps its previous "unknown" meaning on the legacy
    /// channel during migration.
    pub fn to_usage_info(&self) -> ModelUsageInfo {
        ModelUsageInfo {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            reasoning_tokens: self.reasoning_tokens,
            cached_tokens: self.cached_tokens,
            cost: if self.cost > 0.0 {
                Some(self.cost)
            } else {
                None
            },
        }
    }
}

fn add_optional_tokens(current: Option<u32>, incoming: Option<u32>) -> Option<u32> {
    match (current, incoming) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

/// Full usage snapshot across every telemetry scope.
///
/// `session` is the sum of the completed operations of the running process,
/// `prev` is the last completed operation, and `cur` is the in-flight operation
/// that only exists while a turn is running. `operations` counts the completed
/// operations of the session.
///
/// `lifetime` and `lifetime_operations` carry the project total, which
/// accumulates across process restarts and is persisted separately from the
/// session counters. Both lifetime fields are `serde`-defaultable, so a
/// snapshot written before the project scope existed still deserializes and
/// simply starts from zero instead of failing the read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotalsSnapshot {
    #[serde(default)]
    pub session: UsageTotals,
    #[serde(default)]
    pub prev: UsageTotals,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cur: Option<UsageTotals>,
    #[serde(default)]
    pub operations: u64,
    #[serde(default)]
    pub lifetime: UsageTotals,
    #[serde(default)]
    pub lifetime_operations: u64,
}

impl UsageTotalsSnapshot {
    /// Returns the in-flight totals when a turn is active, else the totals of
    /// the last completed operation.
    pub fn operation_totals(&self) -> &UsageTotals {
        self.cur.as_ref().unwrap_or(&self.prev)
    }
}

/// Immutable accounting result of one closed window.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageReceipt {
    pub window_id: u64,
    pub totals: UsageTotals,
    pub cost_complete: bool,
    pub snapshot: UsageTotalsSnapshot,
}

impl UsageReceipt {
    /// Project a completed window for a batch without charging the ledger.
    pub fn batch_usage(&self) -> ModelUsageInfo {
        ModelUsageInfo {
            prompt_tokens: self.totals.prompt_tokens,
            completion_tokens: self.totals.completion_tokens,
            total_tokens: self.totals.total_tokens,
            reasoning_tokens: self.totals.reasoning_tokens,
            cached_tokens: self.totals.cached_tokens,
            cost: self.cost_complete.then_some(self.totals.cost),
        }
    }
}

/// Projects distinct closed windows into batch totals without charging the ledger.
#[derive(Default)]
pub(crate) struct ReceiptAccumulator {
    windows: HashSet<u64>,
    usage: BatchUsage,
}

impl ReceiptAccumulator {
    pub(crate) fn include(&mut self, receipt: &UsageReceipt) {
        if self.windows.insert(receipt.window_id) {
            self.usage.accumulate_outcome(
                &BatchStepOutcome::succeeded().with_usage(receipt.batch_usage()),
            );
        }
    }

    pub(crate) fn usage(&self) -> BatchUsage {
        self.usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::workflow::batch::{BatchStepOutcome, BatchUsage};

    #[test]
    fn receipt_projection_is_repeatable_without_changing_ledger_snapshot() {
        let receipt = UsageReceipt {
            window_id: 7,
            totals: UsageTotals {
                total_tokens: 9,
                cost: -0.2,
                ..Default::default()
            },
            cost_complete: true,
            snapshot: UsageTotalsSnapshot::default(),
        };
        let first = receipt.batch_usage();
        assert_eq!(first, receipt.batch_usage());
        let mut batch = BatchUsage::default();
        batch.accumulate_outcome(&BatchStepOutcome::failed("decode").with_usage(first));
        assert_eq!(batch.total_tokens, 9);
        assert_eq!(batch.cost, Some(-0.2));
        assert_eq!(receipt.snapshot, UsageTotalsSnapshot::default());

        let unknown = UsageReceipt {
            cost_complete: false,
            ..receipt.clone()
        };
        batch.accumulate_outcome(&BatchStepOutcome::cancelled().with_usage(unknown.batch_usage()));
        assert_eq!(batch.total_tokens, 18);
        assert_eq!(batch.cost, None);
        let mut projection = ReceiptAccumulator::default();
        projection.include(&receipt);
        projection.include(&unknown);
        assert_eq!(projection.usage().total_tokens, 9);
        assert_eq!(projection.usage().cost, Some(-0.2));
        assert_eq!(receipt.snapshot, UsageTotalsSnapshot::default());
    }

    #[test]
    fn multiple_attempt_receipts_keep_unknown_cost_and_ignore_duplicates() {
        let mut projection = ReceiptAccumulator::default();
        for (window_id, tokens, complete) in [(1, 4, true), (2, 6, false), (3, 8, true)] {
            let receipt = UsageReceipt {
                window_id,
                totals: UsageTotals {
                    total_tokens: tokens,
                    cost: 0.5,
                    ..Default::default()
                },
                cost_complete: complete,
                snapshot: UsageTotalsSnapshot::default(),
            };
            projection.include(&receipt);
            projection.include(&receipt);
        }
        assert_eq!(projection.usage().total_tokens, 18);
        assert_eq!(projection.usage().cost, None);
    }

    #[test]
    fn provider_reports_keep_legacy_accounting_cost_semantics() {
        for cost in [None, Some(0.0), Some(0.125)] {
            let report = ModelUsageInfo {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                reasoning_tokens: None,
                cached_tokens: Some(0),
                cost,
            };
            let totals = UsageTotals::from_usage(&report);
            assert_eq!(totals.cost, cost.unwrap_or(0.0));
            assert_eq!(totals.duration_ms, 0);
            let legacy = totals.to_usage_info();
            assert_eq!(legacy.reasoning_tokens, None);
            assert_eq!(legacy.cached_tokens, Some(0));
            assert_eq!(legacy.cost, cost.filter(|value| *value > 0.0));
            assert_eq!(legacy.total_tokens, report.total_tokens);
        }
    }

    #[test]
    fn usage_totals_accumulate_saturates() {
        let mut totals = UsageTotals {
            prompt_tokens: u32::MAX - 1,
            completion_tokens: 10,
            total_tokens: 10,
            reasoning_tokens: Some(u32::MAX),
            cached_tokens: None,
            cost: 0.25,
            duration_ms: 1_000,
        };
        let other = UsageTotals {
            prompt_tokens: 5,
            completion_tokens: 3,
            total_tokens: 3,
            reasoning_tokens: Some(2),
            cached_tokens: Some(7),
            cost: 0.5,
            duration_ms: 500,
        };
        totals.accumulate(&other);
        assert_eq!(totals.prompt_tokens, u32::MAX);
        assert_eq!(totals.completion_tokens, 13);
        assert_eq!(totals.total_tokens, 13);
        assert_eq!(totals.reasoning_tokens, Some(u32::MAX));
        assert_eq!(totals.cached_tokens, Some(7));
        assert!((totals.cost - 0.75).abs() < f64::EPSILON);
        assert_eq!(totals.duration_ms, 1_500);
    }

    #[test]
    fn usage_totals_is_empty() {
        assert!(UsageTotals::default().is_empty());
        assert!(!UsageTotals {
            total_tokens: 1,
            ..UsageTotals::default()
        }
        .is_empty());
        assert!(!UsageTotals {
            cost: 0.01,
            ..UsageTotals::default()
        }
        .is_empty());
        assert!(!UsageTotals {
            duration_ms: 1,
            ..UsageTotals::default()
        }
        .is_empty());
        assert!(!UsageTotals {
            cached_tokens: Some(1),
            ..UsageTotals::default()
        }
        .is_empty());
    }

    #[test]
    fn usage_totals_snapshot_operation_totals_prefers_cur() {
        let cur = UsageTotals {
            total_tokens: 5,
            ..UsageTotals::default()
        };
        let prev = UsageTotals {
            total_tokens: 9,
            ..UsageTotals::default()
        };
        let snapshot = UsageTotalsSnapshot {
            prev: prev.clone(),
            cur: Some(cur.clone()),
            ..UsageTotalsSnapshot::default()
        };
        assert_eq!(snapshot.operation_totals(), &cur);
        let without_cur = UsageTotalsSnapshot {
            prev,
            ..UsageTotalsSnapshot::default()
        };
        assert_eq!(without_cur.operation_totals(), &without_cur.prev);
    }
}
