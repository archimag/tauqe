use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
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

/// Wire representation of aggregated token, cost, and duration totals.
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
