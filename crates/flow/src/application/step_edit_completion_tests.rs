use crate::application::step_completion::*;
use crate::application::step_dispatch::StepLoopReport;
use tauqe_core::control::outcome::LoopResult;
use tauqe_core::control::outcome::{FailureReport, LoopSuccess, ResponseEnvelope};
use tauqe_core::control::plan::{
    sequence::{PlanSequenceArtifact, PlanSequenceStop},
    step::PlanStepArtifact,
};
use tauqe_core::control::turn::TurnArtifact;
use tauqe_core::edits::VerificationVerdict;
fn turn(
    hash: Option<&str>,
    error: Option<&str>,
    verification: Option<VerificationVerdict>,
) -> StepLoopReport {
    let mut artifact = TurnArtifact::without_change_set(1, ResponseEnvelope::default());
    artifact.modified_paths = vec!["b".into(), "a".into()];
    artifact.commit_hash = hash.map(str::to_string);
    artifact.commit_error = error.map(str::to_string);
    artifact.verification = verification;
    StepLoopReport::Turn(LoopResult::Success(LoopSuccess::new(
        artifact,
        ResponseEnvelope::default(),
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
fn sequence(stop: PlanSequenceStop) -> StepLoopReport {
    StepLoopReport::Sequence(LoopResult::Success(LoopSuccess::new(
        PlanSequenceArtifact {
            stop,
            executed_steps: vec![
                PlanStepArtifact {
                    step_id: "1".into(),
                    commit: Some("first".into()),
                    modified_paths: vec!["b".into(), "a".into()],
                },
                PlanStepArtifact {
                    step_id: "2".into(),
                    commit: Some("last".into()),
                    modified_paths: vec!["a".into()],
                },
                PlanStepArtifact {
                    step_id: "3".into(),
                    commit: None,
                    modified_paths: vec!["c".into()],
                },
            ],
        },
        ResponseEnvelope::default(),
    )))
}
#[test]
fn completed_sequence_keeps_path_order_duplicates_and_last_available_commit() {
    let completion = classify_step_edit_completion(&sequence(PlanSequenceStop::Completed));
    assert!(completion.outcome.applied);
    assert_eq!(completion.outcome.changed_files, ["b", "a", "a", "c"]);
    assert_eq!(completion.outcome.commit_hash.as_deref(), Some("last"));
    assert_eq!(
        completion.outcome.verification,
        Some(VerificationVerdict::Passed)
    );
    let empty = StepLoopReport::Sequence(LoopResult::Success(LoopSuccess::new(
        PlanSequenceArtifact {
            stop: PlanSequenceStop::Completed,
            executed_steps: vec![],
        },
        ResponseEnvelope::default(),
    )));
    let completion = classify_step_edit_completion(&empty);
    assert!(completion.outcome.applied);
    assert!(completion.outcome.commit_hash.is_none());
}
#[test]
fn cancellation_failure_exhaustion_and_incomplete_sequences_keep_distinct_dispositions() {
    for (report, disposition) in [
        (StepLoopReport::Cancelled, StepEditDisposition::Cancelled),
        (
            StepLoopReport::Failed("host failure".into()),
            StepEditDisposition::Failed,
        ),
        (
            StepLoopReport::Turn(LoopResult::Exhausted(FailureReport::new(
                "budget",
                1,
                vec![],
            ))),
            StepEditDisposition::Refused,
        ),
        (
            StepLoopReport::Turn(LoopResult::Diverged(ResponseEnvelope::default())),
            StepEditDisposition::Completed,
        ),
        (
            sequence(PlanSequenceStop::Stopped),
            StepEditDisposition::Completed,
        ),
        (
            StepLoopReport::Sequence(LoopResult::Exhausted(FailureReport::new(
                "budget",
                1,
                vec![],
            ))),
            StepEditDisposition::Completed,
        ),
    ] {
        let completion = classify_step_edit_completion(&report);
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
