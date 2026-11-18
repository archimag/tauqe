//! Plan document and summary RPC DTOs.
use crate::{ConceptItem, PlanImplementation, PlanItem, PlanItemStatus};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Plan {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub concept: Vec<ConceptItem>,
    /// Execution graph and decomposition provenance of the blueprint.
    ///
    /// The field deliberately carries neither `serde(flatten)` nor
    /// `serde(default)`: the nested form is the only accepted shape, so a
    /// document written in the former flat layout fails to decode instead of
    /// being silently read as a plan without steps.
    pub implementation: PlanImplementation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub total_items: usize,
    pub completed_items: usize,
    #[serde(default)]
    pub status: PlanItemStatus,
}

impl Plan {
    pub fn from_parts(
        id: String,
        title: String,
        description: Option<String>,
        created_at: u64,
        updated_at: u64,
        concept: Vec<ConceptItem>,
        items: Vec<PlanItem>,
    ) -> Self {
        Self {
            id,
            title,
            description,
            created_at,
            updated_at,
            concept,
            implementation: PlanImplementation {
                items,
                ..PlanImplementation::default()
            },
        }
    }
}
