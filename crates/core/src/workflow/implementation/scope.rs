//! Execution scope and frame resolution for the unified implementation pipeline.
//!
//! An implementation horizon is executed by one pipeline regardless of the
//! workflow that owns it. This module names the three sources of work — a local
//! plan, an architectural review group, and a single review finding — and
//! resolves each of them from the authoritative core storages into one
//! [`ImplementationFrame`]. The frame carries the identifiers of its scope, so a
//! terminal outcome is mapped back onto the storage that owns the entity
//! instead of being guessed.

use std::path::Path;

use crate::plan::storage::PlanStorage;
use crate::review::prompt::apply_group_architectural_context;
use crate::review::storage::ReviewStorage;
use crate::workflow::implementation::StepFailure;
use tauqe_protocol::{
    Plan, PlanItem, PlanItemStatus, PlanScopeEvaluation, ReviewItem, ReviewSession,
    DEFAULT_REVIEW_GROUP_ID,
};

/// The source of work an implementation horizon is resolved from.
///
/// The set is closed on purpose: exactly three workflows own a step graph, and
/// every one of them resolves by identifier from a core storage, so a caller
/// never supplies a horizon of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionScope {
    /// A stored local plan.
    Plan { plan_id: String },
    /// An architectural review group executed through its unified horizon.
    ReviewGroup { review_id: String, group_id: String },
    /// A single review finding executed as a degenerate one-step horizon.
    ReviewFinding { review_id: String, item_id: u32 },
}

impl ExecutionScope {
    /// Resolves the frame this scope executes.
    ///
    /// Only the horizon of a review group and the degenerate horizon of a
    /// single finding are assembled here; a local plan is executed exactly as
    /// it is stored. The resolution is read-only: nothing is written back, so a
    /// frame that is never executed leaves the repositories untouched.
    pub fn resolve(&self, repo_root: &Path) -> Result<ImplementationFrame, StepFailure> {
        match self {
            Self::Plan { plan_id } => resolve_plan(repo_root, plan_id),
            Self::ReviewGroup {
                review_id,
                group_id,
            } => resolve_group(repo_root, review_id, group_id),
            Self::ReviewFinding { review_id, item_id } => {
                resolve_finding(repo_root, review_id, *item_id)
            }
        }
    }
}

/// One implementation horizon resolved from the storage that owns it.
///
/// The frame is the only value the pipeline needs: the scope it came from, the
/// horizon to execute together with the task context its steps render, and the
/// findings a completed run closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplementationFrame {
    /// The scope the frame was resolved from.
    pub scope: ExecutionScope,
    /// The horizon to execute, with its header and concept tree.
    pub plan: Plan,
    /// Findings the completion transition closes, in stored order.
    pub findings: Vec<ReviewItem>,
}

/// Resolves a stored local plan into its execution frame.
fn resolve_plan(repo_root: &Path, plan_id: &str) -> Result<ImplementationFrame, StepFailure> {
    let plan = PlanStorage::load_plan(repo_root, plan_id)
        .map_err(|err| StepFailure::infrastructure(err.to_string()))?
        .ok_or_else(|| StepFailure::infrastructure(format!("Plan '{}' not found", plan_id)))?;
    Ok(ImplementationFrame {
        scope: ExecutionScope::Plan {
            plan_id: plan.id.clone(),
        },
        plan,
        findings: Vec::new(),
    })
}

/// Resolves an architectural review group into its execution frame.
///
/// The group is the operational unit of remediation, so the frame carries the
/// group's own horizon together with the Architect provenance it records, and
/// renders the findings that manifest the root cause into the concept tree.
/// Their descriptions therefore reach the context of every step the horizon
/// executes, which is the invariant a sub-step turn relies on.
fn resolve_group(
    repo_root: &Path,
    review_id: &str,
    group_id: &str,
) -> Result<ImplementationFrame, StepFailure> {
    let session = load_review(repo_root, review_id)?;
    let group = session
        .groups
        .iter()
        .find(|group| group.id == group_id)
        .cloned()
        .ok_or_else(|| {
            StepFailure::infrastructure(format!(
                "Review group '{}' not found in session '{}'",
                group_id, review_id
            ))
        })?;
    let findings = group_findings(&session, group_id);
    let mut plan = Plan::from_parts(
        session.id.clone(),
        group.title.clone(),
        group.description.clone(),
        0,
        0,
        Vec::new(),
        Vec::new(),
    );
    // The horizon, its Architect provenance, and the step whose horizon was
    // last reprojected belong to the group: the frame executes exactly them.
    plan.implementation = group.implementation.clone();
    apply_group_architectural_context(&mut plan, &group, &findings);
    Ok(ImplementationFrame {
        scope: ExecutionScope::ReviewGroup {
            review_id: session.id.clone(),
            group_id: group.id.clone(),
        },
        plan,
        findings,
    })
}

/// Resolves a single review finding into a degenerate one-step frame.
///
/// A finding owns no horizon on disk, so its frame exists in memory only and is
/// never persisted: the finding itself is the single leaf step, and the files
/// it declares are the files that step targets.
fn resolve_finding(
    repo_root: &Path,
    review_id: &str,
    item_id: u32,
) -> Result<ImplementationFrame, StepFailure> {
    let session = load_review(repo_root, review_id)?;
    let finding = session.find_item(item_id).cloned().ok_or_else(|| {
        StepFailure::infrastructure(format!(
            "Review item #{} not found in session '{}'",
            item_id, review_id
        ))
    })?;
    Ok(degenerate_finding_frame(review_id, finding))
}

/// Builds the in-memory one-step frame of a single review finding.
fn degenerate_finding_frame(review_id: &str, finding: ReviewItem) -> ImplementationFrame {
    let description = {
        let body = finding.body.trim();
        if body.is_empty() {
            None
        } else {
            Some(body.to_string())
        }
    };
    let step = PlanItem {
        id: finding.id.to_string(),
        title: finding.title.clone(),
        status: PlanItemStatus::Todo,
        files: finding.files.clone(),
        readonly_files: finding.readonly_files.clone(),
        ..PlanItem::default()
    };
    let plan = Plan::from_parts(
        review_id.to_string(),
        finding.title.clone(),
        description,
        0,
        0,
        Vec::new(),
        vec![step],
    );
    ImplementationFrame {
        scope: ExecutionScope::ReviewFinding {
            review_id: review_id.to_string(),
            item_id: finding.id,
        },
        plan,
        findings: vec![finding],
    }
}

/// Loads a stored review session or reports it as an infrastructure failure.
fn load_review(repo_root: &Path, review_id: &str) -> Result<ReviewSession, StepFailure> {
    ReviewStorage::load_review(repo_root, review_id)
        .map_err(|err| StepFailure::infrastructure(err.to_string()))?
        .ok_or_else(|| {
            StepFailure::infrastructure(format!("Review session '{}' not found", review_id))
        })
}

/// Resolves every finding that belongs to `group_id`, in stored order.
///
/// Membership follows the effective group of a finding: a finding that stores
/// no group belongs to the implicit [`DEFAULT_REVIEW_GROUP_ID`] group, exactly
/// as the grouped session view resolves it.
fn group_findings(session: &ReviewSession, group_id: &str) -> Vec<ReviewItem> {
    session
        .items
        .iter()
        .filter(|item| match item.group.as_deref() {
            Some(group) => group == group_id,
            None => group_id == DEFAULT_REVIEW_GROUP_ID,
        })
        .cloned()
        .collect()
}

/// Renders why a plan scope cannot be executed right now.
///
/// The shared scope evaluator is the single source of truth for readiness, so
/// its stall variants are translated into a developer-facing diagnostic here
/// instead of being re-derived from the plan tree.
fn format_scope_stall(step_id: Option<&str>, evaluation: &PlanScopeEvaluation) -> String {
    let target = step_id.unwrap_or("horizon");
    match evaluation {
        PlanScopeEvaluation::BlockedByDiscussion(items) => format!(
            "Sub-step scope '{}' is still in DISCUSSION: {}",
            target,
            items.join(", ")
        ),
        PlanScopeEvaluation::BlockedByBlockers(items) => format!(
            "Sub-step scope '{}' is BLOCKED: {}",
            target,
            items.join(", ")
        ),
        PlanScopeEvaluation::BlockedByDependencies(items) => format!(
            "Sub-step scope '{}' has unmet dependencies: {}",
            target,
            items.join(", ")
        ),
        PlanScopeEvaluation::StepBlocked => format!("Sub-step '{}' is BLOCKED", target),
        PlanScopeEvaluation::Cancelled => format!("Sub-step scope '{}' is CANCELLED", target),
        PlanScopeEvaluation::NotFound => {
            format!("Sub-step '{}' not found in the horizon", target)
        }
        // `Ready` and `AllCompleted` never reach the stall diagnostic.
        PlanScopeEvaluation::Ready(_) | PlanScopeEvaluation::AllCompleted => {
            format!("Sub-step scope '{}' is already complete", target)
        }
    }
}

/// Finds the first leaf step whose dependencies can never be satisfied.
///
/// Every leaf is asked through the shared scope evaluator, so readiness is never
/// re-derived here: the identifier named for replanning is exactly the step the
/// evaluator reports as blocked by unmet dependencies.
fn dependency_stalled_step(plan: &Plan) -> Option<String> {
    fn walk(plan: &Plan, items: &[PlanItem]) -> Option<String> {
        for item in items {
            if item.is_leaf() {
                if matches!(
                    plan.evaluate_scope(Some(item.id.as_str())),
                    PlanScopeEvaluation::BlockedByDependencies(_)
                ) {
                    return Some(item.id.clone());
                }
            } else if let Some(found) = walk(plan, &item.children) {
                return Some(found);
            }
        }
        None
    }
    walk(plan, &plan.items)
}

/// Classifies a stalled implementation scope for the pipeline supervisor.
///
/// A scope stalls on dependencies when its leaf steps wait for work that will
/// never become `Done`: the plan structure itself is wrong, so the horizon has
/// to be reprojected around the stalled dependencies. That outcome is therefore
/// a domain failure attributed to the stalled step, and the supervisor replans
/// the remaining horizon instead of aborting the run. Every other stall is a
/// lifecycle or structural precondition the harness cannot repair by replanning.
pub fn classify_scope_stall(
    plan: &Plan,
    step_id: Option<&str>,
    stall: &PlanScopeEvaluation,
) -> StepFailure {
    let reason = format_scope_stall(step_id, stall);
    match stall {
        PlanScopeEvaluation::BlockedByDependencies(_) => {
            let failure = StepFailure::domain(reason);
            match dependency_stalled_step(plan) {
                Some(step) => failure.with_step(step),
                None => failure,
            }
        }
        _ => StepFailure::infrastructure(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::review::{IssueStatus, IssueType, ReviewGroupKind, ReviewIssue};
    use tauqe_protocol::{PlanImplementation, ReviewGroup};
    use tempfile::tempdir;

    /// Builds a finding that declares one editable and one protected file.
    fn finding(id: u32, title: &str, body: &str) -> ReviewItem {
        ReviewItem::from(ReviewIssue {
            id,
            title: title.to_string(),
            uuid: None,
            commit: None,
            group: Some("arch-1".to_string()),
            model: None,
            issue_type: IssueType::Bug,
            status: IssueStatus::Todo,
            description: body.to_string(),
            recommendations: Vec::new(),
            files: vec!["src/parser.rs".to_string()],
            readonly_files: vec!["src/api.rs".to_string()],
            note: None,
        })
    }

    /// Stores a session holding one architectural group and its single finding.
    fn stored_session(repo_root: &Path) {
        let group = ReviewGroup {
            id: "arch-1".to_string(),
            title: "Unified error handling".to_string(),
            description: Some("Every module invents its own error type.".to_string()),
            kind: ReviewGroupKind::Architectural,
            implementation: PlanImplementation {
                items: vec![PlanItem {
                    id: "s-1".to_string(),
                    title: "Introduce the shared error".to_string(),
                    status: PlanItemStatus::Todo,
                    files: vec!["src/error.rs".to_string()],
                    ..PlanItem::default()
                }],
                last_replanned_step: Some("s-0".to_string()),
                ..PlanImplementation::default()
            },
            ..ReviewGroup::default()
        };
        let open = finding(
            7,
            "Guard the parser",
            "The parser panics on truncated input.",
        );
        let session = ReviewSession {
            id: "rev-frame".to_string(),
            title: "Parser audit".to_string(),
            items: vec![open],
            groups: vec![group],
            ..ReviewSession::default()
        };
        ReviewStorage::save_session(repo_root, &session).unwrap();
    }

    #[test]
    fn plan_scope_resolves_the_stored_horizon() {
        let dir = tempdir().unwrap();
        let plan = Plan::from_parts(
            "frame-plan".to_string(),
            "Frame Plan".to_string(),
            Some("Header".to_string()),
            0,
            0,
            Vec::new(),
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                status: PlanItemStatus::Todo,
                files: vec!["src/lib.rs".to_string()],
                ..PlanItem::default()
            }],
        );
        PlanStorage::save_plan(dir.path(), &plan).unwrap();

        let scope = ExecutionScope::Plan {
            plan_id: "frame-plan".to_string(),
        };
        let frame = scope.resolve(dir.path()).expect("the stored plan resolves");

        assert_eq!(frame.plan.id, "frame-plan");
        assert_eq!(frame.plan.implementation.items.len(), 1);
        assert!(frame.findings.is_empty());
        let expected = ExecutionScope::Plan {
            plan_id: "frame-plan".to_string(),
        };
        assert_eq!(frame.scope, expected);
    }

    #[test]
    fn review_group_scope_renders_the_group_findings_into_the_concept_tree() {
        let dir = tempdir().unwrap();
        stored_session(dir.path());

        let scope = ExecutionScope::ReviewGroup {
            review_id: "rev-frame".to_string(),
            group_id: "arch-1".to_string(),
        };
        let frame = scope
            .resolve(dir.path())
            .expect("the stored group resolves");

        assert_eq!(frame.plan.id, "rev-frame");
        assert_eq!(frame.plan.title, "Unified error handling");
        assert_eq!(
            frame.plan.description.as_deref(),
            Some("Every module invents its own error type.")
        );
        // The horizon and its provenance are the group's own.
        assert_eq!(frame.plan.implementation.items.len(), 1);
        assert_eq!(
            frame.plan.implementation.last_replanned_step.as_deref(),
            Some("s-0")
        );
        // Every finding of the group reaches the concept tree, so its
        // description becomes part of the context of every step.
        assert_eq!(frame.findings.len(), 1);
        let root = frame.plan.concept.first().expect("concept root");
        let node = root
            .children
            .iter()
            .find(|child| child.title.contains("Guard the parser"))
            .expect("the finding reaches the concept tree");
        let details = node.details.as_deref().unwrap_or_default();
        assert!(details.contains("truncated input"));
    }

    #[test]
    fn review_finding_scope_is_a_degenerate_in_memory_horizon() {
        let dir = tempdir().unwrap();
        stored_session(dir.path());

        let scope = ExecutionScope::ReviewFinding {
            review_id: "rev-frame".to_string(),
            item_id: 7,
        };
        let frame = scope
            .resolve(dir.path())
            .expect("the stored finding resolves");

        assert_eq!(frame.plan.id, "rev-frame");
        assert_eq!(frame.plan.title, "Guard the parser");
        assert_eq!(
            frame.plan.description.as_deref(),
            Some("The parser panics on truncated input.")
        );
        let steps = &frame.plan.implementation.items;
        assert_eq!(steps.len(), 1);
        let step = &steps[0];
        assert!(step.is_leaf());
        assert_eq!(step.id, "7");
        assert_eq!(step.status, PlanItemStatus::Todo);
        assert_eq!(step.files, vec!["src/parser.rs".to_string()]);
        assert_eq!(step.readonly_files, vec!["src/api.rs".to_string()]);
        assert_eq!(frame.findings.len(), 1);
        // The degenerate frame exists in memory only.
        let stored = PlanStorage::load_plan(dir.path(), "rev-frame").unwrap();
        assert!(stored.is_none());
    }

    #[test]
    fn unresolved_scopes_are_infrastructure_failures() {
        let dir = tempdir().unwrap();

        let plan = ExecutionScope::Plan {
            plan_id: "ghost".to_string(),
        };
        let group = ExecutionScope::ReviewGroup {
            review_id: "ghost".to_string(),
            group_id: "arch-1".to_string(),
        };
        let finding = ExecutionScope::ReviewFinding {
            review_id: "ghost".to_string(),
            item_id: 1,
        };

        for scope in [plan, group, finding] {
            let failure = scope.resolve(dir.path()).expect_err("nothing is stored");
            assert!(!failure.is_domain());
            assert!(!failure.is_unreverted());
        }
    }

    /// Builds a horizon whose only executable leaf depends on a cancelled step,
    /// so the scope can never become ready.
    fn stalled_plan() -> Plan {
        Plan::from_parts(
            "rev-stall".to_string(),
            "Stalled Horizon".to_string(),
            None,
            0,
            0,
            Vec::new(),
            vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Abandoned approach".to_string(),
                    status: PlanItemStatus::Cancelled,
                    ..PlanItem::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Follow-up step".to_string(),
                    status: PlanItemStatus::Todo,
                    depends_on: vec!["1".to_string()],
                    ..PlanItem::default()
                },
            ],
        )
    }

    /// A scope stalled on unsatisfiable dependencies is a domain failure of the
    /// stalled step, so the supervisor replans the horizon instead of aborting
    /// the run as an infrastructure error.
    #[test]
    fn dependency_stall_is_a_domain_failure_of_the_stalled_step() {
        let plan = stalled_plan();
        let stall = plan.evaluate_scope(None);
        assert!(matches!(
            stall,
            PlanScopeEvaluation::BlockedByDependencies(_)
        ));

        let failure = classify_scope_stall(&plan, None, &stall);
        assert!(failure.is_domain());
        assert_eq!(failure.failed_step(), Some("2"));
    }

    /// A stall the harness cannot repair by replanning the horizon stays an
    /// infrastructure failure that aborts the run.
    #[test]
    fn blocked_scope_remains_an_infrastructure_failure() {
        let mut plan = stalled_plan();
        plan.items[1].status = PlanItemStatus::Blocked;
        let stall = plan.evaluate_scope(None);
        assert!(matches!(stall, PlanScopeEvaluation::BlockedByBlockers(_)));

        let failure = classify_scope_stall(&plan, None, &stall);
        assert!(!failure.is_domain());
        assert_eq!(failure.failed_step(), None);
    }

    /// A ready scope names no stalled step.
    #[test]
    fn ready_scope_names_no_stalled_step() {
        let mut plan = stalled_plan();
        plan.items[0].status = PlanItemStatus::Done;
        assert_eq!(dependency_stalled_step(&plan), None);
    }
}
