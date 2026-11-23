//! Dependency gates for the architecture boundaries shared by the workspace.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use syn::visit::{self, Visit};

// Cargo and Nextest run integration tests from the package directory. Use
// that checkout rather than the build-time path embedded in the test binary.
fn server_dir() -> PathBuf {
    std::env::current_dir().expect("resolve the server test working directory")
}

fn read_source(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read source {}: {err}", path.display()))
}

fn source_dir_entries(path: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(path)
        .unwrap_or_else(|err| panic!("cannot list source directory {}: {err}", path.display()))
        .filter_map(|entry| {
            let entry = entry.unwrap();
            // Emacs lock files retain the source extension and can be dangling symlinks.
            (!entry.file_name().to_string_lossy().starts_with(".#")).then(|| entry.path())
        })
}

#[test]
fn workspace_dependencies_follow_module_boundaries() {
    let root = server_dir().join("../..");
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
            for child in source_dir_entries(path) {
                inspect(&child);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = read_source(path);
            assert!(
                !source.contains("tauqe_protocol"),
                "subsystem must own its models: {}",
                path.display()
            );
        }
    }
    let crates = server_dir().join("..");
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
fn core_history_does_not_run_or_schedule_compaction_models() {
    let sources = [
        include_str!("../../core/src/history.rs"),
        include_str!("../../core/src/history/compaction.rs"),
    ];
    for source in sources {
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "LlmProvider",
            "structured::execute",
            "history_compaction_contract",
            "compact_history_if_needed_with_budget",
            "if !history_manager.needs_compaction",
        ] {
            assert!(
                !production.contains(forbidden),
                "core history schedules a Scribe call: {forbidden}"
            );
        }
    }
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
        "flow/src/workflows/plan/synthesis.rs",
        "flow/src/workflows/plan/synthesis",
        "flow/src/workflows/review/analysis.rs",
        "flow/src/workflows/review/analysis",
        "flow/src/workflows/review/audit",
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
        "flow/src/workflows/step/attempts.rs",
        "flow/src/workflows/step/queue.rs",
        "flow/src/workflows/plan/preparation.rs",
        "flow/src/application/review_batch.rs",
        "flow/src/workflows/review.rs",
        "core/src/plan/prompt/refine.rs",
        "core/src/edits/protocol.rs",
    ]);
}

#[test]
fn core_and_flow_sources_are_independent_of_protocol() {
    assert_protocol_free(&["core/src", "flow/src"]);
}

#[test]
fn review_scenarios_do_not_construct_or_stream_provider_requests() {
    let application = server_dir().join("../flow/src/application");
    for path in source_dir_entries(&application) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("review_") || path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let source = read_source(&path);
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in ["LlmRequest", ".stream(", "LlmProvider"] {
            assert!(
                !production.contains(forbidden),
                "review scenario {} bypasses structured execution: {forbidden}",
                path.display()
            );
        }
    }
}

/// Exclude test modules and comments without treating incidental prose as a call site.
fn production_code(source: &str) -> String {
    source
        .split("#[cfg(test)]")
        .next()
        .unwrap()
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn scenarios_do_not_wire_provider_requests() {
    let flow = server_dir().join("../flow/src");
    for name in [
        "application/ask.rs",
        "workflows/architect.rs",
        "workflows/architect/synthesis.rs",
        "workflows/plan/synthesis.rs",
        "workflows/plan/synthesis/replan.rs",
        "workflows/plan/attempt.rs",
        "workflows/plan.rs",
        "workflows/plan/run.rs",
        "workflows/plan/queue.rs",
        "application/review_batch.rs",
        "workflows/review.rs",
        "application/review_entry.rs",
        "workflows/review/audit.rs",
        "workflows/review/audit/groups.rs",
        "workflows/review/audit/synthesis_execution.rs",
        "workflows/review/analysis.rs",
        "workflows/review/analysis/execution.rs",
        "workflows/review/member.rs",
        "workflows/review/sequence.rs",
        "application/locale_generation.rs",
        "application/git_squash.rs",
        "application/history_compaction.rs",
        "workflows/repl/publication.rs",
    ] {
        let code = production_code(&read_source(&flow.join(name)));
        for forbidden in [
            ".stream(",
            "LlmRequest",
            "PromptProtocol",
            "ApplyPolicy",
            "LlmProvider",
        ] {
            assert!(
                !code.contains(forbidden),
                "scenario {name} bypasses execution infrastructure: {forbidden}"
            );
        }
    }
}

#[test]
fn core_provider_calls_use_the_shared_request_mechanism() {
    fn inspect(path: &Path) {
        if path.is_dir() {
            for child in source_dir_entries(path) {
                inspect(&child);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let code = production_code(&read_source(path));
            assert!(
                !code.contains(".stream("),
                "core bypasses shared request execution: {}",
                path.display()
            );
        }
    }
    let core = server_dir().join("../core/src");
    // Provider implementations (including the retry decorator) implement their
    // own transport. Only the shared request module may invoke it from core.
    for path in source_dir_entries(&core) {
        if path
            .file_name()
            .is_some_and(|name| name == "providers" || name == "providers.rs")
        {
            continue;
        }
        if path.file_name().is_some_and(|name| name == "execution") {
            for child in source_dir_entries(&path) {
                if child.file_name().is_some_and(|name| name == "request.rs") {
                    continue;
                }
                inspect(&child);
            }
        } else {
            inspect(&path);
        }
    }
    assert!(
        production_code(include_str!("../../core/src/execution/request.rs"))
            .contains("provider.stream(")
    );
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
fn production_repl_uses_the_shared_workflow() {
    let factory = include_str!("../../flow/src/application/infrastructure/execution.rs");
    assert!(!factory.contains("DialogueExecution"));
    assert!(!factory.contains("for_dialogue"));
    assert!(!factory.contains("ReplExecution"));
    assert!(!factory.contains("for_repl("));
    let ask = production_code(include_str!("../../flow/src/application/ask.rs"));
    assert!(ask.contains("repl::run("));
    let repl = production_code(include_str!("../../flow/src/workflows/repl.rs"));
    for retired in [
        "ReplWorkflow",
        "finalize_interactive_turn",
        "InteractiveFinalizationObserver",
    ] {
        assert!(
            !repl.contains(retired),
            "retired REPL orchestration: {retired}"
        );
    }
    for source in [
        include_str!("../../flow/src/workflows/repl.rs"),
        include_str!("../../flow/src/workflows/turn.rs"),
        include_str!("../../flow/src/workflows/round.rs"),
    ] {
        let code = production_code(source);
        assert!(!code.contains("LlmProvider"));
        assert!(!code.contains("PromptEnvironment"));
    }
}

#[test]
fn plan_and_review_steps_share_the_isolated_turn_without_legacy_execution() {
    let flow = server_dir().join("../flow/src");
    for (path, required) in [
        ("application/plan_batch.rs", ".launch_batch("),
        ("workflows/plan/queue.rs", "run_batch(resources,"),
        ("workflows/plan/run.rs", "run_queue("),
        (
            "workflows/step/queue.rs",
            "self.leaf.run(&step.id, stream_tx, cancel_rx)",
        ),
        (
            "workflows/step/attempts.rs",
            "self.operations.attempt(request)",
        ),
        ("workflows/plan/attempt.rs", ".step.run_published_attempt("),
        (
            "workflows/step/run.rs",
            "self.execute_prepared_single_step(",
        ),
        ("application/review_batch.rs", ".launch_batch("),
        ("workflows/review.rs", "ReviewSequence {"),
        ("workflows/review/sequence.rs", "run_queue("),
        ("workflows/review/sequence.rs", ".step.prepare_single_step("),
        (
            "workflows/review/sequence.rs",
            ".step.execute_prepared_single_step(",
        ),
        ("workflows/step/run.rs", "attempt.run(disk)"),
        ("workflows/step.rs", "TurnWorkflow {"),
        ("workflows/turn.rs", "RoundWorkflow"),
        ("workflows/round.rs", "PromptExecutor"),
    ] {
        let code = production_code(&read_source(&flow.join(path)));
        let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let required_compact: String = required.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains(&required_compact),
            "{path} must use {required}"
        );
        for forbidden in [
            "ExecutionUnit",
            "run_turn_loop",
            "execute_bounded",
            "EditExecution",
            "for_edit_unit",
            "classify_step_turn",
            "repl::run",
            "StepPersistence",
            "mark_taken(",
        ] {
            assert!(
                !compact.contains(forbidden),
                "retired route in {path}: {forbidden}"
            );
        }
    }
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Path>()
                .is_ok_and(|path| path.is_ident("test"))
    })
}

struct WorkflowBoundary<'a> {
    path: &'a Path,
    uses: Vec<String>,
}

impl WorkflowBoundary<'_> {
    fn check_application_port(&self, name: &str) {
        assert!(
            !matches!(
                name,
                "ask"
                    | "plan_batch"
                    | "review_batch"
                    | "review_entry"
                    | "plan_execution"
                    | "plan_run"
                    | "review_execution"
                    | "review_sequence"
                    | "session"
                    | "*"
            ),
            "{} depends on an application entry or private executor: {name}",
            self.path.display()
        );
    }
}

impl<'ast> Visit<'ast> for WorkflowBoundary<'_> {
    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        assert!(
            !["Application", "PlanRunHost", "ReviewBatchHost"]
                .iter()
                .any(|name| ident == *name),
            "{} depends on the application facade: {ident}",
            self.path.display()
        );
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !test_only(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if !test_only(&item.attrs) {
            visit::visit_item_impl(self, item);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if !test_only(&item.attrs) {
            visit::visit_item_use(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if !test_only(&item.attrs) {
            visit::visit_item_fn(self, item);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        for (segment, next) in path.segments.iter().zip(path.segments.iter().skip(1)) {
            if segment.ident == "application" {
                self.check_application_port(&next.ident.to_string());
            }
        }
        visit::visit_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'ast syn::UseTree) {
        if self.uses.last().is_some_and(|name| name == "application") {
            match tree {
                syn::UseTree::Path(path) => self.check_application_port(&path.ident.to_string()),
                syn::UseTree::Name(name) => self.check_application_port(&name.ident.to_string()),
                syn::UseTree::Rename(rename) => {
                    self.check_application_port(&rename.ident.to_string())
                }
                syn::UseTree::Glob(_) => self.check_application_port("*"),
                syn::UseTree::Group(_) => {}
            }
        }
        if let syn::UseTree::Path(path) = tree {
            self.uses.push(path.ident.to_string());
            visit::visit_use_tree(self, tree);
            self.uses.pop();
        } else {
            visit::visit_use_tree(self, tree);
        }
    }
}

#[test]
fn migrated_workflows_do_not_depend_on_the_application_facade() {
    fn inspect(path: &Path) {
        if path.is_dir() {
            for child in source_dir_entries(path) {
                inspect(&child);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            // The interactive supervisor is not part of the plan/review migration.
            // Shared turn and edit-cycle mechanics are checked independently.
            if [
                "repl.rs",
                "repl/publication.rs",
                "repl/tests.rs",
                "step/run/tests.rs",
            ]
            .iter()
            .any(|name| path.ends_with(name))
            {
                return;
            }
            let file = syn::parse_file(&read_source(path))
                .unwrap_or_else(|error| panic!("cannot parse {}: {error}", path.display()));
            if !test_only(&file.attrs) {
                WorkflowBoundary {
                    path,
                    uses: Vec::new(),
                }
                .visit_file(&file);
            }
        }
    }

    let flow = server_dir().join("../flow/src");
    inspect(&flow.join("workflows.rs"));
    inspect(&flow.join("workflows"));
}

#[test]
fn plan_execution_policy_is_behind_the_workflow_entry_and_document_storage() {
    let flow = server_dir().join("../flow/src");
    let batch = production_code(&read_source(&flow.join("application/plan_batch.rs")));
    for required in [".launch_batch(", ".launch_pipeline("] {
        assert!(
            batch.contains(required),
            "plan launch must delegate to {required}"
        );
    }
    for forbidden in [
        "evaluate_scope(",
        "attempt_replan(",
        "for plan_id in",
        "PlanStorage::",
        "ApplicationEvent::PlanBatchFinished",
        "run_preflight_verification(",
        "try_begin_operation(",
        "tokio::spawn",
    ] {
        assert!(
            !batch.contains(forbidden),
            "plan policy or storage in application launch: {forbidden}"
        );
    }
    let review = production_code(&read_source(&flow.join("application/review_batch.rs")));
    assert!(review.contains(".launch_batch("));
    for forbidden in [
        "ReviewSequence",
        "QueueStop",
        "settle_run(",
        "run_queue(",
        "run_preflight_verification(",
        "try_begin_operation(",
        "tokio::spawn",
        "ApplicationEvent::ReviewBatchFinished",
    ] {
        assert!(
            !review.contains(forbidden),
            "review policy in application: {forbidden}"
        );
    }
    assert!(!flow.join("application/review_sequence.rs").exists());
    assert!(!flow.join("application/review_execution.rs").exists());
    assert!(!flow.join("application/plan_run.rs").exists());
    let plan_run = production_code(&read_source(&flow.join("workflows/plan/run.rs")));
    for required in [
        "impl LeafOperations for",
        "attempt_replan(",
        "ApplicationEvent::PlanDocument",
        "ApplicationEvent::PlanBatchFinished",
    ] {
        assert!(plan_run.contains(required), "plan run must own {required}");
    }
    assert!(!plan_run.contains("ExecutionScope::Review"));
    let plan_queue = production_code(&read_source(&flow.join("workflows/plan/queue.rs")));
    assert!(!plan_queue.contains(".run_plan_batch("));
    for path in [
        "workflows/plan.rs",
        "workflows/plan/run.rs",
        "workflows/plan/queue.rs",
        "workflows/review.rs",
        "application/review_batch.rs",
        "workflows/step/attempts.rs",
        "workflows/step/queue.rs",
    ] {
        let code = production_code(&read_source(&flow.join(path)));
        for forbidden in [
            "PlanStorage",
            "ReviewStorage",
            "std::fs::",
            "spawn_blocking(",
            "tokio::fs::",
            "tauqe_protocol",
        ] {
            assert!(
                !code.contains(forbidden),
                "filesystem dependency in {path}: {forbidden}"
            );
        }
    }
}

#[test]
fn scenario_sources_keep_document_storage_and_plan_execution_separate() {
    fn inspect(path: &Path, review: bool) {
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                return;
            }
            for child in source_dir_entries(path) {
                inspect(&child, review);
            }
            return;
        }
        if path.extension().is_none_or(|ext| ext != "rs")
            || path.file_name().is_some_and(|name| name == "tests.rs")
        {
            return;
        }
        let source = read_source(path);
        if source.lines().any(|line| line.trim() == "#![cfg(test)]") {
            return;
        }
        let code = production_code(&source);
        let identifiers: Vec<_> = code
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|token| !token.is_empty())
            .collect();
        for storage in ["PlanStorage", "ReviewStorage"] {
            assert!(
                !identifiers.contains(&storage),
                "{} must access documents through the application service, not {storage}",
                path.display()
            );
        }
        if review {
            // Core plan types and horizon rules are shared domain mechanisms,
            // not a dependency on the flow scenario or its execution host.
            for dependency in [
                ["workflows", "plan"],
                ["super", "plan"],
                ["application", "plan_run"],
            ] {
                assert!(
                    !identifiers.windows(2).any(|pair| pair == dependency),
                    "{} depends on private plan execution: {}",
                    path.display(),
                    dependency.join("::")
                );
            }
        }
    }

    let flow = server_dir().join("../flow/src");
    for path in ["workflows/plan.rs", "workflows/plan"] {
        inspect(&flow.join(path), false);
    }
    for path in [
        "workflows/review.rs",
        "workflows/review",
        "application/review_batch.rs",
    ] {
        inspect(&flow.join(path), true);
    }
}

#[test]
fn implementation_targets_use_scenario_resolvers_and_document_access() {
    let flow = server_dir().join("../flow/src");
    assert!(!flow.join("application/implementation.rs").exists());
    assert!(!flow.join("workflows/implementation.rs").exists());
    for (path, constructors) in [
        ("workflows/plan.rs", &["ImplementationFrame::from_plan"][..]),
        (
            "workflows/review.rs",
            &[
                "ImplementationFrame::from_group",
                "ImplementationFrame::from_finding",
            ][..],
        ),
    ] {
        let code = production_code(&read_source(&flow.join(path)));
        for &constructor in constructors {
            assert!(code.contains(constructor), "{path} must use {constructor}");
        }
    }
    for path in [
        "workflows/plan/run.rs",
        "workflows/plan/refinement.rs",
        "workflows/plan/preparation.rs",
        "workflows/review/member.rs",
        "workflows/review/sequence.rs",
    ] {
        let code = production_code(&read_source(&flow.join(path)));
        assert!(
            !code.contains("ImplementationFrame::from_"),
            "{path} must use a scenario resolver to construct frames"
        );
        for forbidden in ["PlanStorage", "ReviewStorage"] {
            assert!(
                !code
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|identifier| identifier == forbidden),
                "{path} bypasses target resolution: {forbidden}"
            );
        }
    }
}

#[test]
fn shared_leaf_execution_does_not_choose_storage_or_batch_policy() {
    let leaf = production_code(include_str!("../../flow/src/workflows/step/attempts.rs"));
    for forbidden in [
        "PlanStorage",
        "ReviewStorage",
        "Application",
        "BatchStepOutcome",
        "ExecutionScope::",
        "IssueStatus",
        "write_plan_leaf",
        "write_group_leaf",
        "write_finding_status",
    ] {
        assert!(
            !leaf.contains(forbidden),
            "shared leaf depends on {forbidden}"
        );
    }
}

#[test]
fn shared_step_queue_executes_without_choosing_refinement() {
    let horizon = production_code(include_str!("../../flow/src/workflows/step/queue.rs"));
    assert!(horizon.contains("impl BatchStepExecutor for QueueExecutor"));
    assert!(horizon.contains("BatchExecution::new()"));
    for forbidden in [
        "PlanStorage",
        "ReviewStorage",
        "run_refinement",
        "attempt_replan",
    ] {
        assert!(
            !horizon.contains(forbidden),
            "shared queue depends on {forbidden}"
        );
    }
}

#[test]
fn shared_technical_execution_does_not_own_plan_or_review_status() {
    let crates = server_dir().join("..");
    for path in [
        "flow/src/workflows/step.rs",
        "flow/src/workflows/turn.rs",
        "flow/src/workflows/round.rs",
        "core/src/execution/executor.rs",
    ] {
        let code = production_code(&read_source(&crates.join(path)));
        for forbidden in [
            "PlanItemStatus",
            "IssueStatus",
            "StepTurnResult",
            "StepPersistence",
        ] {
            assert!(
                !code.contains(forbidden),
                "domain lifecycle in {path}: {forbidden}"
            );
        }
    }
}

#[test]
fn workspace_does_not_define_or_depend_on_retired_control() {
    fn inspect(path: &Path) {
        if path.is_dir() {
            for child in source_dir_entries(path) {
                inspect(&child);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !path.ends_with("server/tests/architecture_tests.rs")
        {
            let source = read_source(path);
            let code = source
                .lines()
                .map(|line| line.split("//").next().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !references_legacy_control(&code),
                "source depends on retired control module: {}",
                path.display()
            );
        }
    }
    let crates = server_dir().join("..");
    let core = crates.join("core/src");
    for path in ["control.rs", "control"] {
        assert!(!core.join(path).exists(), "retired path exists: {path}");
    }
    let facade = read_source(&core.join("lib.rs"));
    let tokens = facade
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    assert!(!tokens.windows(2).any(|pair| pair == ["mod", "control"]));
    for krate in source_dir_entries(&crates).filter(|path| path.join("Cargo.toml").is_file()) {
        for directory in ["src", "tests"] {
            inspect(&krate.join(directory));
        }
    }
}

fn references_legacy_control(code: &str) -> bool {
    let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains("control::")
        || code.split("::").skip(1).any(|tail| {
            tail.trim_start()
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                == Some("control")
        })
    {
        return true;
    }
    // Grouped imports can name the module without an adjacent path separator.
    // Inspect entire use statements, including multiline public reexports,
    // rather than treating ordinary prompt prose as a Rust dependency.
    let mut in_use = false;
    for line in code.lines() {
        let line = line.trim_start();
        in_use |= line.starts_with("use ")
            || line.starts_with("pub use ")
            || (line.starts_with("pub(") && line.contains(" use "));
        if in_use {
            if line
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|token| token == "control")
            {
                return true;
            }
            if line.contains(';') {
                in_use = false;
            }
        }
    }
    false
}

#[test]
fn legacy_dependency_gate_distinguishes_imports_from_prompt_prose() {
    assert!(!references_legacy_control(
        "const INTRO: &str = \"engineering control environment\";"
    ));
    assert!(!references_legacy_control("app::controller();"));
    for source in [
        "crate::control::run_turn_loop();",
        "use tauqe_core :: control as legacy;",
        "pub use crate::{execution, control};",
        "pub(crate) use crate::{\n execution,\n control as legacy,\n};",
    ] {
        assert!(references_legacy_control(source), "missed {source}");
    }
}

#[test]
fn architect_research_uses_shared_execution_without_an_edit_turn_or_legacy_control() {
    let synthesis = production_code(include_str!(
        "../../flow/src/workflows/architect/synthesis.rs"
    ));
    let factory = production_code(include_str!(
        "../../flow/src/application/infrastructure/execution.rs"
    ));
    for code in [&synthesis, &factory] {
        assert!(!references_legacy_control(code));
        assert!(!code.contains("StructuredResearchUnit"));
    }
    assert!(synthesis.contains(".execute_round("));
    assert!(factory
        .contains(".execute_decoded(events, cancel, PlanRefinePromptProtocol::decode_round)"));
    assert!(synthesis.contains("execute_research_commands("));
    for retired in ["ApplyPolicy", "TurnWorkflow", "TurnDisk"] {
        assert!(!synthesis.contains(retired));
    }
}

#[test]
fn executor_has_no_turn_policy() {
    let executor = production_code(include_str!("../../core/src/execution/executor.rs"));
    for forbidden in [
        "ApplyPolicy",
        "WorkflowKind",
        "LoopContext",
        "TurnState",
        "TurnTranscript",
        "TurnRoundStarted",
        "accept_round",
        "max_turn_rounds",
        "MAX_EMPTY",
        "run_turn_loop",
        "execute_research_step",
        "execute_edit_step",
    ] {
        assert!(
            !executor.contains(forbidden),
            "executor owns turn policy: {forbidden}"
        );
    }
}

#[test]
fn protocol_model_schema_boundary_remains_intact() {
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
            for child in source_dir_entries(path) {
                inspect(&child);
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
        let source = read_source(path);
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
    inspect(&server_dir().join("src"));
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
