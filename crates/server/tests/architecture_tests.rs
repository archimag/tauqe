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
        ("tauqe-server", &["tauqe-core", "tauqe-protocol"]),
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
            // The core test-support feature is enabled by a self dev-dependency.
            if dependency_name == name && dependency["kind"] == "dev" {
                continue;
            }
            // Composition-root tests compare the real client projection with core.
            if name == "tauqe-server"
                && dependency_name == "tauqe-tui"
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
            if name == "tauqe-core" {
                assert!(
                    !["ratatui", "crossterm", "axum", "tonic", "jsonrpsee"]
                        .contains(&dependency_name),
                    "core must remain headless and transport-free: {dependency_name}"
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
    let core = Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/src");
    for path in paths {
        inspect(&core.join(path));
    }
}
#[test]
fn history_models_do_not_depend_on_protocol() {
    assert_protocol_free(&[
        "history.rs",
        "history",
        "application/history.rs",
        "application/history_operation.rs",
    ]);
}
#[test]
fn context_models_do_not_depend_on_protocol() {
    assert_protocol_free(&[
        "context.rs",
        "context",
        "application/context.rs",
        "application/context_operation.rs",
    ]);
}

#[test]
fn model_configuration_does_not_depend_on_protocol() {
    assert_protocol_free(&[
        "models.rs",
        "models",
        "config/models.rs",
        "config/models",
        "config/providers.rs",
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
        "plan.rs",
        "plan/model",
        "plan/storage.rs",
        "plan/storage",
        "review.rs",
        "review/model",
        "review/storage.rs",
        "review/storage",
        "review/analysis.rs",
        "review/analysis",
        "config/review.rs",
        "application/plan_horizon.rs",
        "application/plan_two_phase.rs",
        "application/review_analysis.rs",
        "application/review_synthesis_completion.rs",
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
        "workflow/kind.rs",
        "workflow/turn.rs",
        "workflow/implementation.rs",
        "workflow/implementation/scope.rs",
        "application/configuration.rs",
        "application/turn_setup.rs",
        "application/implementation.rs",
        "application/implementation_step.rs",
        "application/review_batch.rs",
        "control/refine.rs",
        "edits/protocol.rs",
    ]);
}

#[test]
fn all_core_sources_are_independent_of_protocol() {
    assert_protocol_free(&[""]);
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
fn model_ask_adapter_does_not_load_or_render_domain_documents() {
    let adapter = include_str!("../src/handlers/model/prompt.rs");
    for name in [
        "PlanStorage",
        "ReviewStorage",
        "format_plan_discussion_prompt",
        "format_review_discussion_prompt",
        "effective_groups",
        "parse::<u32>",
    ] {
        assert!(
            !adapter.contains(name),
            "model ask policy remains in server: {name}"
        );
    }
}

#[test]
fn turn_adapter_does_not_classify_step_artifacts_or_select_changed_documents() {
    let adapter = include_str!("../src/handlers/model/turn.rs");
    for policy in [
        ".executed_steps",
        ".commit_error",
        "PlanStorage",
        "ReviewStorage",
        "load_review_session",
        "review_decisions_applied",
    ] {
        assert!(
            !adapter.contains(policy),
            "turn completion policy remains in server: {policy}"
        );
    }
}

#[test]
fn turn_adapter_does_not_assemble_engine_execution_or_read_git_state() {
    let adapter = include_str!("../src/handlers/model/turn.rs");
    for engine_detail in [
        "run_repl_execution",
        "run_step_execution",
        "ReplExecutionConfig",
        "StepExecutionConfig",
        "apply_policy(",
        "context_snapshot()",
        "JsonPromptProtocol",
        "start_interactive_task",
        "run_step_task",
        "get_repository_state",
    ] {
        assert!(
            !adapter.contains(engine_detail),
            "execution assembly remains in server: {engine_detail}"
        );
    }
}

#[test]
fn turn_adapter_delegates_interactive_history_cleanup_and_failure_priority() {
    let adapter = include_str!("../src/handlers/model/turn.rs");
    for policy in [
        "persist_interactive_interruption",
        "persist_interactive_completion",
        "classify_interactive_completion",
        "merge_turn_context",
        "interactive_plan_updates(",
        "interactive_review_updates(",
        "is_cancelled ||",
        "cancel_rx.borrow()",
    ] {
        assert!(
            !adapter.contains(policy),
            "interactive finalization policy remains in server: {policy}"
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
