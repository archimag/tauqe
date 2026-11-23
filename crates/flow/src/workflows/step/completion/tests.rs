use super::*;
use crate::workflows::review::{
    ReviewLeafReceipt, ReviewSequenceResult, ReviewSequenceStop, ReviewStepStop,
};
use crate::workflows::turn::{TurnArtifact, TurnOutcome};
use tauqe_core::edits::{EditOutcome, VerificationVerdict};
use tauqe_core::execution::ResponseEnvelope;

fn settled(response: ResponseEnvelope, edit: Option<EditOutcome>) -> StepAttemptReport {
    StepAttemptReport {
        outcome: Ok(TurnOutcome::Settled(TurnArtifact {
            response,
            response_round: None,
            edit: edit.clone(),
            rounds_spent: 1,
        })),
        edit,
        recovery_error: None,
        cancelled: false,
    }
}
fn turn(
    hash: Option<&str>,
    error: Option<&str>,
    verification: Option<VerificationVerdict>,
) -> StepLoopReport {
    StepLoopReport::Turn(Box::new(settled(
        ResponseEnvelope::default(),
        Some(EditOutcome {
            applied: error.is_none(),
            changed_files: vec!["b".into(), "a".into()],
            commit_hash: hash.map(str::to_string),
            error: error.map(str::to_string),
            verification,
        }),
    )))
}
#[test]
fn commit_failure_takes_priority_over_a_hash_and_keeps_changed_paths_and_verdict() {
    let completion = classify_step_edit_completion(&turn(
        Some("hash"),
        Some("commit failed"),
        Some(VerificationVerdict::Passed),
    ));
    assert_eq!(completion.disposition, StepEditDisposition::Refused);
    assert!(!completion.outcome.applied);
    assert_eq!(completion.outcome.error.as_deref(), Some("commit failed"));
    assert!(completion.outcome.commit_hash.is_none());
    assert_eq!(completion.outcome.changed_files, ["b", "a"]);
    assert_eq!(
        completion.outcome.verification,
        Some(VerificationVerdict::Passed)
    );
}
#[test]
fn terminal_model_declarations_do_not_report_an_applied_edit() {
    use tauqe_core::llm::step::turn_capability_expr;
    use tauqe_core::llm::LlmCapability;
    use tauqe_core::prompt::{json::JsonPromptProtocol, protocol::PromptProtocol};

    let protocol = JsonPromptProtocol::new(turn_capability_expr());
    for (response, expected) in [
        (
            r#"{"message":"","action":{"kind":"edit","changes":[],"commit_message":"","error_message":"","reason":"Already satisfied"}}"#,
            LlmCapability::TurnSkip,
        ),
        (
            r#"{"message":"","action":{"kind":"edit","changes":[],"commit_message":"","error_message":"The model could not complete the step","reason":""}}"#,
            LlmCapability::TurnError,
        ),
    ] {
        let envelope = ResponseEnvelope::from_commands(&protocol.decode(response, &[]));
        assert!(envelope.has(expected));
        let attempt = settled(envelope, None);
        assert_eq!(
            matches!(
                classify_step_attempt(&attempt).result,
                tauqe_core::workflow::implementation::StepTurnResult::Success { .. }
            ),
            expected == LlmCapability::TurnSkip,
        );
        let report = StepLoopReport::Turn(Box::new(attempt));
        let completion = classify_step_edit_completion(&report);
        assert_eq!(completion.disposition, StepEditDisposition::Completed);
        assert_eq!(completion.outcome, Default::default());
    }
}

#[test]
fn a_single_turn_preserves_every_verdict_and_requires_a_commit_for_applied_status() {
    for verdict in [
        None,
        Some(VerificationVerdict::Passed),
        Some(VerificationVerdict::Failed),
        Some(VerificationVerdict::NotConfigured),
        Some(VerificationVerdict::Cancelled),
    ] {
        for hash in [None, Some("abc")] {
            let completion = classify_step_edit_completion(&turn(hash, None, verdict));
            assert_eq!(completion.disposition, StepEditDisposition::Completed);
            assert_eq!(completion.outcome.applied, hash.is_some());
            assert_eq!(completion.outcome.commit_hash.as_deref(), hash);
            assert_eq!(completion.outcome.verification, verdict);
            assert_eq!(completion.outcome.changed_files, ["b", "a"]);
        }
    }
}
fn sequence(stop: ReviewSequenceStop) -> ReviewSequenceResult {
    ReviewSequenceResult {
        stop,
        executed_steps: vec![
            ReviewLeafReceipt {
                commit: Some("first".into()),
                modified_paths: vec!["b".into(), "a".into()],
            },
            ReviewLeafReceipt {
                commit: Some("last".into()),
                modified_paths: vec!["a".into()],
            },
            ReviewLeafReceipt {
                commit: None,
                modified_paths: vec!["c".into()],
            },
        ],
    }
}
#[test]
fn completed_sequence_keeps_path_order_duplicates_and_last_available_commit() {
    let completion = sequence(ReviewSequenceStop::Completed).edit_completion();
    assert!(completion.outcome.applied);
    assert_eq!(completion.outcome.changed_files, ["b", "a", "a", "c"]);
    assert_eq!(completion.outcome.commit_hash.as_deref(), Some("last"));
    assert_eq!(
        completion.outcome.verification,
        Some(VerificationVerdict::Passed)
    );
    let empty = ReviewSequenceResult {
        stop: ReviewSequenceStop::Completed,
        executed_steps: vec![],
    };
    let completion = empty.edit_completion();
    assert!(completion.outcome.applied);
    assert!(completion.outcome.commit_hash.is_none());
}
#[test]
fn cancellation_failure_exhaustion_and_incomplete_sequences_keep_distinct_dispositions() {
    for (completion, disposition) in [
        (
            classify_step_edit_completion(&StepLoopReport::Failed("host failure".into())),
            StepEditDisposition::Failed,
        ),
        (
            classify_step_edit_completion(&StepLoopReport::Turn(Box::new(StepAttemptReport {
                outcome: Ok(TurnOutcome::Exhausted {
                    reason: "budget".into(),
                }),
                ..settled(ResponseEnvelope::default(), None)
            }))),
            StepEditDisposition::Refused,
        ),
        (
            classify_step_edit_completion(&StepLoopReport::Turn(Box::new(settled(
                ResponseEnvelope::default(),
                None,
            )))),
            StepEditDisposition::Completed,
        ),
        (
            sequence(ReviewSequenceStop::Interrupted(ReviewStepStop::Exhausted(
                "budget".into(),
            )))
            .edit_completion(),
            StepEditDisposition::Completed,
        ),
        (
            sequence(ReviewSequenceStop::Interrupted(ReviewStepStop::Blocked(
                "precondition".into(),
            )))
            .edit_completion(),
            StepEditDisposition::Completed,
        ),
        (
            sequence(ReviewSequenceStop::Interrupted(ReviewStepStop::Cancelled)).edit_completion(),
            StepEditDisposition::Cancelled,
        ),
    ] {
        assert_eq!(completion.disposition, disposition);
        assert!(!completion.outcome.applied);
        assert!(completion.outcome.changed_files.is_empty());
        assert!(completion.outcome.commit_hash.is_none());
        assert_eq!(
            completion.outcome.error.as_deref(),
            if disposition == StepEditDisposition::Failed {
                Some("host failure")
            } else {
                None
            }
        );
    }
}

#[test]
fn sequence_preserves_completed_steps_but_does_not_publish_success_on_storage_failure() {
    let sequence = sequence(ReviewSequenceStop::Interrupted(ReviewStepStop::Storage(
        "Cannot save the review horizon".into(),
    )));
    assert_eq!(sequence.executed_steps[1].commit.as_deref(), Some("last"));
    let completion = sequence.edit_completion();
    assert_eq!(completion.disposition, StepEditDisposition::Failed);
    assert!(!completion.outcome.applied);
    assert_eq!(
        completion.outcome.error.as_deref(),
        Some("infrastructure error: Cannot save the review horizon")
    );
}

#[test]
fn cancellation_keeps_the_commit_and_an_incomplete_rollback_keeps_its_diagnostic() {
    use tauqe_core::workflow::implementation::StepTurnResult;
    let StepLoopReport::Turn(mut report) =
        turn(Some("committed"), None, Some(VerificationVerdict::Passed))
    else {
        panic!("expected an isolated step attempt report");
    };
    report.cancelled = true;
    report.outcome = Err(tauqe_core::execution::ExecutionError::Cancelled.into());
    let settlement = classify_step_attempt(&report);
    assert_eq!(settlement.result, StepTurnResult::Cancelled);
    assert_eq!(settlement.confirmed_commit(), Some("committed"));
    assert_eq!(settlement.edit.as_ref().unwrap().changed_files, ["b", "a"]);
    let completion = classify_step_edit_completion(&StepLoopReport::Turn(report));
    assert_eq!(completion.disposition, StepEditDisposition::Cancelled);
    assert!(completion.outcome.applied);
    assert_eq!(completion.outcome.commit_hash.as_deref(), Some("committed"));

    let mut report = settled(ResponseEnvelope::default(), None);
    report.recovery_error = Some("Backup restoration was incomplete".into());
    assert!(matches!(
        classify_step_attempt(&report).result,
        StepTurnResult::Failed(failure) if !failure.is_domain()
            && failure.reason() == "Backup restoration was incomplete"
    ));
    let completion = classify_step_edit_completion(&StepLoopReport::Turn(Box::new(report)));
    assert_eq!(completion.disposition, StepEditDisposition::Failed);
    assert_eq!(
        completion.outcome.error.as_deref(),
        Some("Backup restoration was incomplete")
    );
}

#[test]
fn cancellation_does_not_invent_a_commit_after_rollback_or_finalization_failure() {
    use tauqe_core::workflow::implementation::StepTurnResult;

    for edit in [
        None,
        Some(EditOutcome {
            commit_hash: Some("unconfirmed".into()),
            error: Some("commit failed".into()),
            verification: Some(VerificationVerdict::Passed),
            ..Default::default()
        }),
    ] {
        let mut report = settled(ResponseEnvelope::default(), edit.clone());
        report.cancelled = true;
        let settlement = classify_step_attempt(&report);
        assert_eq!(settlement.result, StepTurnResult::Cancelled);
        assert_eq!(settlement.confirmed_commit(), None);
        assert_eq!(settlement.edit, edit);
    }
}
