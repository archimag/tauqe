//! Exactly-once closure of the edit cycle.
//!
//! `edit/started` and `edit/finished` form one protocol transaction: every
//! opening is answered by exactly one closing that carries an [`EditOutcome`].
//! The cycle belongs to the server, because it is the lifecycle of RPC events;
//! the core engine knows nothing about the pair.
//!
//! The owner of a cycle is an [`EditCycle`] handle shared through an `Arc`.
//! Whoever reaches the end of the cycle first closes it: the closing wins
//! through one atomic `compare_exchange`, so a second and every later closing
//! is a no-op. When the last reference is dropped while the cycle is still
//! open — a panicking turn task or a forced abort — the drop guard closes it
//! as the last resort.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::handlers::common::emit;
use crate::transport::OutChannel;
use tauqe_protocol::edit::{EditFinishedEvent, EditOutcome, EditStartedEvent, VerificationVerdict};
use tauqe_protocol::events;

/// Typed reason a cycle ended.
///
/// The reason is declared by the code that ends the cycle and is never
/// inferred from text. Every variant owns a non-empty description, so a
/// closing without an applied change set always explains itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditTermination {
    /// The turn finished normally without applying a change set.
    Completed,
    /// The operation was cancelled by the developer.
    Cancelled,
    /// The streamed attempt was withdrawn and its edits are void.
    Withdrawn,
    /// The model provider failed before the change set was applied.
    ProviderError,
    /// The model output violated the protocol, so no change set was applied.
    ///
    /// Built from the typed protocol-failure fact of the turn, never from the
    /// wording of the diagnostic that explains it.
    ProtocolFailure,
    Blocked,
    /// The turn was aborted before it could close the cycle itself.
    Aborted,
}

impl EditTermination {
    /// Human-readable reason carried by an unapplied closing.
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Completed => "The turn completed without applying any file change.",
            Self::Cancelled => "The operation was cancelled before the edits were applied.",
            Self::Withdrawn => {
                "The streamed attempt was withdrawn, so its proposed edits were discarded."
            }
            Self::ProviderError => "The model provider failed before the edits could be applied.",
            Self::Blocked => "The model reported a blocker instead of proposing file changes.",
            Self::ProtocolFailure => {
                "The model response violated the edit protocol, so no change set was applied."
            }
            Self::Aborted => "The turn was aborted before the edit cycle could finish normally.",
        }
    }
}

/// Owner of one operation's edit cycle.
pub(crate) struct EditCycle {
    out: OutChannel,
    operation_id: String,
    open: AtomicBool,
}

impl EditCycle {
    /// Creates a handle whose cycle is not open yet.
    pub(crate) fn new(out: OutChannel, operation_id: impl Into<String>) -> Self {
        Self {
            out,
            operation_id: operation_id.into(),
            open: AtomicBool::new(false),
        }
    }

    /// Reports whether the cycle is currently open.
    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    /// Opens the cycle and publishes `edit/started`.
    ///
    /// Opening an already open cycle is a no-op, so a relay and an owner may
    /// both announce the start. After a closing the cycle may be opened again
    /// for the next escalation rung. Returns whether this call opened it.
    pub(crate) fn open(&self) -> bool {
        if self.open.swap(true, Ordering::SeqCst) {
            return false;
        }
        emit(
            &self.out,
            events::EDIT_STARTED,
            serde_json::to_value(EditStartedEvent {
                operation_id: self.operation_id.clone(),
            })
            .ok(),
        );
        true
    }

    /// Closes the cycle from the outcome of an application attempt.
    ///
    /// An applied outcome never carries an error. An unapplied outcome always
    /// carries a reason: the error it already names, or the description of
    /// `termination` when it names none. Returns whether this call won the
    /// closing; a losing call publishes nothing.
    pub(crate) fn close_outcome(
        &self,
        termination: EditTermination,
        mut outcome: EditOutcome,
    ) -> bool {
        if outcome.applied {
            outcome.error = None;
        } else {
            let named = outcome
                .error
                .take()
                .filter(|error| !error.trim().is_empty());
            outcome.error = Some(named.unwrap_or_else(|| termination.description().to_string()));
        }
        self.finish(outcome)
    }

    /// Closes the cycle without an applied change set for a typed reason.
    pub(crate) fn close_unapplied(
        &self,
        termination: EditTermination,
        changed_files: Vec<String>,
        verification: Option<VerificationVerdict>,
    ) -> bool {
        self.close_outcome(
            termination,
            EditOutcome {
                applied: false,
                error: None,
                changed_files,
                commit_hash: None,
                verification,
            },
        )
    }

    /// Arbitrates the closing and publishes `edit/finished` for the winner.
    fn finish(&self, outcome: EditOutcome) -> bool {
        if self
            .open
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        emit(
            &self.out,
            events::EDIT_FINISHED,
            serde_json::to_value(EditFinishedEvent {
                operation_id: self.operation_id.clone(),
                outcome,
            })
            .ok(),
        );
        true
    }
}

impl Drop for EditCycle {
    /// Last resort on unwind: an open cycle is closed as aborted.
    fn drop(&mut self) {
        self.close_unapplied(EditTermination::Aborted, Vec::new(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc::UnboundedReceiver;

    const ALL: [EditTermination; 7] = [
        EditTermination::Completed,
        EditTermination::Cancelled,
        EditTermination::Withdrawn,
        EditTermination::ProviderError,
        EditTermination::Blocked,
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
        let cycle = EditCycle::new(out, "op-1");
        assert!(cycle.open());
        assert!(!cycle.open(), "opening an open cycle is a no-op");

        assert!(cycle.close_unapplied(EditTermination::Cancelled, Vec::new(), None));
        assert!(!cycle.close_unapplied(EditTermination::ProviderError, Vec::new(), None));
        assert!(!cycle.close_outcome(EditTermination::Completed, EditOutcome::default()));

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
        let cycle = EditCycle::new(out, "op-2");
        assert!(!cycle.close_unapplied(EditTermination::Completed, Vec::new(), None));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_refusal_always_carries_a_reason_and_a_success_never_does() {
        let (out, mut rx) = OutChannel::new();
        let cycle = EditCycle::new(out, "op-3");

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
            EditTermination::Completed,
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
        let cycle = EditCycle::new(out, "op-4");
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
        let cycle = Arc::new(EditCycle::new(out, "op-5"));
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
        let cycle = EditCycle::new(out, "op-6");
        cycle.open();
        cycle.close_unapplied(EditTermination::Blocked, Vec::new(), None);
        drop(cycle);
        assert_eq!(
            methods(&drain(&mut rx)),
            vec![events::EDIT_STARTED, events::EDIT_FINISHED]
        );
    }

    #[test]
    fn a_closed_cycle_may_be_reopened_for_the_next_rung() {
        let (out, mut rx) = OutChannel::new();
        let cycle = EditCycle::new(out, "op-7");
        assert!(cycle.open());
        assert!(cycle.close_unapplied(EditTermination::Withdrawn, Vec::new(), None));
        assert!(cycle.open());
        assert!(cycle.is_open());
        assert!(cycle.close_unapplied(EditTermination::Completed, Vec::new(), None));
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
}
