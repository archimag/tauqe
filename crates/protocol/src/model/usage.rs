use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelUsageInfo {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageEvent {
    pub operation_id: String,
    pub usage: ModelUsageInfo,
    pub session_total_cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_cost: Option<f64>,
}

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
    pub fn accumulate(&mut self, other: &UsageTotals) {
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
    pub fn from_usage(usage: &ModelUsageInfo) -> Self {
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

/// Emitted whenever the aggregated usage snapshot changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageTotalsEvent {
    pub operation_id: String,
    pub snapshot: UsageTotalsSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn usage_totals_snapshot_serde_roundtrip_and_cur_omission() {
        let snapshot = UsageTotalsSnapshot {
            session: UsageTotals {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                reasoning_tokens: Some(20),
                cached_tokens: Some(10),
                cost: 0.0123,
                duration_ms: 2_000,
            },
            prev: UsageTotals {
                total_tokens: 150,
                cost: 0.0123,
                duration_ms: 2_000,
                ..UsageTotals::default()
            },
            cur: None,
            operations: 1,
            lifetime: UsageTotals {
                total_tokens: 9_000,
                cost: 0.5,
                duration_ms: 60_000,
                ..UsageTotals::default()
            },
            lifetime_operations: 42,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("\"cur\""), "cur must be omitted when None");
        assert!(
            json.contains("\"lifetime\""),
            "the project scope must always be serialized"
        );
        let de: UsageTotalsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(de, snapshot);
    }

    /// Guards wire compatibility with snapshots written before the project
    /// scope existed. Every optional and lifetime field must backfill from the
    /// serde defaults instead of failing the read.
    #[test]
    fn usage_totals_snapshot_backfills_missing_lifetime_fields() {
        let legacy = r#"{
            "session": {
                "prompt_tokens": 120,
                "completion_tokens": 30,
                "total_tokens": 150,
                "cost": 0.02,
                "duration_ms": 1500
            },
            "operations": 3
        }"#;
        let snapshot: UsageTotalsSnapshot = serde_json::from_str(legacy).unwrap();
        assert_eq!(snapshot.operations, 3);
        assert_eq!(snapshot.session.total_tokens, 150);
        assert!(snapshot.cur.is_none());
        assert_eq!(snapshot.prev, UsageTotals::default());
        assert_eq!(snapshot.lifetime, UsageTotals::default());
        assert_eq!(snapshot.lifetime_operations, 0);
        assert!(snapshot.lifetime.is_empty());
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
