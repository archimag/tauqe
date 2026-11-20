//! Unified implementation machinery shared by every source of work.
//!
//! A step graph (`crate::plan::PlanImplementation`) declares *how* work is
//! executed and is independent of *what* the work is: a local plan, an
//! architectural review group, and a single review finding all execute through
//! the same graph algebra. This module owns the parts of that execution which
//! belong to no single workflow:
//!
//! - [`StepFailure`] — the taxonomy that tells a supervisor whether a failure is
//!   confined to one step, fatal to the whole run, or a working tree the failed
//!   attempt never reverted;
//! - [`ExecutionScope`] — the closed set of sources an implementation horizon
//!   can be resolved from;
//! - [`ImplementationFrame`] — one horizon resolved from the authoritative core
//!   storages, carrying the identifiers that map its outcome back;
//! - [`classify_scope_stall`] — the stall classification that decides whether a
//!   blocked scope justifies an Architect replanning wave;
//!
//! The application owns leaf execution, coder escalation, and the batch driver;
//! this module supplies their shared frame, failure, and request types.

mod turn_outcome;
pub use turn_outcome::{classify_refinement, classify_step_turn};

mod scope;

pub use scope::{classify_scope_stall, ExecutionScope, ImplementationFrame};

use crate::models::{ArchitectTier, ModelSelection, ReasoningEffort};
use crate::plan::{PlanImplementation, PlanItem, PlanItemStatus};
use crate::workflow::turn::TurnWorkQueue;
use tokio::sync::watch;

/// Category of an implementation step failure, controlling pipeline behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepFailureKind {
    /// The step could not be resolved in code: a patch failed to apply, the
    /// compiler still reported errors after healing, the retry budget was
    /// exhausted, or the model reported a semantic block. The failure is
    /// confined to the step and the queue continues with the next one.
    Domain,
    /// The turn failed before producing a usable result: a provider or
    /// transport error, a missing credential, an operation-slot conflict, or an
    /// explicit cancellation. The failure is fatal to the whole run.
    Infrastructure,
    /// The failed change set was not rolled back to the step checkpoint: the
    /// working tree carries the bytes of an aborted attempt. Retrying or
    /// replanning on a dirty tree would build the next attempt on lines nobody
    /// verified, so the failure is fatal to the whole run.
    Unreverted,
}

/// An implementation step failure paired with its pipeline-impact category.
///
/// The category decides whether a supervisor aborts or advances: a
/// [`StepFailureKind::Domain`] failure is confined to the current step, while
/// an [`StepFailureKind::Infrastructure`] failure stops the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    kind: StepFailureKind,
    reason: String,
    failed_step: Option<String>,
}

impl StepFailure {
    /// Creates a domain failure: confine the current step and continue.
    pub fn domain(reason: impl Into<String>) -> Self {
        Self {
            kind: StepFailureKind::Domain,
            reason: reason.into(),
            failed_step: None,
        }
    }

    /// Creates an infrastructure failure: abort the whole run.
    pub fn infrastructure(reason: impl Into<String>) -> Self {
        Self {
            kind: StepFailureKind::Infrastructure,
            reason: reason.into(),
            failed_step: None,
        }
    }

    /// Creates an unreverted-worktree failure: the failed change set was not
    /// rolled back, so the working tree carries bytes nobody verified and the
    /// run must stop instead of building on them.
    pub fn unreverted(reason: impl Into<String>) -> Self {
        Self {
            kind: StepFailureKind::Unreverted,
            reason: reason.into(),
            failed_step: None,
        }
    }

    /// Attributes the failure to the step whose execution failed.
    ///
    /// A step identity is the diagnostic the Architect needs in order to
    /// reproject the remaining horizon around the failed approach, so it is
    /// attached where the step actually failed instead of being reconstructed
    /// by the caller.
    pub fn with_step(mut self, step_id: impl Into<String>) -> Self {
        self.failed_step = Some(step_id.into());
        self
    }

    /// The step whose execution failed, when the failure is attributable to one.
    pub fn failed_step(&self) -> Option<&str> {
        self.failed_step.as_deref()
    }

    /// Whether the failure is confined to the current step.
    pub fn is_domain(&self) -> bool {
        matches!(self.kind, StepFailureKind::Domain)
    }

    /// Whether the failure is a working tree the failed attempt never reverted.
    ///
    /// The predicate is mutually exclusive with [`Self::is_domain`]: a dirty
    /// tree is a transaction failure of the harness, never a content verdict of
    /// the step.
    pub fn is_unreverted(&self) -> bool {
        matches!(self.kind, StepFailureKind::Unreverted)
    }

    /// Human-readable diagnostic carried by the failure.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Consumes the failure and returns its diagnostic.
    pub fn into_reason(self) -> String {
        self.reason
    }
}

/// One bounded step turn requested by the application workflow.
///
/// The request is a core-internal DTO, never a wire type: it carries exactly
/// what the host needs in order to run one turn of a horizon leaf. Application
/// preparation owns context preloading and prompt assembly; the host publishes
/// snapshots and executes the prepared turn.
pub struct StepTurnRequest {
    /// The scope whose horizon the turn executes.
    pub scope: ExecutionScope,
    /// Title of the horizon the step belongs to.
    pub title: String,
    /// Leaves of the run in execution order, with the running position.
    pub queue: TurnWorkQueue,
    /// Prepared prompt of the step.
    pub prompt: String,
    /// Files the step modifies; the host loads them as editable context.
    pub files: Vec<String>,
    /// Files the step reads; the host loads them as read-only context.
    pub readonly_files: Vec<String>,
    /// Coder model the attempt runs on.
    pub selection: ModelSelection,
    /// Reasoning depth of the attempt.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Additional instructions of the run, forwarded to every step turn.
    pub instructions: Option<String>,
    /// Turn marker bound to the tagged control channel.
    pub marker: Option<String>,
    /// Provider session identifier shared by every step of the horizon.
    pub session_id: Option<String>,
    /// Cancellation signal of the running batch.
    pub cancel_rx: watch::Receiver<bool>,
}

/// Terminal outcome of one bounded step turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepTurnResult {
    /// The turn applied verified changes and satisfied the success contract.
    Success {
        /// Summary of the completed work, classified by core.
        summary: String,
        /// Identifier of the Git commit the successful turn produced.
        ///
        /// A verified change set can live entirely outside Git — the
        /// `.tauqe/mockups/` sandbox is the documented case — so the pointer is
        /// optional and the step records it only when its turn produced one.
        commit: Option<String>,
    },
    /// The step cannot be implemented within the approved decision.
    Blocked(String),
    /// The host environment must be repaired by the developer first.
    Abort(String),
    /// The turn observed a cancellation request.
    Cancelled,
    /// The turn failed with a classified failure.
    Failed(StepFailure),
}

/// Terminal reason an implementation run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanImplementationStop {
    /// Every executable leaf of the horizon finished.
    Completed,
    /// A failure the pipeline cannot repair by reprojecting the horizon.
    Failed {
        /// The step the failure is attributed to, when it is attributable.
        step: Option<String>,
        /// Diagnostic of the failure.
        reason: String,
    },
    /// A step cannot proceed until the developer resolves the stated blocker.
    Blocked {
        /// The step the blocker is attributed to, when it is attributable.
        step: Option<String>,
        /// Diagnostic of the blocker.
        reason: String,
    },
    /// The run observed a cancellation request.
    Cancelled,
    /// The run observed a soft-stop request and drained the running step.
    Stopped,
}

/// A durable transition requested by the step executor from the workflow owner.
pub struct LeafTransition {
    pub scope: ExecutionScope,
    pub step_id: String,
    pub status: PlanItemStatus,
    pub blocked_reason: Option<String>,
    pub commit: Option<String>,
    /// The group's current horizon for its whole-horizon storage operation.
    pub group_items: Vec<PlanItem>,
}

/// Data returned after a successful durable transition.
pub enum LeafTransitionResult {
    Horizon(PlanImplementation),
    Finding,
}

/// One Architect refinement wave requested by the application workflow.
///
/// A wave reprojects the remaining horizon around a failed approach, so the
/// request carries the differential the Architect needs: which step failed and
/// what it reported, which Architect configuration runs the wave, and which
/// additional instructions the developer stated.
pub struct RefinementRequest {
    /// The scope whose horizon is reprojected.
    pub scope: ExecutionScope,
    /// Identifier of the step whose approach failed, when one is attributed.
    pub failed_step: Option<String>,
    /// Diagnostic the failed step reported.
    pub failure: Option<String>,
    /// Architect configuration the wave runs on.
    pub selection: Option<ModelSelection>,
    /// Reasoning depth of the wave.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Additional instructions the developer stated for the wave.
    pub instructions: Option<String>,
}

/// Outcome of one Architect refinement wave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefinementResult {
    /// The Architect synthesized the remaining horizon.
    Synthesized {
        /// Steps of the new horizon, appended after the immutable foundation.
        items: Vec<PlanItem>,
        /// Architect configuration that produced the horizon.
        tier: ArchitectTier,
    },
    /// The Architect reported that the horizon cannot be reprojected.
    Blocked {
        /// Diagnostic of the blocked refinement.
        reason: String,
        /// Architect configuration that reported the block.
        tier: ArchitectTier,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreverted_failure_keeps_its_kind_and_reason() {
        let failure = StepFailure::unreverted("working tree is dirty");

        assert!(failure.is_unreverted());
        assert!(!failure.is_domain());
        assert_eq!(failure.reason(), "working tree is dirty");
        assert_eq!(failure.failed_step(), None);
    }

    #[test]
    fn domain_failure_is_never_an_unreverted_failure() {
        let failure = StepFailure::domain("compiler errors persisted");

        assert!(failure.is_domain());
        assert!(!failure.is_unreverted());
    }

    #[test]
    fn step_attribution_survives_into_the_diagnostic() {
        let failure = StepFailure::domain("blocked").with_step("1.2");

        assert_eq!(failure.failed_step(), Some("1.2"));
        assert_eq!(failure.into_reason(), "blocked");
    }
}
