use super::*;

fn parse_models(raw: &str) -> ModelsConfig {
    parse_models_with_catalog(raw, &[])
}

fn parse_models_with_catalog(raw: &str, catalog: &[ModelCatalogEntry]) -> ModelsConfig {
    let raw: ModelsConfigRaw = toml::from_str(raw).expect("parse models config");
    ModelsConfig::resolve(raw, catalog).expect("resolve models config")
}

#[test]
fn test_resolve_effort_defaults_to_none() {
    let config = ModelsConfig::default();
    for role in [
        ModelRole::Scribe,
        ModelRole::Coder,
        ModelRole::Architect,
        ModelRole::Primary,
        ModelRole::Reviewer,
    ] {
        assert_eq!(config.resolve_effort(role), None);
    }
}

#[test]
fn test_resolve_effort_reads_base_entry() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:deepseek/deepseek-chat", effort = "medium" },
    { model = "openrouter:google/gemini-flash-latest", effort = "high" },
]
architect = [{ model = "openrouter:anthropic/claude-sonnet-latest", effort = "high" }]
reviewer = ["openrouter:google/gemini-flash-latest"]
"#,
    );

    // Effort is read from the base (first) entry of the role list.
    assert_eq!(
        config.resolve_effort(ModelRole::Coder),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(
        config.resolve_effort(ModelRole::Architect),
        Some(ReasoningEffort::High)
    );
    assert_eq!(config.resolve_effort(ModelRole::Reviewer), None);
    assert_eq!(config.resolve_effort(ModelRole::Primary), None);
    // Scribe is a single utility model and never carries effort.
    assert_eq!(config.resolve_effort(ModelRole::Scribe), None);
}

#[test]
fn test_coder_single_string_yields_one_rung() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = ["openrouter:anthropic/claude-3.7-sonnet"]
"#,
    );
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 1);
    assert_eq!(
        ladder[0].model,
        ModelRef::openrouter("anthropic/claude-3.7-sonnet")
    );
    assert_eq!(ladder[0].effort, None);
    assert_eq!(config.resolve_role(ModelRole::Coder), ladder[0].model);
    assert_eq!(config.coder_tier(0), ladder[0]);
    assert_eq!(config.coder_tier(7), ladder[0]);
}

#[test]
fn test_coder_single_table_carries_effort() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [{ model = "openrouter:deepseek/deepseek-chat", effort = "high" }]
"#,
    );
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 1);
    assert_eq!(
        ladder[0].model,
        ModelRef::openrouter("deepseek/deepseek-chat")
    );
    assert_eq!(ladder[0].effort, Some(ReasoningEffort::High));
    assert_eq!(config.resolve_role(ModelRole::Coder), ladder[0].model);
    assert_eq!(
        config.resolve_effort(ModelRole::Coder),
        Some(ReasoningEffort::High)
    );
}

#[test]
fn test_coder_array_builds_ordered_ladder() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:base/model",
    { model = "openrouter:strong/model", effort = "xhigh" },
]
"#,
    );
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 2);
    assert_eq!(ladder[0].model, ModelRef::openrouter("base/model"));
    assert_eq!(ladder[0].effort, None);
    assert_eq!(ladder[1].model, ModelRef::openrouter("strong/model"));
    assert_eq!(ladder[1].effort, Some(ReasoningEffort::XHigh));
    assert_eq!(config.resolve_role(ModelRole::Coder), ladder[0].model);
    assert_eq!(config.coder_tier(1), ladder[1]);
    assert_eq!(config.coder_tier(9), ladder[1]);
}

#[test]
fn test_coder_empty_ladder_rejected_by_validate() {
    let config = ModelsConfig {
        primary: vec![ModelRef::openrouter("base/primary").into()],
        coder: Some(Vec::new()),
        ..Default::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn test_coder_absent_inherits_primary_ladder() {
    let config = parse_models(
        r#"
primary = ["openrouter:only/primary", "openrouter:second/primary"]
architect = ["openrouter:only/architect"]
"#,
    );
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 2);
    assert_eq!(ladder[0].model, ModelRef::openrouter("only/primary"));
    assert_eq!(ladder[1].model, ModelRef::openrouter("second/primary"));
    assert_eq!(config.coder_tier(0), ladder[0]);
    assert_eq!(config.coder_tier(5), ladder[1]);
}

#[test]
fn test_architect_ladder_derives_from_role_list() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:planner/one", effort = "high" },
    { model = "openrouter:planner/two", effort = "xhigh" },
]
"#,
    );

    let ladder = config.architect_ladder();
    assert_eq!(ladder.len(), 2);
    assert_eq!(
        ladder[0],
        ArchitectTier::new(
            ModelRef::openrouter("planner/one"),
            Some(ReasoningEffort::High)
        )
    );
    assert_eq!(
        ladder[1],
        ArchitectTier::new(
            ModelRef::openrouter("planner/two"),
            Some(ReasoningEffort::XHigh)
        )
    );

    // Escalation advances one rung at a time and saturates on the strongest.
    assert_eq!(config.next_architect_tier(None), ladder[0]);
    assert_eq!(config.next_architect_tier(Some(&ladder[0])), ladder[1]);
    assert_eq!(config.next_architect_tier(Some(&ladder[1])), ladder[1]);
    assert_eq!(config.architect_tier(7), ladder[1]);
}

#[test]
fn test_architect_ladder_restarts_when_recorded_rung_is_unknown() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
architect = ["openrouter:planner/one"]
"#,
    );

    let unknown = ArchitectTier::new(ModelRef::openrouter("planner/retired"), None);
    assert_eq!(config.architect_tier_index(Some(&unknown)), None);
    // A configuration that no longer exists restarts from the base rung
    // instead of guessing a position on the current ladder.
    assert_eq!(
        config.next_architect_tier(Some(&unknown)),
        ArchitectTier::new(ModelRef::openrouter("planner/one"), None)
    );
}

#[test]
fn test_resolve_selection_effort_picks_matching_model_entry() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:deepseek/flash", effort = "max" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    "openrouter:google/gemini-argon",
]
"#,
    );

    // Role selection returns base effort (max)
    assert_eq!(
        config.resolve_selection_effort(
            ModelRole::Architect,
            &ModelSelection::Role(ModelRole::Architect)
        ),
        Some(ReasoningEffort::Max)
    );

    // Specific gemini-flash returns high, NOT max from deepseek
    assert_eq!(
        config.resolve_selection_effort(
            ModelRole::Architect,
            &ModelSelection::Specific(ModelRef::openrouter("google/gemini-flash"))
        ),
        Some(ReasoningEffort::High)
    );

    // Specific gemini-argon without effort returns None
    assert_eq!(
        config.resolve_selection_effort(
            ModelRole::Architect,
            &ModelSelection::Specific(ModelRef::openrouter("google/gemini-argon"))
        ),
        None
    );

    // Model not in list returns None
    assert_eq!(
        config.resolve_selection_effort(
            ModelRole::Architect,
            &ModelSelection::Specific(ModelRef::openrouter("unknown/model"))
        ),
        None
    );
}

#[test]
fn test_resolve_selection_effort_tier_reads_effort_from_the_dto() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:deepseek/flash", effort = "max" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
]
"#,
    );

    // A tier selection carries the full role-list coordinate, so the
    // resolver reads its depth straight from the DTO: the configured
    // `high` effort of the same model never leaks into the answer.
    let tier = ModelSelection::Tier {
        model: ModelRef::openrouter("google/gemini-flash"),
        effort: Some(ReasoningEffort::XHigh),
    };
    assert_eq!(
        config.resolve_selection_effort(ModelRole::Architect, &tier),
        Some(ReasoningEffort::XHigh)
    );
    assert_eq!(
        config.resolve_model(&tier),
        ModelRef::openrouter("google/gemini-flash")
    );

    // A tier without a depth resolves to no effort instead of inheriting
    // the first entry's configured one.
    let bare = ModelSelection::Tier {
        model: ModelRef::openrouter("google/gemini-flash"),
        effort: None,
    };
    assert_eq!(
        config.resolve_selection_effort(ModelRole::Architect, &bare),
        None
    );
}

#[test]
fn test_architect_tier_index_falls_back_to_model_matching_on_effort_discrepancy() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:deepseek/flash", effort = "max" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    "openrouter:google/gemini-argon",
]
"#,
    );

    // Recorded tier has mismatched effort (Max instead of High)
    let mismatched = ArchitectTier::new(
        ModelRef::openrouter("google/gemini-flash"),
        Some(ReasoningEffort::Max),
    );

    // Falls back to model match and finds index 1 instead of returning None
    assert_eq!(config.architect_tier_index(Some(&mismatched)), Some(1));
    // Escalation proceeds to index 2 (gemini-argon) instead of resetting to tier 0
    assert_eq!(
        config.next_architect_tier(Some(&mismatched)),
        ArchitectTier::new(ModelRef::openrouter("google/gemini-argon"), None)
    );
}

#[test]
fn test_model_entry_polymorphic_parsing_with_effort() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:plain/model",
    { model = "openrouter:detailed/model", effort = "high" },
]
"#,
    );
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 2);
    assert_eq!(ladder[0].model, ModelRef::openrouter("plain/model"));
    assert_eq!(ladder[0].effort, None);
    assert_eq!(ladder[1].model, ModelRef::openrouter("detailed/model"));
    assert_eq!(ladder[1].effort, Some(ReasoningEffort::High));
}

#[test]
fn test_role_entry_rejects_a_display_alias() {
    // A display alias describes the model itself and is declared in the
    // provider catalog, so a role slot that carries one is rejected instead
    // of quietly reinstating a second source of truth for the same label.
    let err = toml::from_str::<ModelsConfigRaw>(
        r#"
primary = [{ model = "openrouter:base/primary", alias = "Fast" }]
"#,
    )
    .expect_err("a role entry carrying an alias must be rejected")
    .to_string();
    assert!(err.contains("alias"), "got: {err}");
}

#[test]
fn test_role_entry_reports_precise_field_errors() {
    // An unrecognized effort surfaces the reasoning-effort diagnostic
    // instead of an opaque "data did not match any variant".
    let err = toml::from_str::<ModelsConfigRaw>(
        r#"
primary = [{ model = "openrouter:base/primary", effort = "extreme" }]
"#,
    )
    .expect_err("an unknown reasoning effort must be rejected")
    .to_string();
    assert!(err.contains("Unknown reasoning effort"), "got: {err}");

    // A repeated key is rejected instead of letting the last value win.
    // TOML refuses a duplicated key inside an inline table while parsing,
    // before the role entry visitor runs, so the diagnostic is the parser's
    // `duplicate key` report and it still names the offending key.
    let err = toml::from_str::<ModelsConfigRaw>(
        r#"
primary = [{ model = "openrouter:a/b", model = "openrouter:c/d" }]
"#,
    )
    .expect_err("a duplicated model key must be rejected")
    .to_string();
    assert!(err.contains("duplicate key"), "got: {err}");
    assert!(err.contains("model"), "got: {err}");

    // A table without a model reference is rejected.
    let err = toml::from_str::<ModelsConfigRaw>(
        r#"
primary = [{ effort = "high" }]
"#,
    )
    .expect_err("a table without a model must be rejected")
    .to_string();
    assert!(err.contains("missing field"), "got: {err}");
}

#[test]
fn test_role_slice_mirrors_role_entries_for_stored_roles() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary", "openrouter:second/primary"]
coder = ["openrouter:only/coder"]
architect = [{ model = "openrouter:only/architect", effort = "high" }]
"#,
    );

    for role in [
        ModelRole::Primary,
        ModelRole::Coder,
        ModelRole::Architect,
        ModelRole::Reviewer,
    ] {
        let slice = config
            .role_slice(role)
            .expect("stored role exposes a borrowed slice");
        assert_eq!(slice, config.role_entries(role).as_slice());
    }

    // Scribe is a derived single-entry role and never exposes a stored list.
    assert!(config.role_slice(ModelRole::Scribe).is_none());
}

#[test]
fn test_role_reference_resolves_catalog_alias() {
    let catalog = [
        ModelCatalogEntry {
            model: ModelRef::openrouter("deepseek/deepseek-chat"),
            alias: Some("Deep".to_string()),
        },
        ModelCatalogEntry {
            model: ModelRef::openrouter("google/gemini-flash-latest"),
            alias: None,
        },
    ];
    let config = parse_models_with_catalog(
        r#"
primary = ["Deep"]
coder = [{ model = "Deep", effort = "max" }]
scribe = "Deep"
"#,
        &catalog,
    );

    let deep = ModelRef::openrouter("deepseek/deepseek-chat");
    assert_eq!(config.resolve_role(ModelRole::Primary), deep);
    assert_eq!(config.scribe, Some(deep.clone()));
    assert_eq!(config.coder_tier(0).model, deep);
    assert_eq!(config.coder_tier(0).effort, Some(ReasoningEffort::Max));

    // A provider specification still resolves without an alias.
    assert_eq!(
        ModelsConfig::resolve_model_spec(&catalog, "google/gemini-flash-latest").unwrap(),
        ModelRef::openrouter("google/gemini-flash-latest")
    );
    assert_eq!(ModelsConfig::model_alias(&catalog, &deep), Some("Deep"));
    assert_eq!(
        ModelsConfig::model_alias(
            &catalog,
            &ModelRef::openrouter("google/gemini-flash-latest")
        ),
        None
    );
}

#[test]
fn test_unresolvable_role_reference_reports_the_role() {
    let raw: ModelsConfigRaw =
        toml::from_str(r#"primary = ["anthropc:claude"]"#).expect("parse models config");
    let err = ModelsConfig::resolve(raw, &[])
        .expect_err("a mistyped provider prefix must fail resolution");
    assert!(err.contains("primary"), "got: {err}");
    assert!(err.contains("anthropc:claude"), "got: {err}");
}

#[test]
fn test_resolve_selection_effort_distinguishes_tiers_of_the_same_model() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:google/gemini-flash", effort = "low" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    { model = "openrouter:google/gemini-flash", effort = "max" },
]
"#,
    );

    let model = ModelRef::openrouter("google/gemini-flash");

    // The same model may occupy several rungs of a role list, each with its
    // own reasoning depth. A tier names exactly one of them, so its depth
    // is read from the DTO and never from the first matching entry.
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::High,
        ReasoningEffort::Max,
    ] {
        let tier = ModelSelection::Tier {
            model: model.clone(),
            effort: Some(effort),
        };
        let resolved = config.resolve_selection_effort(ModelRole::Architect, &tier);
        assert_eq!(resolved, Some(effort));
        assert_eq!(config.resolve_model(&tier), model);
    }

    // A tier without a recorded depth resolves to no effort instead of
    // borrowing the depth of the first configured entry of the same model.
    let bare = ModelSelection::Tier {
        model: model.clone(),
        effort: None,
    };
    assert_eq!(
        config.resolve_selection_effort(ModelRole::Architect, &bare),
        None
    );

    // A bare model reference names no entry, so it keeps the established
    // behavior and reads the depth of the first matching entry.
    let specific = ModelSelection::Specific(model.clone());
    assert_eq!(
        config.resolve_selection_effort(ModelRole::Architect, &specific),
        Some(ReasoningEffort::Low)
    );
}

/// Three rows of one model stay distinguishable end to end: the picker keeps
/// every rung, the confirmed row travels as a `Tier`, and the resolver
/// applies exactly the depth printed on that row (c-1).
#[test]
fn test_picker_rows_of_one_model_reach_the_resolver_with_their_own_effort() {
    let config = parse_models(
        r#"
primary = [
    { model = "openrouter:google/gemini-flash", effort = "low" },
    { model = "openrouter:google/gemini-flash", effort = "high" },
    { model = "openrouter:google/gemini-flash", effort = "max" },
]
"#,
    );
    let model = ModelRef::openrouter("google/gemini-flash");

    // The role list keeps all three rungs of the same model, so the picker
    // presents rows that differ only by their reasoning depth.
    let rows = config.role_entries(ModelRole::Primary);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter().map(|row| row.effort).collect::<Vec<_>>(),
        vec![
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::High),
            Some(ReasoningEffort::Max),
        ]
    );
    for row in &rows {
        assert_eq!(row.model, model);
    }

    // Reopening the picker reproduces the confirmed rung: matching by both
    // coordinates distinguishes the three rows, while matching only the
    // model always lands on the first rung.
    for (index, row) in rows.iter().enumerate() {
        let exact =
            |candidate: &ModelEntry| candidate.model == row.model && candidate.effort == row.effort;
        let same_model = |candidate: &ModelEntry| candidate.model == row.model;
        assert_eq!(rows.iter().position(exact), Some(index));
        // Every rung shares the model, so a search that compares only the
        // model collapses all of them onto the first entry: the depth of
        // the confirmed rung survives only in the `(model, effort)` pair.
        assert_eq!(
            rows.iter().position(same_model),
            Some(0),
            "a model-only search must land on the first rung"
        );
    }

    // Activating a row sends the rung whole, so the depth on screen is the
    // depth the resolver applies.
    for row in &rows {
        let tier = ModelSelection::Tier {
            model: row.model.clone(),
            effort: row.effort,
        };
        assert_eq!(
            config.resolve_selection_effort(ModelRole::Primary, &tier),
            row.effort
        );
        assert_eq!(config.resolve_model(&tier), model);
    }

    // Collapsing a row to `Specific` drops the depth and falls back to the
    // first rung of the same model, which is the defect the tier form
    // removes.
    let collapsed = ModelSelection::Specific(model.clone());
    assert_eq!(
        config.resolve_selection_effort(ModelRole::Primary, &collapsed),
        Some(ReasoningEffort::Low)
    );
}

/// Every role-driven RPC carries the confirmed rung whole, so the server
/// never re-derives the reasoning depth by matching the model against the
/// configured role list (c-3).
#[test]
fn test_role_rpc_tier_reaches_the_server_resolver() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]

reviewer = [
    { model = "openrouter:shared/reviewer", effort = "low" },
    { model = "openrouter:shared/reviewer", effort = "xhigh" },
]

architect = [
    { model = "openrouter:shared/architect", effort = "medium" },
    { model = "openrouter:shared/architect", effort = "max" },
]

coder = [
    { model = "openrouter:shared/coder", effort = "low" },
    { model = "openrouter:shared/coder", effort = "high" },
]
"#,
    );

    for (role, model, first, second) in [
        (
            ModelRole::Reviewer,
            ModelRef::openrouter("shared/reviewer"),
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::XHigh),
        ),
        (
            ModelRole::Architect,
            ModelRef::openrouter("shared/architect"),
            Some(ReasoningEffort::Medium),
            Some(ReasoningEffort::Max),
        ),
        (
            ModelRole::Coder,
            ModelRef::openrouter("shared/coder"),
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::High),
        ),
    ] {
        // Both rungs of the same model stay reachable in the role dialog.
        let rows = config.role_entries(role);
        assert_eq!(rows.len(), 2, "{role}: every rung stays reachable");
        assert_eq!(rows[0].effort, first, "{role}");
        assert_eq!(rows[1].effort, second, "{role}");
        assert_eq!(rows[1].model, model, "{role}");

        // The second rung is the one the developer confirmed, and it
        // reaches the resolver with its own depth instead of the depth of
        // the first entry of the same model.
        let tier = ModelSelection::Tier {
            model: rows[1].model.clone(),
            effort: rows[1].effort,
        };
        assert_eq!(
            config.resolve_selection_effort(role, &tier),
            second,
            "{role}: the confirmed rung keeps its depth"
        );
        assert_eq!(config.resolve_model(&tier), model, "{role}");

        // A legacy client that names only the model reference keeps the
        // established decoding: the first matching entry supplies the depth.
        let legacy = ModelSelection::Specific(model.clone());
        assert_eq!(
            config.resolve_selection_effort(role, &legacy),
            first,
            "{role}: the legacy form decodes unchanged"
        );
    }
}

#[test]
fn test_coder_tier_index_defaults_to_the_base_rung() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:start/model", effort = "low" },
    { model = "openrouter:strong/model", effort = "max" },
]
"#,
    );

    // No explicit selection starts execution at the base of the ladder.
    assert_eq!(config.coder_tier_index(None), 0);

    // A role selection names the model the coder role resolves to, which is
    // the base rung of the very same ladder.
    assert_eq!(
        config.coder_tier_index(Some(&ModelSelection::Role(ModelRole::Coder))),
        0
    );

    // A model the ladder does not carry restarts from the base rung instead
    // of guessing a position.
    assert_eq!(
        config.coder_tier_index(Some(&ModelSelection::Specific(ModelRef::openrouter(
            "unknown/model"
        )))),
        0
    );
}

#[test]
fn test_coder_tier_index_matches_effort_then_model() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:shared/model", effort = "low" },
    { model = "openrouter:shared/model", effort = "high" },
    { model = "openrouter:other/model", effort = "max" },
]
"#,
    );
    let model = ModelRef::openrouter("shared/model");

    // The same model occupies two rungs, so the reasoning depth
    // distinguishes them instead of collapsing onto the first one.
    for (effort, index) in [(ReasoningEffort::Low, 0), (ReasoningEffort::High, 1)] {
        let selection = ModelSelection::Tier {
            model: model.clone(),
            effort: Some(effort),
        };
        assert_eq!(config.coder_tier_index(Some(&selection)), index, "{effort}");
        // The returned position is a usable starting rung: the ladder entry
        // it names is exactly the configured model at the requested depth.
        let rung = config.coder_tier(index);
        assert_eq!(rung.model, model, "{effort}");
        assert_eq!(rung.effort, Some(effort), "{effort}");
    }

    // A rung naming the model at a depth the ladder does not configure
    // falls back to the model match, which lands on its first rung.
    assert_eq!(
        config.coder_tier_index(Some(&ModelSelection::Tier {
            model: model.clone(),
            effort: Some(ReasoningEffort::Max),
        })),
        0
    );

    // A bare model reference carries no depth of its own, so the configured
    // entry supplies it and the ladder position follows.
    assert_eq!(
        config.coder_tier_index(Some(&ModelSelection::Specific(model.clone()))),
        0
    );
    assert_eq!(
        config.coder_tier_index(Some(&ModelSelection::Specific(ModelRef::openrouter(
            "other/model"
        )))),
        2
    );
}
#[test]
fn test_level_up_defaults_to_participation() {
    // A bare string reference and a table without the marker both leave the
    // entry without an explicit marker, so it participates in escalation.
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:one/coder",
    { model = "openrouter:two/coder", effort = "high" },
]
architect = ["openrouter:one/architect"]
"#,
    );

    let catalog = config.role_entries(ModelRole::Coder);
    assert_eq!(catalog.len(), 2);
    for entry in &catalog {
        assert_eq!(entry.level_up(), None);
    }
    for entry in config.coder_ladder() {
        assert_eq!(entry.level_up(), None);
    }
    for tier in config.architect_ladder() {
        assert_eq!(tier.level_up(), None);
    }

    // Without any marker the ladder is the role catalog itself, so a
    // configuration written before the opt-out existed behaves identically.
    assert_eq!(config.coder_ladder(), config.role_entries(ModelRole::Coder));
    let architect_catalog = config.role_entries(ModelRole::Architect);
    let architect_ladder = config.architect_ladder();
    assert_eq!(architect_ladder.len(), architect_catalog.len());
    for (tier, entry) in architect_ladder.iter().zip(&architect_catalog) {
        assert_eq!(tier.model, entry.model);
        assert_eq!(tier.effort, entry.effort);
        assert_eq!(tier.level_up, entry.level_up);
    }
    // An unmarked entry is a supported declaration, not an incomplete one.
    assert!(config.validate().is_ok());
}

#[test]
fn test_level_up_false_keeps_the_model_in_the_role_catalog() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:fast/coder", effort = "low" },
    { model = "openrouter:premium/coder", effort = "max", level_up = false },
]
architect = [{ model = "openrouter:premium/architect", level_up = false }]
"#,
    );

    // The role catalog keeps every configured model selectable, including
    // the one excluded from automatic escalation.
    let catalog = config.role_entries(ModelRole::Coder);
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[0].level_up(), None);
    assert_eq!(catalog[1].model, ModelRef::openrouter("premium/coder"));
    assert_eq!(catalog[1].effort, Some(ReasoningEffort::Max));
    assert_eq!(catalog[1].level_up(), Some(false));

    // The escalation ladder drops the opted-out rung, so a model the
    // developer reserved for a manual run is never entered automatically.
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 1);
    assert_eq!(ladder[0].model, ModelRef::openrouter("fast/coder"));

    // A ladder whose every rung opted out keeps one base rung instead of
    // becoming empty, so escalation always has somewhere to start.
    let tiers = config.architect_ladder();
    assert_eq!(tiers.len(), 1);
    assert_eq!(tiers[0].model, ModelRef::openrouter("premium/architect"));

    // Excluding a model is a supported declaration, not an invalid one.
    assert!(config.validate().is_ok());
}

#[test]
fn test_coder_ladder_skips_excluded_rungs_in_declaration_order() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:start/model", effort = "low" },
    { model = "openrouter:premium/model", effort = "max", level_up = false },
    { model = "openrouter:strong/model", effort = "xhigh" },
]
"#,
    );

    // The catalog keeps every configured model, so the excluded one stays
    // available for a manual run.
    assert_eq!(config.role_entries(ModelRole::Coder).len(), 3);

    // The ladder skips the opted-out rung and keeps the declared order of
    // the remaining ones.
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 2);
    assert_eq!(ladder[0].model, ModelRef::openrouter("start/model"));
    assert_eq!(ladder[0].effort, Some(ReasoningEffort::Low));
    assert_eq!(ladder[1].model, ModelRef::openrouter("strong/model"));
    assert_eq!(ladder[1].effort, Some(ReasoningEffort::XHigh));
    for rung in &ladder {
        assert_ne!(rung.model, ModelRef::openrouter("premium/model"));
    }
}

#[test]
fn test_ladder_walk_skips_an_opted_out_rung() {
    // `next_coder_tier` advances one ladder index at a time and reads the
    // rungs through `coder_tier`, so the filtered ladder alone makes the
    // walk step over the excluded model.
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:start/model", effort = "low" },
    { model = "openrouter:premium/model", effort = "max", level_up = false },
    { model = "openrouter:strong/model", effort = "xhigh" },
]
"#,
    );

    let base = config.coder_tier(0);
    let advanced = config.coder_tier(1);
    assert_eq!(base.model, ModelRef::openrouter("start/model"));
    assert_eq!(advanced.model, ModelRef::openrouter("strong/model"));
    assert_ne!(advanced.model, ModelRef::openrouter("premium/model"));

    // The walk saturates on the last rung instead of leaving the ladder.
    assert_eq!(config.coder_tier(2), advanced);
    assert_eq!(config.coder_tier(7), advanced);

    // A selection naming the excluded model is a manual run: it is not a
    // ladder position, so it starts on the strongest eligible rung and the
    // ladder is already saturated from there. A `Tier` selection that
    // references the filtered rung is clamped the same way.
    let manual = ModelSelection::Specific(ModelRef::openrouter("premium/model"));
    assert_eq!(config.coder_tier_index(Some(&manual)), 1);
    assert!(config.next_coder_tier(1).is_none());
    let tier = ModelSelection::Tier {
        model: ModelRef::openrouter("premium/model"),
        effort: Some(ReasoningEffort::Max),
    };
    assert_eq!(config.coder_tier_index(Some(&tier)), 1);
}

#[test]
fn test_architect_ladder_skips_excluded_tiers() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
architect = [
    { model = "openrouter:planner/first", effort = "high" },
    { model = "openrouter:planner/premium", effort = "max", level_up = false },
    { model = "openrouter:planner/second", effort = "xhigh" },
]
"#,
    );

    // Every configured Architect stays selectable in the role catalog.
    assert_eq!(config.role_entries(ModelRole::Architect).len(), 3);

    let ladder = config.architect_ladder();
    assert_eq!(ladder.len(), 2);
    assert_eq!(
        ladder[0],
        ArchitectTier::new(
            ModelRef::openrouter("planner/first"),
            Some(ReasoningEffort::High)
        )
    );
    assert_eq!(
        ladder[1],
        ArchitectTier::new(
            ModelRef::openrouter("planner/second"),
            Some(ReasoningEffort::XHigh)
        )
    );

    // Escalation advances across the filtered ladder only.
    assert_eq!(config.next_architect_tier(None), ladder[0]);
    assert_eq!(config.next_architect_tier(Some(&ladder[0])), ladder[1]);
    assert_eq!(config.next_architect_tier(Some(&ladder[1])), ladder[1]);
    assert_eq!(config.architect_tier(1), ladder[1]);
    assert_eq!(config.architect_tier_index(Some(&ladder[1])), Some(1));
}

#[test]
fn test_excluded_from_escalation_reports_the_declared_marker() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:base/coder",
    { model = "openrouter:premium/coder", level_up = false },
]
architect = [{ model = "openrouter:premium/architect", level_up = false }]
"#,
    );

    // The predicate reads the marker declared on the role entry, so a manual
    // run is recognized without comparing model names by text.
    assert!(!config.excluded_from_escalation(ModelRole::Coder, &ModelRef::openrouter("base/coder")));
    assert!(
        config.excluded_from_escalation(ModelRole::Coder, &ModelRef::openrouter("premium/coder"))
    );
    // A model the role does not configure at all carries no marker.
    assert!(
        !config.excluded_from_escalation(ModelRole::Coder, &ModelRef::openrouter("unknown/coder"))
    );
    assert!(config.excluded_from_escalation(
        ModelRole::Architect,
        &ModelRef::openrouter("premium/architect")
    ));
    // Scribe is a derived single model and never declares an exclusion.
    assert!(
        !config.excluded_from_escalation(ModelRole::Scribe, &ModelRef::openrouter("premium/coder"))
    );
}

#[test]
fn test_manual_architect_selection_saturates_the_ladder() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
architect = [
    "openrouter:planner/one",
    { model = "openrouter:planner/manual", level_up = false },
    "openrouter:planner/two",
]
"#,
    );
    let manual = ArchitectTier::from(ModelRef::openrouter("planner/manual"));

    // The developer pinned an Architect the ladder excludes, so a repeated
    // failure keeps that configuration instead of restarting from the base
    // rung, which would silently downgrade the chosen model.
    assert_eq!(config.next_architect_tier(Some(&manual)), manual);
    // The ladder itself still advances across its eligible rungs.
    let first = ArchitectTier::from(ModelRef::openrouter("planner/one"));
    let second = ArchitectTier::from(ModelRef::openrouter("planner/two"));
    assert_eq!(config.next_architect_tier(Some(&first)), second);
}

#[test]
fn test_ladder_keeps_a_base_rung_when_every_rung_opts_out() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [{ model = "openrouter:only/coder", effort = "max", level_up = false }]
architect = [{ model = "openrouter:only/architect", level_up = false }]
"#,
    );

    // A ladder always keeps one base rung, so escalation has somewhere to
    // start even when every configured rung opted out.
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 1);
    assert_eq!(config.coder_tier(0), ladder[0]);
    assert_eq!(config.coder_tier(3), ladder[0]);

    let tiers = config.architect_ladder();
    assert_eq!(tiers.len(), 1);
    assert_eq!(config.architect_tier(0), tiers[0]);

    // The declaration itself survives in the role catalog.
    assert_eq!(
        config.role_entries(ModelRole::Coder)[0].level_up(),
        Some(false)
    );
    assert_eq!(
        config.role_entries(ModelRole::Architect)[0].level_up(),
        Some(false)
    );
    assert!(config.validate().is_ok());
}

#[test]
fn test_resolve_role_still_reaches_an_opted_out_model() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    { model = "openrouter:premium/coder", effort = "max", level_up = false },
    "openrouter:base/coder",
]
architect = [
    { model = "openrouter:premium/architect", level_up = false },
    "openrouter:base/architect",
]
"#,
    );

    // Manual selection resolves against the catalog, so a model excluded
    // from escalation stays reachable as a role target.
    let premium = ModelRef::openrouter("premium/coder");
    assert_eq!(config.resolve_role(ModelRole::Coder), premium);
    assert_eq!(
        config.resolve_effort(ModelRole::Coder),
        Some(ReasoningEffort::Max)
    );
    let manual = ModelSelection::Specific(premium.clone());
    assert_eq!(config.resolve_model(&manual), premium);

    // The same model is absent from the ladder the harness may enter on
    // its own.
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 1);
    assert_eq!(ladder[0].model, ModelRef::openrouter("base/coder"));
    assert_eq!(
        config.resolve_role(ModelRole::Architect),
        ModelRef::openrouter("premium/architect")
    );
}

#[test]
fn test_level_up_marker_survives_a_configuration_round_trip() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:base/coder",
    { model = "openrouter:premium/coder", effort = "max", level_up = false },
]
"#,
    );
    let catalog = config.role_entries(ModelRole::Coder);
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[1].level_up(), Some(false));

    // Writing the section back preserves the marker, so a round-trip
    // through disk never turns an excluded rung into a participating one.
    let serialized = toml::to_string(&config).expect("serialize models config");
    let raw: ModelsConfigRaw = toml::from_str(&serialized).expect("re-parse models config");
    let restored = ModelsConfig::resolve(raw, &[]).expect("resolve models config");
    assert_eq!(restored.role_entries(ModelRole::Coder), catalog);
    assert_eq!(restored.coder_ladder(), config.coder_ladder());
    assert_eq!(restored.coder_ladder().len(), 1);
}

#[test]
fn test_level_up_marker_field_rules() {
    // An explicit `true` is the explicit form of the default and survives.
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [{ model = "openrouter:one/coder", level_up = true }]
"#,
    );
    assert_eq!(config.coder_ladder()[0].level_up(), Some(true));

    // A repeated marker is rejected instead of letting the last value win.
    let err = toml::from_str::<ModelsConfigRaw>(
        r#"
primary = ["openrouter:base/primary"]
coder = [{ model = "openrouter:one/coder", level_up = true, level_up = false }]
"#,
    )
    .expect_err("a duplicated level_up key must be rejected")
    .to_string();
    assert!(err.contains("duplicate key"), "got: {err}");
    assert!(err.contains("level_up"), "got: {err}");

    // A marker must be a boolean literal: any other type is a configuration
    // error instead of a silently ignored annotation.
    assert!(
        toml::from_str::<ModelsConfigRaw>(
            r#"
primary = ["openrouter:base/primary"]
coder = [{ model = "openrouter:one/coder", level_up = "false" }]
"#,
        )
        .is_err(),
        "a non-boolean level_up must be rejected"
    );
}

/// One configuration exercises every concept of the level-up opt-out end to
/// end: the role catalog keeps both models selectable, the escalation ladder
/// drops the excluded one, and a manual run on it leaves the ladder
/// saturated (c-1, c-2, c-3, c-4).
#[test]
fn test_level_up_opt_out_end_to_end() {
    let config = parse_models(
        r#"
primary = ["openrouter:base/primary"]
coder = [
    "openrouter:cheap/coder",
    { model = "openrouter:expensive/coder", effort = "max", level_up = false },
]
"#,
    );

    let cheap = ModelRef::openrouter("cheap/coder");
    let expensive = ModelRef::openrouter("expensive/coder");

    // c-1: an omitted marker means participation, while the explicit marker
    // survives resolution together with the reasoning depth of its entry.
    let catalog = config.role_entries(ModelRole::Coder);
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[0].model, cheap);
    assert_eq!(catalog[0].level_up(), None);
    assert_eq!(catalog[1].model, expensive);
    assert_eq!(catalog[1].effort, Some(ReasoningEffort::Max));
    assert_eq!(catalog[1].level_up(), Some(false));

    // c-4: that catalog is the payload of the role summary a selection
    // dialog renders, so the marker reaches the client. A participating
    // rung keeps its terse bare-string wire form, which a payload written
    // before the marker existed decodes unchanged.
    let wire = serde_json::to_string(&catalog).expect("serialize role catalog");
    assert!(wire.contains(r#""level_up":false"#), "got: {wire}");
    assert!(wire.contains(r#""openrouter:cheap/coder""#), "got: {wire}");
    let restored: Vec<ModelEntry> = serde_json::from_str(&wire).expect("decode role catalog");
    assert_eq!(restored, catalog);

    // c-2: the ladder filters the opted-out rung out, so sequential
    // escalation never enters the expensive model.
    let ladder = config.coder_ladder();
    assert_eq!(ladder.len(), 1);
    assert_eq!(ladder[0].model, cheap);
    assert!(
        config.next_coder_tier(0).is_none(),
        "a single-rung ladder is already saturated"
    );

    // c-3: a manual run on the excluded model occupies no ladder position,
    // so it starts on the strongest eligible rung and never escalates.
    let manual = ModelSelection::Specific(expensive);
    let manual_index = config.coder_tier_index(Some(&manual));
    assert_eq!(manual_index, 0, "a manual run starts on the strongest rung");
    assert!(
        config.next_coder_tier(manual_index).is_none(),
        "an opted-out model is already saturated"
    );
}
