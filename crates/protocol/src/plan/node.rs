use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::ArchitectTier;

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
    /// its step starts from a warm context instead of a cold discovery round.
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

/// Canonical comparison key for a leaf file path.
///
/// `files` and `readonly_files` hold user-authored path strings that may differ
/// only by cosmetic spelling (`./src/x.rs`, `src//x.rs`, `src\x.rs`). Collapsing
/// them through the crate's shared [`crate::edit::normalize_repo_path`] routine
/// lets `normalize_leaf_files` treat those spellings as the same file, so a path
/// listed as both editable and reference is resolved consistently. When a path
/// cannot be normalized (it is absolute, traversing, or empty) the trimmed raw
/// string is kept as the key, so the comparison stays total and never drops a
/// badge through a failed normalization.
fn path_key(raw: &str) -> String {
    crate::edit::normalize_repo_path(raw).unwrap_or_else(|_| raw.trim().to_string())
}

/// Reports whether a leaf with this status must declare its target files.
///
/// A `Discussion` leaf is still a draft whose files are premature, a `Done`
/// leaf has already been executed (a legacy plan without files keeps saving
/// unchanged), and a `Cancelled` leaf is never executed at all. Every other
/// status is executable work that must name the context it targets.
fn requires_target_files(status: PlanItemStatus) -> bool {
    !matches!(
        status,
        PlanItemStatus::Discussion | PlanItemStatus::Done | PlanItemStatus::Cancelled
    )
}

/// Reports whether a leaf declares at least one usable target file.
///
/// A blank entry names nothing, so the declaration is satisfied only by an
/// entry with non-empty trimmed text in `files` or `readonly_files`. A step
/// that merely verifies behavior declares `readonly_files`, while a step that
/// creates a file declares its future path in `files`; either list satisfies
/// the requirement.
fn declares_target_files(item: &PlanItem) -> bool {
    item.files
        .iter()
        .chain(item.readonly_files.iter())
        .any(|path| !path.trim().is_empty())
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
    /// The Architect configuration that produced this plan's decomposition.
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

/// Single-pass accumulator over a set of statuses used to derive an aggregate
/// status without materializing an intermediate vector.
///
/// Each flag records whether at least one status of that kind was observed, and
/// `total` records how many statuses were folded in so an empty set can be
/// distinguished from an all-`Cancelled` one.
#[derive(Default)]
struct StatusSummary {
    total: usize,
    has_discussion: bool,
    has_blocked: bool,
    has_in_progress: bool,
    has_done: bool,
    has_todo: bool,
    has_cancelled: bool,
}

impl StatusSummary {
    fn push(&mut self, status: PlanItemStatus) {
        self.total += 1;
        match status {
            PlanItemStatus::Discussion => self.has_discussion = true,
            PlanItemStatus::Todo => self.has_todo = true,
            PlanItemStatus::InProgress => self.has_in_progress = true,
            PlanItemStatus::Done => self.has_done = true,
            PlanItemStatus::Blocked => self.has_blocked = true,
            PlanItemStatus::Cancelled => self.has_cancelled = true,
        }
    }

    /// Collapses the accumulated flags into a single aggregate status.
    ///
    /// The precedence preserves the historical aggregation contract: any
    /// `Discussion` dominates, then `Blocked`, then `InProgress`; a mix of
    /// `Done` and `Todo` is still `InProgress`; otherwise a set whose active
    /// (non-`Cancelled`) members are all `Done` is `Done`, a set whose active
    /// members are all `Todo` is `Todo`, an all-`Cancelled` set is `Cancelled`,
    /// and an empty set defaults to `Todo`.
    fn into_status(self) -> PlanItemStatus {
        if self.total == 0 {
            return PlanItemStatus::Todo;
        }
        if self.has_discussion {
            return PlanItemStatus::Discussion;
        }
        if self.has_blocked {
            return PlanItemStatus::Blocked;
        }
        if self.has_in_progress {
            return PlanItemStatus::InProgress;
        }
        if self.has_done && self.has_todo {
            return PlanItemStatus::InProgress;
        }
        if self.has_done {
            return PlanItemStatus::Done;
        }
        if self.has_todo {
            return PlanItemStatus::Todo;
        }
        if self.has_cancelled {
            return PlanItemStatus::Cancelled;
        }
        PlanItemStatus::InProgress
    }
}

/// Computes the aggregate status of a set of already-resolved statuses.
///
/// This is the allocation-free primitive shared by a container's status refresh
/// and the readiness scan. Callers pass whatever statuses they already hold —
/// leaf statuses, freshly derived child statuses, or recursively computed
/// subtree statuses — and receive the same aggregation that
/// [`compute_items_status`] produces for a tree, without building an
/// intermediate `Vec`.
pub(super) fn compute_direct_status<I>(statuses: I) -> PlanItemStatus
where
    I: IntoIterator<Item = PlanItemStatus>,
{
    let mut summary = StatusSummary::default();
    for status in statuses {
        summary.push(status);
    }
    summary.into_status()
}

/// Recursively computes the aggregated status of a collection of plan items.
pub fn compute_items_status(items: &[PlanItem]) -> PlanItemStatus {
    compute_direct_status(items.iter().map(|item| {
        if item.is_leaf() {
            item.status
        } else {
            compute_items_status(&item.children)
        }
    }))
}

/// Counts executable, non-cancelled leaf items and how many of them are `Done`.
///
/// This is the single authoritative leaf-progress metric of a plan horizon. Only
/// leaves carry an explicit,
/// authoritative status; a container's status is derived from its children and
/// may be stale in memory. Counting leaves never double-counts a container
/// alongside the children it aggregates, and `Cancelled` leaves are excluded
/// from `total` so a completed plan never reports a `completed/total` ratio
/// below one.
pub(crate) fn count_leaf_stats(items: &[PlanItem]) -> (usize, usize) {
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

    /// Reports whether implementation work has already started or finished.
    ///
    /// The client uses this to choose the confirmation strength when it clears
    /// the implementation horizon: a plan whose leaves are all pending has no
    /// progress recorded in Git commits yet, so a single confirmation suffices,
    /// while any leaf that is `Done` or `InProgress` may already be captured by
    /// a commit and therefore requires the strict two-tier confirmation.
    ///
    /// Only executable leaves are inspected. A container's status is derived
    /// from its descendants, so it carries no independent progress and its
    /// stored value is never trusted here; its children are still traversed.
    /// The method is pure: no I/O, no mutation, and no allocation beyond the
    /// recursion itself.
    pub fn has_started_or_completed_steps(&self) -> bool {
        fn walk(items: &[PlanItem]) -> bool {
            items.iter().any(|item| {
                if item.is_leaf() {
                    matches!(
                        item.status,
                        PlanItemStatus::Done | PlanItemStatus::InProgress
                    )
                } else {
                    walk(&item.children)
                }
            })
        }
        walk(&self.items)
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
                    item.status =
                        compute_direct_status(item.children.iter().map(|child| child.status));
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
                        let file_keys: HashSet<String> =
                            item.files.iter().map(|f| path_key(f)).collect();
                        item.readonly_files
                            .retain(|r| !file_keys.contains(&path_key(r)));
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
                if key.is_empty() {
                    errors.push(PlanValidationError::EmptyConceptId);
                } else if !seen.insert(key) {
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
                // An executable leaf must name the context it targets: the
                // preload and the context collection read these lists, so a
                // leaf without them starts its step from a cold discovery
                // round. A draft leaf settles its files in the dialogue, a
                // finished leaf already recorded its work, and a cancelled
                // leaf is never executed, so all three are exempt. A container
                // is never executed either and carries no list of its own (see
                // [`Plan::normalize_leaf_files`]).
                if item.is_leaf()
                    && requires_target_files(item.status)
                    && !declares_target_files(item)
                {
                    errors.push(PlanValidationError::MissingLeafFiles {
                        item_id: item.id.clone(),
                    });
                }
                walk_items(&item.children, seen, id_display, errors);
            }
        }
        walk_items(
            &self.items,
            &mut seen_item_ids,
            &mut id_display,
            &mut errors,
        );

        // 3. Validate concept_refs, depends_on existence, and self-dependency
        let mut dep_graph: HashMap<String, Vec<String>> = HashMap::new();
        // Validates the reference and dependency edges of a subtree and returns
        // the normalized keys of that subtree's leaves. A leaf reports its own
        // key; a container reports the union of its children's keys. Returning
        // them from the same recursion that validates the children means a
        // container learns the leaves it implicitly depends on without a second
        // descent over the subtree it just visited.
        fn walk_refs(
            items: &[PlanItem],
            seen_concepts: &HashSet<String>,
            seen_items: &HashSet<String>,
            dep_graph: &mut HashMap<String, Vec<String>>,
            errors: &mut Vec<PlanValidationError>,
        ) -> Vec<String> {
            let mut leaf_keys = Vec::new();
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

                let mut child_leaf_keys =
                    walk_refs(&item.children, seen_concepts, seen_items, dep_graph, errors);

                if !item_key.is_empty() {
                    // A container becomes `Done` once all of its non-cancelled
                    // descendant leaves are `Done` (see `StatusSummary`), so it
                    // implicitly depends on exactly those leaves. Recording those
                    // edges lets the cycle detector expose an active leaf that
                    // depends on its own ancestor: the ancestor can never finish
                    // without the leaf, and the leaf can never start without the
                    // ancestor. Only the derived status of a container is
                    // meaningful, so the implicit edges are added for non-leaf
                    // items only.
                    if !item.is_leaf() {
                        deps_for_item.extend(child_leaf_keys.iter().cloned());
                    }
                    dep_graph.insert(item_key.clone(), deps_for_item);
                }

                if item.is_leaf() {
                    // A `Cancelled` leaf never gates an ancestor's completion:
                    // the container becomes `Done` as soon as every
                    // non-cancelled descendant leaf is `Done`, so reporting a
                    // cancelled leaf here would fabricate an implicit
                    // container-to-leaf edge. A leftover `depends_on` naming that
                    // ancestor would then surface a deadlock that can never occur
                    // at runtime.
                    if !item_key.is_empty() && item.status != PlanItemStatus::Cancelled {
                        leaf_keys.push(item_key);
                    }
                } else {
                    leaf_keys.append(&mut child_leaf_keys);
                }
            }
            leaf_keys
        }
        walk_refs(
            &self.items,
            &seen_concept_ids,
            &seen_item_ids,
            &mut dep_graph,
            &mut errors,
        );

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
                        let cycle_start = path.iter().position(|n| n == &neighbor).unwrap_or(0);
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
