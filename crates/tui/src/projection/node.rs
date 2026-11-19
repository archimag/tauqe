//! Client-side plan lookup, summary, and validation.
#[cfg(test)]
use super::implementation::id_key;
#[cfg(test)]
use super::PlanImplementationProjection;
#[cfg(test)]
use std::collections::HashSet;
use std::fmt;
#[cfg(test)]
use tauqe_protocol::{ConceptItem, Plan, PlanItem, PlanSummary};
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[cfg(test)]
pub(crate) trait PlanProjection {
    fn summary(&self) -> PlanSummary;
    fn find_concept_item(&self, item_id: &str) -> Option<&ConceptItem>;
    #[cfg(test)]
    fn find_concept_item_mut(&mut self, item_id: &str) -> Option<&mut ConceptItem>;
    fn validate(&self) -> Result<(), Vec<PlanValidationError>>;
}
#[cfg(test)]
impl PlanProjection for Plan {
    fn summary(&self) -> PlanSummary {
        let (total, completed) = self.implementation.count_stats();
        PlanSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            total_items: total,
            completed_items: completed,
            status: self.implementation.status(),
        }
    }
    fn find_concept_item(&self, item_id: &str) -> Option<&ConceptItem> {
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
    #[cfg(test)]
    fn find_concept_item_mut(&mut self, item_id: &str) -> Option<&mut ConceptItem> {
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
    fn validate(&self) -> Result<(), Vec<PlanValidationError>> {
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
