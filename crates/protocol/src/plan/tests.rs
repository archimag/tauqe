use super::*;
use crate::model::{ModelRef, ModelRole, ModelSelection, ReasoningEffort};

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
        architect: None,
        last_replanned_step: None,
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

/// The legacy `id` spelling of `plan/refine` lives on the DTO, so the wire
/// contract stays the single parser for every accepted field name, and a
/// malformed field fails the typed parse instead of being dropped by a
/// call-site fallback.
#[test]
fn test_plan_refine_params_accepts_legacy_id_alias() {
    let params: PlanRefineParams = serde_json::from_value(serde_json::json!({
        "id": "legacy-plan",
        "model": "architect",
        "instructions": "Decompose"
    }))
    .unwrap();
    assert_eq!(params.plan_id, "legacy-plan");
    assert_eq!(
        params.model,
        Some(ModelSelection::Role(ModelRole::Architect))
    );
    assert_eq!(params.instructions.as_deref(), Some("Decompose"));

    assert!(
        serde_json::from_value::<PlanRefineParams>(serde_json::json!({ "plan_id": 123 })).is_err()
    );
}

/// The client requests the active plan with an empty object, so the DTO —
/// not a server-side fallback — defines the empty payload as "no target id",
/// while a malformed id still fails the typed parse.
#[test]
fn test_plan_get_params_defaults_the_absent_id() {
    let params: PlanGetParams = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(params.id, "");
    assert!(serde_json::from_value::<PlanGetParams>(serde_json::json!({ "id": 123 })).is_err());
}

#[test]
fn test_plan_execute_params_carry_coder_selection_and_instructions() {
    // The starting rung of the coder ladder travels whole: the model together
    // with the reasoning depth it was configured with, so the server never
    // re-derives the depth from the configured role lists.
    let params = PlanExecuteParams {
        plan_id: "core-feature".to_string(),
        step_id: Some("2".to_string()),
        model: Some(ModelSelection::Tier {
            model: ModelRef::openrouter("anthropic/claude-3.5-sonnet"),
            effort: Some(ReasoningEffort::High),
        }),
        instructions: Some("Keep the public API stable".to_string()),
    };
    let json = serde_json::to_string(&params).unwrap();
    assert!(json.contains("\"effort\":\"high\""), "{json}");
    assert!(json.contains("Keep the public API stable"), "{json}");
    let de: PlanExecuteParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, params);

    // A request that predates the fields keeps decoding unchanged and reports
    // no explicit selection, so the ladder starts at its base rung.
    let legacy: PlanExecuteParams = serde_json::from_value(serde_json::json!({
        "plan_id": "legacy-plan",
        "step_id": "1"
    }))
    .unwrap();
    assert_eq!(legacy.plan_id, "legacy-plan");
    assert!(legacy.model.is_none());
    assert!(legacy.instructions.is_none());

    // An absent selection stays out of the wire form instead of emitting
    // explicit nulls.
    let bare_json = serde_json::to_string(&legacy).unwrap();
    assert!(!bare_json.contains("model"), "{bare_json}");
    assert!(!bare_json.contains("instructions"), "{bare_json}");

    // A malformed selection fails the typed parse rather than being dropped.
    assert!(
        serde_json::from_value::<PlanExecuteParams>(serde_json::json!({
            "plan_id": "p",
            "model": 42
        }))
        .is_err()
    );
}

#[test]
fn test_plan_execute_pipeline_params_carry_coder_selection_and_instructions() {
    let params = PlanExecutePipelineParams {
        plan_ids: vec!["plan-a".to_string(), "plan-b".to_string()],
        model: Some(ModelSelection::Role(ModelRole::Coder)),
        instructions: Some("Run the queue end to end".to_string()),
    };
    let json = serde_json::to_string(&params).unwrap();
    assert!(json.contains("\"coder\""), "{json}");
    assert!(json.contains("Run the queue end to end"), "{json}");
    let de: PlanExecutePipelineParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, params);

    // A legacy queue payload still decodes to an empty selection and empty
    // directives, so the pipeline starts from the base rung of the ladder.
    let legacy: PlanExecutePipelineParams =
        serde_json::from_value(serde_json::json!({ "plan_ids": ["plan-a"] })).unwrap();
    assert_eq!(legacy.plan_ids, vec!["plan-a".to_string()]);
    assert!(legacy.model.is_none());
    assert!(legacy.instructions.is_none());

    // An absent selection adds no key to the document.
    let bare_json = serde_json::to_string(&legacy).unwrap();
    assert!(!bare_json.contains("model"), "{bare_json}");
    assert!(!bare_json.contains("instructions"), "{bare_json}");
}

#[test]
fn test_plan_item_status_lifecycle() {
    let status = PlanItemStatus::Discussion;
    assert_eq!(status.next(), PlanItemStatus::Todo);
    assert_eq!(status.next().next(), PlanItemStatus::InProgress);
    assert_eq!(status.next().next().next(), PlanItemStatus::Done);
    assert_eq!(status.next().next().next().next(), PlanItemStatus::Blocked);
    assert_eq!(
        status.next().next().next().next().next(),
        PlanItemStatus::Cancelled
    );
    assert_eq!(
        status.next().next().next().next().next().next(),
        PlanItemStatus::Discussion
    );
    assert_eq!(status.to_string(), "DISCUSSION");
    assert_eq!(PlanItemStatus::Blocked.to_string(), "BLOCKED");
}

#[test]
fn test_concept_item_and_two_section_plan_serde() {
    let concept = vec![ConceptItem {
        id: "c-1".to_string(),
        title: "Stateless Auth".to_string(),
        details: Some("Tokens must be signed with ed25519".to_string()),
        children: vec![ConceptItem {
            id: "c-1.1".to_string(),
            title: "Token Expiration Invariant".to_string(),
            details: None,
            children: vec![],
        }],
    }];

    let mut step = item("s-1", "Implement JWT signer", PlanItemStatus::Todo, vec![]);
    step.files = vec!["crates/core/src/auth.rs".to_string()];
    step.concept_refs = vec!["c-1.1".to_string()];

    let mut plan = Plan {
        id: "auth-plan".to_string(),
        title: "Auth Plan".to_string(),
        description: Some("Architecture & steps".to_string()),
        created_at: 100,
        updated_at: 200,
        architect: None,
        last_replanned_step: None,
        concept,
        items: vec![step],
    };

    // Concept navigation
    assert!(plan.find_concept_item("c-1").is_some());
    assert_eq!(
        plan.find_concept_item("c-1.1").unwrap().title,
        "Token Expiration Invariant"
    );
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
    assert_eq!(
        plan.find_concept_item("c-1.1").unwrap().title,
        "Updated Invariant"
    );
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
    let legacy: PlanItem =
        serde_json::from_str(r#"{"id":"2","title":"Old step","model":"junior"}"#).unwrap();
    assert_eq!(legacy.id, "2");
    assert_eq!(legacy.title, "Old step");
}

#[test]
fn test_plan_item_blocked_reason_serde() {
    // A blocked step carries the diagnostic report of the semantic blocker
    // through the wire form unchanged.
    let mut blocked = item("1", "Blocked step", PlanItemStatus::Blocked, vec![]);
    blocked.blocked_reason = Some("Missing capability: cargo nextest".to_string());

    let json = serde_json::to_string(&blocked).unwrap();
    assert!(json.contains("\"blocked_reason\":\"Missing capability: cargo nextest\""));
    let de: PlanItem = serde_json::from_str(&json).unwrap();
    assert_eq!(de, blocked);

    // A step that was never blocked keeps its compact record: an absent reason
    // adds no key to the document.
    let clean = item("2", "Clean step", PlanItemStatus::Todo, vec![]);
    assert!(!serde_json::to_string(&clean)
        .unwrap()
        .contains("blocked_reason"));

    // A plan written before the field existed stays readable and reports no
    // reason instead of failing the typed parse.
    let legacy: PlanItem =
        serde_json::from_str(r#"{"id":"3","title":"Legacy step","status":"blocked"}"#).unwrap();
    assert_eq!(legacy.status, PlanItemStatus::Blocked);
    assert!(legacy.blocked_reason.is_none());
}

/// The blocker report survives the wire form of a whole plan, not only of one
/// item: nested leaves keep their reasons, and a step that is not blocked adds
/// no key to the document.
#[test]
fn test_plan_roundtrip_keeps_nested_blocked_reasons() {
    let reason = "Missing capability: cargo nextest";
    let mut leaf = item("1.1", "Leaf", PlanItemStatus::Blocked, vec![]);
    leaf.blocked_reason = Some(reason.to_string());
    let mut container = item("1", "Group", PlanItemStatus::Blocked, vec![leaf]);
    container.files = vec!["src/leaf.rs".to_string()];

    let mut clean = item("2", "Clean step", PlanItemStatus::Todo, vec![]);
    clean.blocked_reason = None;

    let plan = Plan {
        id: "nested-blocked".to_string(),
        title: "Nested Blocked".to_string(),
        items: vec![container, clean],
        ..Default::default()
    };

    let json = serde_json::to_string(&plan).unwrap();
    // Exactly one step carries a report, so the key appears once: an
    // unblocked step keeps its compact representation.
    assert_eq!(json.matches("blocked_reason").count(), 1, "{json}");

    let de: Plan = serde_json::from_str(&json).unwrap();
    assert_eq!(de, plan);

    let stored_leaf = de.find_item("1.1").unwrap();
    assert_eq!(stored_leaf.status, PlanItemStatus::Blocked);
    assert_eq!(stored_leaf.blocked_reason.as_deref(), Some(reason));
    assert!(de.find_item("2").unwrap().blocked_reason.is_none());
}

#[test]
fn test_plan_stats_and_summary() {
    let plan = Plan {
        id: "jwt-auth".to_string(),
        title: "JWT Authentication".to_string(),
        description: Some("Replace session cookies".to_string()),
        created_at: 1700000000,
        updated_at: 1700000010,
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
    assert_eq!(
        plan.find_item("1").unwrap().status,
        PlanItemStatus::Discussion
    );

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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
    assert_eq!(
        plan.find_item("parent").unwrap().status,
        PlanItemStatus::InProgress
    );

    // Update first sub-item to Discussion: parent becomes Discussion (highest attention priority)
    plan.update_item_status("sub-1", PlanItemStatus::Discussion);
    assert_eq!(
        plan.find_item("parent").unwrap().status,
        PlanItemStatus::Discussion
    );

    // Update first sub-item to Done, second is still Todo: parent becomes InProgress
    plan.update_item_status("sub-1", PlanItemStatus::Done);
    assert_eq!(
        plan.find_item("parent").unwrap().status,
        PlanItemStatus::InProgress
    );

    // Update second sub-item to done: parent automatically transitions to Done
    plan.update_item_status("sub-2", PlanItemStatus::Done);
    assert_eq!(
        plan.find_item("parent").unwrap().status,
        PlanItemStatus::Done
    );
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
        architect: None,
        last_replanned_step: None,
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
    assert_eq!(
        plan.items[1].readonly_files,
        vec!["src/leaf_ref.rs".to_string()]
    );
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
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
        architect: None,
        last_replanned_step: None,
        concept: vec![],
        items: vec![item("Step-1", "Step", PlanItemStatus::Todo, vec![])],
    };

    // `find_item` and `update_item_status` must agree on the id matching rule.
    assert!(plan.find_item("step-1").is_some());
    assert!(plan.update_item_status("step-1", PlanItemStatus::Done));
    assert_eq!(
        plan.find_item("STEP-1").unwrap().status,
        PlanItemStatus::Done
    );
}

#[test]
fn test_plan_update_item_status_rejects_container_targets() {
    let mut plan = Plan {
        id: "container-guard".to_string(),
        title: "Container Guard".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        architect: None,
        last_replanned_step: None,
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
fn test_plan_has_started_or_completed_steps() {
    fn plan_with(concept: Vec<ConceptItem>, items: Vec<PlanItem>) -> Plan {
        Plan {
            id: "started-steps".to_string(),
            title: "Started Steps".to_string(),
            description: None,
            created_at: 1,
            updated_at: 1,
            architect: None,
            last_replanned_step: None,
            concept,
            items,
        }
    }

    // A purely conceptual plan carries no implementation steps, so nothing is
    // started or completed and a single confirmation is enough to clear it.
    let conceptual = plan_with(
        vec![ConceptItem {
            id: "c-1".to_string(),
            title: "Invariant".to_string(),
            details: None,
            children: vec![],
        }],
        vec![],
    );
    assert!(!conceptual.has_started_or_completed_steps());

    // A single pending leaf records no progress.
    let todo_only = plan_with(
        vec![],
        vec![item("1", "Pending", PlanItemStatus::Todo, vec![])],
    );
    assert!(!todo_only.has_started_or_completed_steps());

    // An in-progress leaf means implementation work is already underway.
    let in_progress = plan_with(
        vec![],
        vec![item("1", "Active", PlanItemStatus::InProgress, vec![])],
    );
    assert!(in_progress.has_started_or_completed_steps());

    // A finished leaf means progress may already be captured by a Git commit.
    let done = plan_with(
        vec![],
        vec![item("1", "Finished", PlanItemStatus::Done, vec![])],
    );
    assert!(done.has_started_or_completed_steps());

    // A finished leaf counts at any depth, even beside pending siblings and
    // with a stale container status.
    let nested = plan_with(
        vec![],
        vec![item(
            "1",
            "Group",
            PlanItemStatus::Todo,
            vec![
                item("1.1", "Pending", PlanItemStatus::Todo, vec![]),
                item("1.2", "Finished", PlanItemStatus::Done, vec![]),
            ],
        )],
    );
    assert!(nested.has_started_or_completed_steps());

    // A container whose leaves are all pending holds no progress of its own: the
    // container status is derived from its children and is never inspected, so a
    // stale `Done` on the container does not count as started work.
    let pending_group = plan_with(
        vec![],
        vec![item(
            "1",
            "Group",
            PlanItemStatus::Done,
            vec![
                item("1.1", "A", PlanItemStatus::Todo, vec![]),
                item("1.2", "B", PlanItemStatus::Todo, vec![]),
            ],
        )],
    );
    assert!(!pending_group.has_started_or_completed_steps());
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
        architect: None,
        last_replanned_step: None,
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
                files: vec!["src/step2.rs".to_string()],
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
    let mut empty_id_item = item("  ", "Empty", PlanItemStatus::Todo, vec![]);
    empty_id_item.files = vec!["src/empty.rs".to_string()];
    let empty_id = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        items: vec![empty_id_item],
        ..Default::default()
    };
    assert_eq!(
        empty_id.validate().unwrap_err(),
        vec![PlanValidationError::EmptyItemId]
    );

    // 2. Duplicate item ID (case-insensitive)
    let mut first_dup = item("Step-1", "A", PlanItemStatus::Todo, vec![]);
    first_dup.files = vec!["src/a.rs".to_string()];
    let mut second_dup = item("step-1", "B", PlanItemStatus::Todo, vec![]);
    second_dup.files = vec!["src/b.rs".to_string()];
    let dup_id = Plan {
        id: "p".to_string(),
        title: "P".to_string(),
        items: vec![first_dup, second_dup],
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
            files: vec!["src/step1.rs".to_string()],
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
            files: vec!["src/step1.rs".to_string()],
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
            files: vec!["src/step1.rs".to_string()],
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
                files: vec!["src/step_a.rs".to_string()],
                ..Default::default()
            },
            PlanItem {
                id: "B".to_string(),
                title: "Step B".to_string(),
                depends_on: vec!["A".to_string()],
                files: vec!["src/step_b.rs".to_string()],
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
    dependent.files = vec!["src/dependent.rs".to_string()];

    let plan = Plan {
        id: "normalized-ids".to_string(),
        title: "Normalized Ids".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        architect: None,
        last_replanned_step: None,
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
    leaf.files = vec!["src/child.rs".to_string()];

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
    deep.files = vec!["src/deep.rs".to_string()];

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

#[test]
fn test_plan_validate_detects_container_cycle_through_dependent_leaf() {
    // The implicit container-to-leaf edge participates in cycle detection: the
    // group can only finish once its child is Done, the child waits for the
    // dependent step, and the dependent step waits for the group.
    let mut child = item("1.1", "Child", PlanItemStatus::Todo, vec![]);
    child.depends_on = vec!["2".to_string()];
    child.files = vec!["src/child.rs".to_string()];

    let mut dependent = item("2", "Dependent", PlanItemStatus::Todo, vec![]);
    dependent.depends_on = vec!["1".to_string()];
    dependent.files = vec!["src/dependent.rs".to_string()];

    let plan = Plan {
        id: "container-cycle".to_string(),
        title: "Container Cycle".to_string(),
        items: vec![
            item("1", "Group", PlanItemStatus::Todo, vec![child]),
            dependent,
        ],
        ..Default::default()
    };

    let errs = plan.validate().unwrap_err();
    assert_eq!(errs.len(), 1);
    match &errs[0] {
        PlanValidationError::CyclicDependency(cycle) => {
            for id in ["1", "1.1", "2"] {
                assert!(
                    cycle.contains(&id.to_string()),
                    "cycle {cycle:?} misses step {id}"
                );
            }
        }
        other => panic!("Expected CyclicDependency, got {:?}", other),
    }
}

#[test]
fn test_plan_validate_ignores_cancelled_leaf_naming_its_ancestor() {
    // A `Cancelled` leaf never gates its ancestor's completion: the container is
    // `Done` once all of its non-cancelled descendant leaves are `Done`. A
    // leftover `depends_on` naming the ancestor therefore creates no runtime
    // deadlock, so validation must accept the plan instead of reporting a
    // `CyclicDependency` the derived status can never trigger.
    let mut cancelled = item("1.1", "Cancelled child", PlanItemStatus::Cancelled, vec![]);
    cancelled.depends_on = vec!["1".to_string()];
    cancelled.files = vec!["src/child.rs".to_string()];

    let mut plan = Plan {
        id: "cancelled-ancestor-dep".to_string(),
        title: "Cancelled Ancestor Dep".to_string(),
        items: vec![item("1", "Group", PlanItemStatus::Todo, vec![cancelled])],
        ..Default::default()
    };
    assert!(plan.validate().is_ok());

    // Flipping the leaf back to an active status restores the genuine deadlock:
    // the container can only finish once the leaf is `Done`, and the leaf cannot
    // start before the container.
    assert!(plan.update_item_status("1.1", PlanItemStatus::Todo));
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
fn test_compute_direct_status_aggregation_edge_cases() {
    use super::node::compute_direct_status;

    // An empty set has no active member and defaults to Todo.
    assert_eq!(
        compute_direct_status(std::iter::empty::<PlanItemStatus>()),
        PlanItemStatus::Todo
    );
    // Discussion dominates every other status.
    assert_eq!(
        compute_direct_status(vec![
            PlanItemStatus::Done,
            PlanItemStatus::Blocked,
            PlanItemStatus::Discussion,
            PlanItemStatus::InProgress,
            PlanItemStatus::Todo,
        ]),
        PlanItemStatus::Discussion
    );
    // Blocked outranks InProgress.
    assert_eq!(
        compute_direct_status(vec![PlanItemStatus::InProgress, PlanItemStatus::Blocked]),
        PlanItemStatus::Blocked
    );
    // InProgress outranks a partial Done/Todo mix.
    assert_eq!(
        compute_direct_status(vec![
            PlanItemStatus::Done,
            PlanItemStatus::Todo,
            PlanItemStatus::InProgress,
        ]),
        PlanItemStatus::InProgress
    );
    // A Done/Todo mix is InProgress, never Done.
    assert_eq!(
        compute_direct_status(vec![PlanItemStatus::Done, PlanItemStatus::Todo]),
        PlanItemStatus::InProgress
    );
    // A cancelled member does not prevent a finished set from being Done.
    assert_eq!(
        compute_direct_status(vec![PlanItemStatus::Done, PlanItemStatus::Cancelled]),
        PlanItemStatus::Done
    );
    // A cancelled member does not disturb an all-Todo set.
    assert_eq!(
        compute_direct_status(vec![PlanItemStatus::Todo, PlanItemStatus::Cancelled]),
        PlanItemStatus::Todo
    );
    // A set whose members are all cancelled is Cancelled.
    assert_eq!(
        compute_direct_status(vec![PlanItemStatus::Cancelled, PlanItemStatus::Cancelled]),
        PlanItemStatus::Cancelled
    );
    // A single InProgress stays InProgress.
    assert_eq!(
        compute_direct_status(vec![PlanItemStatus::InProgress]),
        PlanItemStatus::InProgress
    );
}

#[test]
fn test_refresh_parent_statuses_and_scope_on_deep_hierarchy() {
    let mut plan = Plan {
        id: "deep-hierarchy".to_string(),
        title: "Deep Hierarchy".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        architect: None,
        last_replanned_step: None,
        concept: vec![],
        items: vec![item(
            "1",
            "Top",
            PlanItemStatus::Todo,
            vec![
                item(
                    "1.1",
                    "Mid",
                    PlanItemStatus::Todo,
                    vec![
                        item("1.1.1", "Finished", PlanItemStatus::Done, vec![]),
                        item("1.1.2", "Pending", PlanItemStatus::Todo, vec![]),
                    ],
                ),
                item("1.2", "Abandoned", PlanItemStatus::Cancelled, vec![]),
            ],
        )],
    };

    // Derived container statuses propagate through every nesting level: the
    // partially finished group is InProgress and so is its parent.
    plan.refresh_parent_statuses();
    assert_eq!(
        plan.find_item("1.1").unwrap().status,
        PlanItemStatus::InProgress
    );
    assert_eq!(
        plan.find_item("1").unwrap().status,
        PlanItemStatus::InProgress
    );
    assert_eq!(plan.status(), PlanItemStatus::InProgress);
    // Only executable leaves count: the cancelled leaf stays out of the metric.
    assert_eq!(plan.count_stats(), (2, 1));

    // The readiness scan schedules exactly the pending leaf: the finished leaf is
    // already satisfied and the cancelled leaf is skipped instead of stalling it.
    match plan.evaluate_scope(None) {
        PlanScopeEvaluation::Ready(leaves) => {
            let ids: Vec<&str> = leaves.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(ids, vec!["1.1.2"]);
        }
        other => panic!("Expected Ready, got {:?}", other),
    }

    // A composite scope at the middle level reports the same single pending leaf.
    match plan.evaluate_scope(Some("1.1")) {
        PlanScopeEvaluation::Ready(leaves) => {
            let ids: Vec<&str> = leaves.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(ids, vec!["1.1.2"]);
        }
        other => panic!("Expected Ready, got {:?}", other),
    }

    // Finishing the last executable leaf cascades to the top, and the cancelled
    // sibling keeps the plan Done instead of blocking completion.
    assert!(plan.update_item_status("1.1.2", PlanItemStatus::Done));
    assert_eq!(plan.find_item("1.1").unwrap().status, PlanItemStatus::Done);
    assert_eq!(plan.find_item("1").unwrap().status, PlanItemStatus::Done);
    assert_eq!(plan.status(), PlanItemStatus::Done);
    assert_eq!(plan.count_stats(), (2, 2));
    assert_eq!(plan.evaluate_scope(None), PlanScopeEvaluation::AllCompleted);
}

#[test]
fn test_plan_to_markdown_keeps_metadata_on_single_lines() {
    let mut step = item("1", "Step", PlanItemStatus::Todo, vec![]);
    // Hand-edited plan JSON can carry surrounding whitespace, embedded line
    // breaks and empty entries. Each key must still render as exactly one
    // indented line, and a key with no usable entry must render no line.
    step.files = vec![
        "  src/a.rs  ".to_string(),
        String::new(),
        "src/b\n.rs".to_string(),
    ];
    step.readonly_files = vec!["   ".to_string()];
    step.depends_on = vec!["0\r".to_string()];

    let plan = Plan {
        id: "metadata-lines".to_string(),
        title: "Metadata Lines".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        architect: None,
        last_replanned_step: None,
        concept: vec![],
        items: vec![step],
    };

    let md = plan.to_markdown();
    assert!(md.contains("    files: src/a.rs, src/b .rs\n"));
    assert!(!md.contains("readonly_files"));
    assert!(md.contains("    depends_on: 0\n"));
    // No carriage return survives into the rendered document.
    assert!(!md.contains('\r'));
}

#[test]
fn test_plan_last_replanned_step_serde_and_backward_compatibility() {
    // The step whose horizon was reprojected most recently is persisted with
    // the plan, so a repeated failure of the same step is recognized from disk
    // state instead of in-memory supervisor state, and a manual replan
    // dispatched through the UI or RPC is accounted for identically.
    let plan = Plan {
        id: "replanned".to_string(),
        title: "Replanned".to_string(),
        last_replanned_step: Some("2".to_string()),
        ..Default::default()
    };
    let json = serde_json::to_string(&plan).unwrap();
    assert!(json.contains("\"last_replanned_step\":\"2\""));
    let de: Plan = serde_json::from_str(&json).unwrap();
    assert_eq!(de.last_replanned_step.as_deref(), Some("2"));

    // A record written before the field existed stays readable: the absent
    // marker simply reports that no step has been reprojected yet.
    let legacy = r#"{"id":"legacy","title":"Legacy","created_at":1,"updated_at":2,"items":[]}"#;
    let legacy_plan: Plan = serde_json::from_str(legacy).unwrap();
    assert!(legacy_plan.last_replanned_step.is_none());

    // An unset marker stays absent from the wire form, so a plan that was never
    // replanned keeps its compact representation.
    let fresh = Plan {
        id: "fresh".to_string(),
        title: "Fresh".to_string(),
        ..Default::default()
    };
    let fresh_json = serde_json::to_string(&fresh).unwrap();
    assert!(!fresh_json.contains("last_replanned_step"));
}

#[test]
fn test_plan_to_markdown_collapses_line_breaks_in_title_and_ids() {
    // A hand-edited or model-authored plan can carry line breaks in any
    // interpolated field. The renderer must keep exactly one heading line and
    // one line per concept and step entry instead of emitting extra lines and
    // shifting the `#id` marker off its line.
    let plan = Plan {
        id: "line-breaks".to_string(),
        title: "Plan\nTitle\rHere".to_string(),
        description: None,
        created_at: 1,
        updated_at: 1,
        architect: None,
        last_replanned_step: None,
        concept: vec![ConceptItem {
            id: "c-1\nx".to_string(),
            title: "Concept\rTitle".to_string(),
            details: None,
            children: vec![],
        }],
        items: vec![item("1\n2", "Step\rTitle", PlanItemStatus::Todo, vec![])],
    };

    let md = plan.to_markdown();
    let lines: Vec<&str> = md.lines().collect();

    // The heading gains no extra lines: the title collapses onto one line.
    assert_eq!(lines.first().copied(), Some("# Plan: Plan Title Here"));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("# Plan:"))
            .count(),
        1
    );

    // The concept id marker stays on the concept line.
    let concept_line = lines
        .iter()
        .find(|line| line.ends_with("#c-1 x: Concept Title"))
        .expect("concept line must keep its id marker");
    assert!(concept_line.starts_with("- #"));

    // The step id marker stays on the step line.
    let step_line = lines
        .iter()
        .find(|line| line.ends_with("#1 2: Step Title"))
        .expect("step line must keep its id marker");
    assert!(step_line.starts_with("- [ ] "));

    // No carriage return survives into the rendered document.
    assert!(!md.contains('\r'));
}

/// An executable leaf must name the context it targets, while a draft, a
/// finished step, and a cancelled step are exempt from the requirement.
#[test]
fn test_plan_validate_requires_target_files_for_executable_leaves() {
    let plan_with = |status: PlanItemStatus| Plan {
        id: "leaf-files".to_string(),
        title: "Leaf Files".to_string(),
        items: vec![item("1", "Step", status, vec![])],
        ..Default::default()
    };

    // A pending leaf that declares nothing is rejected and named by its id.
    assert_eq!(
        plan_with(PlanItemStatus::Todo).validate().unwrap_err(),
        vec![PlanValidationError::MissingLeafFiles {
            item_id: "1".to_string(),
        }]
    );
    assert_eq!(
        PlanValidationError::MissingLeafFiles {
            item_id: "1".to_string(),
        }
        .to_string(),
        "Step '1' must declare a target file in 'files' or 'readonly_files'"
    );

    // The draft, the finished step, and the cancelled step are exempt.
    assert!(plan_with(PlanItemStatus::Discussion).validate().is_ok());
    assert!(plan_with(PlanItemStatus::Done).validate().is_ok());
    assert!(plan_with(PlanItemStatus::Cancelled).validate().is_ok());

    // Either list satisfies the requirement: a verification-only step declares
    // its reference file, while a step that creates a file declares its path.
    let mut reference_only = item("1", "Step", PlanItemStatus::InProgress, vec![]);
    reference_only.readonly_files = vec!["src/contract.rs".to_string()];
    let mut creating = item("2", "Step", PlanItemStatus::Blocked, vec![]);
    creating.files = vec!["src/new.rs".to_string()];
    let plan = Plan {
        id: "leaf-files".to_string(),
        title: "Leaf Files".to_string(),
        items: vec![reference_only, creating],
        ..Default::default()
    };
    assert!(plan.validate().is_ok());

    // A blank entry names nothing, so it does not satisfy the requirement.
    let mut blank = item("1", "Step", PlanItemStatus::Todo, vec![]);
    blank.files = vec!["   ".to_string()];
    let plan = Plan {
        id: "blank-files".to_string(),
        title: "Blank Files".to_string(),
        items: vec![blank],
        ..Default::default()
    };
    assert_eq!(
        plan.validate().unwrap_err(),
        vec![PlanValidationError::MissingLeafFiles {
            item_id: "1".to_string(),
        }]
    );
}
