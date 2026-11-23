use super::implementation::LeafTransitionError;
use super::{Plan, PlanImplementation, PlanItem, PlanItemStatus};
fn item(id: &str, title: &str, status: PlanItemStatus, children: Vec<PlanItem>) -> PlanItem {
    PlanItem {
        id: id.into(),
        title: title.into(),
        status,
        children,
        ..Default::default()
    }
}
#[test]
fn test_leaf_transition_clears_blocker_and_can_finish_without_commit() {
    use PlanItemStatus::{Blocked, Done, InProgress, Todo};

    let mut implementation = PlanImplementation {
        items: vec![item(
            "parent",
            "Parent",
            Todo,
            vec![item("Leaf", "Leaf", Todo, vec![])],
        )],
        ..Default::default()
    };
    assert_eq!(
        implementation.transition_leaf("missing", Blocked, None, None),
        Ok(None)
    );
    assert_eq!(
        implementation.transition_leaf("parent", Blocked, None, None),
        Err(LeafTransitionError::Container("parent".into()))
    );
    assert_eq!(
        implementation.transition_leaf(" leaf ", Blocked, Some(" waiting "), Some("ignored")),
        Ok(Some(true))
    );
    let leaf = implementation.find_item("Leaf").unwrap();
    assert_eq!(leaf.blocked_reason.as_deref(), Some("waiting"));
    assert_eq!(leaf.commit, None);
    assert_eq!(implementation.items[0].status, Blocked);

    assert_eq!(
        implementation.transition_leaf("Leaf", InProgress, None, None),
        Ok(Some(true))
    );
    assert_eq!(
        implementation.find_item("Leaf").unwrap().blocked_reason,
        None
    );
    assert_eq!(
        implementation.transition_leaf("Leaf", InProgress, None, None),
        Ok(Some(false))
    );
    assert_eq!(
        implementation.transition_leaf("Leaf", Done, None, None),
        Ok(Some(true))
    );
    assert_eq!(implementation.find_item("Leaf").unwrap().commit, None);
    assert_eq!(implementation.items[0].status, Done);

    let completed = implementation.clone();
    assert_eq!(
        implementation.transition_leaf("Leaf", Done, None, None),
        Ok(Some(false))
    );
    assert_eq!(
        implementation.transition_leaf("Leaf", Done, None, Some("late")),
        Err(LeafTransitionError::DoneCommit("Leaf".into()))
    );
    assert_eq!(
        implementation.transition_leaf("Leaf", Todo, None, None),
        Err(LeafTransitionError::DoneStatus {
            id: "Leaf".into(),
            status: Todo
        })
    );
    assert_eq!(implementation, completed);
}

#[test]
fn test_normalize_lifecycle_provenance_drops_unearned_claims() {
    let mut done = item("1", "Finished", PlanItemStatus::Done, vec![]);
    done.commit = Some("0123456789abcdef".to_string());
    done.blocked_reason = Some("Resolved long ago".to_string());

    let mut pending = item("2", "Pending", PlanItemStatus::Todo, vec![]);
    pending.commit = Some("fedcba9876543210".to_string());
    pending.blocked_reason = Some("Missing capability: cargo nextest".to_string());

    let mut blocked = item("3", "Blocked", PlanItemStatus::Blocked, vec![]);
    blocked.blocked_reason = Some("Waiting on an external contract".to_string());
    blocked.files = vec!["src/blocked.rs".to_string()];

    let mut container = item("4", "Group", PlanItemStatus::Done, vec![]);
    container.commit = Some("0011223344556677".to_string());
    container.children = vec![item("4.1", "Leaf", PlanItemStatus::Todo, vec![])];

    let mut plan = Plan::from_parts(
        "lifecycle-provenance".to_string(),
        "Lifecycle Provenance".to_string(),
        None,
        0,
        0,
        Vec::new(),
        vec![done, pending, blocked, container],
    );

    plan.implementation.normalize_lifecycle_provenance();

    // A finished step keeps the commit that carried it, while the blocker
    // report it left behind is dropped.
    assert_eq!(
        plan.implementation.items[0].commit.as_deref(),
        Some("0123456789abcdef")
    );
    assert!(plan.implementation.items[0].blocked_reason.is_none());

    // A step that is not done never advertises a change set, and a step that
    // is not blocked never advertises a blocker.
    assert!(plan.implementation.items[1].commit.is_none());
    assert!(plan.implementation.items[1].blocked_reason.is_none());

    // A blocked step keeps the report that explains why it stalled.
    assert_eq!(
        plan.implementation.items[2].blocked_reason.as_deref(),
        Some("Waiting on an external contract")
    );

    // A container is never executed, so it carries no provenance of its own.
    assert!(plan.implementation.items[3].commit.is_none());
    assert!(plan.implementation.items[3].children[0].commit.is_none());
}
