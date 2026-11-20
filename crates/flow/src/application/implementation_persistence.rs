//! Durable leaf transitions of an implementation run.

use crate::api::Application;
use std::path::Path;
use tauqe_core::models::ArchitectTier;
use tauqe_core::plan::storage::PlanStorage;
use tauqe_core::plan::{PlanItem, PlanItemStatus};
use tauqe_core::review::storage::ReviewStorage;
use tauqe_core::review::IssueStatus;
use tauqe_core::workflow::implementation::{
    ExecutionScope, LeafTransition, LeafTransitionResult, PlanImplementationStop, StepFailure,
};

const GROUP_COMPLETION_NOTE: &str =
    "Resolved by the completed implementation horizon of the review group";

impl Application {
    /// Completes a review scope before the pipeline returns its report.
    /// A failed settlement is logged, not substituted for the run outcome.
    pub(crate) async fn settle_implementation_scope(
        &self,
        root: &Path,
        scope: &ExecutionScope,
        stop: &PlanImplementationStop,
        succeeded: usize,
        scheduled: usize,
    ) {
        let root = root.to_path_buf();
        let scope = scope.clone();
        let stop = stop.clone();
        if let Err(error) = tauqe_core::blocking::run_blocking_async(move || {
            settle_scope(&root, &scope, &stop, succeeded, scheduled);
        })
        .await
        {
            tracing::warn!(%error, "the implementation scope settlement could not run");
        }
    }

    pub(crate) async fn persist_implementation_leaf(
        &self,
        root: &Path,
        transition: LeafTransition,
    ) -> Result<LeafTransitionResult, StepFailure> {
        let root = root.to_path_buf();
        tauqe_core::blocking::run_blocking_async(move || persist(&root, transition))
            .await
            .map_err(|reason| {
                StepFailure::infrastructure(format!(
                    "The implementation leaf could not be persisted: blocking task failed: {reason}"
                ))
            })?
    }

    pub(crate) async fn replace_implementation_horizon(
        &self,
        root: &Path,
        scope: ExecutionScope,
        items: Vec<PlanItem>,
        tier: ArchitectTier,
        last_replanned_step: Option<String>,
    ) -> Result<(), StepFailure> {
        let root = root.to_path_buf();
        tauqe_core::blocking::run_blocking_async(move || match scope {
            ExecutionScope::Plan { plan_id, .. } => PlanStorage::replace_uncompleted_horizon(
                &root,
                &plan_id,
                items,
                tier,
                last_replanned_step,
            )
            .map(|_| ())
            .map_err(|err| StepFailure::infrastructure(err.to_string())),
            ExecutionScope::ReviewGroup {
                review_id,
                group_id,
            } => ReviewStorage::replace_uncompleted_group_subplan(
                &root,
                &review_id,
                &group_id,
                items,
                tier,
                last_replanned_step,
            )
            .map_err(|err| StepFailure::infrastructure(err.to_string()))?
            .map(|_| ())
            .ok_or_else(|| {
                StepFailure::infrastructure(format!(
                    "Review group '{}' of session '{}' is not stored; the synthesized horizon was not persisted",
                    group_id, review_id
                ))
            }),
            ExecutionScope::ReviewFinding { .. } => Err(StepFailure::infrastructure(
                "A single review finding owns no persisted horizon to rewrite",
            )),
        })
        .await
        .map_err(|reason| {
            StepFailure::infrastructure(format!(
                "The implementation horizon could not be persisted: blocking task failed: {reason}"
            ))
        })?
    }

    pub(crate) async fn persist_group_blocker(
        &self,
        root: &Path,
        review_id: &str,
        group_id: &str,
        reason: Option<&str>,
    ) -> Result<bool, StepFailure> {
        let root = root.to_path_buf();
        let review_id = review_id.to_string();
        let group_id = group_id.to_string();
        let reason = reason.map(str::to_string);
        tauqe_core::blocking::run_blocking_async(move || {
            ReviewStorage::set_group_blocked_reason(&root, &review_id, &group_id, reason.as_deref())
                .map(|group| group.is_some())
                .map_err(|err| StepFailure::infrastructure(err.to_string()))
        })
        .await
        .map_err(|reason| {
            StepFailure::infrastructure(format!(
                "The review group blocker could not be persisted: blocking task failed: {reason}"
            ))
        })?
    }
}

/// Review verdicts belong to the scenario, not the technical executor.
fn settle_scope(
    root: &Path,
    scope: &ExecutionScope,
    stop: &PlanImplementationStop,
    succeeded: usize,
    scheduled: usize,
) {
    match scope {
        ExecutionScope::Plan { .. } => {}
        ExecutionScope::ReviewGroup {
            review_id,
            group_id,
        } => {
            // Read the persisted aggregate, not the run report: a partial or
            // empty horizon is not a completed remediation.
            let complete = ReviewStorage::load_review(root, review_id)
                .ok()
                .flatten()
                .and_then(|session| {
                    session
                        .groups
                        .into_iter()
                        .find(|group| group.id == *group_id)
                })
                .is_some_and(|group| {
                    !group.implementation.items.is_empty()
                        && group.implementation.status() == PlanItemStatus::Done
                });
            if !complete {
                return;
            }
            match ReviewStorage::close_group_implementation(
                root,
                review_id,
                group_id,
                GROUP_COMPLETION_NOTE,
            ) {
                Ok(Some(closed)) => tracing::info!(
                    review_id,
                    group_id,
                    closed,
                    "review group implementation closed"
                ),
                Ok(None) => tracing::warn!(
                    review_id,
                    group_id,
                    "the review group completion was not persisted: the group was not found"
                ),
                Err(error) => {
                    tracing::warn!(review_id, group_id, %error, "the review group completion was not persisted")
                }
            }
        }
        ExecutionScope::ReviewFinding { review_id, item_id } => {
            let (status, note) = match stop {
                PlanImplementationStop::Completed => (
                    IssueStatus::Fixed,
                    format!("Addressed by the implementation run: {succeeded} of {scheduled} scheduled step(s) succeeded"),
                ),
                PlanImplementationStop::Failed { reason, .. }
                | PlanImplementationStop::Blocked { reason, .. } => (
                    IssueStatus::Skipped,
                    format!("The implementation run could not address this finding: {reason}"),
                ),
                PlanImplementationStop::Cancelled | PlanImplementationStop::Stopped => return,
            };
            match ReviewStorage::update_session_item(root, review_id, *item_id, |issue| {
                issue.status = status;
                // The step already recorded its commit; keep it and any note
                // supplied by the developer.
                if !issue
                    .note
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|note| !note.is_empty())
                {
                    issue.note = Some(note);
                }
            }) {
                Ok(Some(_)) => {
                    tracing::info!(review_id, item_id, ?status, "review finding settled")
                }
                Ok(None) => tracing::warn!(
                    review_id,
                    item_id,
                    "the review finding verdict was not persisted: the finding was not found"
                ),
                Err(error) => {
                    tracing::warn!(review_id, item_id, %error, "the review finding verdict was not persisted")
                }
            }
        }
    }
}

fn persist(root: &Path, transition: LeafTransition) -> Result<LeafTransitionResult, StepFailure> {
    let LeafTransition {
        scope,
        step_id,
        status,
        blocked_reason,
        commit,
        mut group_items,
    } = transition;
    match scope {
        ExecutionScope::Plan { plan_id, .. } => {
            let plan = PlanStorage::update_item(
                root,
                &plan_id,
                &step_id,
                Some(status),
                blocked_reason.as_deref(),
                commit.as_deref(),
            )
            .map_err(|err| {
                StepFailure::infrastructure(format!(
                    "Failed to persist the status of step '{}': {}",
                    step_id, err
                ))
            })?
            .ok_or_else(|| {
                StepFailure::infrastructure(format!(
                    "Plan '{}' does not store step '{}'",
                    plan_id, step_id
                ))
            })?;
            Ok(LeafTransitionResult::Horizon(plan.implementation))
        }
        ExecutionScope::ReviewGroup {
            review_id,
            group_id,
        } => {
            if !set_leaf_state(
                &mut group_items,
                &step_id,
                status,
                blocked_reason.as_deref(),
                commit.as_deref(),
            ) {
                return Err(StepFailure::infrastructure(format!(
                    "Review group '{}' of session '{}' does not store step '{}'",
                    group_id, review_id, step_id
                )));
            }
            let group =
                ReviewStorage::update_group_subplan(root, &review_id, &group_id, group_items)
                    .map_err(|err| {
                        StepFailure::infrastructure(format!(
                            "Failed to persist the status of step '{}': {}",
                            step_id, err
                        ))
                    })?
                    .ok_or_else(|| {
                        StepFailure::infrastructure(format!(
                            "Review group '{}' of session '{}' is not stored",
                            group_id, review_id
                        ))
                    })?;
            Ok(LeafTransitionResult::Horizon(group.implementation))
        }
        ExecutionScope::ReviewFinding { review_id, item_id } => {
            let updated = match status {
                PlanItemStatus::InProgress | PlanItemStatus::Todo => {
                    let issue_status = if status == PlanItemStatus::InProgress {
                        IssueStatus::InProgress
                    } else {
                        IssueStatus::Todo
                    };
                    ReviewStorage::update_session_item_status(
                        root,
                        &review_id,
                        item_id,
                        issue_status,
                    )
                }
                PlanItemStatus::Done => {
                    let Some(hash) = commit
                        .as_deref()
                        .map(str::trim)
                        .filter(|hash| !hash.is_empty())
                    else {
                        return Ok(LeafTransitionResult::Finding);
                    };
                    ReviewStorage::update_session_item(root, &review_id, item_id, |issue| {
                        issue.commit = Some(hash.to_string());
                    })
                }
                PlanItemStatus::Blocked
                | PlanItemStatus::Discussion
                | PlanItemStatus::Cancelled => {
                    return Ok(LeafTransitionResult::Finding);
                }
            }
            .map_err(|err| {
                StepFailure::infrastructure(format!(
                    "Failed to persist the status of review finding #{}: {}",
                    item_id, err
                ))
            })?;
            if updated.is_none() {
                return Err(StepFailure::infrastructure(format!(
                    "Review session '{}' does not store finding #{}",
                    review_id, item_id
                )));
            }
            Ok(LeafTransitionResult::Finding)
        }
    }
}

fn set_leaf_state(
    items: &mut [PlanItem],
    step_id: &str,
    status: PlanItemStatus,
    blocked_reason: Option<&str>,
    commit: Option<&str>,
) -> bool {
    for item in items {
        if item.id.trim().eq_ignore_ascii_case(step_id.trim()) {
            if !item.is_leaf() {
                return false;
            }
            item.status = status;
            if let Some(commit) = commit.map(str::trim).filter(|hash| !hash.is_empty()) {
                item.commit = Some(commit.to_string());
            }
            item.blocked_reason = if status == PlanItemStatus::Blocked {
                blocked_reason
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .map(str::to_string)
            } else {
                None
            };
            return true;
        }
        if set_leaf_state(&mut item.children, step_id, status, blocked_reason, commit) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::plan::{Plan, PlanImplementation};
    use tauqe_core::review::{IssueType, ReviewGroup, ReviewIssue, ReviewItem, ReviewSession};

    fn review_with_finding(root: &Path, note: Option<&str>) {
        ReviewStorage::save_session(
            root,
            &ReviewSession {
                id: "review".into(),
                title: "Review".into(),
                items: vec![ReviewItem::from(ReviewIssue {
                    id: 7,
                    title: "Finding".into(),
                    uuid: None,
                    commit: Some("abc1234".into()),
                    group: Some("group".into()),
                    model: None,
                    issue_type: IssueType::Bug,
                    status: IssueStatus::InProgress,
                    description: "Fix".into(),
                    recommendations: vec![],
                    files: vec!["src/lib.rs".into()],
                    readonly_files: vec![],
                    note: note.map(str::to_string),
                })],
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn finding_settlement_keeps_commit_and_user_note() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let scope = ExecutionScope::ReviewFinding {
            review_id: "review".into(),
            item_id: 7,
        };
        let app = Application::default();
        review_with_finding(root, Some("Developer decision"));
        app.settle_implementation_scope(root, &scope, &PlanImplementationStop::Completed, 1, 1)
            .await;
        let session = ReviewStorage::load_review(root, "review").unwrap().unwrap();
        assert_eq!(session.items[0].status, IssueStatus::Fixed);
        assert_eq!(session.items[0].commit.as_deref(), Some("abc1234"));
        assert_eq!(
            session.items[0].resolution_notes.as_deref(),
            Some("Developer decision")
        );

        review_with_finding(root, None);
        app.settle_implementation_scope(
            root,
            &scope,
            &PlanImplementationStop::Blocked {
                step: Some("7".into()),
                reason: "not implementable".into(),
            },
            0,
            1,
        )
        .await;
        let session = ReviewStorage::load_review(root, "review").unwrap().unwrap();
        assert_eq!(session.items[0].status, IssueStatus::Skipped);
        assert_eq!(session.items[0].commit.as_deref(), Some("abc1234"));
        assert!(session.items[0]
            .resolution_notes
            .as_deref()
            .is_some_and(|note| note.contains("not implementable")));

        for stop in [
            PlanImplementationStop::Cancelled,
            PlanImplementationStop::Stopped,
        ] {
            review_with_finding(root, None);
            app.settle_implementation_scope(root, &scope, &stop, 0, 1)
                .await;
            let session = ReviewStorage::load_review(root, "review").unwrap().unwrap();
            assert_eq!(session.items[0].status, IssueStatus::InProgress);
            assert!(session.items[0].resolution_notes.is_none());
        }
    }

    #[tokio::test]
    async fn group_settlement_requires_a_persisted_done_horizon() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let scope = ExecutionScope::ReviewGroup {
            review_id: "review".into(),
            group_id: "group".into(),
        };
        let app = Application::default();
        for status in [PlanItemStatus::Todo, PlanItemStatus::Done] {
            review_with_finding(root, None);
            ReviewStorage::update_session_item(root, "review", 7, |issue| {
                issue.status = IssueStatus::Todo;
            })
            .unwrap();
            ReviewStorage::update_group_subplan(root, "review", "group", vec![]).unwrap();
            let mut session = ReviewStorage::load_review(root, "review").unwrap().unwrap();
            session.groups.push(ReviewGroup {
                id: "group".into(),
                title: "Group".into(),
                implementation: PlanImplementation {
                    items: vec![PlanItem {
                        id: "s-1".into(),
                        title: "Step".into(),
                        status,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            });
            ReviewStorage::save_session(root, &session).unwrap();
            app.settle_implementation_scope(root, &scope, &PlanImplementationStop::Completed, 1, 1)
                .await;
            let stored = ReviewStorage::load_review(root, "review").unwrap().unwrap();
            assert_eq!(
                stored.items[0].status,
                if status == PlanItemStatus::Done {
                    IssueStatus::Fixed
                } else {
                    IssueStatus::Todo
                }
            );
        }
    }

    fn transition(scope: ExecutionScope, step_id: &str, status: PlanItemStatus) -> LeafTransition {
        LeafTransition {
            scope,
            step_id: step_id.into(),
            status,
            blocked_reason: None,
            commit: None,
            group_items: Vec::new(),
        }
    }

    #[tokio::test]
    async fn plan_transition_persists_blocker_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        PlanStorage::save_plan(
            root,
            &Plan::from_parts(
                "plan".into(),
                "Plan".into(),
                None,
                0,
                0,
                vec![],
                vec![PlanItem {
                    id: "1".into(),
                    title: "Step".into(),
                    files: vec!["src/step.rs".into()],
                    ..Default::default()
                }],
            ),
        )
        .unwrap();
        let scope = ExecutionScope::Plan {
            plan_id: "plan".into(),
            step_id: None,
        };
        let app = Application::default();
        app.persist_implementation_leaf(
            root,
            transition(scope.clone(), "1", PlanItemStatus::InProgress),
        )
        .await
        .unwrap();
        let mut blocked = transition(scope.clone(), "1", PlanItemStatus::Blocked);
        blocked.blocked_reason = Some("  incompatible  ".into());
        app.persist_implementation_leaf(root, blocked)
            .await
            .unwrap();
        assert_eq!(
            PlanStorage::load_plan(root, "plan")
                .unwrap()
                .unwrap()
                .implementation
                .find_item("1")
                .unwrap()
                .blocked_reason
                .as_deref(),
            Some("incompatible")
        );
        app.persist_implementation_leaf(root, transition(scope.clone(), "1", PlanItemStatus::Todo))
            .await
            .unwrap();
        let mut done = transition(scope, "1", PlanItemStatus::Done);
        done.commit = Some("abc1234".into());
        app.persist_implementation_leaf(root, done).await.unwrap();
        let stored = PlanStorage::load_plan(root, "plan").unwrap().unwrap();
        let leaf = stored.implementation.find_item("1").unwrap();
        assert_eq!(leaf.status, PlanItemStatus::Done);
        assert_eq!(leaf.commit.as_deref(), Some("abc1234"));
        assert!(leaf.blocked_reason.is_none());
    }

    #[tokio::test]
    async fn group_transition_updates_nested_leaf_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let leaf = PlanItem {
            id: "S-1".into(),
            title: "Leaf".into(),
            files: vec!["src/leaf.rs".into()],
            ..Default::default()
        };
        let parent = PlanItem {
            id: "parent".into(),
            title: "Parent".into(),
            children: vec![leaf],
            ..Default::default()
        };
        ReviewStorage::save_session(
            root,
            &ReviewSession {
                id: "review".into(),
                title: "Review".into(),
                groups: vec![ReviewGroup {
                    id: "group".into(),
                    title: "Group".into(),
                    implementation: PlanImplementation {
                        items: vec![parent.clone()],
                        ..Default::default()
                    },
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let scope = ExecutionScope::ReviewGroup {
            review_id: "review".into(),
            group_id: "group".into(),
        };
        let app = Application::default();
        let mut done = transition(scope, "s-1", PlanItemStatus::Done);
        done.group_items = vec![parent];
        done.commit = Some("def5678".into());
        app.persist_implementation_leaf(root, done).await.unwrap();
        let session = ReviewStorage::load_review(root, "review").unwrap().unwrap();
        let leaf = &session.groups[0].implementation.items[0].children[0];
        assert_eq!(leaf.status, PlanItemStatus::Done);
        assert_eq!(leaf.commit.as_deref(), Some("def5678"));
    }

    #[tokio::test]
    async fn finding_transition_records_commit_without_verdict_and_reset_is_todo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        ReviewStorage::save_session(
            root,
            &ReviewSession {
                id: "review".into(),
                title: "Review".into(),
                items: vec![ReviewItem::from(ReviewIssue {
                    id: 7,
                    title: "Finding".into(),
                    uuid: None,
                    commit: None,
                    group: None,
                    model: None,
                    issue_type: IssueType::Bug,
                    status: IssueStatus::Todo,
                    description: "Fix".into(),
                    recommendations: vec![],
                    files: vec!["src/lib.rs".into()],
                    readonly_files: vec![],
                    note: None,
                })],
                ..Default::default()
            },
        )
        .unwrap();
        let scope = ExecutionScope::ReviewFinding {
            review_id: "review".into(),
            item_id: 7,
        };
        let app = Application::default();
        app.persist_implementation_leaf(
            root,
            transition(scope.clone(), "7", PlanItemStatus::InProgress),
        )
        .await
        .unwrap();
        let mut done = transition(scope.clone(), "7", PlanItemStatus::Done);
        done.commit = Some("abc1234".into());
        app.persist_implementation_leaf(root, done).await.unwrap();
        let session = ReviewStorage::load_review(root, "review").unwrap().unwrap();
        assert_eq!(session.items[0].status, IssueStatus::InProgress);
        assert_eq!(session.items[0].commit.as_deref(), Some("abc1234"));
        app.persist_implementation_leaf(root, transition(scope, "7", PlanItemStatus::Todo))
            .await
            .unwrap();
        assert_eq!(
            ReviewStorage::load_review(root, "review")
                .unwrap()
                .unwrap()
                .items[0]
                .status,
            IssueStatus::Todo
        );
    }

    #[tokio::test]
    async fn horizon_replacement_preserves_done_foundation_and_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let done = PlanItem {
            id: "1".into(),
            title: "Finished".into(),
            status: PlanItemStatus::Done,
            commit: Some("abc1234".into()),
            ..Default::default()
        };
        PlanStorage::save_plan(
            root,
            &Plan::from_parts(
                "plan".into(),
                "Plan".into(),
                None,
                0,
                0,
                vec![],
                vec![
                    done.clone(),
                    PlanItem {
                        id: "2".into(),
                        title: "Old".into(),
                        files: vec!["src/old.rs".into()],
                        ..Default::default()
                    },
                ],
            ),
        )
        .unwrap();
        let tier = ArchitectTier::from(tauqe_core::models::ModelRef::openrouter("architect/one"));
        Application::default()
            .replace_implementation_horizon(
                root,
                ExecutionScope::Plan {
                    plan_id: "plan".into(),
                    step_id: None,
                },
                vec![PlanItem {
                    id: "3".into(),
                    title: "New".into(),
                    files: vec!["src/new.rs".into()],
                    ..Default::default()
                }],
                tier.clone(),
                Some("2".into()),
            )
            .await
            .unwrap();
        let stored = PlanStorage::load_plan(root, "plan").unwrap().unwrap();
        assert_eq!(stored.implementation.items[0], done);
        assert!(stored.implementation.find_item("2").is_none());
        assert!(stored.implementation.find_item("3").is_some());
        assert_eq!(stored.implementation.architect, Some(tier));
        assert_eq!(
            stored.implementation.last_replanned_step.as_deref(),
            Some("2")
        );
    }

    #[tokio::test]
    async fn missing_group_refuses_horizon_and_blocker_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        ReviewStorage::save_session(
            root,
            &ReviewSession {
                id: "review".into(),
                title: "Review".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let app = Application::default();
        let failure = app
            .replace_implementation_horizon(
                root,
                ExecutionScope::ReviewGroup {
                    review_id: "review".into(),
                    group_id: "missing".into(),
                },
                vec![PlanItem {
                    id: "1".into(),
                    title: "New".into(),
                    ..Default::default()
                }],
                ArchitectTier::from(tauqe_core::models::ModelRef::openrouter("architect/one")),
                None,
            )
            .await
            .unwrap_err();
        assert!(failure.into_reason().contains("not stored"));
        assert!(!app
            .persist_group_blocker(root, "review", "missing", Some("blocked"))
            .await
            .unwrap());
        assert!(ReviewStorage::load_review(root, "review")
            .unwrap()
            .unwrap()
            .groups
            .is_empty());
    }
}
