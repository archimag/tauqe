use std::collections::HashSet;

use super::item::{PlanItem, PlanItemStatus};
use super::node::{compute_direct_status, id_key, Plan};

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
/// from a plan subtree.
///
/// Only executable leaf items are inspected. A container's status is derived from
/// its children, so reporting the container too would list a `Discussion` (or
/// blocked) child twice and could depend on a stale in-memory container status.
///
/// A candidate leaf is an executable step whose status is `Todo` or `InProgress`.
/// A blocked leaf is a step whose status is `Blocked`; it is surfaced separately
/// instead of being silently skipped, so a stalled scope is never mistaken for a
/// finished one.
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

impl Plan {
    /// Evaluates a scope (leaf item, composite subtree, or entire plan if `scope_id` is None)
    /// for autonomous execution.
    ///
    /// A candidate leaf is ready only when every identifier in its `depends_on`
    /// list refers to a step that is already `Done` or is scheduled earlier in the
    /// same scope, enforcing the DAG readiness contract.
    ///
    /// `Blocked` leaves are surfaced as [`PlanScopeEvaluation::BlockedByBlockers`]
    /// instead of being silently skipped, so a stalled plan is never mistaken for a
    /// finished one.
    pub fn evaluate_scope(&self, scope_id: Option<&str>) -> PlanScopeEvaluation {
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
                        PlanItemStatus::Discussion => PlanScopeEvaluation::BlockedByDiscussion(vec![
                            format!("#{} {}", target.id, target.title),
                        ]),
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

                    walk_candidate_leaves(&target.children, &mut discussion, &mut blocked, &mut candidates);

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

    /// Filters executable candidate leaves by DAG readiness, mapping the outcome
    /// onto a scope evaluation.
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

    /// Topologically resolves which candidate leaves are ready for execution.
    ///
    /// Returns the ordered ready leaves together with the descriptions of the
    /// leaves that remain blocked by unmet dependencies.
    fn resolve_dependency_readiness(&self, candidates: Vec<PlanItem>) -> (Vec<PlanItem>, Vec<String>) {
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
                    compute_direct_status(
                        item.children.iter().map(|child| walk(child, satisfied)),
                    )
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
}
