//! Dependency gates for the architecture boundaries shared by the workspace.
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

#[test]
fn workspace_dependencies_follow_module_boundaries() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .current_dir(&root)
        .output()
        .expect("cargo metadata must run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let packages = metadata["packages"].as_array().unwrap();
    let allowed: &[(&str, &[&str])] = &[
        ("tauqe-protocol", &[]),
        ("tauqe-tools", &[]),
        ("tauqe-core", &["tauqe-tools"]),
        ("tauqe-flow", &["tauqe-core"]),
        ("tauqe-server", &["tauqe-flow", "tauqe-protocol"]),
        ("tauqe-tui", &["tauqe-tools", "tauqe-protocol"]),
    ];
    let workspace_names: BTreeSet<_> = packages
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    let reviewed_names: BTreeSet<_> = allowed.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        workspace_names, reviewed_names,
        "new workspace crates require an explicit boundary review"
    );
    for &(name, permitted) in allowed {
        let package = packages.iter().find(|p| p["name"] == name).unwrap();
        for dependency in package["dependencies"].as_array().unwrap() {
            let dependency_name = dependency["name"].as_str().unwrap();
            // The test-support feature is enabled by a self dev-dependency.
            if dependency_name == name && dependency["kind"] == "dev" {
                continue;
            }
            // Server tests compare client and engine projections. Flow tests create Git fixtures.
            if ((name == "tauqe-server" && ["tauqe-tui", "tauqe-core"].contains(&dependency_name))
                || (name == "tauqe-flow" && dependency_name == "tauqe-tools"))
                && dependency["kind"] == "dev"
            {
                continue;
            }
            if workspace_names.contains(dependency_name) {
                assert!(
                    permitted.contains(&dependency_name),
                    "forbidden dependency: {name} -> {dependency_name}"
                );
            }
            if matches!(name, "tauqe-core" | "tauqe-flow") {
                assert!(
                    !["ratatui", "crossterm", "axum", "tonic", "jsonrpsee"]
                        .contains(&dependency_name),
                    "{name} must remain headless and transport-free: {dependency_name}"
                );
            }
            if name == "tauqe-protocol" {
                assert!(
                    ["serde", "serde_json", "schemars"].contains(&dependency_name),
                    "protocol must remain an I/O-free leaf: {dependency_name}"
                );
            }
        }
    }
}

#[test]
fn core_facade_does_not_reexport_the_rpc_contract() {
    let facade = include_str!("../../core/src/lib.rs");
    assert!(!facade.contains("pub use tauqe_protocol"));
}

fn assert_protocol_free(paths: &[&str]) {
    fn inspect(path: &Path) {
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                inspect(&entry.unwrap().path());
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = std::fs::read_to_string(path).unwrap();
            assert!(
                !source.contains("tauqe_protocol"),
                "subsystem must own its models: {}",
                path.display()
            );
        }
    }
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for path in paths {
        inspect(&crates.join(path));
    }
}
#[test]
fn history_models_do_not_depend_on_protocol() {
    assert_protocol_free(&[
        "core/src/history.rs",
        "core/src/history",
        "flow/src/application/history.rs",
        "flow/src/application/history_operation.rs",
    ]);
}
#[test]
fn context_models_do_not_depend_on_protocol() {
    assert_protocol_free(&[
        "core/src/context.rs",
        "core/src/context",
        "flow/src/application/context.rs",
        "flow/src/application/context_operation.rs",
    ]);
}

#[test]
fn model_configuration_does_not_depend_on_protocol() {
    assert_protocol_free(&[
        "core/src/models.rs",
        "core/src/models",
        "core/src/config/models.rs",
        "core/src/config/models",
        "core/src/config/providers.rs",
    ]);
}
#[test]
fn protocol_does_not_resolve_generation_attempt_budgets() {
    let contract = include_str!("../../protocol/src/model/entry.rs");
    assert!(!contract.contains("fn attempts("));
    assert!(!contract.contains("fn resolve_attempts("));
}

#[test]
fn plan_and_review_documents_and_storage_do_not_depend_on_protocol() {
    assert_protocol_free(&[
        "core/src/plan.rs",
        "core/src/plan/model",
        "core/src/plan/storage.rs",
        "core/src/plan/storage",
        "core/src/review.rs",
        "core/src/review/model",
        "core/src/review/storage.rs",
        "core/src/review/storage",
        "core/src/review/analysis.rs",
        "core/src/review/analysis",
        "core/src/config/review.rs",
        "flow/src/application/plan_horizon.rs",
        "flow/src/application/plan_two_phase.rs",
        "flow/src/application/review_analysis.rs",
        "flow/src/application/review_synthesis_completion.rs",
    ]);
}

#[test]
fn protocol_does_not_own_model_only_analysis_or_durable_provenance_rules() {
    let review = include_str!("../../protocol/src/review.rs");
    assert!(!review.contains("PostAnalysisOutput"));
    let graph = include_str!("../../protocol/src/plan/implementation.rs");
    assert!(!graph.contains("fn normalize_lifecycle_provenance("));
}

#[test]
fn execution_identity_and_queue_entry_points_do_not_depend_on_protocol() {
    assert_protocol_free(&[
        "core/src/workflow/kind.rs",
        "core/src/workflow/turn.rs",
        "core/src/workflow/implementation.rs",
        "core/src/workflow/implementation/scope.rs",
        "flow/src/application/configuration.rs",
        "flow/src/application/turn_setup.rs",
        "flow/src/application/implementation.rs",
        "flow/src/application/implementation_step.rs",
        "flow/src/application/review_batch.rs",
        "core/src/control/refine.rs",
        "core/src/edits/protocol.rs",
    ]);
}

#[test]
fn core_and_flow_sources_are_independent_of_protocol() {
    assert_protocol_free(&["core/src", "flow/src"]);
}

#[test]
fn protocol_does_not_compose_model_schemas_or_select_edit_operations() {
    let discussion = include_str!("../../protocol/src/discussion.rs");
    assert!(!discussion.contains("pub mod schema;"));
    let proposals = include_str!("../../protocol/src/edit/proposal.rs");
    assert!(!proposals.contains("to_edit_operation"));
    assert!(!proposals.contains("impl schemars::JsonSchema"));
}

#[test]
fn protocol_does_not_export_retired_model_only_payloads() {
    let sources = [
        include_str!("../../protocol/src/lib.rs"),
        include_str!("../../protocol/src/discussion.rs"),
        include_str!("../../protocol/src/config.rs"),
        include_str!("../../protocol/src/review/item.rs"),
    ]
    .join("\n");
    for name in [
        "DiscussionResponse",
        "DiscussionPlanAction",
        "DiscussionPlanUpdate",
        "DirListRequest",
        "ReviewDecisionStatus",
        "ReviewUpdateProposal",
        "PatchStrategy",
    ] {
        assert!(
            !sources.contains(name),
            "model-only payload remains in protocol: {name}"
        );
    }
}
#[test]
fn server_production_sources_use_application_ports_for_domain_io_and_mutations() {
    fn inspect(path: &Path) {
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                return;
            }
            for entry in std::fs::read_dir(path).unwrap() {
                inspect(&entry.unwrap().path());
            }
            return;
        }
        if path.extension().is_none_or(|extension| extension != "rs")
            || path
                .file_name()
                .is_some_and(|name| name == "tests.rs" || name == "logging.rs")
        {
            return;
        }
        let source = std::fs::read_to_string(path).unwrap();
        if source.lines().any(|line| line.trim() == "#![cfg(test)]") {
            return;
        }
        let production = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap()
            .split("#[cfg(test)]\npub(crate) mod test_support")
            .next()
            .unwrap();
        let code = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "PlanStorage::",
            "ReviewStorage::",
            "tauqe_core::git::",
            "load_config_with_options(",
            "spawn_blocking(",
            "std::fs::",
            "tokio::fs::",
            "std::process::Command",
            "offload_plan(",
            "offload_review(",
            "normalize_leaf_files(",
            "find_duplicate_item_ids(",
            "effective_groups(",
            ".is_leaf()",
            "ServerImplementationHost",
            "create_provider(",
            ".begin_configuration_change(",
            ".begin_context_change(",
            ".begin_history_change(",
            ".documents_at(",
            ".prepare_implementation_step(",
            ".prepare_implementation_refinement(",
            ".run_architect_contour(",
            ".classify_step_edit_completion(",
            "start_model_turn(",
            "PreparedTurn",
            "build_model_ask_prompt(",
            ".start_interactive_execution(",
            ".finalize_interactive_turn(",
            ".prepare_step_execution(",
            ".execute_prepared_step(",
            ".run_step_loop(",
            ".prepare_plan_execution(",
            ".prepare_plan_queue(",
            ".execute_plan_queue(",
            ".prepare_review_batch(",
            ".execute_review_post_analysis(",
            ".prepare_review_synthesis_request(",
            ".execute_review_synthesis(",
            "async fn run_member(",
            "async fn critic_provider(",
            "async fn post_analysis(",
            "async fn reviewer(",
        ] {
            assert!(
                !code.contains(forbidden),
                "domain implementation bypasses Application in {}: {forbidden}",
                path.display()
            );
        }
    }
    inspect(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
}

#[test]
fn rpc_projections_only_import_the_application_facade() {
    for source in [
        include_str!("../src/contracts.rs"),
        include_str!("../src/publication.rs"),
        include_str!("../src/discussion.rs"),
    ] {
        for dependency in source.split("tauqe_flow::").skip(1) {
            assert!(
                dependency.starts_with("api::") || dependency.starts_with("api as "),
                "RPC projection bypasses the application facade: {}",
                dependency.lines().next().unwrap()
            );
        }
    }
}

#[test]
fn application_publication_has_no_workflow_access() {
    let publication = include_str!("../src/publication.rs");
    for forbidden in [
        "AppState",
        "Application::",
        ".application",
        "tokio::spawn",
        ".flush().await",
    ] {
        assert!(
            !publication.contains(forbidden),
            "publication has workflow access: {forbidden}"
        );
    }
}

#[test]
fn every_routed_method_has_an_application_scenario_and_admission_record() {
    let dispatch = include_str!("../src/handlers.rs");
    let routes = dispatch
        .lines()
        .filter_map(|line| {
            line.trim_start()
                .strip_prefix("methods::")
                .and_then(|line| line.split_once(" =>"))
                .map(|(name, _)| name)
        })
        .collect::<BTreeSet<_>>();
    let mapped = include_str!("../../../docs/rpc-application-boundary.md")
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .map(|line| {
            let columns: Vec<_> = line.split('|').collect();
            assert_eq!(columns.len(), 5, "{line}");
            assert!(!columns[1].trim().is_empty(), "{line}");
            assert!(
                matches!(columns[2].trim(), "Immediate" | "Synchronous" | "Launch"),
                "{line}"
            );
            assert!(
                matches!(columns[3].trim(), "Exclusive" | "Observe" | "Open"),
                "{line}"
            );
            columns[0].trim_end().trim_end_matches('`')
        })
        .collect::<Vec<_>>();
    assert_eq!(mapped.len(), routes.len());
    assert_eq!(mapped.into_iter().collect::<BTreeSet<_>>(), routes);
}

#[test]
fn protocol_does_not_export_the_engine_edit_execution_proposal() {
    let proposals = include_str!("../../protocol/src/edit/proposal.rs");
    assert!(!proposals.contains("pub struct EditProposal"));
}

#[test]
fn protocol_documents_do_not_implement_client_computations() {
    let sources = [
        include_str!("../../protocol/src/plan/implementation.rs"),
        include_str!("../../protocol/src/plan/node.rs"),
        include_str!("../../protocol/src/plan/item.rs"),
        include_str!("../../protocol/src/review/session.rs"),
        include_str!("../../protocol/src/review/item.rs"),
        include_str!("../../protocol/src/review/group.rs"),
    ]
    .join("\n");
    for computation in [
        "fn evaluate_scope",
        "fn compute_items_status",
        "fn count_stats",
        "fn normalize_leaf_files",
        "fn refresh_parent_statuses",
        "fn update_item_status",
        "fn to_markdown",
        "fn next(",
        "fn effective_groups",
        "fn stats(",
        "fn find_item(",
        "fn find_concept_item(",
        "fn is_leaf(",
        "fn validate_structure",
    ] {
        assert!(
            !sources.contains(computation),
            "client computation remains in protocol: {computation}"
        );
    }
    let facade = include_str!("../../protocol/src/lib.rs");
    assert!(!facade.contains("PlanScopeEvaluation"));
    assert!(!facade.contains("PlanValidationError"));
}
