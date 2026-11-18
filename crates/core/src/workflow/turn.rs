//! Headless turn identity, queue coordinates, and execution phases.

/// Execution context of a model turn driven by a plan step.
///
/// Carries the owning plan identifier, the currently executing step, and the
/// compact ordered list of step identifiers that form the batch scope.
/// These coordinates are resolved before transport projection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelTurnPlanContext {
    pub plan_id: String,
    pub step_id: String,
    pub batch_step_ids: Vec<String>,
}

/// One member of the ordered queue a batch turn advances through.
///
/// A queue is described by data, not by the interface: a plan step lists the
/// steps it executes, a review group step lists the sub-steps of the group
/// implementation, and a review step lists the findings of the session. `id` is
/// identifier the owning subsystem already uses — a plan or sub-step
/// identifier, or a finding number rendered as text. The queue preserves the
/// execution order across work kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnWorkItem {
    pub id: String,
    pub title: String,
}

/// Ordered queue of work a batch turn advances through, addressed by position.
///
/// `active_index` addresses `items`: the member the turn currently executes.
/// [`Self::active`] performs the lookup, so a turn whose index falls outside
/// the list reports no active member instead of forcing every client to
/// bounds-check the pair itself. An empty queue means the turn is not a member
/// of a batch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnWorkQueue {
    pub items: Vec<TurnWorkItem>,
    pub active_index: usize,
}

impl TurnWorkQueue {
    /// The member the turn currently executes, or `None` outside the list.
    pub fn active(&self) -> Option<&TurnWorkItem> {
        self.items.get(self.active_index)
    }
}

/// Lifecycle phase of a review run that executes no plan step of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewWorkPhase {
    /// The audit that produces the findings of a session or one of its groups.
    Audit,
    /// The just-in-time decomposition of an architectural group into its implementation horizon.
    Decomposition,
    /// The cross-group classification of the produced findings.
    PostAnalysis,
}

/// Work the model executes in a turn, declared by whoever starts the turn.
///
/// A turn never infers its own task: the harness states it here, so the
/// interface renders the running work — its kind, the entity that owns it, the
/// active member, and its position in the batch queue — from typed fields
/// instead of reconstructing them from the stored plans, reviews, and group
/// lists. The variant is the work kind, so an interactive answer can never be
/// mistaken for a plan step and no kind is decoded from a title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnWorkContext {
    /// Interactive dialogue turn that answers the developer directly.
    Interactive,
    /// One step of a local plan.
    PlanStep {
        plan_id: String,
        plan_title: String,
        queue: TurnWorkQueue,
    },
    /// One sub-step of an architectural review group implementation.
    ReviewGroupStep {
        review_id: String,
        review_title: String,
        group_id: String,
        group_title: String,
        queue: TurnWorkQueue,
    },
    /// One finding of a review session executed on its own.
    ReviewStep {
        review_id: String,
        review_title: String,
        queue: TurnWorkQueue,
    },
    /// A review lifecycle phase that executes no plan step.
    ReviewPhase {
        review_id: String,
        review_title: String,
        phase: ReviewWorkPhase,
        queue: TurnWorkQueue,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnPhase {
    Research,
    Proposal,
    Staging,
    Verification,
    Healing,
}

impl std::fmt::Display for TurnPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnPhase::Research => f.write_str("research"),
            TurnPhase::Proposal => f.write_str("proposal"),
            TurnPhase::Staging => f.write_str("staging"),
            TurnPhase::Verification => f.write_str("verification"),
            TurnPhase::Healing => f.write_str("healing"),
        }
    }
}
