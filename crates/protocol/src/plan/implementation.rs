//! Client-visible implementation document.
use crate::{ArchitectTier, PlanItem};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanImplementation {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PlanItem>,
    /// The Architect configuration that produced this horizon.
    ///
    /// Recording the pair makes a later refinement reproducible with the same
    /// Architect configuration instead of re-deriving one, and lets the harness
    /// escalate along the architect ladder when a wave must be reprojected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architect: Option<ArchitectTier>,
    /// Identifier of the step whose horizon was most recently reprojected.
    ///
    /// The harness persists it with every horizon replacement so the decision
    /// whether a repeated failure deserves an Architect escalation is derived
    /// from disk state rather than from in-memory supervisor state, and manual
    /// replans dispatched through the UI or RPC are accounted for identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replanned_step: Option<String>,
}
