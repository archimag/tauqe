//! Prepare one implementation attempt before provider execution.

use super::{resolve_frame, PlanResources};
use crate::api::{OperationGuard, OuterDocsRefusal};
use crate::application::documents::Documents;
use crate::application::CompactionEnvironment;
use std::path::Path;
use tauqe_core::config::AppConfig;
use tauqe_core::context::ContextState;
use tauqe_core::outer_docs::OuterDocsRegistry;
use tauqe_core::prompt::outer_docs::append_outer_docs_diagnostics;
use tauqe_core::workflow::implementation::{
    ExecutionScope, StepFailure, StepTurnRequest, StepTurnResult,
};

#[derive(Debug)]
pub enum StepPreparationError {
    OperationNotOwned,
    Documentation(OuterDocsRefusal),
}

impl StepPreparationError {
    pub fn into_step_result(self) -> StepTurnResult {
        match self {
            Self::OperationNotOwned => StepTurnResult::Failed(StepFailure::infrastructure(
                "Step preparation requires an active operation for this session",
            )),
            Self::Documentation(refusal) => StepTurnResult::Abort(refusal.to_message()),
        }
    }
}

/// Publish snapshots synchronously, including updates that precede a trust refusal.
pub(crate) async fn prepare_implementation_step(
    resources: &PlanResources,
    operation: &OperationGuard,
    root: &Path,
    config: &AppConfig,
    compaction: &CompactionEnvironment,
    request: &StepTurnRequest,
    on_context: impl Fn(ContextState) + Send,
) -> Result<String, StepPreparationError> {
    if !resources.step.execution.owns(operation) {
        return Err(StepPreparationError::OperationNotOwned);
    }
    if let Err(error) = resources
        .history
        .compact_history(&resources.step.execution, operation, compaction)
        .await
    {
        tracing::warn!(error = %error, "session history is unavailable for compaction");
    }
    if let Ok(snapshot) = resources.clear_transient_context().await {
        on_context(snapshot);
    }
    let context = &resources.step.context;
    if let Ok(admission) = context
        .preload_declared_context(
            root.to_path_buf(),
            request.files.clone(),
            request.readonly_files.clone(),
        )
        .await
    {
        if !admission.outcome.added.is_empty() {
            on_context(admission.state);
        }
    }
    // Re-read only the active leaf's declarations. An unavailable plan is
    // not fatal to this optional preload.
    let declared = match (&request.scope, request.queue.active()) {
        (ExecutionScope::Plan { plan_id, step_id }, Some(active)) => {
            resolve_frame(&Documents::new(root), plan_id, step_id.as_deref())
                .await
                .ok()
                .and_then(|frame| {
                    frame
                        .plan
                        .implementation
                        .find_item(&active.id)
                        .map(|leaf| leaf.outer_docs.clone())
                })
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let documents = context
        .preload_declared_outer_docs(
            root,
            &OuterDocsRegistry::new(&config.outer_docs),
            &declared,
            config.context.token_budget,
            &request.cancel_rx,
        )
        .await
        .map_err(StepPreparationError::Documentation)?;
    if let Some(snapshot) = documents.context_update {
        on_context(snapshot);
    }
    let mut prompt = request.prompt.clone();
    append_developer_directives(&mut prompt, request.instructions.as_deref());
    append_outer_docs_diagnostics(&mut prompt, &documents.diagnostics);
    Ok(prompt)
}

/// Append the run's developer directives without changing their section format.
fn append_developer_directives(prompt: &mut String, instructions: Option<&str>) {
    if let Some(dirs) = instructions.map(str::trim).filter(|d| !d.is_empty()) {
        prompt.push_str("\n## Developer Directives:\n");
        prompt.push_str(dirs);
        prompt.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use crate::application::PlanExecutionEnvironment;
    use std::sync::Mutex;
    use tauqe_core::context::{ContextAccess, ContextLayer};
    use tauqe_core::models::{ModelRef, ModelSelection};
    use tauqe_core::plan::{Plan, PlanItem};
    use tauqe_core::workflow::turn::TurnWorkItem;
    use tauqe_core::workflow::turn::TurnWorkQueue;

    impl Application {
        async fn prepare_implementation_step(
            &self,
            operation: &OperationGuard,
            root: &Path,
            config: &AppConfig,
            compaction: &CompactionEnvironment,
            request: &StepTurnRequest,
            publish: impl Fn(ContextState) + Send,
        ) -> Result<String, StepPreparationError> {
            prepare_implementation_step(
                &self.plan_resources(),
                operation,
                root,
                config,
                compaction,
                request,
                publish,
            )
            .await
        }
    }

    fn fixture(root: &Path, documents: &[&str]) -> (Application, AppConfig, StepTurnRequest) {
        tauqe_core::git::init_repository(Some(root), true).unwrap();
        let paths: Vec<String> = ["old.rs", "kept.rs", "target.rs", "reference.rs"]
            .into_iter()
            .map(str::to_string)
            .collect();
        for path in &paths {
            std::fs::write(root.join(path), "source").unwrap();
        }
        tauqe_core::git::cmd::stage_files(root, &paths).unwrap();
        let cached = root
            .join(tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join("vendor/guide.md");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(cached, "API reference").unwrap();
        let plan = Plan::from_parts(
            "step-plan".into(),
            "Implementation".into(),
            None,
            0,
            0,
            Vec::new(),
            vec![PlanItem {
                id: "1".into(),
                title: "Step".into(),
                files: vec!["target.rs".into()],
                outer_docs: documents.iter().map(|id| id.to_string()).collect(),
                ..Default::default()
            }],
        );
        tauqe_core::plan::storage::PlanStorage::save_plan(root, &plan).unwrap();
        let config = AppConfig {
            outer_docs: vec![
                crate::application::outer_doc_entry(
                    "vendor",
                    "API",
                    "https://example.test/guide.md",
                ),
                crate::application::outer_doc_entry(
                    "refused",
                    "Refused",
                    "https://user:password@example.test/private.md",
                ),
            ],
            ..Default::default()
        };
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(config.clone(), root.to_path_buf(), history);
        let (_, cancel_rx) = tokio::sync::watch::channel(false);
        let request = StepTurnRequest {
            scope: ExecutionScope::Plan {
                plan_id: "step-plan".into(),
                step_id: None,
            },
            title: "Implementation".into(),
            queue: TurnWorkQueue {
                items: vec![TurnWorkItem {
                    id: "1".into(),
                    title: "Step".into(),
                }],
                active_index: 0,
            },
            prompt: "Execute step.".into(),
            files: vec!["target.rs".into()],
            readonly_files: vec!["reference.rs".into()],
            selection: ModelSelection::Tier {
                model: ModelRef::openrouter("coder/model"),
                effort: None,
            },
            reasoning_effort: None,
            instructions: Some("  Keep API  ".into()),
            marker: None,
            session_id: None,
            cancel_rx,
        };
        (app, config, request)
    }

    #[tokio::test]
    async fn preparation_clears_then_preloads_and_publishes_each_state_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let (app, config, request) = fixture(dir.path(), &["vendor"]);
        app.test_mutate_context(|context| {
            context
                .add_auto_file("old.rs", ContextAccess::ReadOnly)
                .unwrap();
            context
                .add_file("kept.rs", ContextAccess::ReadOnly)
                .unwrap();
        })
        .await
        .unwrap();
        let identity = app.session_uuid().await;
        let updates = Mutex::new(Vec::new());
        let operation = app.try_begin_operation().unwrap();
        let compaction = PlanExecutionEnvironment::new(app.configuration().await, &operation)
            .compaction_environment();
        let prompt = app
            .prepare_implementation_step(
                &operation,
                dir.path(),
                &config,
                &compaction,
                &request,
                |snapshot| updates.lock().unwrap().push(snapshot),
            )
            .await
            .unwrap();
        assert_eq!(
            prompt,
            "Execute step.\n## Developer Directives:\nKeep API\n"
        );
        let updates = updates.into_inner().unwrap();
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0].items.len(), 1);
        assert_eq!(updates[0].items[0].path, "kept.rs");
        assert_eq!(updates[0].items[0].layer, ContextLayer::User);
        assert_eq!(updates[1].items.len(), 3);
        let target = updates[1]
            .items
            .iter()
            .find(|item| item.path == "target.rs")
            .unwrap();
        let reference = updates[1]
            .items
            .iter()
            .find(|item| item.path == "reference.rs")
            .unwrap();
        assert_eq!(target.layer, ContextLayer::Auto);
        assert_eq!(target.access, ContextAccess::Editable);
        assert_eq!(reference.access, ContextAccess::ReadOnly);
        assert_eq!(updates[2].items.len(), 4);
        let document = updates[2]
            .items
            .iter()
            .find(|item| item.layer == ContextLayer::OuterDocs)
            .unwrap();
        assert_eq!(document.path, "vendor/guide.md");
        assert_eq!(document.access, ContextAccess::ReadOnly);
        assert!(updates
            .windows(2)
            .all(|pair| pair[0].revision < pair[1].revision));
        assert_eq!(updates[2].revision, app.context_state().await.revision);
        assert_ne!(identity, app.session_uuid().await);
        assert!(app.try_begin_operation().is_err());
    }

    #[tokio::test]
    async fn trust_refusal_retains_the_prior_cleanup_and_file_admission_updates() {
        let dir = tempfile::tempdir().unwrap();
        let (app, config, request) = fixture(dir.path(), &["vendor", "refused"]);
        let operation = app.try_begin_operation().unwrap();
        let compaction = PlanExecutionEnvironment::new(app.configuration().await, &operation)
            .compaction_environment();
        let updates = Mutex::new(Vec::new());
        let error = app
            .prepare_implementation_step(
                &operation,
                dir.path(),
                &config,
                &compaction,
                &request,
                |snapshot| updates.lock().unwrap().push(snapshot),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, StepPreparationError::Documentation(_)));
        assert!(
            matches!(error.into_step_result(), StepTurnResult::Abort(message) if message.contains("The step was not started"))
        );
        let updates = updates.into_inner().unwrap();
        assert_eq!(updates.len(), 2);
        assert!(updates[0].items.is_empty());
        assert_eq!(updates[1].items.len(), 2);
        assert_eq!(updates[1].revision, app.context_state().await.revision);
        assert!(app
            .context_state()
            .await
            .items
            .iter()
            .all(|item| item.layer != ContextLayer::OuterDocs));
    }

    #[tokio::test]
    async fn foreign_operation_is_rejected_before_any_preparation_side_effect() {
        let dir = tempfile::tempdir().unwrap();
        let (app, config, request) = fixture(dir.path(), &["vendor"]);
        app.test_mutate_context(|context| context.add_auto_file("old.rs", ContextAccess::ReadOnly))
            .await
            .unwrap()
            .unwrap();
        let other = Application::default();
        let foreign = other.try_begin_operation().unwrap();
        let _own = app.try_begin_operation().unwrap();
        let compaction = PlanExecutionEnvironment::new(app.configuration().await, &_own)
            .compaction_environment();
        let identity = app.session_uuid().await;
        let before = app.context_state().await;
        let updates = Mutex::new(Vec::new());
        let error = app
            .prepare_implementation_step(
                &foreign,
                dir.path(),
                &config,
                &compaction,
                &request,
                |snapshot| updates.lock().unwrap().push(snapshot),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, StepPreparationError::OperationNotOwned));
        assert_eq!(
            error.into_step_result(),
            StepTurnResult::Failed(StepFailure::infrastructure(
                "Step preparation requires an active operation for this session"
            ))
        );
        assert!(updates.into_inner().unwrap().is_empty());
        assert_eq!(before.revision, app.context_state().await.revision);
        assert_eq!(identity, app.session_uuid().await);
    }

    #[tokio::test]
    async fn delivery_diagnostics_follow_directives_without_failing_the_step() {
        let dir = tempfile::tempdir().unwrap();
        let (app, config, request) = fixture(dir.path(), &["missing"]);
        let operation = app.try_begin_operation().unwrap();
        let compaction = PlanExecutionEnvironment::new(app.configuration().await, &operation)
            .compaction_environment();
        let prompt = app
            .prepare_implementation_step(
                &operation,
                dir.path(),
                &config,
                &compaction,
                &request,
                |_| {},
            )
            .await
            .unwrap();
        let directives = prompt.find("## Developer Directives:").unwrap();
        let diagnostics = prompt
            .find("## Declared External Documentation Unavailable")
            .unwrap();
        assert!(directives < diagnostics);
        assert!(prompt.contains("`missing`"));
        assert!(prompt.contains("route around the missing document"));
    }

    #[tokio::test]
    async fn preload_uses_the_frozen_registry_and_budget() {
        let dir = tempfile::tempdir().unwrap();
        let (_, mut frozen, request) = fixture(dir.path(), &["vendor"]);
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let app =
            Application::for_repository(AppConfig::default(), dir.path().to_path_buf(), history);
        let operation = app.try_begin_operation().unwrap();
        let compaction = PlanExecutionEnvironment::new(app.configuration().await, &operation)
            .compaction_environment();

        frozen.context.token_budget = 0;
        let prompt = app
            .prepare_implementation_step(
                &operation,
                dir.path(),
                &frozen,
                &compaction,
                &request,
                |_| {},
            )
            .await
            .unwrap();
        assert!(prompt.contains("exceed the configured token budget"));
        assert!(app
            .context_state()
            .await
            .items
            .iter()
            .all(|item| item.layer != ContextLayer::OuterDocs));

        frozen.context.token_budget = usize::MAX;
        let prompt = app
            .prepare_implementation_step(
                &operation,
                dir.path(),
                &frozen,
                &compaction,
                &request,
                |_| {},
            )
            .await
            .unwrap();
        assert!(!prompt.contains("Declared External Documentation Unavailable"));
        assert!(app.context_state().await.items.iter().any(|item| {
            item.layer == ContextLayer::OuterDocs && item.path == "vendor/guide.md"
        }));
    }
}
