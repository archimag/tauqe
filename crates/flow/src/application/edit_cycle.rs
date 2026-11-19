use std::sync::atomic::{AtomicBool, Ordering};
use tauqe_core::edits::{EditOutcome, VerificationVerdict};

pub enum EditCycleEvent {
    Started,
    Finished(EditOutcome),
}

/// Typed reason a cycle ended.
///
/// The reason is declared by the code that ends the cycle and is never
/// inferred from text. Every variant owns a non-empty description, so a
/// closing without an applied change set always explains itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTermination {
    /// The turn concluded with a conversational answer instead of a change set.
    ///
    /// A dialogue closes an answered turn through [`EditCycle::close_answered`],
    /// which reports no error; this variant classifies the endings of the step
    /// contours, where a step that answers instead of committing did not deliver
    /// the change it owed.
    Answered,
    /// The turn proposed file edits, but they could not be converged or applied by virtual staging.
    StagingRefused,
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
    /// The turn was aborted before it could close the cycle itself.
    Aborted,
}

impl EditTermination {
    /// Human-readable reason carried by an unapplied closing.
    pub fn description(self) -> &'static str {
        match self {
            Self::Answered => {
                "The turn concluded with an answer without proposing any file change."
            }
            Self::StagingRefused => {
                "The proposed edits could not be staged or applied to the repository."
            }
            Self::Cancelled => "The operation was cancelled before the edits were applied.",
            Self::Withdrawn => {
                "The streamed attempt was withdrawn, so its proposed edits were discarded."
            }
            Self::ProviderError => "The model provider failed before the edits could be applied.",
            Self::ProtocolFailure => {
                "The model response violated the edit protocol, so no change set was applied."
            }
            Self::Aborted => "The turn was aborted before the edit cycle could finish normally.",
        }
    }
}

/// Owner of one operation's edit cycle.
pub struct EditCycle {
    publish: Box<dyn Fn(EditCycleEvent) + Send + Sync>,
    open: AtomicBool,
}

impl EditCycle {
    /// Creates a handle whose cycle is not open yet.
    pub fn new(publish: impl Fn(EditCycleEvent) + Send + Sync + 'static) -> Self {
        Self {
            publish: Box::new(publish),
            open: AtomicBool::new(false),
        }
    }

    /// Opens the cycle and publishes `edit/started`.
    ///
    /// Opening an already open cycle is a no-op, so a relay and an owner may
    /// both announce the start. After a closing the cycle may be opened again
    /// for the next escalation rung. Returns whether this call opened it.
    pub fn open(&self) -> bool {
        if self.open.swap(true, Ordering::SeqCst) {
            return false;
        }
        (self.publish)(EditCycleEvent::Started);
        true
    }

    /// Closes the cycle from the outcome of an application attempt.
    ///
    /// An applied outcome never carries an error. An unapplied outcome always
    /// carries a reason: the error it already names, or the description of
    /// `termination` when it names none. The one ending outside that rule is a
    /// conversational answer, which [`EditCycle::close_answered`] closes without
    /// an error because nothing was refused. Returns whether this call won the
    /// closing; a losing call publishes nothing.
    pub fn close_outcome(&self, termination: EditTermination, mut outcome: EditOutcome) -> bool {
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
    pub fn close_unapplied(
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

    /// Closes the cycle for a turn that answered without a change set.
    ///
    /// A conversational answer is a successful ending of a dialogue rather than
    /// a refused change, so the closing names no error: nothing was staged,
    /// nothing was lost, and there is nothing for the developer to repair.
    /// Returns whether this call won the closing.
    pub fn close_answered(&self) -> bool {
        self.finish(EditOutcome {
            applied: false,
            error: None,
            changed_files: Vec::new(),
            commit_hash: None,
            verification: None,
        })
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
        (self.publish)(EditCycleEvent::Finished(outcome));
        true
    }
}

impl Drop for EditCycle {
    /// Last resort on unwind: an open cycle is closed as aborted.
    fn drop(&mut self) {
        self.close_unapplied(EditTermination::Aborted, Vec::new(), None);
    }
}
