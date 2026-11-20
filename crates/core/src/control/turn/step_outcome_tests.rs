//! Regressions for the supervising workflow classification of turn artifacts.

use super::TurnArtifact;
use crate::control::outcome::{FailureReport, LoopSuccess};
use crate::control::outcome::{LoopResult, ResponseEnvelope};
use crate::edits::VerificationVerdict;
use crate::llm::LlmCommand;
use crate::llm::{
    AbortCommand, BlockedCommand, CommitMessageCommand, TextCommand, TurnErrorCommand,
    TurnSkipCommand,
};
use crate::workflow::implementation::{classify_step_turn, StepFailure, StepTurnResult};

fn success(
    verdict: Option<VerificationVerdict>,
    commands: &[LlmCommand],
    commit_error: Option<&str>,
) -> LoopResult<TurnArtifact> {
    let envelope = ResponseEnvelope::from_commands(commands);
    LoopResult::Success(LoopSuccess::new(
        TurnArtifact {
            modified_paths: vec!["a.rs".into(), "b.rs".into()],
            commit_hash: Some("commit-id".into()),
            commit_error: commit_error.map(str::to_string),
            verification: verdict,
            rounds_spent: 1,
            envelope: envelope.clone(),
        },
        envelope,
    ))
}

fn committed() -> LlmCommand {
    LlmCommand::CommitMessage(CommitMessageCommand {
        commit_message: "fix: complete step".into(),
    })
}

fn blocked() -> LlmCommand {
    LlmCommand::Blocked(BlockedCommand {
        reason: " missing prerequisite ".into(),
        details: " dependency details ".into(),
    })
}

fn aborted() -> LlmCommand {
    LlmCommand::Abort(AbortCommand {
        reason: " tool unavailable ".into(),
        recommendation: " install the tool ".into(),
    })
}

#[test]
fn passed_and_unconfigured_verification_keep_summary_and_commit() {
    for verdict in [
        VerificationVerdict::Passed,
        VerificationVerdict::NotConfigured,
    ] {
        let result = classify_step_turn(success(
            Some(verdict),
            &[
                LlmCommand::Text(TextCommand {
                    content: "Completed".into(),
                }),
                LlmCommand::Text(TextCommand {
                    content: "Verified".into(),
                }),
            ],
            None,
        ));
        assert_eq!(
            result,
            StepTurnResult::Success {
                summary: "Completed\nVerified".into(),
                commit: Some("commit-id".into())
            }
        );
    }
    assert_eq!(
        classify_step_turn(success(
            Some(VerificationVerdict::Passed),
            &[LlmCommand::Text(TextCommand {
                content: "  ".into()
            }),],
            None
        )),
        StepTurnResult::Success {
            summary: "a.rs, b.rs".into(),
            commit: Some("commit-id".into())
        }
    );
}

#[test]
fn an_explicit_success_or_skip_is_required_when_verification_is_absent() {
    for command in [
        committed(),
        LlmCommand::TurnSkip(TurnSkipCommand {
            reason: "already satisfied".into(),
        }),
    ] {
        assert!(matches!(
            classify_step_turn(success(None, &[command], None)),
            StepTurnResult::Success { .. }
        ));
    }
    for commands in [
        vec![],
        vec![LlmCommand::Text(TextCommand {
            content: "All done".into(),
        })],
        vec![LlmCommand::TurnError(TurnErrorCommand {
            error_message: "failed".into(),
        })],
    ] {
        assert_eq!(
            classify_step_turn(success(None, &commands, None)),
            StepTurnResult::Failed(StepFailure::domain(
                "The step turn produced neither a verified change set nor a declared outcome"
            ))
        );
    }
}

#[test]
fn a_declared_success_does_not_override_failed_or_cancelled_verification() {
    for verdict in [VerificationVerdict::Failed, VerificationVerdict::Cancelled] {
        assert_eq!(
            classify_step_turn(success(Some(verdict), &[committed()], None)),
            StepTurnResult::Failed(StepFailure::domain(
                "The change set of the step did not pass toolchain verification"
            ))
        );
    }
}

#[test]
fn commit_failure_takes_precedence_and_stops_the_run_as_infrastructure_failure() {
    assert_eq!(
        classify_step_turn(success(
            Some(VerificationVerdict::Passed),
            &[blocked(), aborted(), committed()],
            Some("disk error")
        )),
        StepTurnResult::Failed(StepFailure::infrastructure(
            "Failed to commit the change set of the step: disk error"
        ))
    );
}

#[test]
fn semantic_block_and_abort_keep_their_precedence_and_messages_in_both_loop_outcomes() {
    let commands = [blocked(), aborted()];
    for result in [
        success(Some(VerificationVerdict::Passed), &commands, None),
        LoopResult::Diverged(ResponseEnvelope::from_commands(&commands)),
    ] {
        assert_eq!(
            classify_step_turn(result),
            StepTurnResult::Blocked("missing prerequisite\n\ndependency details".into())
        );
    }
    let commands = [aborted()];
    for result in [
        success(Some(VerificationVerdict::Passed), &commands, None),
        LoopResult::Diverged(ResponseEnvelope::from_commands(&commands)),
    ] {
        assert_eq!(
            classify_step_turn(result),
            StepTurnResult::Abort(
                "Execution aborted: tool unavailable\n\nRecommended action:\ninstall the tool"
                    .into()
            )
        );
    }
    let commands = [LlmCommand::Blocked(BlockedCommand {
        reason: " reason ".into(),
        details: "  ".into(),
    })];
    assert_eq!(
        classify_step_turn(LoopResult::Diverged(ResponseEnvelope::from_commands(
            &commands
        ))),
        StepTurnResult::Blocked("reason".into())
    );
}

#[test]
fn exhaustion_and_undeclared_divergence_are_domain_failures() {
    assert_eq!(
        classify_step_turn(LoopResult::Exhausted(FailureReport::new(
            "budget exhausted",
            3,
            Vec::new()
        ))),
        StepTurnResult::Failed(StepFailure::domain("budget exhausted"))
    );
    assert_eq!(
        classify_step_turn(LoopResult::Diverged(ResponseEnvelope::from_commands(&[
            committed()
        ]))),
        StepTurnResult::Failed(StepFailure::domain(
            "The step turn diverged without a completed change set"
        ))
    );
}

#[test]
fn technical_result_exposes_decoded_commands_and_rounds_without_raw_text() {
    let result = success(
        Some(VerificationVerdict::NotConfigured),
        &[committed()],
        None,
    );
    let LoopResult::Success(success) = result else {
        panic!("expected success");
    };
    assert_eq!(success.value.rounds_spent, 1);
    assert_eq!(success.value.commit_hash.as_deref(), Some("commit-id"));
    assert_eq!(
        success.value.verification,
        Some(VerificationVerdict::NotConfigured)
    );
    assert!(matches!(
        success.response.commands(),
        [LlmCommand::CommitMessage(_)]
    ));
}
