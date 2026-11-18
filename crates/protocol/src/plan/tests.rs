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
        critic_model: Some(ModelSelection::Specific(ModelRef::openrouter(
            "openai/gpt-5",
        ))),
        skip_critique: false,
        instructions: Some("Decompose into atomic steps".to_string()),
    };
    let json_params = serde_json::to_string(&params).unwrap();
    assert!(json_params.contains("\"core-feature\""));
    assert!(json_params.contains("\"architect\""));
    // Both phases of the modification travel on the wire: the critique
    // selection names its own Architect entry, and the opt-out has its own key.
    assert!(json_params.contains("\"openai/gpt-5\""), "{json_params}");
    assert!(
        json_params.contains("\"skip_critique\":false"),
        "{json_params}"
    );
    let de_params: PlanRefineParams = serde_json::from_str(&json_params).unwrap();
    assert_eq!(de_params, params);

    let plan = Plan::from_parts(
        "core-feature".to_string(),
        "Feature".to_string(),
        None,
        10,
        20,
        Vec::new(),
        Vec::new(),
    );

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

// The two-phase creation pipeline travels whole: the directives, the Architect
// entry of each phase, and the explicit opt-out of the critique. The planner
// and the critic are independent selections, so one typed dialog configures
// both phases of the synthesis.
#[test]
fn test_plan_create_pipeline_params_carry_both_phases() {
    let params = PlanCreatePipelineParams {
        directives: "Introduce an audit log for every turn".to_string(),
        planner_model: Some(ModelSelection::Tier {
            model: ModelRef::openrouter("anthropic/claude-3.5-sonnet"),
            effort: Some(ReasoningEffort::High),
        }),
        critic_model: Some(ModelSelection::Role(ModelRole::Architect)),
        skip_critique: false,
    };
    let json = serde_json::to_string(&params).unwrap();
    assert!(
        json.contains("\"directives\":\"Introduce an audit log for every turn\""),
        "{json}"
    );
    assert!(json.contains("\"effort\":\"high\""), "{json}");
    assert!(json.contains("\"critic_model\":\"architect\""), "{json}");
    assert!(json.contains("\"skip_critique\":false"), "{json}");

    let de: PlanCreatePipelineParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, params);
}

// The directives are the only mandatory coordinate of the pipeline: a payload
// that carries them alone starts both phases from their runtime defaults,
// while an absent directives key and a malformed value fail the typed parse
// instead of being dropped or guessed.
#[test]
fn test_plan_create_pipeline_params_defaults_and_rejections() {
    let minimal: PlanCreatePipelineParams =
        serde_json::from_value(serde_json::json!({ "directives": "Harden the turn boundary" }))
            .unwrap();
    assert_eq!(minimal.directives, "Harden the turn boundary");
    assert!(minimal.planner_model.is_none());
    assert!(minimal.critic_model.is_none());
    assert!(!minimal.skip_critique);

    // An absent selection stays off the wire form instead of emitting nulls.
    let json = serde_json::to_string(&minimal).unwrap();
    assert!(!json.contains("planner_model"), "{json}");
    assert!(!json.contains("critic_model"), "{json}");

    // A request without directives has nothing to plan, so the typed parse
    // rejects the payload.
    assert!(serde_json::from_value::<PlanCreatePipelineParams>(serde_json::json!({})).is_err());

    // A malformed value fails the typed parse rather than being dropped.
    assert!(
        serde_json::from_value::<PlanCreatePipelineParams>(serde_json::json!({
            "directives": "x",
            "planner_model": 42
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<PlanCreatePipelineParams>(serde_json::json!({
            "directives": "x",
            "skip_critique": "yes"
        }))
        .is_err()
    );
}

// The critique fields extend the established refinement payload without
// breaking it: a request written before they existed decodes with no explicit
// critic and the critique phase enabled, so an older client keeps working.
#[test]
fn test_plan_refine_params_accept_a_payload_without_the_critique_fields() {
    let legacy: PlanRefineParams = serde_json::from_value(serde_json::json!({
        "plan_id": "core-feature",
        "model": "architect",
        "instructions": "Tighten the horizon"
    }))
    .unwrap();
    assert_eq!(legacy.plan_id, "core-feature");
    assert!(legacy.critic_model.is_none());
    assert!(!legacy.skip_critique);

    // The opt-out travels as its own key, so a pipeline that skips the critique
    // is never confused with the absence of a critic selection.
    let skipped = PlanRefineParams {
        plan_id: "core-feature".to_string(),
        model: None,
        critic_model: None,
        skip_critique: true,
        instructions: None,
    };
    let json = serde_json::to_string(&skipped).unwrap();
    assert!(json.contains("\"skip_critique\":true"), "{json}");
    let de: PlanRefineParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, skipped);
}

// The client requests the active plan with an empty object, so the DTO —
// not a server-side fallback — defines the empty payload as "no target id",
// while a malformed id still fails the typed parse.
#[test]
fn test_plan_get_params_defaults_the_absent_id() {
    let params: PlanGetParams = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(params.id, "");
    assert!(serde_json::from_value::<PlanGetParams>(serde_json::json!({ "id": 123 })).is_err());
}

#[test]
fn test_plan_execute_params_carry_coder_selection() {
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
    };
    let json = serde_json::to_string(&params).unwrap();
    assert!(json.contains("\"effort\":\"high\""), "{json}");
    let de: PlanExecuteParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, params);

    // A request that predates the selection field keeps decoding unchanged and
    // reports no explicit selection, so the ladder starts at its base rung.
    let legacy: PlanExecuteParams = serde_json::from_value(serde_json::json!({
        "plan_id": "legacy-plan",
        "step_id": "1"
    }))
    .unwrap();
    assert_eq!(legacy.plan_id, "legacy-plan");
    assert!(legacy.model.is_none());

    // An absent selection stays out of the wire form instead of emitting
    // explicit nulls.
    let bare_json = serde_json::to_string(&legacy).unwrap();
    assert!(!bare_json.contains("model"), "{bare_json}");

    // A malformed selection fails the typed parse rather than being dropped.
    assert!(
        serde_json::from_value::<PlanExecuteParams>(serde_json::json!({
            "plan_id": "bad-selection",
            "model": 42
        }))
        .is_err()
    );
}

#[test]
fn test_plan_execute_pipeline_params_carry_coder_selection() {
    let params = PlanExecutePipelineParams {
        plan_ids: vec!["plan-a".to_string(), "plan-b".to_string()],
        model: Some(ModelSelection::Role(ModelRole::Coder)),
    };
    let json = serde_json::to_string(&params).unwrap();
    assert!(json.contains("\"coder\""), "{json}");
    let de: PlanExecutePipelineParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, params);

    // A legacy queue payload still decodes to an empty selection, so the
    // pipeline starts from the base rung of the ladder.
    let legacy: PlanExecutePipelineParams =
        serde_json::from_value(serde_json::json!({ "plan_ids": ["plan-a"] })).unwrap();
    assert_eq!(legacy.plan_ids, vec!["plan-a".to_string()]);
    assert!(legacy.model.is_none());

    // An absent selection adds no key to the document.
    let bare_json = serde_json::to_string(&legacy).unwrap();
    assert!(!bare_json.contains("model"), "{bare_json}");

    // A malformed selection fails the typed parse rather than being dropped.
    assert!(
        serde_json::from_value::<PlanExecutePipelineParams>(serde_json::json!({
            "plan_ids": ["plan-a"],
            "model": 42
        }))
        .is_err()
    );
}

// A queue launch starts from the same rung an isolated execution starts from:
// the model and its reasoning depth travel together, so the server never
// re-derives the depth from the configured role lists.
#[test]
fn test_plan_execute_pipeline_params_carry_an_explicit_rung() {
    let params = PlanExecutePipelineParams {
        plan_ids: vec!["plan-a".to_string()],
        model: Some(ModelSelection::Tier {
            model: ModelRef::openrouter("anthropic/claude-3.5-sonnet"),
            effort: Some(ReasoningEffort::High),
        }),
    };
    let json = serde_json::to_string(&params).unwrap();
    assert!(json.contains("\"effort\":\"high\""), "{json}");
    let de: PlanExecutePipelineParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, params);
}

// A manual replan starts from the same rung an execution starts from: the
// model and its reasoning depth travel together, an absent selection stays off
// the wire, and a malformed one fails the typed parse instead of being
// dropped.
#[test]
fn test_plan_replan_params_carry_coder_selection() {
    let params = PlanReplanParams {
        plan_id: "core-feature".to_string(),
        step_id: "2".to_string(),
        reason: Some("blocked on an unstable invariant".to_string()),
        model: Some(ModelSelection::Tier {
            model: ModelRef::openrouter("anthropic/claude-3.5-sonnet"),
            effort: Some(ReasoningEffort::High),
        }),
        instructions: Some("Keep the existing contract".to_string()),
    };
    let json = serde_json::to_string(&params).unwrap();
    assert!(json.contains("\"effort\":\"high\""), "{json}");
    let de: PlanReplanParams = serde_json::from_str(&json).unwrap();
    assert_eq!(de, params);

    // A request that predates the selection field keeps decoding unchanged and
    // reports no explicit selection, so the ladder starts at its base rung.
    let legacy: PlanReplanParams = serde_json::from_value(serde_json::json!({
        "plan_id": "legacy-plan",
        "step_id": "1"
    }))
    .unwrap();
    assert_eq!(legacy.step_id, "1");
    assert!(legacy.model.is_none());

    // An absent selection stays out of the wire form instead of emitting
    // explicit nulls.
    let bare_json = serde_json::to_string(&legacy).unwrap();
    assert!(!bare_json.contains("model"), "{bare_json}");

    // A malformed selection fails the typed parse rather than being dropped.
    assert!(
        serde_json::from_value::<PlanReplanParams>(serde_json::json!({
            "plan_id": "bad-selection",
            "step_id": "1",
            "model": 42
        }))
        .is_err()
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

// The blocker report survives the wire form of a whole plan, not only of one
// item: nested leaves keep their reasons, and a step that is not blocked adds
// no key to the document.

// Assigning a leaf the status it already carries is not a durable change, so a
// caller is told that the update did not happen instead of receiving a success
// it would have to undo. An optimistic-update consumer needs that distinction
// to decide whether to emit an update event or perform a rollback.

#[test]
fn test_plan_implementation_provenance_serde() {
    // The provenance of a horizon travels with its step graph, so a repeated
    // failure of the same step is recognized from disk state instead of
    // in-memory supervisor state, and a manual replan is accounted for
    // identically.
    let mut plan = Plan::from_parts(
        "replanned".to_string(),
        "Replanned".to_string(),
        None,
        0,
        0,
        Vec::new(),
        Vec::new(),
    );
    plan.implementation.last_replanned_step = Some("2".to_string());
    let json = serde_json::to_string(&plan).unwrap();
    assert!(json.contains("\"last_replanned_step\":\"2\""));
    let de: Plan = serde_json::from_str(&json).unwrap();
    assert_eq!(de.implementation.last_replanned_step.as_deref(), Some("2"));
    assert_eq!(de.implementation, plan.implementation);

    // An unset marker stays absent from the wire form, so a horizon that was
    // never replanned keeps its compact representation, while the step graph
    // itself is always present as its own section.
    let fresh = Plan::from_parts(
        "fresh".to_string(),
        "Fresh".to_string(),
        None,
        0,
        0,
        Vec::new(),
        Vec::new(),
    );
    let fresh_json = serde_json::to_string(&fresh).unwrap();
    assert!(!fresh_json.contains("last_replanned_step"));
    assert!(fresh_json.contains("\"implementation\":{"), "{fresh_json}");
}

// An executable leaf must name the context it targets, while a draft, a
// finished step, and a cancelled step are exempt from the requirement.

// Every plan is constructed through [`Plan::from_parts`], so the constructor
// must leave the decomposition provenance empty and keep the structural parts
// exactly as supplied.
#[test]
fn test_plan_from_parts_leaves_provenance_empty() {
    let concept = vec![ConceptItem {
        id: "c-1".to_string(),
        title: "Invariant".to_string(),
        details: None,
        children: vec![],
    }];
    let items = vec![item("1", "Step", PlanItemStatus::Todo, vec![])];
    let plan = Plan::from_parts(
        "from-parts".to_string(),
        "From Parts".to_string(),
        Some("Single construction point".to_string()),
        7,
        9,
        concept.clone(),
        items.clone(),
    );

    assert_eq!(plan.id, "from-parts");
    assert_eq!(plan.title, "From Parts");
    assert_eq!(
        plan.description.as_deref(),
        Some("Single construction point")
    );
    assert_eq!(plan.created_at, 7);
    assert_eq!(plan.updated_at, 9);
    assert!(plan.implementation.architect.is_none());
    assert!(plan.implementation.last_replanned_step.is_none());
    assert_eq!(plan.concept, concept);
    assert_eq!(plan.implementation.items, items);
}

// The canonical wire form keeps the step graph in its own `implementation`
// object: the document header and the horizon are separate sections, and the
// nested form round-trips unchanged.
#[test]
fn test_plan_wire_form_nests_the_step_graph() {
    let mut step = item("1", "Step", PlanItemStatus::Todo, vec![]);
    step.files = vec!["src/a.rs".to_string()];
    let mut plan = Plan::from_parts(
        "nested".to_string(),
        "Nested".to_string(),
        Some("Nested implementation".to_string()),
        1,
        2,
        Vec::new(),
        vec![step],
    );
    plan.implementation.last_replanned_step = Some("1".to_string());

    let json = serde_json::to_string(&plan).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let implementation = value
        .get("implementation")
        .expect("the step graph is a nested object");
    assert_eq!(implementation["items"][0]["id"], serde_json::json!("1"));
    assert_eq!(
        implementation["last_replanned_step"],
        serde_json::json!("1")
    );
    // The header keeps no copy of the graph, so the two sections can never
    // disagree about the steps of the horizon.
    assert!(value.get("items").is_none(), "{json}");

    let decoded: Plan = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, plan);
}

// The former flat layout is not readable: the nested form is the only accepted
// shape, so an old document fails to decode instead of silently becoming a
// plan without steps.
#[test]
fn test_plan_rejects_the_legacy_flat_form() {
    let flat =
        r#"{"id":"legacy","title":"Legacy","created_at":1,"updated_at":2,"concept":[],"items":[]}"#;
    let decoded: Result<Plan, _> = serde_json::from_str(flat);
    assert!(
        decoded.is_err(),
        "a flat plan document decoded: {decoded:?}"
    );
}

// The structural validation of a horizon reports identifier, target-file,
// dependency, and cycle violations, while the plan composes that report with
// its own concept reference check.

// A plan rejection carries its category as typed wire data and renders its
// reported wording verbatim, so a surface that printed the historical reason
// string keeps its exact bytes and no consumer classifies the refusal from text.
#[test]
fn test_plan_rejection_carries_its_kind_and_renders_its_message() {
    let cases = [
        (PlanRejectionKind::PayloadDefect, "\"payload_defect\""),
        (PlanRejectionKind::ModeForbidden, "\"mode_forbidden\""),
        (PlanRejectionKind::Abandoned, "\"abandoned\""),
    ];
    for (kind, wire_kind) in cases {
        let rejection = PlanRejection::new(kind, "Cannot save plan 'x': duplicate step ID: '1'");
        let json = serde_json::to_string(&rejection).unwrap();
        assert!(json.contains(wire_kind), "{json}");
        assert!(json.contains("\"message\""), "{json}");

        let decoded: PlanRejection = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, rejection);
        assert_eq!(decoded.kind, kind);
    }

    // The named constructors are the single way a producer states its category,
    // so a refusal never carries a kind its birthplace did not decide.
    assert_eq!(
        PlanRejection::payload_defect("defect").kind,
        PlanRejectionKind::PayloadDefect
    );
    assert_eq!(
        PlanRejection::mode_forbidden("regime").kind,
        PlanRejectionKind::ModeForbidden
    );
    assert_eq!(
        PlanRejection::abandoned("abandoned").kind,
        PlanRejectionKind::Abandoned
    );

    // An unknown category fails the typed parse instead of degrading into a
    // guessed one.
    assert!(
        serde_json::from_str::<PlanRejection>(r#"{"kind":"mystery","message":"refused"}"#).is_err()
    );
}
