//! Resolve stored implementation frames and prepare declared step documents.

use crate::api::{Application, OuterDocsPreload, OuterDocsRefusal};
use crate::config::AppConfig;
use crate::outer_docs::OuterDocsRegistry;
use crate::workflow::implementation::{ExecutionScope, ImplementationFrame, StepFailure};
use crate::workflow::turn::TurnWorkQueue;
use std::path::Path;
use tokio::sync::watch;

impl Application {
    pub async fn implementation_frame(
        &self,
        scope: &ExecutionScope,
        root: &Path,
    ) -> Result<ImplementationFrame, StepFailure> {
        resolve_frame(scope, root).await
    }

    /// Re-resolve the active leaf from storage and use the running turn's frozen
    /// configuration. Failed frame resolution retains the existing empty preload.
    pub(crate) async fn preload_implementation_outer_docs(
        &self,
        root: &Path,
        scope: &ExecutionScope,
        queue: &TurnWorkQueue,
        config: &AppConfig,
        cancel: &watch::Receiver<bool>,
    ) -> Result<OuterDocsPreload, OuterDocsRefusal> {
        let declared = declared_outer_docs(scope, queue, root).await;
        if declared.is_empty() {
            return Ok(OuterDocsPreload {
                diagnostics: Vec::new(),
                context_update: None,
            });
        }
        let registry = OuterDocsRegistry::new(&config.outer_docs);
        self.preload_declared_outer_docs(
            root,
            &registry,
            &declared,
            config.context.token_budget,
            cancel,
        )
        .await
    }
}

/// Resolves the frame a scope executes without occupying an async worker.
///
/// Resolving a frame reads the plan or the review session from disk and
/// takes the process-wide storage mutex the storage engines hold, which is
/// the blocking work the application offloads.
/// A resolution the blocking pool could not complete is reported as an
/// infrastructure failure instead of stalling an executor thread.
async fn resolve_frame(
    scope: &ExecutionScope,
    repo_root: &Path,
) -> Result<ImplementationFrame, StepFailure> {
    let scope = scope.clone();
    let root = repo_root.to_path_buf();
    match crate::blocking::run_blocking_async(move || scope.resolve(&root)).await {
        Ok(resolved) => resolved,
        Err(reason) => Err(StepFailure::infrastructure(format!(
            "The implementation frame could not be resolved: blocking task failed: {reason}"
        ))),
    }
}

/// Resolves the external documents the active step of the run declares.
///
/// The step identity comes from the queue the request carries, so the
/// documents belong to the leaf the turn actually executes rather than to the
/// horizon as a whole. The horizon is re-resolved from the storage that owns
/// it, exactly as the pipeline resolves it before launching the step, so the
/// application never carries a second copy of the step graph. A scope that cannot be
/// read declares nothing: the pipeline reports that failure through the
/// diagnostic it owns.
async fn declared_outer_docs(
    scope: &ExecutionScope,
    queue: &TurnWorkQueue,
    repo_root: &Path,
) -> Vec<String> {
    let Ok(frame) = resolve_frame(scope, repo_root).await else {
        return Vec::new();
    };
    let Some(step_id) = queue.active().map(|item| item.id.as_str()) else {
        return Vec::new();
    };
    frame
        .plan
        .implementation
        .find_item(step_id)
        .map(|step| step.outer_docs.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::storage::PlanStorage;
    use crate::plan::{Plan, PlanItem};
    use crate::workflow::turn::TurnWorkItem;
    #[tokio::test]
    async fn the_active_step_declares_the_external_documents_it_preloads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut plan = Plan::from_parts(
            "outer-docs-plan".to_string(),
            "Outer Docs Plan".to_string(),
            None,
            0,
            0,
            Vec::new(),
            vec![
                PlanItem {
                    id: "1".to_string(),
                    title: "Read the published contract".to_string(),
                    // A leaf always declares what it targets, so the horizon
                    // validator accepts it and the step preloads real files.
                    files: vec!["src/client.rs".to_string()],
                    outer_docs: vec!["vendor/api.md".to_string()],
                    ..PlanItem::default()
                },
                PlanItem {
                    id: "2".to_string(),
                    title: "Wire the client".to_string(),
                    files: vec!["src/client.rs".to_string()],
                    ..PlanItem::default()
                },
            ],
        );
        PlanStorage::save_plan(&root, &plan).expect("store the plan");
        let scope = ExecutionScope::Plan {
            plan_id: "outer-docs-plan".to_string(),
            step_id: None,
        };
        let queue = |id: &str, title: &str| TurnWorkQueue {
            items: vec![TurnWorkItem {
                id: id.to_string(),
                title: title.to_string(),
            }],
            active_index: 0,
        };

        assert_eq!(
            declared_outer_docs(&scope, &queue("1", "Read the published contract"), &root).await,
            vec!["vendor/api.md".to_string()]
        );
        // The declaration belongs to the running leaf, so a step that declares
        // none mounts nothing even though the horizon declares documentation.
        assert!(
            declared_outer_docs(&scope, &queue("2", "Wire the client"), &root)
                .await
                .is_empty()
        );
        plan.implementation.items[0].outer_docs = vec!["vendor/revised.md".into()];
        PlanStorage::save_plan(&root, &plan).unwrap();
        assert_eq!(
            declared_outer_docs(&scope, &queue("1", "Read the published contract"), &root).await,
            vec!["vendor/revised.md"]
        );
        assert!(
            declared_outer_docs(&scope, &queue("missing", "Missing"), &root)
                .await
                .is_empty()
        );
        let invalid_queue = TurnWorkQueue {
            items: Vec::new(),
            active_index: 10,
        };
        assert!(declared_outer_docs(&scope, &invalid_queue, &root)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_scope_that_cannot_be_resolved_declares_no_external_documents() {
        let queue = TurnWorkQueue {
            items: vec![TurnWorkItem {
                id: "1".to_string(),
                title: "Step".to_string(),
            }],
            active_index: 0,
        };
        let scope = ExecutionScope::Plan {
            plan_id: "absent".to_string(),
            step_id: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let declared = declared_outer_docs(&scope, &queue, &root);
        assert!(declared.await.is_empty());
        let app = Application::default();
        assert_eq!(
            app.implementation_frame(&scope, &root).await.unwrap_err(),
            StepFailure::infrastructure("Plan 'absent' not found")
        );
        let (_, cancel) = watch::channel(false);
        let preload = app
            .preload_implementation_outer_docs(
                &root,
                &scope,
                &queue,
                &AppConfig::default(),
                &cancel,
            )
            .await
            .unwrap();
        assert!(preload.context_update.is_none());
        assert!(preload.diagnostics.is_empty());
    }
    #[tokio::test]
    async fn step_preload_uses_the_runs_frozen_registry_and_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cached = root
            .join(crate::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join("vendor/guide.md");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, "API reference").unwrap();
        let plan = Plan::from_parts(
            "preload".into(),
            "Preload".into(),
            None,
            0,
            0,
            Vec::new(),
            vec![PlanItem {
                id: "1".into(),
                title: "Step".into(),
                files: vec!["client.rs".into()],
                outer_docs: vec!["vendor".into()],
                ..Default::default()
            }],
        );
        PlanStorage::save_plan(root, &plan).unwrap();
        let scope = ExecutionScope::Plan {
            plan_id: "preload".into(),
            step_id: None,
        };
        let queue = TurnWorkQueue {
            items: vec![TurnWorkItem {
                id: "1".into(),
                title: "Step".into(),
            }],
            active_index: 0,
        };
        let (history, _) = tokio::sync::mpsc::unbounded_channel();
        let app = Application::for_repository(AppConfig::default(), root.to_path_buf(), history);
        let frame = app.implementation_frame(&scope, root).await.unwrap();
        assert_eq!(frame.plan.id, "preload");
        let mut frozen = AppConfig {
            outer_docs: vec![crate::config::OuterDocEntry {
                id: "vendor".into(),
                title: "API".into(),
                description: None,
                source: "https://example.test/guide.md".into(),
            }],
            ..Default::default()
        };
        frozen.context.token_budget = 0;
        let (_, cancel) = watch::channel(false);
        let omitted = app
            .preload_implementation_outer_docs(root, &scope, &queue, &frozen, &cancel)
            .await
            .unwrap();
        assert!(omitted.context_update.is_none());
        assert_eq!(omitted.diagnostics.len(), 1);
        assert!(omitted.diagnostics[0].contains("exceed the configured token budget"));
        frozen.context.token_budget = usize::MAX;
        let mounted = app
            .preload_implementation_outer_docs(root, &scope, &queue, &frozen, &cancel)
            .await
            .unwrap();
        assert!(mounted.diagnostics.is_empty());
        let snapshot = mounted.context_update.unwrap();
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].path, "vendor/guide.md");
        assert!(app.configuration().await.config.outer_docs.is_empty());
        assert!(cached.exists());
    }
}
