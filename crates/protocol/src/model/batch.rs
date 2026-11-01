use serde::{Deserialize, Serialize};

/// Terminal status of a batch supervisor run.
///
/// Batch supervisors execute a bounded sequence of isolated turns (plan steps or
/// review findings). Their terminal state is distinct from a single turn's
/// outcome because a batch may stop early while already-completed members remain
/// committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchFinishStatus {
    /// Every scheduled member completed successfully.
    Completed,
    /// A member failed and the batch stopped early.
    Failed,
    /// A member emitted a semantic blocker and the batch stopped early.
    Blocked,
    /// The batch was cancelled by the user.
    Cancelled,
    /// The user stopped the queue gracefully after the current member finished
    /// its turn; already-completed members remain committed.
    Stopped,
}

/// Compact summary of a completed batch run.
///
/// Carries the identifiers and progress counters shared by plan and review
/// batches so clients can render a single aggregated completion notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchFinishSummary {
    /// Identifier of the owning plan or review session.
    pub owner_id: String,
    pub status: BatchFinishStatus,
    /// Members executed before the batch stopped.
    pub executed_count: usize,
    /// Total members scheduled for the batch.
    pub total_count: usize,
    /// Wall-clock duration of the batch in seconds.
    pub elapsed_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Findings the batch actually fixed.
    ///
    /// Only a queue that classifies its members reports the counter; a batch
    /// that cannot distinguish fixed members leaves the field absent instead of
    /// publishing an invented zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_count: Option<usize>,
    /// Findings of the architectural groups deferred by the unresolved-questions
    /// gate.
    ///
    /// A group whose findings still carry discussion questions is not ready for
    /// autonomous execution, so it is postponed for the developer and counted
    /// here rather than silently dropped from the queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_count: Option<usize>,
    /// Members that failed locally while the queue continued.
    ///
    /// A best-effort traversal keeps walking after a domain failure whose Git
    /// transaction was rolled back cleanly, so the counter separates a member
    /// that stopped early from a queue that never reached it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_count: Option<usize>,
}

/// Emitted once when a plan batch supervisor terminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBatchFinishedEvent {
    pub operation_id: String,
    pub summary: BatchFinishSummary,
}

/// Emitted once when a review batch supervisor terminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBatchFinishedEvent {
    pub operation_id: String,
    pub summary: BatchFinishSummary,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_finish_status_serializes_every_variant() {
        let cases = [
            (BatchFinishStatus::Completed, "\"completed\""),
            (BatchFinishStatus::Failed, "\"failed\""),
            (BatchFinishStatus::Blocked, "\"blocked\""),
            (BatchFinishStatus::Cancelled, "\"cancelled\""),
            (BatchFinishStatus::Stopped, "\"stopped\""),
        ];
        for (status, wire) in cases {
            assert_eq!(serde_json::to_string(&status).unwrap(), wire);
            let de: BatchFinishStatus = serde_json::from_str(wire).unwrap();
            assert_eq!(de, status);
        }
    }
    /// Builds a completed summary carrying whichever finding counters the case
    /// under test needs, so a `None` counter stays distinguishable from zero.
    fn summary_with_counters(
        fixed_count: Option<usize>,
        deferred_count: Option<usize>,
        failed_count: Option<usize>,
    ) -> BatchFinishSummary {
        BatchFinishSummary {
            owner_id: "rev-1".to_string(),
            status: BatchFinishStatus::Completed,
            executed_count: 4,
            total_count: 6,
            elapsed_seconds: 120,
            error: None,
            fixed_count,
            deferred_count,
            failed_count,
        }
    }

    #[test]
    fn test_batch_finish_summary_serializes_the_finding_counters() {
        let summary = summary_with_counters(Some(2), Some(1), Some(1));
        let json = serde_json::to_value(&summary).unwrap();

        assert_eq!(json["fixed_count"], serde_json::json!(2));
        assert_eq!(json["deferred_count"], serde_json::json!(1));
        assert_eq!(json["failed_count"], serde_json::json!(1));

        let decoded: BatchFinishSummary = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, summary);
    }

    #[test]
    fn test_batch_finish_summary_omits_absent_finding_counters() {
        let summary = summary_with_counters(None, None, None);
        let json = serde_json::to_value(&summary).unwrap();
        let object = json.as_object().unwrap();
        for key in ["fixed_count", "deferred_count", "failed_count"] {
            assert!(
                !object.contains_key(key),
                "{key} must stay absent from the wire form"
            );
        }

        // A plan batch published before the counters existed still decodes.
        let legacy = serde_json::json!({
            "owner_id": "plan-1",
            "status": "completed",
            "executed_count": 2,
            "total_count": 2,
            "elapsed_seconds": 30,
        });
        let decoded: BatchFinishSummary = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.fixed_count, None);
        assert_eq!(decoded.deferred_count, None);
        assert_eq!(decoded.failed_count, None);
        assert_eq!(decoded.error, None);
    }
}
