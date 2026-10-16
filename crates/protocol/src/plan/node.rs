use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::item::{ConceptItem, PlanItem, PlanItemStatus};

/// Structured validation errors representing plan integrity violations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanValidationError {
    EmptyItemId,
    DuplicateItemId(String),
    DuplicateConceptId(String),
    MissingDependency { item_id: String, depends_on: String },
    SelfDependency(String),
    CyclicDependency(Vec<String>),
    MissingConceptRef { item_id: String, concept_ref: String },
}

impl fmt::Display for PlanValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyItemId => write!(f, "Step ID cannot be empty"),
            Self::DuplicateItemId(id) => write!(f, "Duplicate step ID: '{id}'"),
            Self::DuplicateConceptId(id) => write!(f, "Duplicate concept ID: '{id}'"),
            Self::MissingDependency { item_id, depends_on } => {
                write!(f, "Step '{item_id}' references non-existent dependency '{depends_on}'")
            }
            Self::SelfDependency(id) => {
                write!(f, "Step '{id}' cannot depend on itself")
            }
            Self::CyclicDependency(cycle) => {
                write!(f, "Cyclic dependency detected: {}", cycle.join(" -> "))
            }
            Self::MissingConceptRef { item_id, concept_ref } => {
                write!(f, "Step '{item_id}' references non-existent concept '{concept_ref}'")
            }
        }
    }
}

impl std::error::Error for PlanValidationError {}

/// Canonical, shared comparison key for every plan and concept identifier.
///
/// Validation, lookup, and scheduling must agree on identifier identity,
/// otherwise a plan can pass [`Plan::validate`] yet stay blocked forever on a
/// dependency it was supposed to satisfy. Trimming and ASCII-lowercasing here
/// collapses cosmetic spelling differences such as `"1"`, `"1 "`, and `" 1"`
/// into one key used everywhere.
pub(super) fn id_key(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

/// Two-section engineering plan: Concept Tree ("What") and Implementation Blueprint ("How").
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PlanItem>,
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

/// Recursively computes the aggregated status of a collection of plan items.
pub fn compute_items_status(items: &[PlanItem]) -> PlanItemStatus {
    if items.is_empty() {
        return PlanItemStatus::Todo;
    }
    let statuses: Vec<PlanItemStatus> = items
        .iter()
        .map(|item| {
            if item.is_leaf() {
                item.status
            } else {
                compute_items_status(&item.children)
            }
        })
        .collect();

    if statuses.contains(&PlanItemStatus::Discussion) {
        return PlanItemStatus::Discussion;
    }
    if statuses.contains(&PlanItemStatus::Blocked) {
        return PlanItemStatus::Blocked;
    }
    if statuses.contains(&PlanItemStatus::InProgress) {
        return PlanItemStatus::InProgress;
    }
    let has_done = statuses.contains(&PlanItemStatus::Done);
    let has_todo = statuses.contains(&PlanItemStatus::Todo);
    if has_done && has_todo {
        return PlanItemStatus::InProgress;
    }
    if statuses.iter().all(|s| *s == PlanItemStatus::Done) {
        return PlanItemStatus::Done;
    }
    if statuses.iter().all(|s| *s == PlanItemStatus::Cancelled) {
        return PlanItemStatus::Cancelled;
    }
    if statuses.iter().all(|s| *s == PlanItemStatus::Todo) {
        return PlanItemStatus::Todo;
    }
    let active: Vec<_> = statuses.iter().filter(|s| **s != PlanItemStatus::Cancelled).collect();
    if !active.is_empty() && active.iter().all(|s| **s == PlanItemStatus::Done) {
        return PlanItemStatus::Done;
    }
    if !active.is_empty() && active.iter().all(|s| **s == PlanItemStatus::Todo) {
        return PlanItemStatus::Todo;
    }
    PlanItemStatus::InProgress
}

/// Counts executable, non-cancelled leaf items and how many of them are `Done`.
///
/// This is the single authoritative leaf-progress metric shared by a full plan
/// ([`Plan::count_stats`]) and a review finding's embedded subplan
/// ([`crate::review::ReviewItem::subplan_stats`]). Only leaves carry an explicit,
/// authoritative status; a container's status is derived from its children and
/// may be stale in memory. Counting leaves never double-counts a container
/// alongside the children it aggregates, and `Cancelled` leaves are excluded
/// from `total` so a completed plan never reports a `completed/total` ratio
/// below one.
pub fn count_leaf_stats(items: &[PlanItem]) -> (usize, usize) {
    fn walk(items: &[PlanItem], total: &mut usize, completed: &mut usize) {
        for item in items {
            if item.is_leaf() {
                if item.status == PlanItemStatus::Cancelled {
                    continue;
                }
                *total += 1;
                if item.status == PlanItemStatus::Done {
                    *completed += 1;
                }
            } else {
                walk(&item.children, total, completed);
            }
        }
    }
    let mut total = 0;
    let mut completed = 0;
    walk(items, &mut total, &mut completed);
    (total, completed)
}

impl Plan {
    /// Computes the overall plan status deterministically based on its items.
    pub fn status(&self) -> PlanItemStatus {
        compute_items_status(&self.items)
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
            status: self.status(),
        }
    }

    pub fn stats(&self) -> PlanStats {
        let (total, completed) = self.count_stats();
        PlanStats { total, completed }
    }

    /// Counts executable, non-cancelled leaf items and how many of them are
    /// `Done`.
    ///
    /// Delegates to [`count_leaf_stats`], the single authoritative leaf-progress
    /// metric also used for a review finding's embedded subplan, so a plan and a
    /// subplan can never drift apart. See that function for the precise
    /// semantics.
    pub fn count_stats(&self) -> (usize, usize) {
        count_leaf_stats(&self.items)
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

    pub fn find_item(&self, item_id: &str) -> Option<&PlanItem> {
        fn walk<'a>(items: &'a [PlanItem], target: &str) -> Option<&'a PlanItem> {
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
        walk(&self.items, &id_key(item_id))
    }

    pub fn is_leaf_item(&self, item_id: &str) -> Option<bool> {
        self.find_item(item_id).map(|item| item.is_leaf())
    }

    /// Recomputes the derived status of every container item from its children.
    ///
    /// Only leaves carry an authoritative status; a container's status is a pure
    /// function of its descendants (see [`compute_items_status`]). This normalizer
    /// is the single place that materializes that derived value onto containers,
    /// so a caller that mutates a leaf refreshes the tree through it instead of
    /// hand-rolling aggregation. It performs no I/O and is allocation-only, which
    /// is why it legitimately lives in `protocol` (see `Conventions.md` §2).
    pub fn refresh_parent_statuses(&mut self) {
        fn walk(items: &mut [PlanItem]) {
            for item in items.iter_mut() {
                if !item.children.is_empty() {
                    walk(&mut item.children);
                    item.status = compute_items_status(&item.children);
                }
            }
        }
        walk(&mut self.items);
    }

    /// Enforces the leaf-only file invariant across the plan.
    ///
    /// Parent items act as conceptual milestones, phases, and task groups.
    /// They are never executed autonomously, so any `files`, `readonly_files`,
    /// or `depends_on` list attached to a non-leaf item is cleared. Only leaf
    /// items (`is_leaf()`) define target context files and DAG dependencies.
    pub fn normalize_leaf_files(&mut self) {
        fn walk(items: &mut [PlanItem]) {
            for item in items.iter_mut() {
                if item.children.is_empty() {
                    if !item.readonly_files.is_empty() && !item.files.is_empty() {
                        item.readonly_files.retain(|r| !item.files.contains(r));
                    }
                    continue;
                }
                item.files.clear();
                item.readonly_files.clear();
                item.depends_on.clear();
                walk(&mut item.children);
            }
        }
        walk(&mut self.items);
    }

    /// Updates the status of a plan item and refreshes the aggregated status of
    /// its ancestors.
    ///
    /// Only executable leaf items hold an explicit status. A non-leaf item is a
    /// conceptual container whose status is derived deterministically from its
    /// children by [`Self::refresh_parent_statuses`], so assigning a status to a
    /// container would be recomputed away immediately. Such a container target is
    /// therefore rejected. The method returns `true` only when a leaf item matched
    /// and its status was durably changed; it returns `false` both for an unknown
    /// identifier and for a container target, so a caller never mistakes a
    /// discarded assignment for a successful update.
    ///
    /// The `tui` crate uses this helper for optimistic status updates and their
    /// rollback, so it is part of the sanctioned, I/O-free `protocol` surface
    /// documented in `Conventions.md` §2.
    pub fn update_item_status(&mut self, item_id: &str, status: PlanItemStatus) -> bool {
        // Locates the target and classifies it: `Some(true)` for an assignable
        // leaf, `Some(false)` for a non-assignable container, `None` when absent.
        fn walk(items: &mut [PlanItem], target: &str, status: PlanItemStatus) -> Option<bool> {
            for item in items {
                if id_key(&item.id) == target {
                    if item.is_leaf() {
                        item.status = status;
                        return Some(true);
                    }
                    return Some(false);
                }
                if let Some(updated) = walk(&mut item.children, target, status) {
                    return Some(updated);
                }
            }
            None
        }

        if walk(&mut self.items, &id_key(item_id), status) == Some(true) {
            self.refresh_parent_statuses();
            true
        } else {
            false
        }
    }

    /// Validates the structural integrity and dependency graph of the plan.
    ///
    /// Checks:
    /// - Non-empty and unique step IDs (case-insensitive).
    /// - Non-empty and unique concept IDs (case-insensitive).
    /// - Existence of referenced concept IDs in `item.concept_refs`.
    /// - Existence of referenced dependencies in `item.depends_on`.
    /// - No self-dependencies (`item` depends on itself).
    /// - Acyclicity of the dependency graph (no circular `depends_on` chains).
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
                if !key.is_empty() && !seen.insert(key) {
                    errors.push(PlanValidationError::DuplicateConceptId(c.id.clone()));
                }
                walk_concepts(&c.children, seen, errors);
            }
        }
        walk_concepts(&self.concept, &mut seen_concept_ids, &mut errors);

        // 2. Validate plan item IDs
        let mut seen_item_ids = HashSet::new();
        let mut id_display = HashMap::new();
        fn walk_items(
            items: &[PlanItem],
            seen: &mut HashSet<String>,
            id_display: &mut HashMap<String, String>,
            errors: &mut Vec<PlanValidationError>,
        ) {
            for item in items {
                let key = id_key(&item.id);
                if key.is_empty() {
                    errors.push(PlanValidationError::EmptyItemId);
                } else if !seen.insert(key.clone()) {
                    errors.push(PlanValidationError::DuplicateItemId(item.id.clone()));
                } else {
                    id_display.insert(key, item.id.clone());
                }
                walk_items(&item.children, seen, id_display, errors);
            }
        }
        walk_items(&self.items, &mut seen_item_ids, &mut id_display, &mut errors);

        // 3. Validate concept_refs, depends_on existence, and self-dependency
        let mut dep_graph: HashMap<String, Vec<String>> = HashMap::new();
        fn walk_refs(
            items: &[PlanItem],
            seen_concepts: &HashSet<String>,
            seen_items: &HashSet<String>,
            dep_graph: &mut HashMap<String, Vec<String>>,
            errors: &mut Vec<PlanValidationError>,
        ) {
            for item in items {
                let item_key = id_key(&item.id);

                // Validate concept_refs
                for c_ref in &item.concept_refs {
                    let c_key = id_key(c_ref);
                    if !seen_concepts.contains(&c_key) {
                        errors.push(PlanValidationError::MissingConceptRef {
                            item_id: item.id.clone(),
                            concept_ref: c_ref.clone(),
                        });
                    }
                }

                // Validate depends_on
                let mut deps_for_item = Vec::new();
                for dep in &item.depends_on {
                    let dep_key = id_key(dep);
                    if dep_key.is_empty() {
                        continue;
                    }
                    if dep_key == item_key {
                        errors.push(PlanValidationError::SelfDependency(item.id.clone()));
                    } else if !seen_items.contains(&dep_key) {
                        errors.push(PlanValidationError::MissingDependency {
                            item_id: item.id.clone(),
                            depends_on: dep.clone(),
                        });
                    } else {
                        deps_for_item.push(dep_key);
                    }
                }

                if !item_key.is_empty() {
                    // A container becomes `Done` exactly when all of its
                    // descendant leaves are `Done`, so it implicitly depends on
                    // them. Recording those edges lets the cycle detector expose
                    // a leaf that depends on its own ancestor: the ancestor can
                    // never finish without the leaf, and the leaf can never start
                    // without the ancestor. Only the derived status of a
                    // container is meaningful, so the implicit edges are added
                    // for non-leaf items only.
                    if !item.is_leaf() {
                        collect_descendant_leaf_keys(item, &mut deps_for_item);
                    }
                    dep_graph.insert(item_key, deps_for_item);
                }

                walk_refs(&item.children, seen_concepts, seen_items, dep_graph, errors);
            }
        }
        walk_refs(&self.items, &seen_concept_ids, &seen_item_ids, &mut dep_graph, &mut errors);

        // 4. Validate acyclicity of depends_on
        if let Some(cycle) = detect_dependency_cycle(&dep_graph, &id_display) {
            errors.push(PlanValidationError::CyclicDependency(cycle));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Collects the normalized keys of every leaf reachable beneath `item`.
///
/// A container's completion is defined by its descendant leaves, so these keys
/// are the implicit dependencies that connect a container to the leaves whose
/// `depends_on` may point back at it.
fn collect_descendant_leaf_keys(item: &PlanItem, out: &mut Vec<String>) {
    for child in &item.children {
        if child.is_leaf() {
            let key = id_key(&child.id);
            if !key.is_empty() {
                out.push(key);
            }
        } else {
            collect_descendant_leaf_keys(child, out);
        }
    }
}

/// Detects a dependency cycle in a directed adjacency graph.
///
/// Uses an explicit work stack instead of recursion so that a flat list with a
/// very long `depends_on` chain cannot overflow the call stack. The traversal
/// order, node marks, and reported cycle path are identical to a recursive
/// depth-first search.
fn detect_dependency_cycle(
    graph: &HashMap<String, Vec<String>>,
    id_display: &HashMap<String, String>,
) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        Visiting,
        Visited,
    }

    let mut marks: HashMap<String, Mark> = HashMap::new();
    // The active DFS chain from the root to the node currently being explored.
    let mut path: Vec<String> = Vec::new();

    let mut keys: Vec<&String> = graph.keys().collect();
    keys.sort();
    for key in keys {
        if marks.contains_key(key) {
            continue;
        }
        marks.insert(key.clone(), Mark::Visiting);
        path.push(key.clone());
        // Each frame records a node and the index of its next unvisited
        // neighbor, mirroring the same position a recursive call would resume
        // from after returning.
        let mut stack: Vec<(String, usize)> = vec![(key.clone(), 0)];

        while let Some((node, index)) = stack.pop() {
            let next_neighbor = graph
                .get(&node)
                .and_then(|neighbors| neighbors.get(index))
                .cloned();
            if let Some(neighbor) = next_neighbor {
                // Requeue this node to resume at the following neighbor.
                stack.push((node.clone(), index + 1));
                match marks.get(&neighbor) {
                    Some(Mark::Visiting) => {
                        let cycle_start =
                            path.iter().position(|n| n == &neighbor).unwrap_or(0);
                        let mut cycle: Vec<String> = path[cycle_start..]
                            .iter()
                            .map(|k| id_display.get(k).cloned().unwrap_or_else(|| k.clone()))
                            .collect();
                        cycle.push(
                            id_display
                                .get(&neighbor)
                                .cloned()
                                .unwrap_or_else(|| neighbor.clone()),
                        );
                        return Some(cycle);
                    }
                    Some(Mark::Visited) => {}
                    None => {
                        marks.insert(neighbor.clone(), Mark::Visiting);
                        path.push(neighbor.clone());
                        stack.push((neighbor, 0));
                    }
                }
            } else {
                marks.insert(node, Mark::Visited);
                path.pop();
            }
        }
    }
    None
}
