use super::*;
use crate::model::{ModelRole, ModelSelection};

fn item(id: &str, title: &str, status: PlanItemStatus, children: Vec<PlanItem>) -> PlanItem {
    PlanItem {
        id: id.to_string(),
        title: title.to_string(),
        status,
        children,
        ..Default::default()
    }
}

#[test]
fn test_plan_refine_types_serde_and_helpers() {
    let params = PlanRefineParams {
        plan_id: "core-feature".to_string(),
        model: Some(ModelSelection::Role(ModelRole::Architect)),
        instructions: Some("Decompose into atomic steps".to_string()),
    };
    let json_params = serde_json::to_string(&params).unwrap();
    assert!(json_params.contains("\"core-feature\""));
    assert!(json_params.contains("\"architect\""));
    let de_params: PlanRefineParams = serde_json::from_str(&json_params).unwrap();
    assert_eq!(de_params, params);

    let plan = Plan {
        id: "core-feature".to_string(),
        title: "Feature".to_string(),
        description: None,
        created_at: 10,
        updated_at: 20,
        concept: vec![],
        items: vec![],
    };

    let success_res = PlanRefineResult::Success {
        plan: plan.clone(),
        explanation: Some("All decomposed".to_string()),
    };
    assert!(success_res.is_success());
    assert!(!success_res.is_blocked());
    assert_eq!(success_res.plan().unwrap().id, "core-feature");
    assert_eq!(success_res.reason(), None);

    let json_success = serde_json::to_string(&success_res).unwrap();
    assert!(json_success.contains("\"status\":\"success\""));
    let de_success: PlanRefineResult = serde_json::from_str(&json_success).unwrap();
    assert_eq!(de_success, success_res);

    let blocked_res = PlanRefineResult::Blocked {
        reason: "Contradiction in requirements".to_string(),
    };
    assert!(!blocked_res.is_success());
    assert!(blocked_res.is_blocked());
    assert_eq!(blocked_res.plan(), None);
    assert_eq!(blocked_res.reason(), Some("Contradiction in requirements"));

    let json_blocked = serde_json::to_string(&blocked_res).unwrap();
    assert!(json_blocked.contains("\"status\":\"blocked\""));
    assert!(json_blocked.contains("Contradiction in requirements"));
    let de_blocked: PlanRefineResult = serde_json::from_str(&json_blocked).unwrap();
    assert_eq!(de_blocked, blocked_res);
}

#[test]
fn test_plan_item_status_lifecycle() {
    let status = PlanItemStatus::Discussion;
    assert_eq!(status.next(), PlanItemStatus::Todo);
    assert_eq!(status.next().next(), PlanItemStatus::InProgress);
    assert_eq!(status.next().next().next(), PlanItemStatus::Done);
    assert_eq!(status.next().next().next().next(), PlanItemStatus::Blocked);
    assert_eq!(status.next().next().next().next().next(), PlanItemStatus::Cancelled);
    assert_eq!(
        status.next().next().next().next().next().next(),
        PlanItemStatus::Discussion
    );
    assert_eq!(status.to_string(), "DISCUSSION");
    assert_eq!(PlanItemStatus::Blocked.to_string(), "BLOCKED");
}

#[test]
fn test_concept_item_and_two_section_plan_serde() {
    let concept = vec![
        ConceptItem {
            id: "c-1".to_string(),
            title: "Stateless Auth".to_string(),
            details: Some("Tokens must be signed with ed25519".to_string()),
            children: vec![ConceptItem {
                id: "c-1.1".to_string(),
                title: "Token Expiration Invariant".to_string(),
                details: None,
                children: vec![],
            }],
        },
    ];

    let mut step = item("s-1", "Implement JWT signer", PlanItemStatus::Todo, vec![]);
    step.files = vec!["crates/core/src/auth.rs".to_string()];
    step.concept_refs = vec!["c-1.1".to_string()];

    let mut plan = Plan {
        id: "auth-plan".to_string(),
        title: "Auth Plan".to_string(),
        description: Some("Architecture & steps".to_string()),
        created_at: 100,
        updated_at: 200,
        concept,
        items: vec![step],
    };

    // Concept navigation
    assert!(plan.find_concept_item("c-1").is_some());
    assert_eq!(plan.find_concept_item("c-1.1").unwrap().title, "Token Expiration Invariant");
    assert!(plan.find_concept_item("non-existent").is_none());

    // Serde roundtrip
    let json = serde_json::to_string(&plan).unwrap();
    assert!(json.contains("\"concept\""));
    assert!(json.contains("\"files\""));
    assert!(json.contains("\"concept_refs\""));

    let de: Plan = serde_json::from_str(&json).unwrap();
    assert_eq!(de, plan);

    // Markdown rendering with concept and files
    let md = plan.to_markdown();
    assert!(md.contains("## Concept"));
    assert!(md.contains("- #c-1: Stateless Auth"));
    assert!(md.contains("    Tokens must be signed with ed25519"));
    assert!(md.contains("  - #c-1.1: Token Expiration Invariant"));
    assert!(md.contains("## Implementation"));
    assert!(md.contains("files: crates/core/src/auth.rs"));
    assert!(md.contains("ref: c-1.1"));

    // Mutate concept item
    if let Some(c) = plan.find_concept_item_mut("c-1.1") {
        c.title = "Updated Invariant".to_string();
    }
    assert_eq!(plan.find_concept_item("c-1.1").unwrap().title, "Updated Invariant");
}

#[test]
fn test_plan_item_serde() {
    let mut it = item("1.1", "Define claims", PlanItemStatus::InProgress, vec![]);
    it.details = Some("Claims struct with exp and sub".to_string());
    it.files = vec!["src/claims.rs".to_string()];
    it.readonly_files = vec!["src/utils.rs".to_string()];
    it.concept_refs = vec!["c-1".to_string()];
    it.depends_on = vec!["1.0".to_string()];

    let json = serde_json::to_string(&it).unwrap();
    assert!(json.contains("\"in_progress\""));
    assert!(json.contains("\"files\""));
    assert!(json.contains("\"readonly_files\""));
    assert!(json.contains("\"concept_refs\""));
    assert!(json.contains("\"depends_on\""));

    let de: PlanItem = serde_json::from_str(&json).unwrap();
    assert_eq!(de, it);

    // Backward compatibility: missing fields deserialize to empty defaults.
    let legacy_json = r#"{"id":"1.1","title":"Define claims","status":"in_progress"}"#;
    let legacy_item: PlanItem = serde_json::from_str(legacy_json).unwrap();
    assert!(legacy_item.files.is_empty());
    assert!(legacy_item.readonly_files.is_empty());
    assert!(legacy_item.concept_refs.is_empty());
    assert!(legacy_item.depends_on.is_empty());

    let legacy_plan_json = r#"{"id":"p1","title":"Plan","created_at":1,"updated_at":2,"items":[]}"#;
    let legacy_plan: Plan = serde_json::from_str(legacy_plan_json).unwrap();
    assert!(legacy_plan.concept.is_empty());
}

#[test]
fn test_plan_item_ignores_legacy_model_field() {
    let legacy: PlanItem = serde_json::from_str(r#"{"id":"2","title":"Old step","model":"junior"}"#).unwrap();
    assert_eq!(legacy.id, "2");
    assert_eq!(legacy.title, "Old step");
}

#[test]
fn test_plan_stats_and_summary() {
    let plan = Plan {
        id: "jwt-auth".to_string(),
        title: "JWT Authentication".to_string(),
        description: Some("Replace session cookies".to_string()),
        created_at: 1700000000,
        updated_at: 1700000010,
        concept: vec![],
        items: vec![
            item(
                "1",
                "Models",
                PlanItemStatus::Done,
                vec![item("1.1", "Claims", PlanItemStatus::Done, vec![])],
            ),
            item("2", "Endpoint", PlanItemStatus::InProgress, vec![]),
        ],
    };

    // Progress counts executable leaves only: the "1" container is a conceptual
    // milestone and is not double-counted alongside its "1.1" child.
    let (total, completed) = plan.count_stats();
    assert_eq!(total, 2);
    assert_eq!(completed, 1);

    let summary = plan.summary();
    assert_eq!(summary.total_items, 2);
    assert_eq!(summary.completed_items, 1);
    assert_eq!(summary.id, "jwt-auth");
    assert_eq!(summary.status, PlanItemStatus::InProgress);
    assert_eq!(plan.status(), PlanItemStatus::InProgress);
}

#[test]
fn test_count_stats_reports_only_leaves() {
    let plan = Plan {
        id: "count-leaves".to_string(),
        title: "Count Leaves".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item(
                "1",
                "Group",
                PlanItemStatus::Todo,
                vec![
                    item("1.1", "A", PlanItemStatus::Done, vec![]),
                    item("1.2", "B", PlanItemStatus::Done, vec![]),
                ],
            ),
            item("2", "Leaf", PlanItemStatus::Todo, vec![]),
        ],
    };

    // Containers are conceptual and carry no authoritative status, so progress
    // counts the three executable leaves, never the container alongside them.
    assert_eq!(plan.count_stats(), (3, 2));
    let summary = plan.summary();
    assert_eq!(summary.total_items, 3);
    assert_eq!(summary.completed_items, 2);
}

#[test]
fn test_count_stats_excludes_cancelled_leaves_and_agrees_with_done_status() {
    let plan = Plan {
        id: "done-with-cancelled".to_string(),
        title: "Done With Cancelled".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item("1", "Done step", PlanItemStatus::Done, vec![]),
            item("2", "Cancelled step", PlanItemStatus::Cancelled, vec![]),
        ],
    };

    // `compute_items_status` treats the plan as Done because every active
    // (non-cancelled) leaf is Done. The progress metric must agree: the cancelled
    // leaf is excluded from `total`, so the ratio stays `1/1`, never `1/2`.
    assert_eq!(plan.status(), PlanItemStatus::Done);
    assert_eq!(plan.count_stats(), (1, 1));

    let summary = plan.summary();
    assert_eq!(summary.status, PlanItemStatus::Done);
    assert_eq!(summary.total_items, 1);
    assert_eq!(summary.completed_items, 1);
}

#[test]
fn test_count_stats_excludes_cancelled_leaves_in_mixed_plan() {
    let plan = Plan {
        id: "mixed-cancelled".to_string(),
        title: "Mixed Cancelled".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item("1", "Done step", PlanItemStatus::Done, vec![]),
            item("2", "Todo step", PlanItemStatus::Todo, vec![]),
            item("3", "Cancelled step", PlanItemStatus::Cancelled, vec![]),
        ],
    };

    // A mix of Done and Todo leaves keeps the plan InProgress; the cancelled leaf
    // is excluded from the denominator, so the ratio reflects only executable
    // remaining work.
    assert_eq!(plan.status(), PlanItemStatus::InProgress);
    assert_eq!(plan.count_stats(), (2, 1));
}

#[test]
fn test_count_stats_all_cancelled_reports_zero_and_cancelled_status() {
    let plan = Plan {
        id: "all-cancelled".to_string(),
        title: "All Cancelled".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item("1", "Cancelled A", PlanItemStatus::Cancelled, vec![]),
            item("2", "Cancelled B", PlanItemStatus::Cancelled, vec![]),
        ],
    };

    // With every leaf cancelled there is no executable work left, so the metric
    // is `0/0` and the aggregated status is Cancelled rather than a misleading
    // `0/2` against a cancelled plan.
    assert_eq!(plan.status(), PlanItemStatus::Cancelled);
    assert_eq!(plan.count_stats(), (0, 0));
}

#[test]
fn test_evaluate_scope_reports_only_leaves_for_discussion() {
    let mut plan = Plan {
        id: "leaf-only-scope".to_string(),
        title: "Leaf Only Scope".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![item(
            "1",
            "Group",
            PlanItemStatus::Todo,
            vec![
                item("1.1", "Ready", PlanItemStatus::Todo, vec![]),
                item("1.2", "Discuss", PlanItemStatus::Discussion, vec![]),
            ],
        )],
    };

    // Refreshing propagates the Discussion child onto its container. Only the
    // offending leaf is reported, never the container as well.
    plan.refresh_parent_statuses();
    assert_eq!(plan.find_item("1").unwrap().status, PlanItemStatus::Discussion);

    match plan.evaluate_scope(None) {
        PlanScopeEvaluation::BlockedByDiscussion(items) => {
            assert_eq!(items.len(), 1);
            assert!(items[0].contains("1.2"));
        }
        other => panic!("Expected BlockedByDiscussion, got {:?}", other),
    }

    match plan.evaluate_scope(Some("1")) {
        PlanScopeEvaluation::BlockedByDiscussion(items) => {
            assert_eq!(items.len(), 1);
            assert!(items[0].contains("1.2"));
        }
        other => panic!("Expected BlockedByDiscussion, got {:?}", other),
    }
}

#[test]
fn test_evaluate_scope_dependency_uses_derived_container_status() {
    let mut dependent = item("2", "Dependent", PlanItemStatus::Todo, vec![]);
    dependent.depends_on = vec!["1".to_string()];

    let plan = Plan {
        id: "parent-dep".to_string(),
        title: "Parent Dep".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item(
                "1",
                "Group",
                PlanItemStatus::Todo,
                vec![item("1.1", "A", PlanItemStatus::Done, vec![])],
            ),
            dependent,
        ],
    };

    // The container's stored status is stale (Todo) while all of its children
    // are Done. Readiness derives the container status from its children instead
    // of trusting the stale stored value, so the dependent step is ready.
    match plan.evaluate_scope(None) {
        PlanScopeEvaluation::Ready(leaves) => {
            let ids: Vec<&str> = leaves.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(ids, vec!["2"]);
        }
        other => panic!("Expected Ready, got {:?}", other),
    }
}

#[test]
fn test_plan_status_computation() {
    let mut plan = Plan {
        id: "status-test".to_string(),
        title: "Status Test".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item("1", "Step 1", PlanItemStatus::Todo, vec![]),
            item("2", "Step 2", PlanItemStatus::Todo, vec![]),
        ],
    };

    // All Todo -> Todo
    assert_eq!(plan.status(), PlanItemStatus::Todo);

    // One InProgress -> InProgress
    plan.items[0].status = PlanItemStatus::InProgress;
    assert_eq!(plan.status(), PlanItemStatus::InProgress);

    // One Done, one Todo -> InProgress
    plan.items[0].status = PlanItemStatus::Done;
    assert_eq!(plan.status(), PlanItemStatus::InProgress);

    // One Discussion -> Discussion (top priority)
    plan.items[1].status = PlanItemStatus::Discussion;
    assert_eq!(plan.status(), PlanItemStatus::Discussion);

    // All Done -> Done
    plan.items[1].status = PlanItemStatus::Done;
    assert_eq!(plan.status(), PlanItemStatus::Done);

    // One Cancelled, one Done -> Done
    plan.items[0].status = PlanItemStatus::Cancelled;
    assert_eq!(plan.status(), PlanItemStatus::Done);

    // All Cancelled -> Cancelled
    plan.items[1].status = PlanItemStatus::Cancelled;
    assert_eq!(plan.status(), PlanItemStatus::Cancelled);
}

#[test]
fn test_plan_item_leaf_and_refresh_parent_statuses() {
    let mut plan = Plan {
        id: "leaf-test".to_string(),
        title: "Leaf Test".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![item(
            "parent",
            "Parent",
            PlanItemStatus::Todo,
            vec![
                item("sub-1", "Sub 1", PlanItemStatus::Todo, vec![]),
                item("sub-2", "Sub 2", PlanItemStatus::Todo, vec![]),
            ],
        )],
    };

    assert_eq!(plan.is_leaf_item("parent"), Some(false));
    assert_eq!(plan.is_leaf_item("sub-1"), Some(true));
    assert_eq!(plan.is_leaf_item("unknown"), None);

    // Update first sub-item to InProgress: parent becomes InProgress
    plan.update_item_status("sub-1", PlanItemStatus::InProgress);
    assert_eq!(plan.find_item("parent").unwrap().status, PlanItemStatus::InProgress);

    // Update first sub-item to Discussion: parent becomes Discussion (highest attention priority)
    plan.update_item_status("sub-1", PlanItemStatus::Discussion);
    assert_eq!(plan.find_item("parent").unwrap().status, PlanItemStatus::Discussion);

    // Update first sub-item to Done, second is still Todo: parent becomes InProgress
    plan.update_item_status("sub-1", PlanItemStatus::Done);
    assert_eq!(plan.find_item("parent").unwrap().status, PlanItemStatus::InProgress);

    // Update second sub-item to done: parent automatically transitions to Done
    plan.update_item_status("sub-2", PlanItemStatus::Done);
    assert_eq!(plan.find_item("parent").unwrap().status, PlanItemStatus::Done);
}

#[test]
fn test_plan_normalize_leaf_files() {
    let mut inner_leaf = item("1.1", "Inner Leaf", PlanItemStatus::Todo, vec![]);
    inner_leaf.files = vec!["src/inner.rs".to_string(), "src/shared.rs".to_string()];
    inner_leaf.readonly_files = vec!["src/ref.rs".to_string(), "src/shared.rs".to_string()];
    inner_leaf.depends_on = vec!["2".to_string()];

    let mut mid = item("1", "Mid Group", PlanItemStatus::Todo, vec![inner_leaf]);
    mid.files = vec!["src/mid.rs".to_string()];
    mid.readonly_files = vec!["src/mid_ref.rs".to_string()];
    mid.depends_on = vec!["2".to_string()];

    let mut top = item("root", "Root Group", PlanItemStatus::Todo, vec![mid]);
    top.files = vec!["src/top.rs".to_string()];
    top.depends_on = vec!["2".to_string()];

    let mut leaf = item("2", "Top Leaf", PlanItemStatus::Todo, vec![]);
    leaf.files = vec!["src/leaf.rs".to_string()];
    leaf.readonly_files = vec!["src/leaf_ref.rs".to_string()];

    let mut plan = Plan {
        id: "leaf-files".to_string(),
        title: "Leaf Files".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![top, leaf],
    };

    plan.normalize_leaf_files();

    // Containers are cleared at every depth.
    assert!(plan.items[0].files.is_empty());
    assert!(plan.items[0].readonly_files.is_empty());
    assert!(plan.items[0].depends_on.is_empty());
    assert!(plan.items[0].children[0].files.is_empty());
    assert!(plan.items[0].children[0].readonly_files.is_empty());
    assert!(plan.items[0].children[0].depends_on.is_empty());
    // Leaves keep their files, overlapping readonly_files are removed.
    assert_eq!(
        plan.items[0].children[0].children[0].files,
        vec!["src/inner.rs".to_string(), "src/shared.rs".to_string()]
    );
    assert_eq!(
        plan.items[0].children[0].children[0].readonly_files,
        vec!["src/ref.rs".to_string()]
    );
    assert_eq!(plan.items[1].files, vec!["src/leaf.rs".to_string()]);
    assert_eq!(plan.items[1].readonly_files, vec!["src/leaf_ref.rs".to_string()]);
    assert_eq!(
        plan.items[0].children[0].children[0].depends_on,
        vec!["2".to_string()]
    );
}

#[test]
fn test_normalize_leaf_files_dedups_readonly_by_canonical_path() {
    let mut leaf = item("1", "Leaf", PlanItemStatus::Todo, vec![]);
    // The same file spelled three different ways is one path: `files` wins and
    // every cosmetically different `readonly_files` spelling is dropped, while a
    // genuinely distinct reference file is preserved.
    leaf.files = vec!["src/keep.rs".to_string()];
    leaf.readonly_files = vec![
        "./src/keep.rs".to_string(),
        "src//keep.rs".to_string(),
        r"src\keep.rs".to_string(),
        "src/ref.rs".to_string(),
    ];

    let mut plan = Plan {
        id: "leaf-readonly-dedup".to_string(),
        title: "Leaf Readonly Dedup".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![leaf],
    };

    plan.normalize_leaf_files();

    assert_eq!(plan.items[0].files, vec!["src/keep.rs".to_string()]);
    assert_eq!(plan.items[0].readonly_files, vec!["src/ref.rs".to_string()]);
}

#[test]
fn test_plan_evaluate_scope() {
    let plan = Plan {
        id: "batch-test".to_string(),
        title: "Batch Test".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item(
                "1",
                "Parent Group",
                PlanItemStatus::Todo,
                vec![
                    item("1.1", "Step 1.1", PlanItemStatus::Todo, vec![]),
                    item("1.2", "Step 1.2", PlanItemStatus::Discussion, vec![]),
                ],
            ),
            item("2", "Step 2", PlanItemStatus::Todo, vec![]),
        ],
    };

    // 1. Entire plan has step 1.2 in Discussion -> Blocked
    match plan.evaluate_scope(None) {
        PlanScopeEvaluation::BlockedByDiscussion(items) => {
            assert_eq!(items.len(), 1);
            assert!(items[0].contains("1.2"));
        }
        other => panic!("Expected BlockedByDiscussion, got {:?}", other),
    }

    // 2. Leaf step 1.1 is Todo -> Ready with 1 item
    match plan.evaluate_scope(Some("1.1")) {
        PlanScopeEvaluation::Ready(leaves) => {
            assert_eq!(leaves.len(), 1);
            assert_eq!(leaves[0].id, "1.1");
        }
        other => panic!("Expected Ready, got {:?}", other),
    }

    // 3. Leaf step 1.2 is Discussion -> Blocked
    match plan.evaluate_scope(Some("1.2")) {
        PlanScopeEvaluation::BlockedByDiscussion(items) => {
            assert_eq!(items.len(), 1);
        }
        other => panic!("Expected BlockedByDiscussion, got {:?}", other),
    }

    // 4. Once 1.2 is approved to Todo -> entire plan is Ready with 3 items
    let mut clean_plan = plan.clone();
    clean_plan.update_item_status("1.2", PlanItemStatus::Todo);
    match clean_plan.evaluate_scope(None) {
        PlanScopeEvaluation::Ready(leaves) => {
            assert_eq!(leaves.len(), 3);
            assert_eq!(leaves[0].id, "1.1");
            assert_eq!(leaves[1].id, "1.2");
            assert_eq!(leaves[2].id, "2");
        }
        other => panic!("Expected Ready, got {:?}", other),
    }

    // 5. Group 1 is Ready with 2 items
    match clean_plan.evaluate_scope(Some("1")) {
        PlanScopeEvaluation::Ready(leaves) => {
            assert_eq!(leaves.len(), 2);
            assert_eq!(leaves[0].id, "1.1");
            assert_eq!(leaves[1].id, "1.2");
        }
        other => panic!("Expected Ready, got {:?}", other),
    }
}

#[test]
fn test_plan_evaluate_scope_dependencies() {
    let first = item("1", "First", PlanItemStatus::Todo, vec![]);
    let mut second = item("2", "Second", PlanItemStatus::Todo, vec![]);
    second.depends_on = vec!["1".to_string()];
    let mut third = item("3", "Third", PlanItemStatus::Todo, vec![]);
    third.depends_on = vec!["2".to_string()];

    let plan = Plan {
        id: "dag".to_string(),
        title: "DAG".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![first, second, third],
    };

    // The whole chain is scheduled in dependency order.
    match plan.evaluate_scope(None) {
        PlanScopeEvaluation::Ready(leaves) => {
            let ids: Vec<&str> = leaves.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(ids, vec!["1", "2", "3"]);
        }
        other => panic!("Expected Ready, got {:?}", other),
    }

    // A step whose dependency is not DONE yet is blocked when targeted, surfacing unmet prerequisites.
    match plan.evaluate_scope(Some("2")) {
        PlanScopeEvaluation::BlockedByDependencies(items) => {
            assert_eq!(items.len(), 1);
            assert!(items[0].contains("#2"));
            assert!(items[0].contains("unmet dependencies: #1"));
        }
        other => panic!("Expected BlockedByDependencies, got {:?}", other),
    }

    // A step in BLOCKED status cannot be executed.
    let mut blocked_plan = plan.clone();
    blocked_plan.update_item_status("1", PlanItemStatus::Blocked);
    assert_eq!(
        blocked_plan.evaluate_scope(Some("1")),
        PlanScopeEvaluation::StepBlocked
    );
}

#[test]
fn test_plan_evaluate_scope_surfaces_blocked_leaves() {
    // A plan whose remaining leaves are all BLOCKED must not be reported as
    // completed: each blocked leaf is surfaced by ID.
    let all_blocked = Plan {
        id: "blocked-all".to_string(),
        title: "Blocked All".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item("1", "Stuck A", PlanItemStatus::Blocked, vec![]),
            item("2", "Stuck B", PlanItemStatus::Blocked, vec![]),
        ],
    };
    match all_blocked.evaluate_scope(None) {
        PlanScopeEvaluation::BlockedByBlockers(items) => {
            assert_eq!(items.len(), 2);
            assert!(items[0].contains("#1"));
            assert!(items[1].contains("#2"));
        }
        other => panic!("Expected BlockedByBlockers, got {:?}", other),
    }

    // A mix of ready and blocked leaves surfaces the blocked ones instead of
    // silently skipping them.
    let mixed = Plan {
        id: "blocked-mixed".to_string(),
        title: "Blocked Mixed".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item("1", "Ready", PlanItemStatus::Todo, vec![]),
            item("2", "Stuck", PlanItemStatus::Blocked, vec![]),
        ],
    };
    match mixed.evaluate_scope(None) {
        PlanScopeEvaluation::BlockedByBlockers(items) => {
            assert_eq!(items.len(), 1);
            assert!(items[0].contains("#2"));
        }
        other => panic!("Expected BlockedByBlockers, got {:?}", other),
    }

    // Composite scopes surface blocked children the same way.
    let group = Plan {
        id: "blocked-group".to_string(),
        title: "Blocked Group".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![item(
            "g",
            "Group",
            PlanItemStatus::Blocked,
            vec![
                item("g.1", "Ready", PlanItemStatus::Todo, vec![]),
                item("g.2", "Stuck", PlanItemStatus::Blocked, vec![]),
            ],
        )],
    };
    match group.evaluate_scope(Some("g")) {
        PlanScopeEvaluation::BlockedByBlockers(items) => {
            assert_eq!(items.len(), 1);
            assert!(items[0].contains("#g.2"));
        }
        other => panic!("Expected BlockedByBlockers, got {:?}", other),
    }
}

#[test]
fn test_plan_update_item_status_matches_ids_case_insensitively() {
    let mut plan = Plan {
        id: "case".to_string(),
        title: "Case".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![item("Step-1", "Step", PlanItemStatus::Todo, vec![])],
    };

    // `find_item` and `update_item_status` must agree on the id matching rule.
    assert!(plan.find_item("step-1").is_some());
    assert!(plan.update_item_status("step-1", PlanItemStatus::Done));
    assert_eq!(plan.find_item("STEP-1").unwrap().status, PlanItemStatus::Done);
}

#[test]
fn test_plan_update_item_status_rejects_container_targets() {
    let mut plan = Plan {
        id: "container-guard".to_string(),
        title: "Container Guard".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![item(
            "g",
            "Group",
            PlanItemStatus::Todo,
            vec![item("g.1", "Child", PlanItemStatus::Todo, vec![])],
        )],
    };

    // A container target is rejected: its status is derived from its children, so
    // the assignment would be recomputed away immediately. The caller is told the
    // update did not happen instead of silently losing the change.
    assert!(!plan.update_item_status("g", PlanItemStatus::Done));
    assert_eq!(plan.find_item("g").unwrap().status, PlanItemStatus::Todo);

    // A leaf target is updated and its ancestors are refreshed.
    assert!(plan.update_item_status("g.1", PlanItemStatus::Done));
    assert_eq!(plan.find_item("g.1").unwrap().status, PlanItemStatus::Done);
    assert_eq!(plan.find_item("g").unwrap().status, PlanItemStatus::Done);

    // An unknown identifier is rejected as well.
    assert!(!plan.update_item_status("missing", PlanItemStatus::Done));
}

#[test]
fn test_plan_to_markdown() {
    let mut models = item(
        "1",
        "Models",
        PlanItemStatus::Done,
        vec![item("1.1", "Claims", PlanItemStatus::Done, vec![])],
    );
    models.details = Some("Create token struct".to_string());
    let mut endpoint = item("2", "Endpoint", PlanItemStatus::InProgress, vec![]);
    endpoint.readonly_files = vec!["src/models.rs".to_string()];

    let plan = Plan {
        id: "jwt-auth".to_string(),
        title: "JWT Authentication".to_string(),
        description: Some("Replace session cookies".to_string()),
        created_at: 1700000000,
        updated_at: 1700000010,
        concept: vec![],
        items: vec![models, endpoint],
    };

    let md = plan.to_markdown();
    assert!(md.contains("# Plan: JWT Authentication"));
    assert!(md.contains("Replace session cookies"));
    assert!(md.contains("- [x] DONE #1: Models"));
    assert!(md.contains("    Create token struct"));
    assert!(md.contains("  - [x] DONE #1.1: Claims"));
    assert!(md.contains("- [>] IN_PROGRESS #2: Endpoint"));
    assert!(md.contains("    readonly_files: src/models.rs"));
}

#[test]
fn test_plan_validate_success() {
    let plan = Plan {
        id: "valid-plan".to_string(),
        title: "Valid".to_string(),
        concept: vec![ConceptItem {
            id: "c-1".to_string(),
            title: "Arch Invariant".to_string(),
            details: None,
            children: vec![],
        }],
        items: vec![
            item("1", "Step 1", PlanItemStatus::Done, vec![]),
            PlanItem {
                id: "2".to_string(),
                title: "Step 2".to_string(),
                status: PlanItemStatus::Todo,
                depends_on: vec!["1".to_string()],
                concept_refs: vec!["c-1".to_string()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert!(plan.validate().is_ok());
}

#[test]
fn test_plan_validate_item_errors() {
    // 1. Empty item ID
    let empty_id = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        items: vec![item("  ", "Empty", PlanItemStatus::Todo, vec![])],
        ..Default::default()
    };
    assert_eq!(empty_id.validate().unwrap_err(), vec![PlanValidationError::EmptyItemId]);

    // 2. Duplicate item ID (case-insensitive)
    let dup_id = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        items: vec![
            item("Step-1", "A", PlanItemStatus::Todo, vec![]),
            item("step-1", "B", PlanItemStatus::Todo, vec![]),
        ],
        ..Default::default()
    };
    assert_eq!(
        dup_id.validate().unwrap_err(),
        vec![PlanValidationError::DuplicateItemId("step-1".to_string())]
    );

    // 2b. Empty concept ID (including whitespace-only and nested)
    let empty_concept = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        concept: vec![ConceptItem {
            id: "  ".to_string(),
            title: "Empty root".to_string(),
            details: None,
            children: vec![ConceptItem {
                id: "".to_string(),
                title: "Empty child".to_string(),
                details: None,
                children: vec![],
            }],
        }],
        ..Default::default()
    };
    assert_eq!(
        empty_concept.validate().unwrap_err(),
        vec![
            PlanValidationError::EmptyConceptId,
            PlanValidationError::EmptyConceptId,
        ]
    );

    // 3. Duplicate concept ID
    let dup_concept = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        concept: vec![
            ConceptItem {
                id: "c-1".to_string(),
                title: "A".to_string(),
                details: None,
                children: vec![],
            },
            ConceptItem {
                id: "C-1".to_string(),
                title: "B".to_string(),
                details: None,
                children: vec![],
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        dup_concept.validate().unwrap_err(),
        vec![PlanValidationError::DuplicateConceptId("C-1".to_string())]
    );

    // 4. Missing concept ref
    let missing_concept = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        items: vec![PlanItem {
            id: "1".to_string(),
            title: "Step 1".to_string(),
            concept_refs: vec!["c-missing".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        missing_concept.validate().unwrap_err(),
        vec![PlanValidationError::MissingConceptRef {
            item_id: "1".to_string(),
            concept_ref: "c-missing".to_string(),
        }]
    );
}

#[test]
fn test_plan_validate_dependency_errors() {
    // 1. Missing dependency
    let missing_dep = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        items: vec![PlanItem {
            id: "1".to_string(),
            title: "Step 1".to_string(),
            depends_on: vec!["ghost".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        missing_dep.validate().unwrap_err(),
        vec![PlanValidationError::MissingDependency {
            item_id: "1".to_string(),
            depends_on: "ghost".to_string(),
        }]
    );

    // 2. Self dependency
    let self_dep = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        items: vec![PlanItem {
            id: "1".to_string(),
            title: "Step 1".to_string(),
            depends_on: vec!["1".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        self_dep.validate().unwrap_err(),
        vec![PlanValidationError::SelfDependency("1".to_string())]
    );

    // 3. Cyclic dependency
    let cyclic = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        items: vec![
            PlanItem {
                id: "A".to_string(),
                title: "Step A".to_string(),
                depends_on: vec!["B".to_string()],
                ..Default::default()
            },
            PlanItem {
                id: "B".to_string(),
                title: "Step B".to_string(),
                depends_on: vec!["A".to_string()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let errs = cyclic.validate().unwrap_err();
    assert_eq!(errs.len(), 1);
    match &errs[0] {
        PlanValidationError::CyclicDependency(cycle) => {
            assert!(cycle.contains(&"A".to_string()));
            assert!(cycle.contains(&"B".to_string()));
        }
        other => panic!("Expected CyclicDependency, got {:?}", other),
    }

    // Display implementation
    assert_eq!(
        PlanValidationError::EmptyItemId.to_string(),
        "Step ID cannot be empty"
    );
    assert_eq!(
        PlanValidationError::EmptyConceptId.to_string(),
        "Concept ID cannot be empty"
    );
    assert_eq!(
        PlanValidationError::SelfDependency("1".to_string()).to_string(),
        "Step '1' cannot depend on itself"
    );
}

#[test]
fn test_identifier_normalization_is_consistent_across_validation_lookup_and_readiness() {
    let mut dependent = item("2", "Dependent", PlanItemStatus::Todo, vec![]);
    // The dependency spelling differs from the stored id only by surrounding
    // whitespace; validation trims it, so readiness must resolve it too.
    dependent.depends_on = vec!["1 ".to_string()];

    let plan = Plan {
        id: "normalized-ids".to_string(),
        title: "Normalized Ids".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        concept: vec![],
        items: vec![
            item(" 1 ", "Producer", PlanItemStatus::Done, vec![]),
            dependent,
        ],
    };

    // Validation accepts the plan: the dependency key collapses onto the item key.
    assert!(plan.validate().is_ok());

    // Lookup by any spelling of the id resolves to the same step.
    assert!(plan.find_item("1").is_some());
    assert!(plan.find_item(" 1 ").is_some());

    // The dependency `"1 "` is satisfied by the Done producer `" 1 "`, so the
    // dependent step is scheduled instead of staying blocked forever.
    match plan.evaluate_scope(None) {
        PlanScopeEvaluation::Ready(leaves) => {
            let ids: Vec<&str> = leaves.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(ids, vec!["2"]);
        }
        other => panic!("Expected Ready, got {:?}", other),
    }
}

#[test]
fn test_plan_validate_detects_leaf_depending_on_its_own_ancestor() {
    // A leaf that lists its own container in `depends_on` can never be ready: the
    // container is `Done` only when all of its descendant leaves are `Done`.
    // `normalize_leaf_files` leaves no explicit `depends_on` on the container, so
    // the deadlock is implicit and only validation can surface it.
    let mut leaf = item("1.1", "Child", PlanItemStatus::Todo, vec![]);
    leaf.depends_on = vec!["1".to_string()];

    let plan = Plan {
        id: "ancestor-dep".to_string(),
        title: "Ancestor Dep".to_string(),
        items: vec![item("1", "Group", PlanItemStatus::Todo, vec![leaf])],
        ..Default::default()
    };

    let errs = plan.validate().unwrap_err();
    assert_eq!(errs.len(), 1);
    match &errs[0] {
        PlanValidationError::CyclicDependency(cycle) => {
            assert!(cycle.contains(&"1".to_string()));
            assert!(cycle.contains(&"1.1".to_string()));
        }
        other => panic!("Expected CyclicDependency, got {:?}", other),
    }
}

#[test]
fn test_plan_validate_detects_transitive_ancestor_deadlock() {
    // A leaf nested two levels deep that depends on the top container closes the
    // same implicit cycle across nesting levels: the top container cannot finish
    // without the deep leaf, and the deep leaf cannot start without the container.
    let mut deep = item("1.a.1", "Deep leaf", PlanItemStatus::Todo, vec![]);
    deep.depends_on = vec!["1".to_string()];

    let plan = Plan {
        id: "deep-ancestor-dep".to_string(),
        title: "Deep Ancestor Dep".to_string(),
        items: vec![item(
            "1",
            "Top",
            PlanItemStatus::Todo,
            vec![item("1.a", "Mid", PlanItemStatus::Todo, vec![deep])],
        )],
        ..Default::default()
    };

    match plan.validate().unwrap_err().as_slice() {
        [PlanValidationError::CyclicDependency(cycle)] => {
            assert!(cycle.contains(&"1".to_string()));
            assert!(cycle.contains(&"1.a.1".to_string()));
        }
        other => panic!("Expected a single CyclicDependency, got {:?}", other),
    }
}
