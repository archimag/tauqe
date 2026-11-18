//! Compatibility of headless execution models with the client contract.
#[path = "../src/contracts.rs"]
mod contracts;
use contracts::Project;
use serde_json::{json, Value};
use tauqe_core::workflow::turn::*;
use tauqe_core::workflow::{EditProtocolKind, WorkflowKind};
use tauqe_protocol as wire;

#[test]
fn work_context_projection_preserves_every_kind_and_queue_coordinate() {
    let queue = TurnWorkQueue {
        items: vec![
            TurnWorkItem {
                id: "2".into(),
                title: "Second".into(),
            },
            TurnWorkItem {
                id: "1".into(),
                title: "First".into(),
            },
        ],
        active_index: 1,
    };
    let queue_json =
        json!({"items":[{"id":"2","title":"Second"},{"id":"1","title":"First"}],"active_index":1});
    let mut cases = vec![
        (TurnWorkContext::Interactive, json!({"kind":"interactive"})),
        (
            TurnWorkContext::PlanStep {
                plan_id: "p".into(),
                plan_title: "Plan".into(),
                queue: queue.clone(),
            },
            json!({"kind":"plan_step","plan_id":"p","plan_title":"Plan","queue":queue_json}),
        ),
        (
            TurnWorkContext::ReviewGroupStep {
                review_id: "r".into(),
                review_title: "Review".into(),
                group_id: "g".into(),
                group_title: "Group".into(),
                queue: queue.clone(),
            },
            json!({"kind":"review_group_step","review_id":"r","review_title":"Review","group_id":"g","group_title":"Group","queue":queue_json}),
        ),
        (
            TurnWorkContext::ReviewStep {
                review_id: "r".into(),
                review_title: "Review".into(),
                queue: queue.clone(),
            },
            json!({"kind":"review_step","review_id":"r","review_title":"Review","queue":queue_json}),
        ),
    ];
    for (phase, name) in [
        (ReviewWorkPhase::Audit, "audit"),
        (ReviewWorkPhase::Decomposition, "decomposition"),
        (ReviewWorkPhase::PostAnalysis, "post_analysis"),
    ] {
        cases.push((TurnWorkContext::ReviewPhase { review_id:"r".into(), review_title:"Review".into(), phase, queue:queue.clone() },
            json!({"kind":"review_phase","review_id":"r","review_title":"Review","phase":name,"queue":queue_json})));
    }
    for (context, expected) in cases {
        let projected = context.clone().project();
        assert_eq!(serde_json::to_value(&projected).unwrap(), expected);
        assert_eq!(projected.project(), context);
        let decoded: wire::TurnWorkContext = serde_json::from_value(expected).unwrap();
        assert_eq!(decoded.project(), context);
    }
}

#[test]
fn empty_and_invalid_queue_coordinates_are_not_repaired_by_projection() {
    for items in [
        Vec::new(),
        vec![TurnWorkItem {
            id: "1".into(),
            title: "Step".into(),
        }],
    ] {
        for active_index in [0, 1, 99] {
            let queue = TurnWorkQueue {
                items: items.clone(),
                active_index,
            };
            let projected = queue.clone().project();
            assert_eq!(
                projected.active().map(|item| &item.id),
                queue.active().map(|item| &item.id)
            );
            assert_eq!(projected.project(), queue);
        }
    }
    let empty = TurnWorkQueue::default().project();
    assert_eq!(
        serde_json::to_value(empty).unwrap(),
        json!({"active_index":0})
    );
    for ids in [Vec::new(), vec!["2".into(), "1".into()]] {
        let context = ModelTurnPlanContext {
            plan_id: "p".into(),
            step_id: "1".into(),
            batch_step_ids: ids.clone(),
        };
        let projected = context.clone().project();
        let value = serde_json::to_value(&projected).unwrap();
        assert_eq!(value["plan_id"], "p");
        assert_eq!(value["step_id"], "1");
        assert_eq!(
            value.get("batch_step_ids").cloned(),
            if ids.is_empty() {
                None
            } else {
                Some(json!(ids))
            }
        );
        assert_eq!(projected.project(), context);
    }
}

#[test]
fn execution_phases_keep_their_rpc_names() {
    for (phase, name) in [
        (TurnPhase::Research, "research"),
        (TurnPhase::Proposal, "proposal"),
        (TurnPhase::Staging, "staging"),
        (TurnPhase::Verification, "verification"),
        (TurnPhase::Healing, "healing"),
    ] {
        let projected = phase.project();
        assert_eq!(serde_json::to_value(projected).unwrap(), name);
        assert_eq!(projected.project(), phase);
        assert_eq!(phase.to_string(), name);
    }
}

#[test]
fn workflow_configuration_keeps_canonical_names_aliases_and_errors() {
    for text in ["git", "naive", " GIT\n", " NaIvE ", "", "unknown", "xml"] {
        let parsed = text.parse::<WorkflowKind>();
        let wire_parsed = text.parse::<wire::WorkflowKind>();
        assert_eq!(parsed.clone().map(Project::project), wire_parsed);
        let value = Value::String(text.into());
        assert_eq!(
            serde_json::from_value::<WorkflowKind>(value.clone()).ok(),
            parsed.ok()
        );
        if let Ok(model) = serde_json::from_value::<WorkflowKind>(value) {
            let projected = model.project();
            assert_eq!(
                serde_json::to_value(model).unwrap(),
                serde_json::to_value(projected).unwrap()
            );
            assert_eq!(model.to_string(), projected.to_string());
            assert_eq!(projected.project(), model);
        }
    }
    for text in [
        "structured",
        " STRUCTURED\n",
        "xml",
        "mixed",
        " AUTO ",
        "",
        "unknown",
    ] {
        let parsed = text.parse::<EditProtocolKind>();
        assert_eq!(
            parsed.clone().map(Project::project),
            text.parse::<wire::EditProtocolKind>()
        );
        assert_eq!(
            serde_json::from_value::<EditProtocolKind>(json!(text)).ok(),
            parsed.clone().ok()
        );
        if let Ok(model) = parsed {
            let projected = model.project();
            assert_eq!(serde_json::to_value(model).unwrap(), json!("structured"));
            assert_eq!(model.to_string(), projected.to_string());
            assert_eq!(projected.project(), model);
        }
    }
}
