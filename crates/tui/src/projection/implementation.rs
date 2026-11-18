//! Client-side plan graph projections; durable rules remain in core.
use std::collections::{HashMap, HashSet};

use super::PlanItemProjection;
use tauqe_protocol::PlanImplementation;

use super::node::PlanValidationError;
use tauqe_protocol::{PlanItem, PlanItemStatus};

/// Canonical, shared comparison key for every plan and concept identifier.
///
/// Validation, lookup, and scheduling must agree on identifier identity,
/// otherwise an implementation can pass validation yet stay blocked forever on
/// a dependency it was supposed to satisfy. Trimming and ASCII-lowercasing here
/// collapses cosmetic spelling differences such as `"1"`, `"1 "`, and `" 1"`
/// into one key used everywhere.
pub(super) fn id_key(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

/// Canonical comparison key for a leaf file path.
///
/// `files` and `readonly_files` hold user-authored path strings that may differ
/// only by cosmetic spelling (`./src/x.rs`, `src//x.rs`, `src\\x.rs`). Collapsing
/// them through the crate's shared [`tauqe_protocol::edit::normalize_repo_path`] routine
/// lets [`PlanImplementationProjection::normalize_leaf_files`] treat those spellings as
/// the same file, so a path listed as both editable and reference is resolved
/// consistently. When a path cannot be normalized (it is absolute, traversing,
/// or empty) the trimmed raw string is kept as the key, so the comparison stays
/// total and never drops a badge through a failed normalization.
fn path_key(raw: &str) -> String {
    tauqe_protocol::edit::normalize_repo_path(raw).unwrap_or_else(|_| raw.trim().to_string())
}

/// Reports whether a leaf with this status must declare its target files.
///
/// A `Discussion` leaf is still a draft whose files are premature, a `Done`
/// leaf has already been executed (a horizon recorded without files keeps
/// saving unchanged), and a `Cancelled` leaf is never executed at all. Every
/// other status is executable work that must name the context it targets.
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
/// This is the allocation-free primitive shared by the container status refresh
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
/// This is the single authoritative leaf-progress metric of a horizon. Only
/// leaves carry an explicit, authoritative status; a container's status is
/// derived from its children and may be stale in memory. Counting leaves never
/// double-counts a container alongside the children it aggregates, and
/// `Cancelled` leaves are excluded from `total` so a completed horizon never
/// reports a `completed/total` ratio below one.
fn count_leaf_stats(items: &[PlanItem]) -> (usize, usize) {
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

/// Evaluation result of a targeted plan execution scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanScopeEvaluation {
    Ready(Vec<PlanItem>),
    BlockedByDiscussion(Vec<String>),
    /// One or more leaves in the evaluated scope are `Blocked` and cannot run
    /// until their reported blockers are resolved.
    BlockedByBlockers(Vec<String>),
    BlockedByDependencies(Vec<String>),
    StepBlocked,
    AllCompleted,
    Cancelled,
    NotFound,
}

/// Collects DISCUSSION markers, executable candidate leaves, and blocked leaves
/// from an implementation subtree.
///
/// Only executable leaf items are inspected. A container's status is derived
/// from its children, so reporting the container too would list a `Discussion`
/// (or blocked) child twice and could depend on a stale in-memory container
/// status.
///
/// A candidate leaf is an executable step whose status is `Todo` or
/// `InProgress`. A blocked leaf is a step whose status is `Blocked`; it is
/// surfaced separately instead of being silently skipped, so a stalled scope is
/// never mistaken for a finished one.
fn walk_candidate_leaves(
    items: &[PlanItem],
    discussion: &mut Vec<String>,
    blocked: &mut Vec<String>,
    candidates: &mut Vec<PlanItem>,
) {
    for item in items {
        if item.is_leaf() {
            match item.status {
                PlanItemStatus::Discussion => {
                    discussion.push(format!("#{} {}", item.id, item.title))
                }
                PlanItemStatus::Todo | PlanItemStatus::InProgress => candidates.push(item.clone()),
                PlanItemStatus::Blocked => blocked.push(format!("#{} {}", item.id, item.title)),
                _ => {}
            }
        } else {
            walk_candidate_leaves(&item.children, discussion, blocked, candidates);
        }
    }
}

/// Client-side graph evaluation and optimistic derived state over wire documents.
pub trait PlanImplementationProjection {
    fn status(&self) -> PlanItemStatus;
    fn count_stats(&self) -> (usize, usize);
    fn has_started_or_completed_steps(&self) -> bool;
    fn find_item(&self, item_id: &str) -> Option<&PlanItem>;
    #[cfg(test)]
    fn is_leaf_item(&self, item_id: &str) -> Option<bool>;
    fn refresh_parent_statuses(&mut self);
    fn normalize_leaf_files(&mut self);
    fn update_item_status(&mut self, item_id: &str, status: PlanItemStatus) -> bool;
    fn evaluate_scope(&self, scope_id: Option<&str>) -> PlanScopeEvaluation;
    fn resolve_scope_candidates(&self, candidates: Vec<PlanItem>) -> PlanScopeEvaluation;
    fn resolve_dependency_readiness(
        &self,
        candidates: Vec<PlanItem>,
    ) -> (Vec<PlanItem>, Vec<String>);
    fn validate_structure(&self) -> Result<(), Vec<PlanValidationError>>;
}
impl PlanImplementationProjection for PlanImplementation {
    fn status(&self) -> PlanItemStatus {
        compute_items_status(&self.items)
    }
    fn count_stats(&self) -> (usize, usize) {
        count_leaf_stats(&self.items)
    }
    fn has_started_or_completed_steps(&self) -> bool {
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
    fn find_item(&self, item_id: &str) -> Option<&PlanItem> {
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
    #[cfg(test)]
    fn is_leaf_item(&self, item_id: &str) -> Option<bool> {
        self.find_item(item_id).map(|item| item.is_leaf())
    }
    fn refresh_parent_statuses(&mut self) {
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
    fn normalize_leaf_files(&mut self) {
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
                item.outer_docs.clear();
                item.depends_on.clear();
                walk(&mut item.children);
            }
        }
        walk(&mut self.items);
    }
    fn update_item_status(&mut self, item_id: &str, status: PlanItemStatus) -> bool {
        // Locates the target and classifies it: `Some(true)` for a leaf whose
        // status actually changes, `Some(false)` for a non-assignable container
        // or an already-satisfied leaf, `None` when absent.
        fn walk(items: &mut [PlanItem], target: &str, status: PlanItemStatus) -> Option<bool> {
            for item in items {
                if id_key(&item.id) == target {
                    if item.is_leaf() && item.status != status {
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
    fn evaluate_scope(&self, scope_id: Option<&str>) -> PlanScopeEvaluation {
        match scope_id {
            None => {
                let mut discussion = Vec::new();
                let mut blocked = Vec::new();
                let mut candidates = Vec::new();
                walk_candidate_leaves(&self.items, &mut discussion, &mut blocked, &mut candidates);

                if !discussion.is_empty() {
                    return PlanScopeEvaluation::BlockedByDiscussion(discussion);
                }
                if !blocked.is_empty() {
                    return PlanScopeEvaluation::BlockedByBlockers(blocked);
                }
                self.resolve_scope_candidates(candidates)
            }
            Some(id) => {
                let target = match self.find_item(id) {
                    Some(it) => it,
                    None => return PlanScopeEvaluation::NotFound,
                };

                if target.is_leaf() {
                    match target.status {
                        PlanItemStatus::Discussion => PlanScopeEvaluation::BlockedByDiscussion(
                            vec![format!("#{} {}", target.id, target.title)],
                        ),
                        PlanItemStatus::Blocked => PlanScopeEvaluation::StepBlocked,
                        PlanItemStatus::Cancelled => PlanScopeEvaluation::Cancelled,
                        PlanItemStatus::Done => PlanScopeEvaluation::AllCompleted,
                        PlanItemStatus::Todo | PlanItemStatus::InProgress => {
                            self.resolve_scope_candidates(vec![target.clone()])
                        }
                    }
                } else {
                    let mut discussion = Vec::new();
                    let mut blocked = Vec::new();
                    let mut candidates = Vec::new();

                    walk_candidate_leaves(
                        &target.children,
                        &mut discussion,
                        &mut blocked,
                        &mut candidates,
                    );

                    if !discussion.is_empty() {
                        return PlanScopeEvaluation::BlockedByDiscussion(discussion);
                    }
                    if !blocked.is_empty() {
                        return PlanScopeEvaluation::BlockedByBlockers(blocked);
                    }
                    self.resolve_scope_candidates(candidates)
                }
            }
        }
    }
    fn resolve_scope_candidates(&self, candidates: Vec<PlanItem>) -> PlanScopeEvaluation {
        if candidates.is_empty() {
            return PlanScopeEvaluation::AllCompleted;
        }
        let (ready, blocked) = self.resolve_dependency_readiness(candidates);
        if ready.is_empty() {
            PlanScopeEvaluation::BlockedByDependencies(blocked)
        } else {
            PlanScopeEvaluation::Ready(ready)
        }
    }
    fn resolve_dependency_readiness(
        &self,
        candidates: Vec<PlanItem>,
    ) -> (Vec<PlanItem>, Vec<String>) {
        let mut satisfied: HashSet<String> = HashSet::new();
        // An item satisfies a dependency when it is effectively `Done`: a leaf
        // carries its own authoritative status, while a container is `Done`
        // exactly when its children aggregate to `Done`. Deriving the container
        // status here keeps readiness independent of a possibly stale stored
        // value that was never refreshed in memory.
        fn collect_done(items: &[PlanItem], satisfied: &mut HashSet<String>) {
            // Post-order walk: a node's status is derived from the statuses its
            // children return, so each subtree is scanned exactly once instead
            // of re-running `compute_items_status` on every container.
            fn walk(item: &PlanItem, satisfied: &mut HashSet<String>) -> PlanItemStatus {
                let status = if item.is_leaf() {
                    item.status
                } else {
                    compute_direct_status(item.children.iter().map(|child| walk(child, satisfied)))
                };
                if status == PlanItemStatus::Done {
                    satisfied.insert(id_key(&item.id));
                }
                status
            }
            for item in items {
                walk(item, satisfied);
            }
        }
        collect_done(&self.items, &mut satisfied);

        let mut ready = Vec::new();
        let mut pending = candidates;
        loop {
            let mut progressed = false;
            let mut still = Vec::new();
            for leaf in pending {
                let deps_met = leaf
                    .depends_on
                    .iter()
                    .all(|dep| satisfied.contains(&id_key(dep)));
                if deps_met {
                    satisfied.insert(id_key(&leaf.id));
                    ready.push(leaf);
                    progressed = true;
                } else {
                    still.push(leaf);
                }
            }
            pending = still;
            if !progressed {
                break;
            }
        }

        let blocked = pending
            .into_iter()
            .map(|leaf| {
                let unmet: Vec<String> = leaf
                    .depends_on
                    .iter()
                    .filter(|dep| !satisfied.contains(&id_key(dep.as_str())))
                    .map(|dep| format!("#{dep}"))
                    .collect();
                if unmet.is_empty() {
                    format!("#{} {}", leaf.id, leaf.title)
                } else {
                    format!(
                        "#{} {} (unmet dependencies: {})",
                        leaf.id,
                        leaf.title,
                        unmet.join(", ")
                    )
                }
            })
            .collect();
        (ready, blocked)
    }
    fn validate_structure(&self) -> Result<(), Vec<PlanValidationError>> {
        let mut errors = Vec::new();

        // 1. Validate step IDs and the leaf target-file invariant
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
                // leaf without them starts its step from a cold research
                // round. A draft leaf settles its files in the dialogue, a
                // finished leaf already recorded its work, and a cancelled
                // leaf is never executed, so all three are exempt. A container
                // is never executed either and carries no list of its own (see
                // [`Self::normalize_leaf_files`]).
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

        // 2. Validate dependency existence, self-dependency, and the implicit
        //    container-to-leaf edges
        let mut dep_graph: HashMap<String, Vec<String>> = HashMap::new();
        // Validates the dependency edges of a subtree and returns the normalized
        // keys of that subtree's leaves. A leaf reports its own key; a container
        // reports the union of its children's keys. Returning them from the same
        // recursion that validates the children means a container learns the
        // leaves it implicitly depends on without a second descent over the
        // subtree it just visited.
        fn walk_refs(
            items: &[PlanItem],
            seen_items: &HashSet<String>,
            dep_graph: &mut HashMap<String, Vec<String>>,
            errors: &mut Vec<PlanValidationError>,
        ) -> Vec<String> {
            let mut leaf_keys = Vec::new();
            for item in items {
                let item_key = id_key(&item.id);

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

                let mut child_leaf_keys = walk_refs(&item.children, seen_items, dep_graph, errors);

                if !item_key.is_empty() {
                    // A container becomes `Done` once every descendant leaf
                    // that still gates its derived status is `Done` (see
                    // `gates_ancestor_completion`), so it implicitly depends on
                    // exactly those leaves. Recording those edges lets the
                    // cycle detector expose an active leaf that depends on its
                    // own ancestor: the ancestor can never finish without the
                    // leaf, and the leaf can never start without the ancestor.
                    // Only the derived status of a container is meaningful, so
                    // the implicit edges are added for non-leaf items only.
                    if !item.is_leaf() {
                        deps_for_item.extend(child_leaf_keys.iter().cloned());
                    }
                    dep_graph.insert(item_key.clone(), deps_for_item);
                }

                if item.is_leaf() {
                    // Only a leaf that still gates its ancestor's completion
                    // contributes an implicit container-to-leaf edge: a
                    // cancelled or finished leaf is never scheduled again, so
                    // such an edge would surface a deadlock that cannot occur.
                    if !item_key.is_empty() && gates_ancestor_completion(item.status) {
                        leaf_keys.push(item_key);
                    }
                } else {
                    leaf_keys.append(&mut child_leaf_keys);
                }
            }
            leaf_keys
        }
        walk_refs(&self.items, &seen_item_ids, &mut dep_graph, &mut errors);

        // 3. Validate acyclicity of depends_on
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

/// Reports whether a leaf still gates its ancestor's completion.
///
/// A container becomes `Done` once every descendant leaf that is neither
/// `Cancelled` nor already `Done` has finished (see `StatusSummary`), so only
/// those leaves are genuine prerequisites of the container's derived status.
/// Recording an implicit edge to a `Cancelled` or `Done` leaf would fabricate a
/// prerequisite that the derived status can never wait on, and a leftover
/// `depends_on` naming that ancestor would then surface a deadlock that cannot
/// occur at runtime: a cancelled leaf is never scheduled at all, and a finished
/// leaf is never scheduled again.
fn gates_ancestor_completion(status: PlanItemStatus) -> bool {
    !matches!(status, PlanItemStatus::Cancelled | PlanItemStatus::Done)
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
