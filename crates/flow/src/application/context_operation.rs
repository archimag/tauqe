//! Exclusive manual context editing, independent of RPC transport.

use crate::api::{
    Application, ContextClearScope, ContextError, OperationGuard, OperationInProgress,
};
use tauqe_core::context::{ContextAccess, ContextLayer, ContextState};

pub struct ContextOperation {
    application: Application,
    _guard: OperationGuard,
}

pub struct ContextPatternAddition {
    pub added_count: usize,
    pub added_tokens: u64,
    pub truncated: bool,
    pub state: ContextState,
}

impl Application {
    pub(crate) fn begin_context_change(&self) -> Result<ContextOperation, OperationInProgress> {
        let guard = self.try_begin_operation()?;
        Ok(ContextOperation {
            application: self.clone(),
            _guard: guard,
        })
    }
}

impl ContextOperation {
    pub(crate) async fn remove_outer_docs_cache(
        &self,
        path: String,
    ) -> Result<crate::api::OuterDocsCacheRemoval, crate::api::CacheError> {
        self.application.remove_outer_docs_cache(path).await
    }

    pub(crate) async fn add_file(
        &self,
        path: String,
        access: ContextAccess,
        layer: Option<ContextLayer>,
    ) -> Result<ContextState, ContextError> {
        let layer = layer.unwrap_or(ContextLayer::User);
        match layer {
            ContextLayer::Pinned => return Err(ContextError::PinnedAddition),
            ContextLayer::OuterDocs => return Err(ContextError::RegistryManaged),
            _ => {}
        }
        self.application
            .mutate_context(move |context| {
                context
                    .add_file_to_layer(&path, access, layer)
                    .map_err(|error| ContextError::Mutation(error.to_string()))?;
                Ok(context.get_state())
            })
            .await?
    }

    async fn add_pattern(
        &self,
        pattern: String,
        access: ContextAccess,
    ) -> Result<ContextPatternAddition, ContextError> {
        // Resolve configuration and subprocess inputs before taking the context lock.
        let budget = self
            .application
            .configuration()
            .await
            .config
            .context
            .token_budget;
        let root = self.application.repository_root().await;
        let files = super::context::repository_files(root).await;
        self.application
            .mutate_context(move |context| {
                let outcome = context
                    .add_files_by_pattern(&pattern, access, &files, budget)
                    .map_err(|error| ContextError::Mutation(error.to_string()))?;
                Ok(ContextPatternAddition {
                    added_count: outcome.added_items.len(),
                    added_tokens: outcome.added_tokens,
                    truncated: outcome.truncated,
                    state: context.get_state(),
                })
            })
            .await?
    }

    async fn remove_file(&self, path: String) -> Result<ContextState, ContextError> {
        self.application
            .mutate_context(move |context| {
                context
                    .remove_file(&path)
                    .map_err(|error| ContextError::Mutation(error.to_string()))?;
                Ok(context.get_state())
            })
            .await?
    }

    async fn set_access(
        &self,
        path: String,
        access: ContextAccess,
    ) -> Result<ContextState, ContextError> {
        self.application
            .mutate_context(move |context| {
                context
                    .set_access(&path, access)
                    .map_err(|error| ContextError::Mutation(error.to_string()))?;
                Ok(context.get_state())
            })
            .await?
    }

    async fn clear(&self, layer: Option<ContextLayer>) -> Result<ContextState, ContextError> {
        let scope = match layer {
            Some(ContextLayer::Pinned) => return Err(ContextError::PinnedClear),
            Some(ContextLayer::Auto) => ContextClearScope::Auto,
            Some(ContextLayer::User) => ContextClearScope::User,
            Some(ContextLayer::OuterDocs) => ContextClearScope::OuterDocs,
            None => ContextClearScope::All,
        };
        self.application.clear_context(scope).await
    }
}

impl Application {
    pub async fn add_context_file(
        &self,
        path: String,
        access: ContextAccess,
        layer: Option<ContextLayer>,
        observer: &dyn crate::api::ApplicationObserver,
    ) -> Result<ContextState, ContextError> {
        let operation = self
            .begin_context_change()
            .map_err(|_| ContextError::Busy)?;
        let state = operation.add_file(path, access, layer).await?;
        publish_context(observer, &state);
        Ok(state)
    }

    pub async fn add_context_pattern(
        &self,
        pattern: String,
        access: ContextAccess,
        observer: &dyn crate::api::ApplicationObserver,
    ) -> Result<ContextPatternAddition, ContextError> {
        let operation = self
            .begin_context_change()
            .map_err(|_| ContextError::Busy)?;
        let result = operation.add_pattern(pattern, access).await?;
        publish_context(observer, &result.state);
        Ok(result)
    }

    pub async fn remove_context_file(
        &self,
        path: String,
        observer: &dyn crate::api::ApplicationObserver,
    ) -> Result<ContextState, ContextError> {
        let operation = self
            .begin_context_change()
            .map_err(|_| ContextError::Busy)?;
        let state = operation.remove_file(path).await?;
        publish_context(observer, &state);
        Ok(state)
    }

    pub async fn set_context_access(
        &self,
        path: String,
        access: ContextAccess,
        observer: &dyn crate::api::ApplicationObserver,
    ) -> Result<ContextState, ContextError> {
        let operation = self
            .begin_context_change()
            .map_err(|_| ContextError::Busy)?;
        let state = operation.set_access(path, access).await?;
        publish_context(observer, &state);
        Ok(state)
    }

    pub async fn clear_manual_context(
        &self,
        layer: Option<ContextLayer>,
        observer: &dyn crate::api::ApplicationObserver,
    ) -> Result<ContextState, ContextError> {
        let operation = self
            .begin_context_change()
            .map_err(|_| ContextError::Busy)?;
        let state = operation.clear(layer).await?;
        publish_context(observer, &state);
        Ok(state)
    }

    pub async fn remove_cached_documents(
        &self,
        path: String,
        observer: &dyn crate::api::ApplicationObserver,
    ) -> Result<tauqe_core::outer_docs::CacheRemoval, crate::api::CacheError> {
        let operation = self
            .begin_context_change()
            .map_err(|_| crate::api::CacheError::Busy)?;
        let outcome = operation.remove_outer_docs_cache(path).await?;
        if let Some(state) = outcome.context_update {
            observer.publish("", crate::api::ApplicationEvent::Context(state));
        }
        Ok(outcome.removed)
    }
}

fn publish_context(observer: &dyn crate::api::ApplicationObserver, state: &ContextState) {
    observer.publish("", crate::api::ApplicationEvent::Context(state.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_core::config::AppConfig;

    fn application(root: &std::path::Path, config: AppConfig) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(config, root.to_path_buf(), sender)
    }

    #[tokio::test]
    async fn manual_edits_hold_exclusive_admission_through_errors_and_publication() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path(), AppConfig::default());
        let operation = app.begin_context_change().unwrap();
        assert!(app.begin_configuration_change().is_err());
        assert!(app.begin_history_change().is_err());
        assert!(app.begin_context_change().is_err());
        assert!(operation
            .add_file("missing.rs".into(), ContextAccess::ReadOnly, None)
            .await
            .is_err());
        assert!(app.try_begin_operation().is_err());
        drop(operation);
        assert!(app.try_begin_operation().is_ok());
    }

    #[tokio::test]
    async fn protected_layers_are_rejected_before_path_validation_without_changes() {
        let root = tempfile::tempdir().unwrap();
        let app = application(root.path(), AppConfig::default());
        let before = app.context_state().await;
        let identity = app.session_uuid().await;
        let operation = app.begin_context_change().unwrap();
        assert!(matches!(
            operation
                .add_file(
                    "../missing".into(),
                    ContextAccess::Editable,
                    Some(ContextLayer::Pinned)
                )
                .await,
            Err(ContextError::PinnedAddition)
        ));
        assert!(matches!(
            operation
                .add_file(
                    "../missing".into(),
                    ContextAccess::Editable,
                    Some(ContextLayer::OuterDocs)
                )
                .await,
            Err(ContextError::RegistryManaged)
        ));
        assert!(matches!(
            operation.clear(Some(ContextLayer::Pinned)).await,
            Err(ContextError::PinnedClear)
        ));
        assert_eq!(before.revision, app.context_state().await.revision);
        assert_eq!(identity, app.session_uuid().await);
    }

    #[tokio::test]
    async fn manual_edit_commands_return_the_state_they_committed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.rs"), "fn main() {}\n").unwrap();
        let app = application(root.path(), AppConfig::default());
        let operation = app.begin_context_change().unwrap();
        let added = operation
            .add_file("file.rs".into(), ContextAccess::ReadOnly, None)
            .await
            .unwrap();
        assert_eq!(added.items.len(), 1);
        assert_eq!(added.items[0].layer, ContextLayer::User);
        let changed = operation
            .set_access("file.rs".into(), ContextAccess::Editable)
            .await
            .unwrap();
        assert_eq!(changed.items[0].access, ContextAccess::Editable);
        assert!(changed.revision > added.revision);
        let removed = operation.remove_file("file.rs".into()).await.unwrap();
        assert!(removed.items.is_empty());
        assert_eq!(removed.revision, app.context_state().await.revision);
    }

    #[tokio::test]
    async fn pattern_uses_configured_budget_but_declared_preload_is_unbounded() {
        let root = tempfile::tempdir().unwrap();
        tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
        for file in ["target.rs", "reference.rs"] {
            std::fs::write(root.path().join(file), "fn main() {}\n").unwrap();
        }
        tauqe_core::git::cmd::stage_files(
            root.path(),
            &["target.rs".into(), "reference.rs".into()],
        )
        .unwrap();
        let mut config = AppConfig::default();
        config.context.token_budget = 0;
        let app = application(root.path(), config);
        let operation = app.begin_context_change().unwrap();
        let pattern = operation
            .add_pattern("*.rs".into(), ContextAccess::ReadOnly)
            .await
            .unwrap();
        assert_eq!(pattern.added_count, 0);
        assert!(pattern.truncated);
        assert!(pattern.state.items.is_empty());
        // A workflow preload uses declared files despite the discovery budget.
        let admission = app
            .preload_declared_context(
                root.path().to_path_buf(),
                vec![" ./target.rs ".into()],
                vec!["/reference.rs".into()],
            )
            .await
            .unwrap();
        assert_eq!(admission.outcome.added.len(), 2);
        assert!(admission.outcome.limit_exceeded.is_none());
        assert!(admission
            .state
            .items
            .iter()
            .all(|item| item.layer == ContextLayer::Auto));
        let target = admission
            .state
            .items
            .iter()
            .find(|item| item.path == "target.rs")
            .unwrap();
        let reference = admission
            .state
            .items
            .iter()
            .find(|item| item.path == "reference.rs")
            .unwrap();
        assert_eq!(target.access, ContextAccess::Editable);
        assert_eq!(reference.access, ContextAccess::ReadOnly);
        assert_eq!(admission.state.revision, app.context_state().await.revision);
    }
}
