use crate::contracts::Project;
use crate::handlers::common::emit;
use crate::transport::OutChannel;
use tauqe_flow::api::EditCycleEvent;
#[cfg(test)]
use tauqe_flow::api::{EditCycle, EditTermination};
use tauqe_protocol::edit::{EditFinishedEvent, EditStartedEvent};
use tauqe_protocol::events;

#[cfg(test)]
pub(crate) fn edit_cycle(out: OutChannel, operation_id: impl Into<String>) -> EditCycle {
    let operation_id = operation_id.into();
    EditCycle::new(move |event| publish(&out, &operation_id, event))
}

pub(crate) fn publish(out: &OutChannel, operation_id: &str, event: EditCycleEvent) {
    match event {
        EditCycleEvent::Started => emit(
            out,
            events::EDIT_STARTED,
            serde_json::to_value(EditStartedEvent {
                operation_id: operation_id.into(),
            })
            .ok(),
        ),
        EditCycleEvent::Finished(outcome) => emit(
            out,
            events::EDIT_FINISHED,
            serde_json::to_value(EditFinishedEvent {
                operation_id: operation_id.into(),
                outcome: outcome.project(),
            })
            .ok(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tauqe_core::edits::{EditOutcome, VerificationVerdict};
    use tokio::sync::mpsc::UnboundedReceiver;

    const ALL: [EditTermination; 7] = [
        EditTermination::Answered,
        EditTermination::StagingRefused,
        EditTermination::Cancelled,
        EditTermination::Withdrawn,
        EditTermination::ProviderError,
        EditTermination::ProtocolFailure,
        EditTermination::Aborted,
    ];

    /// Drains the queued lines as parsed events.
    fn drain(rx: &mut UnboundedReceiver<String>) -> Vec<serde_json::Value> {
        let mut events = Vec::new();
        while let Ok(line) = rx.try_recv() {
            events.push(serde_json::from_str(&line).expect("event line is JSON"));
        }
        events
    }

    fn methods(events: &[serde_json::Value]) -> Vec<String> {
        events
            .iter()
            .map(|event| event["method"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn every_termination_has_a_non_empty_description() {
        for termination in ALL {
            assert!(
                !termination.description().trim().is_empty(),
                "{termination:?} must explain itself"
            );
        }
    }

    #[test]
    fn a_second_closing_publishes_nothing() {
        let (out, mut rx) = OutChannel::new();
        let cycle = edit_cycle(out, "op-1");
        assert!(cycle.open());
        assert!(!cycle.open(), "opening an open cycle is a no-op");

        assert!(cycle.close_unapplied(EditTermination::Cancelled, Vec::new(), None));
        assert!(!cycle.close_unapplied(EditTermination::ProviderError, Vec::new(), None));
        assert!(!cycle.close_outcome(EditTermination::Answered, EditOutcome::default()));

        let events = drain(&mut rx);
        assert_eq!(
            methods(&events),
            vec![events::EDIT_STARTED, events::EDIT_FINISHED]
        );
        assert_eq!(
            events[1]["params"]["error"],
            EditTermination::Cancelled.description()
        );
    }

    #[test]
    fn a_closing_without_an_open_cycle_publishes_nothing() {
        let (out, mut rx) = OutChannel::new();
        let cycle = edit_cycle(out, "op-2");
        assert!(!cycle.close_unapplied(EditTermination::Answered, Vec::new(), None));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_refusal_always_carries_a_reason_and_a_success_never_does() {
        let (out, mut rx) = OutChannel::new();
        let cycle = edit_cycle(out, "op-3");

        for termination in ALL {
            cycle.open();
            cycle.close_outcome(termination, EditOutcome::default());
            cycle.open();
            cycle.close_outcome(
                termination,
                EditOutcome {
                    applied: false,
                    error: Some("   ".to_string()),
                    ..EditOutcome::default()
                },
            );
        }
        for event in drain(&mut rx)
            .iter()
            .filter(|event| event["method"] == events::EDIT_FINISHED)
        {
            let error = event["params"]["error"].as_str().unwrap_or_default();
            assert!(
                !error.trim().is_empty(),
                "refusal without a reason: {event}"
            );
        }

        cycle.open();
        cycle.close_outcome(
            EditTermination::Answered,
            EditOutcome {
                applied: true,
                error: Some("stale".to_string()),
                changed_files: vec!["src/lib.rs".to_string()],
                commit_hash: Some("abc1234".to_string()),
                verification: Some(VerificationVerdict::Passed),
            },
        );
        let events = drain(&mut rx);
        let finished = &events[1]["params"];
        assert_eq!(finished["applied"], true);
        assert!(finished.get("error").is_none(), "success carried an error");
        assert_eq!(finished["commit_hash"], "abc1234");
    }

    #[test]
    fn a_named_error_is_kept_for_an_unapplied_outcome() {
        let (out, mut rx) = OutChannel::new();
        let cycle = edit_cycle(out, "op-4");
        cycle.open();
        // The typed protocol failure is the classification of this ending, and the
        // named error the outcome carries must still reach the client verbatim
        // instead of being replaced by the termination description.
        cycle.close_outcome(
            EditTermination::ProtocolFailure,
            EditOutcome {
                applied: false,
                error: Some("patch did not match".to_string()),
                ..EditOutcome::default()
            },
        );
        let events = drain(&mut rx);
        assert_eq!(events[1]["params"]["error"], "patch did not match");
    }

    #[test]
    fn dropping_an_open_cycle_closes_it_as_aborted() {
        let (out, mut rx) = OutChannel::new();
        let cycle = Arc::new(edit_cycle(out, "op-5"));
        cycle.open();
        let sibling = Arc::clone(&cycle);
        drop(cycle);
        assert!(
            drain(&mut rx)
                .iter()
                .all(|event| event["method"] != events::EDIT_FINISHED),
            "the cycle must stay open while a reference lives"
        );

        drop(sibling);
        let events = drain(&mut rx);
        assert_eq!(methods(&events), vec![events::EDIT_FINISHED]);
        assert_eq!(
            events[0]["params"]["error"],
            EditTermination::Aborted.description()
        );
    }

    #[test]
    fn dropping_a_closed_cycle_publishes_nothing() {
        let (out, mut rx) = OutChannel::new();
        let cycle = edit_cycle(out, "op-6");
        cycle.open();
        cycle.close_unapplied(EditTermination::Answered, Vec::new(), None);
        drop(cycle);
        assert_eq!(
            methods(&drain(&mut rx)),
            vec![events::EDIT_STARTED, events::EDIT_FINISHED]
        );
    }

    #[test]
    fn a_closed_cycle_may_be_reopened_for_the_next_rung() {
        let (out, mut rx) = OutChannel::new();
        let cycle = edit_cycle(out, "op-7");
        assert!(cycle.open());
        assert!(cycle.close_unapplied(EditTermination::Withdrawn, Vec::new(), None));
        assert!(cycle.open());
        assert!(cycle.close_unapplied(EditTermination::Answered, Vec::new(), None));
        assert_eq!(
            methods(&drain(&mut rx)),
            vec![
                events::EDIT_STARTED,
                events::EDIT_FINISHED,
                events::EDIT_STARTED,
                events::EDIT_FINISHED
            ]
        );
    }

    #[test]
    fn an_answered_ending_carries_no_error() {
        let (out, mut rx) = OutChannel::new();
        let cycle = edit_cycle(out, "op-8");
        assert!(cycle.open());
        assert!(cycle.close_answered());
        assert!(!cycle.close_answered(), "a closed cycle publishes nothing");

        let events = drain(&mut rx);
        assert_eq!(
            methods(&events),
            vec![events::EDIT_STARTED, events::EDIT_FINISHED]
        );
        let finished = &events[1]["params"];
        assert_eq!(finished["applied"], false);
        assert!(
            finished.get("error").is_none(),
            "an answer is not a failure: {finished}"
        );
    }
}
