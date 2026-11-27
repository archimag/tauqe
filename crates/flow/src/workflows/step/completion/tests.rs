use super::*;
use crate::workflows::review::{ReviewSequenceResult, ReviewSequenceStop, ReviewStepStop};
use crate::workflows::turn::{StepRecovery, TurnArtifact, TurnOutcome};
use std::sync::Arc;
use tauqe_core::edits::transaction::RollbackOutcome;
use tauqe_core::edits::{EditOutcome, VerificationVerdict};
use tauqe_core::execution::ResponseEnvelope;
use tauqe_core::workflow::implementation::{StepTurnResult, StepTurnSettlement};

fn settled(response: ResponseEnvelope, edit: Option<EditOutcome>) -> StepAttemptReport {
    StepAttemptReport {
        outcome: Ok(TurnOutcome::Settled(TurnArtifact {
            response,
            response_round: None,
            edit: edit.clone(),
            rounds_spent: 1,
        })),
        edit,
        recovery: StepRecovery::RolledBack(Arc::new(RollbackOutcome::NothingToDo)),
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
#[test]
fn verified_direct_application_keeps_its_disk_receipt_without_claiming_a_commit() {
    use tauqe_core::workflow::implementation::StepTurnResult;

    for verdict in [
        VerificationVerdict::Passed,
        VerificationVerdict::NotConfigured,
    ] {
        let report = settled(
            ResponseEnvelope::default(),
            Some(EditOutcome {
                applied: true,
                changed_files: vec!["source.rs".into()],
                verification: Some(verdict),
                ..Default::default()
            }),
        );
        assert!(matches!(
            classify_step_attempt(&report).result,
            StepTurnResult::Success { commit: None, .. }
        ));
        assert!(report.edit.as_ref().unwrap().applied);
        let published = classify_step_edit_completion(&StepLoopReport::Turn(Box::new(report)));
        assert_eq!(published.disposition, StepEditDisposition::Completed);
        assert!(!published.outcome.applied);
        assert_eq!(published.outcome.changed_files, ["source.rs"]);
        assert_eq!(published.outcome.verification, Some(verdict));
        assert!(published.outcome.commit_hash.is_none());
    }
}

fn sequence(stop: ReviewSequenceStop) -> ReviewSequenceResult {
    ReviewSequenceResult {
        stop,
        usage: None,
        executed_steps: vec![
            receipt(
                Some("first"),
                &["b", "a"],
                Some(VerificationVerdict::Passed),
            ),
            receipt(Some("last"), &["a"], Some(VerificationVerdict::Passed)),
            receipt(None, &["c"], Some(VerificationVerdict::Passed)),
        ],
    }
}

fn receipt(
    hash: Option<&str>,
    paths: &[&str],
    verdict: Option<VerificationVerdict>,
) -> StepTurnSettlement {
    StepTurnSettlement {
        result: StepTurnResult::Success {
            summary: String::new(),
            commit: hash.map(str::to_owned),
        },
        edit: Some(EditOutcome {
            applied: true,
            commit_hash: hash.map(str::to_owned),
            changed_files: paths.iter().map(|path| (*path).to_owned()).collect(),
            verification: verdict,
            ..Default::default()
        }),
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
        usage: None,
    };
    let completion = empty.edit_completion();
    assert!(completion.outcome.applied);
    assert!(completion.outcome.commit_hash.is_none());
}
#[test]
fn sequence_verification_comes_from_receipts_not_completion() {
    let sequence = ReviewSequenceResult {
        stop: ReviewSequenceStop::Completed,
        usage: None,
        executed_steps: vec![
            receipt(Some("first"), &["a"], Some(VerificationVerdict::Passed)),
            receipt(None, &["b"], Some(VerificationVerdict::NotConfigured)),
        ],
    };
    let facts = sequence.facts();
    assert!(facts.completed);
    assert!(facts.applied);
    assert_eq!(facts.commit.as_deref(), Some("first"));
    assert_eq!(facts.verification, Some(VerificationVerdict::NotConfigured));
    assert_eq!(
        sequence.edit_completion().outcome.verification,
        facts.verification
    );

    let empty = ReviewSequenceResult {
        stop: ReviewSequenceStop::Completed,
        usage: None,
        executed_steps: vec![StepTurnResult::Success {
            summary: "Already satisfied".into(),
            commit: None,
        }
        .into()],
    };
    assert!(empty.facts().completed);
    assert!(!empty.facts().applied);
    assert_eq!(empty.facts().verification, None);
    // The historical completion projection does not equate its applied flag
    // with the internal disk fact.
    assert!(empty.edit_completion().outcome.applied);
    assert_eq!(empty.edit_completion().outcome.verification, None);
}

#[test]
fn cancelled_sequence_retains_committed_disk_facts_without_publishing_success() {
    let sequence = ReviewSequenceResult {
        stop: ReviewSequenceStop::Interrupted(ReviewStepStop::Cancelled),
        usage: None,
        executed_steps: vec![StepTurnSettlement {
            result: StepTurnResult::Cancelled,
            edit: Some(EditOutcome {
                applied: true,
                changed_files: vec!["committed.rs".into()],
                commit_hash: Some("confirmed".into()),
                verification: Some(VerificationVerdict::Passed),
                ..Default::default()
            }),
        }],
    };
    let facts = sequence.facts();
    assert!(!facts.completed);
    assert!(facts.applied);
    assert_eq!(facts.commit.as_deref(), Some("confirmed"));
    assert_eq!(facts.changed_files, ["committed.rs"]);
    assert_eq!(facts.verification, Some(VerificationVerdict::Passed));
    let published = sequence.edit_completion();
    assert_eq!(published.disposition, StepEditDisposition::Cancelled);
    assert!(!published.outcome.applied);
    assert_eq!(published.outcome.commit_hash, facts.commit);
    assert_eq!(published.outcome.changed_files, facts.changed_files);
    assert_eq!(published.outcome.verification, facts.verification);
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
fn interrupted_sequences_publish_the_reason_and_keep_prior_receipts() {
    for stop in [
        ReviewStepStop::Blocked("Verification failed in the S3 pacing test".into()),
        ReviewStepStop::Exhausted("Coder attempt budget exhausted".into()),
    ] {
        let reason = stop.to_string();
        let mut report = sequence(ReviewSequenceStop::Interrupted(stop));
        let completion = report.edit_completion();
        assert_eq!(completion.disposition, StepEditDisposition::Refused);
        assert!(!completion.outcome.applied);
        assert_eq!(completion.outcome.error.as_deref(), Some(reason.as_str()));
        assert_eq!(completion.outcome.changed_files, ["b", "a", "a", "c"]);
        assert_eq!(completion.outcome.commit_hash.as_deref(), Some("last"));
        assert_eq!(
            completion.outcome.verification,
            Some(VerificationVerdict::Passed)
        );

        report.executed_steps.clear();
        let completion = report.edit_completion();
        assert_eq!(completion.disposition, StepEditDisposition::Refused);
        assert!(!completion.outcome.applied);
        assert_eq!(completion.outcome.error.as_deref(), Some(reason.as_str()));
        assert!(completion.outcome.changed_files.is_empty());
        assert!(completion.outcome.commit_hash.is_none());
        assert!(completion.outcome.verification.is_none());
    }
}

#[test]
fn sequence_preserves_completed_steps_but_does_not_publish_success_on_storage_failure() {
    let sequence = sequence(ReviewSequenceStop::Interrupted(ReviewStepStop::Storage(
        "Cannot save the review horizon".into(),
    )));
    assert_eq!(sequence.executed_steps[1].confirmed_commit(), Some("last"));
    let completion = sequence.edit_completion();
    assert_eq!(completion.disposition, StepEditDisposition::Failed);
    assert!(!completion.outcome.applied);
    assert_eq!(completion.outcome.changed_files, ["b", "a", "a", "c"]);
    assert_eq!(completion.outcome.commit_hash.as_deref(), Some("last"));
    assert_eq!(
        completion.outcome.verification,
        Some(VerificationVerdict::Passed)
    );
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
    report.recovery = StepRecovery::RolledBack(Arc::new(RollbackOutcome::Incomplete {
        policy_error: None,
        backup_error: "Backup restoration was incomplete".into(),
    }));
    assert!(matches!(
        classify_step_attempt(&report).result,
        StepTurnResult::Failed(failure) if failure.is_unreverted()
            && failure.reason().contains("Backup restoration was incomplete")
    ));
    let completion = classify_step_edit_completion(&StepLoopReport::Turn(Box::new(report)));
    assert_eq!(completion.disposition, StepEditDisposition::Failed);
    assert_eq!(
        completion.outcome.error.as_deref(),
        Some("Backup restoration was incomplete: Backup restoration was incomplete")
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

#[test]
fn recovery_receipts_distinguish_confirmed_rollback_from_unreverted_work() {
    use tauqe_core::workflow::implementation::StepTurnResult;

    for (recovery, unreverted) in [
        (
            StepRecovery::RolledBack(Arc::new(RollbackOutcome::RestoredByPolicy)),
            false,
        ),
        (
            StepRecovery::RolledBack(Arc::new(RollbackOutcome::RestoredFromBackup {
                policy_error: None,
            })),
            false,
        ),
        (
            StepRecovery::RolledBack(Arc::new(RollbackOutcome::RestoredFromBackup {
                policy_error: Some("index locked".into()),
            })),
            true,
        ),
        (
            StepRecovery::RolledBack(Arc::new(RollbackOutcome::Incomplete {
                policy_error: None,
                backup_error: "file conflict".into(),
            })),
            true,
        ),
        (
            StepRecovery::RolledBack(Arc::new(RollbackOutcome::Incomplete {
                policy_error: Some("policy rollback failed".into()),
                backup_error: "file restoration failed".into(),
            })),
            true,
        ),
    ] {
        let mut report = settled(ResponseEnvelope::default(), None);
        report.outcome =
            Err(tauqe_core::execution::ExecutionError::Other("provider failed".into()).into());
        report.recovery = recovery;
        let StepTurnResult::Failed(failure) = classify_step_attempt(&report).result else {
            panic!("expected a failed attempt");
        };
        assert_eq!(failure.is_unreverted(), unreverted);
        assert!(failure.reason().contains("provider failed"));
        if unreverted {
            assert!(failure.reason().contains("Recovery:"));
            assert_eq!(
                classify_step_edit_completion(&StepLoopReport::Turn(Box::new(report))).disposition,
                StepEditDisposition::Failed
            );
        }
    }
}

#[test]
fn finalization_attempt_keeps_the_commit_receipt_and_does_not_claim_rollback() {
    use tauqe_core::workflow::implementation::StepTurnResult;

    let mut report = settled(ResponseEnvelope::default(), None);
    report.recovery = StepRecovery::FinalizationAttempted;
    assert!(matches!(classify_step_attempt(&report).result,
        StepTurnResult::Failed(failure) if failure.is_unreverted()));

    report.edit = Some(EditOutcome {
        applied: true,
        commit_hash: Some("confirmed".into()),
        verification: Some(VerificationVerdict::Passed),
        ..Default::default()
    });
    report.cancelled = true;
    let settlement = classify_step_attempt(&report);
    assert_eq!(settlement.result, StepTurnResult::Cancelled);
    assert_eq!(settlement.confirmed_commit(), Some("confirmed"));
}
