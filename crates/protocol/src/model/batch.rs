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
}
