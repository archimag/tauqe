//! Prepare one implementation attempt before provider execution.

use crate::api::{Application, ContextClearScope, OperationGuard, OuterDocsRefusal};
use crate::config::AppConfig;
use crate::context::ContextState;
use crate::prompt::outer_docs::append_outer_docs_diagnostics;
use crate::workflow::implementation::{StepFailure, StepTurnRequest, StepTurnResult};
use std::path::Path;

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

impl Application {
    /// Publish snapshots synchronously at their original boundaries, including
    /// updates that precede a trust refusal. The callback receives no shared state.
    pub async fn prepare_implementation_step(
        &self,
        operation: &OperationGuard,
        root: &Path,
        config: &AppConfig,
        request: &StepTurnRequest,
        on_context: impl Fn(ContextState) + Send,
    ) -> Result<String, StepPreparationError> {
        if !self.owns_operation(operation) {
            return Err(StepPreparationError::OperationNotOwned);
        }
        if let Err(error) = self
            .compact_history(operation, config, request.cancel_rx.clone())
            .await
        {
            tracing::warn!(error = %error, "session history is unavailable for compaction");
        }
        if let Ok(snapshot) = self.clear_context(ContextClearScope::Transient).await {
            on_context(snapshot);
        }
        if let Ok(admission) = self
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
        let documents = self
            .preload_implementation_outer_docs(
                root,
                &request.scope,
                &request.queue,
                config,
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
    use crate::config::OuterDocEntry;
    use crate::context::{ContextAccess, ContextLayer};
    use crate::models::{ModelRef, ModelSelection};
    use crate::plan::{Plan, PlanItem};
    use crate::workflow::implementation::ExecutionScope;
    use crate::workflow::turn::TurnWorkItem;
    use crate::workflow::turn::TurnWorkQueue;
    use std::sync::Mutex;

    fn fixture(root: &Path, documents: &[&str]) -> (Application, AppConfig, StepTurnRequest) {
        crate::git::init_repository(Some(root), true).unwrap();
        let paths: Vec<String> = ["old.rs", "kept.rs", "target.rs", "reference.rs"]
            .into_iter()
            .map(str::to_string)
            .collect();
        for path in &paths {
            std::fs::write(root.join(path), "source").unwrap();
        }
        crate::git::cmd::stage_files(root, &paths).unwrap();
        let cached = root
            .join(crate::outer_docs::OUTER_DOCS_CACHE_DIR)
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
        crate::plan::storage::PlanStorage::save_plan(root, &plan).unwrap();
        let config = AppConfig {
            outer_docs: vec![
                OuterDocEntry {
                    id: "vendor".into(),
                    title: "API".into(),
                    description: None,
                    source: "https://example.test/guide.md".into(),
                },
                OuterDocEntry {
                    id: "refused".into(),
                    title: "Refused".into(),
                    description: None,
                    source: "https://user:password@example.test/private.md".into(),
                },
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
        let prompt = app
            .prepare_implementation_step(&operation, dir.path(), &config, &request, |snapshot| {
                updates.lock().unwrap().push(snapshot)
            })
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
        let updates = Mutex::new(Vec::new());
        let error = app
            .prepare_implementation_step(&operation, dir.path(), &config, &request, |snapshot| {
                updates.lock().unwrap().push(snapshot)
            })
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
        let identity = app.session_uuid().await;
        let before = app.context_state().await;
        let updates = Mutex::new(Vec::new());
        let error = app
            .prepare_implementation_step(&foreign, dir.path(), &config, &request, |snapshot| {
                updates.lock().unwrap().push(snapshot)
            })
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
        let prompt = app
            .prepare_implementation_step(&operation, dir.path(), &config, &request, |_| {})
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
}
