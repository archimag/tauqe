//! Compatibility checks for independent model configuration and wire contracts.
#[path = "../src/contracts.rs"]
mod contracts;
use contracts::Project;
use tauqe_core::models as domain;
use tauqe_protocol as wire;

#[test]
fn model_selection_projection_keeps_roles_explicit_tiers_and_effort() {
    let model = domain::ModelRef::openrouter("vendor/model:free");
    let mut selections: Vec<_> = [
        domain::ModelRole::Scribe,
        domain::ModelRole::Coder,
        domain::ModelRole::Architect,
        domain::ModelRole::Gatekeeper,
        domain::ModelRole::REPL,
        domain::ModelRole::Reviewer,
    ]
    .into_iter()
    .map(domain::ModelSelection::Role)
    .collect();
    selections.push(domain::ModelSelection::Specific(model.clone()));
    for effort in [
        None,
        Some(domain::ReasoningEffort::Low),
        Some(domain::ReasoningEffort::Medium),
        Some(domain::ReasoningEffort::High),
        Some(domain::ReasoningEffort::XHigh),
        Some(domain::ReasoningEffort::Max),
    ] {
        selections.push(domain::ModelSelection::Tier {
            model: model.clone(),
            effort,
        });
    }
    for selection in selections {
        let value = serde_json::to_value(&selection).unwrap();
        let projected: wire::ModelSelection = selection.clone().project();
        assert_eq!(serde_json::to_value(&projected).unwrap(), value);
        let restored: domain::ModelSelection = projected.project();
        assert_eq!(restored, selection);
    }
}

#[test]
fn model_summary_projection_keeps_budget_markers_catalog_and_every_role() {
    let model = domain::ModelRef::openrouter("vendor/model");
    let entry = domain::ModelEntry {
        model: model.clone(),
        effort: Some(domain::ReasoningEffort::High),
        attempts: Some(7),
        level_up: Some(false),
    };
    let summary = domain::ModelRolesSummary {
        scribe: model.clone(),
        coder: model.clone(),
        architect: model.clone(),
        repl: model.clone(),
        reviewer: model.clone(),
        efforts: Some(domain::ModelRolesEffort {
            scribe: Some(domain::ReasoningEffort::Low),
            coder: Some(domain::ReasoningEffort::Medium),
            architect: Some(domain::ReasoningEffort::High),
            repl: Some(domain::ReasoningEffort::XHigh),
            reviewer: Some(domain::ReasoningEffort::Max),
        }),
        entries: Some(domain::ModelRolesEntries {
            scribe: vec![entry.clone()],
            coder: vec![entry.clone()],
            architect: vec![entry.clone()],
            repl: vec![entry.clone()],
            reviewer: vec![entry],
            catalog: Some(vec![domain::ModelCatalogEntry {
                model,
                alias: Some("configured alias".into()),
            }]),
        }),
    };
    let expected = serde_json::to_value(&summary).unwrap();
    let projected: wire::ModelRolesSummary = summary.clone().project();
    assert_eq!(serde_json::to_value(&projected).unwrap(), expected);
    let restored: domain::ModelRolesSummary = projected.project();
    assert_eq!(restored, summary);
    let tier = domain::ArchitectTier {
        model: summary.architect,
        effort: Some(domain::ReasoningEffort::Max),
        attempts: Some(9),
        level_up: Some(false),
    };
    let projected: wire::ArchitectTier = tier.clone().project();
    assert_eq!(
        serde_json::to_value(&projected).unwrap(),
        serde_json::to_value(&tier).unwrap()
    );
    let restored: domain::ArchitectTier = projected.project();
    assert_eq!(restored, tier);
}

#[test]
fn model_configuration_and_wire_parsers_keep_the_existing_forms_and_errors() {
    macro_rules! compatible {
        ($domain:ty, $wire:ty, $cases:expr) => {
            for raw in $cases {
                let domain = serde_json::from_str::<$domain>(raw)
                    .map(|value| serde_json::to_value(value).unwrap())
                    .map_err(|error| error.to_string());
                let wire = serde_json::from_str::<$wire>(raw)
                    .map(|value| serde_json::to_value(value).unwrap())
                    .map_err(|error| error.to_string());
                assert_eq!(domain, wire, "contract drift for {raw}");
            }
        };
    }
    compatible!(
        domain::ModelEntry,
        wire::ModelEntry,
        [
            r#""vendor/model:free""#,
            r#"{"model":"openrouter:vendor/model","effort":" HIGH ","attempts":4,"level_up":false}"#,
            r#"{"model":"vendor/model","effort":" ","level_up":"invalid"}"#,
            r#"{"model":"vendor/model","attempts":0}"#,
            r#"{"model":"vendor/model","effort":"unknown"}"#,
            r#"{"model":"vendor/model","attempts":1,"attempts":2}"#,
            r#"{"model":"typo:vendor/model"}"#,
            r#"{"model":"vendor/model","alias":"forbidden"}"#,
        ]
    );
    compatible!(
        domain::ArchitectTier,
        wire::ArchitectTier,
        [
            r#""vendor/model""#,
            r#"{"model":"vendor/model","effort":"max","level_up":false}"#,
            r#"{"model":"vendor/model","attempts":0}"#,
        ]
    );
    compatible!(
        domain::ModelCatalogEntry,
        wire::ModelCatalogEntry,
        [
            r#""vendor/model""#,
            r#"{"model":"vendor/model","alias":"  display name  "}"#,
            r#"{"model":"vendor/model","alias":" "}"#,
            r#"{"model":"vendor/model","effort":"high"}"#,
        ]
    );
    compatible!(
        domain::ModelRole,
        wire::ModelRole,
        [r#"" Coder ""#, r#""gatekeeper""#, r#""unknown""#]
    );
    compatible!(
        domain::ReasoningEffort,
        wire::ReasoningEffort,
        [r#"" XHIGH ""#, r#""max""#, r#""unknown""#]
    );
}
