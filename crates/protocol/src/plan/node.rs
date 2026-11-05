use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::implementation::{id_key, PlanImplementation};
use super::item::{ConceptItem, PlanItem, PlanItemStatus};

/// Structured validation errors representing plan integrity violations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanValidationError {
    EmptyItemId,
    EmptyConceptId,
    DuplicateItemId(String),
    DuplicateConceptId(String),
    MissingDependency {
        item_id: String,
        depends_on: String,
    },
    SelfDependency(String),
    CyclicDependency(Vec<String>),
    MissingConceptRef {
        item_id: String,
        concept_ref: String,
    },
    /// An executable leaf declares no target file.
    ///
    /// A leaf that can still be executed must name the context it targets, so
    /// its step starts from a warm context instead of a cold research round.
    MissingLeafFiles {
        item_id: String,
    },
}

impl fmt::Display for PlanValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyItemId => write!(f, "Step ID cannot be empty"),
            Self::EmptyConceptId => write!(f, "Concept ID cannot be empty"),
            Self::DuplicateItemId(id) => write!(f, "Duplicate step ID: '{id}'"),
            Self::DuplicateConceptId(id) => write!(f, "Duplicate concept ID: '{id}'"),
            Self::MissingDependency {
                item_id,
                depends_on,
            } => {
                write!(
                    f,
                    "Step '{item_id}' references non-existent dependency '{depends_on}'"
                )
            }
            Self::SelfDependency(id) => {
                write!(f, "Step '{id}' cannot depend on itself")
            }
            Self::CyclicDependency(cycle) => {
                write!(f, "Cyclic dependency detected: {}", cycle.join(" -> "))
            }
            Self::MissingConceptRef {
                item_id,
                concept_ref,
            } => {
                write!(
                    f,
                    "Step '{item_id}' references non-existent concept '{concept_ref}'"
                )
            }
            Self::MissingLeafFiles { item_id } => {
                write!(
                    f,
                    "Step '{item_id}' must declare a target file in 'files' or 'readonly_files'"
                )
            }
        }
    }
}

impl std::error::Error for PlanValidationError {}

/// Two-section engineering plan: Concept Tree ("What") and Implementation Blueprint ("How").
///
/// The plan owns the document header and the concept tree, while the step graph
/// and the provenance of its decomposition live in a nested
/// [`PlanImplementation`]. The implementation is a plain field and never a
/// flattened set of keys, so the header and the graph stay distinct sections of
/// the wire form. The same nested shape is shared by every entity that owns an
/// implementation horizon — a local plan and an architectural review group — so
/// one algebra governs both.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanStats {
    pub total: usize,
    pub completed: usize,
}

impl Plan {
    /// Constructs a plan from its structural parts.
    ///
    /// This is the single construction point for a [`Plan`]. The step graph is
    /// collected into its own [`PlanImplementation`], and decomposition
    /// provenance (`architect`, `last_replanned_step`) starts empty, so the few
    /// sites that record it assign it after construction instead of repeating a
    /// field list that every structural change would have to edit.
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

    pub fn summary(&self) -> PlanSummary {
        let stats = self.stats();
        PlanSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            total_items: stats.total,
            completed_items: stats.completed,
            status: self.implementation.status(),
        }
    }

    /// Counts executable, non-cancelled leaf items and how many are `Done`.
    pub fn stats(&self) -> PlanStats {
        let (total, completed) = self.implementation.count_stats();
        PlanStats { total, completed }
    }

    pub fn find_concept_item(&self, item_id: &str) -> Option<&ConceptItem> {
        fn walk<'a>(items: &'a [ConceptItem], target: &str) -> Option<&'a ConceptItem> {
            for item in items {
                if id_key(&item.id) == target {
                    return Some(item);
                }
                if let Some(found) = walk(&item.children, target) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.concept, &id_key(item_id))
    }

    pub fn find_concept_item_mut(&mut self, item_id: &str) -> Option<&mut ConceptItem> {
        fn walk<'a>(items: &'a mut [ConceptItem], target: &str) -> Option<&'a mut ConceptItem> {
            for item in items {
                if id_key(&item.id) == target {
                    return Some(item);
                }
                if let Some(found) = walk(&mut item.children, target) {
                    return Some(found);
                }
            }
            None
        }
        walk(&mut self.concept, &id_key(item_id))
    }

    /// Validates the structural integrity of the plan.
    ///
    /// The concept tree and the step graph are validated by their respective
    /// owners: this method checks the concept identifiers, resolves every
    /// `concept_refs` entry against that tree, and appends the structural
    /// report of [`PlanImplementation::validate_structure`], so a caller
    /// receives one composed list of errors.
    pub fn validate(&self) -> Result<(), Vec<PlanValidationError>> {
        let mut errors = Vec::new();

        // 1. Validate concept tree IDs
        let mut seen_concept_ids = HashSet::new();
        fn walk_concepts(
            concepts: &[ConceptItem],
            seen: &mut HashSet<String>,
            errors: &mut Vec<PlanValidationError>,
        ) {
            for c in concepts {
                let key = id_key(&c.id);
                if key.is_empty() {
                    errors.push(PlanValidationError::EmptyConceptId);
                } else if !seen.insert(key) {
                    errors.push(PlanValidationError::DuplicateConceptId(c.id.clone()));
                }
                walk_concepts(&c.children, seen, errors);
            }
        }
        walk_concepts(&self.concept, &mut seen_concept_ids, &mut errors);

        // 2. Validate that every concept reference resolves
        fn walk_concept_refs(
            items: &[PlanItem],
            seen_concepts: &HashSet<String>,
            errors: &mut Vec<PlanValidationError>,
        ) {
            for item in items {
                for c_ref in &item.concept_refs {
                    if !seen_concepts.contains(&id_key(c_ref)) {
                        errors.push(PlanValidationError::MissingConceptRef {
                            item_id: item.id.clone(),
                            concept_ref: c_ref.clone(),
                        });
                    }
                }
                walk_concept_refs(&item.children, seen_concepts, errors);
            }
        }
        walk_concept_refs(&self.implementation.items, &seen_concept_ids, &mut errors);

        // 3. Delegate the step graph to its owner
        if let Err(mut structural) = self.implementation.validate_structure() {
            errors.append(&mut structural);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
