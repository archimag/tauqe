//! Targeting a single leaf sub-step of an architectural group's unified plan.
//!
//! A targeted sub-step is validated through the same plan readiness rules a
//! plan batch uses, so the server never re-implements leaf and dependency
//! semantics for the unified subplan of an architectural group. A review
//! finding owns no subplan: it executes as a single isolated turn.

use super::{
    dependent_subplan_item, execute_group_substep, save_grouped_session, subplan_container,
    subplan_item, test_state, TestDir,
};

use tauqe_protocol::{errors, PlanItemStatus};

#[tokio::test]
async fn group_step_target_requires_a_decomposed_group() {
    let dir = TestDir::new();
    save_grouped_session(&dir, "rev-plain-group", "arch-1", Vec::new());
    let state = test_state(&dir);

    let res = execute_group_substep(&state, "rev-plain-group", "arch-1", "s-1").await;

    // A group without a horizon is synthesized just in time, so a step
    // target cannot name a sub-step of a horizon that does not exist yet.
    assert_eq!(
        res.error.expect("an undecomposed group has no target").code,
        errors::INVALID_PARAMS
    );
}

#[tokio::test]
async fn group_step_target_reports_an_unknown_sub_step() {
    let dir = TestDir::new();
    save_grouped_session(
        &dir,
        "rev-unknown-group-step",
        "arch-1",
        vec![subplan_item(
            "s-1",
            "Step 1",
            PlanItemStatus::Todo,
            vec!["src/target.rs"],
        )],
    );
    let state = test_state(&dir);

    let res = execute_group_substep(&state, "rev-unknown-group-step", "arch-1", "ghost").await;

    assert_eq!(
        res.error.expect("the sub-step is unknown").code,
        errors::NOT_FOUND
    );
}

#[tokio::test]
async fn group_step_target_rejects_a_conceptual_container() {
    let dir = TestDir::new();
    save_grouped_session(
        &dir,
        "rev-container-group",
        "arch-1",
        vec![subplan_container(
            "s-container",
            "Container",
            vec![subplan_item(
                "s-1",
                "Step 1",
                PlanItemStatus::Todo,
                vec!["src/target.rs"],
            )],
        )],
    );
    let state = test_state(&dir);

    let res = execute_group_substep(&state, "rev-container-group", "arch-1", "s-container").await;

    assert_eq!(
        res.error.expect("a container is not executable").code,
        errors::INVALID_PARAMS
    );
}

#[tokio::test]
async fn group_step_target_rejects_a_finished_sub_step() {
    let dir = TestDir::new();
    save_grouped_session(
        &dir,
        "rev-done-group-step",
        "arch-1",
        vec![subplan_item(
            "s-1",
            "Step 1",
            PlanItemStatus::Done,
            vec!["src/target.rs"],
        )],
    );
    let state = test_state(&dir);

    let res = execute_group_substep(&state, "rev-done-group-step", "arch-1", "s-1").await;

    // Decision immutability: a finished group sub-step is never re-executed.
    assert_eq!(
        res.error.expect("a finished sub-step is immutable").code,
        errors::NOTHING_TO_EXECUTE
    );
}

#[tokio::test]
async fn group_step_target_blocks_on_an_unfinished_dependency() {
    let dir = TestDir::new();
    save_grouped_session(
        &dir,
        "rev-group-dependency",
        "arch-1",
        vec![
            subplan_item("s-1", "Step 1", PlanItemStatus::Todo, vec!["src/target.rs"]),
            dependent_subplan_item("s-2", "Step 2", &["s-1"]),
        ],
    );
    let state = test_state(&dir);

    let res = execute_group_substep(&state, "rev-group-dependency", "arch-1", "s-2").await;

    assert_eq!(
        res.error.expect("the dependency is unmet").code,
        errors::STEP_BLOCKED_BY_DEPENDENCIES
    );
}
