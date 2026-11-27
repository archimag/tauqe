//! Cache inspection and removal with resident-context reconciliation.

use crate::api::Application;
use tauqe_core::outer_docs::{self, CacheError, CacheRemoval, CachedDocument};

pub struct OuterDocsCacheRemoval {
    pub removed: CacheRemoval,
}

impl Application {
    pub async fn outer_docs_cache(&self) -> Result<Vec<CachedDocument>, CacheError> {
        let root = self.repository_root().await;
        tauqe_core::blocking::run_blocking_async(move || outer_docs::list_cache_entries(&root))
            .await
            .map_err(|error| CacheError::Failed(format!("blocking task failed: {error}")))?
            .map_err(CacheError::Failed)
    }

    /// Called only through the context operation that owns exclusive admission.
    pub(crate) async fn remove_outer_docs_cache(
        &self,
        path: String,
    ) -> Result<OuterDocsCacheRemoval, CacheError> {
        let logical = path.trim().to_string();
        let root = self.repository_root().await;
        let target = logical.clone();
        let removed = tauqe_core::blocking::run_blocking_async(move || {
            outer_docs::remove_cache_entry(&root, &target)
        })
        .await
        .map_err(|error| CacheError::Failed(format!("blocking task failed: {error}")))??;
        self.release_cached_documents(logical)
            .await
            .map_err(|error| CacheError::Failed(error.to_string()))?;
        Ok(OuterDocsCacheRemoval { removed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn application(root: &Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(
            tauqe_core::config::AppConfig::default(),
            root.to_path_buf(),
            sender,
        )
    }

    fn cache_file(root: &Path, logical: &str, body: &str) -> String {
        let relative = format!("{}/{logical}", outer_docs::OUTER_DOCS_CACHE_DIR);
        let path = root.join(&relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        relative
    }

    #[tokio::test]
    async fn subtree_removal_reconciles_only_affected_documents_and_rotates_identity() {
        let root = tempfile::tempdir().unwrap();
        let removed = cache_file(root.path(), "vendor/api.md", "api");
        cache_file(root.path(), "vendor/models.md", "models");
        let kept = cache_file(root.path(), "vendor-next/api.md", "other");
        let app = application(root.path());
        app.test_mutate_context(move |context| {
            context.add_outer_docs_file(&removed).unwrap();
            context.add_outer_docs_file(&kept).unwrap();
        })
        .await
        .unwrap();
        let identity = app.session_uuid().await;
        let operation = app.begin_context_change().unwrap();
        let outcome = operation
            .remove_outer_docs_cache(" vendor ".into())
            .await
            .unwrap();
        assert_eq!(outcome.removed.removed_files, 2);
        assert_eq!(outcome.removed.removed_bytes, 9);
        let update = app.context_state().await;
        assert_eq!(update.items.len(), 1);
        assert_eq!(update.items[0].path, "vendor-next/api.md");
        assert_eq!(update.revision, app.context_state().await.revision);
        assert_ne!(identity, app.session_uuid().await);
        assert!(app.try_begin_operation().is_err());
        assert!(!root
            .path()
            .join(outer_docs::OUTER_DOCS_CACHE_DIR)
            .join("vendor")
            .exists());
        assert!(root
            .path()
            .join(outer_docs::OUTER_DOCS_CACHE_DIR)
            .join("vendor-next/api.md")
            .exists());
        drop(operation);
        assert!(app.try_begin_operation().is_ok());
    }

    #[tokio::test]
    async fn removing_nonresident_material_keeps_context_revision_and_dialogue_identity() {
        let root = tempfile::tempdir().unwrap();
        cache_file(root.path(), "vendor/api.md", "api");
        let app = application(root.path());
        let identity = app.session_uuid().await;
        let revision = app.context_state().await.revision;
        let operation = app.begin_context_change().unwrap();
        let outcome = operation
            .remove_outer_docs_cache("vendor/api.md".into())
            .await
            .unwrap();
        assert_eq!(identity, app.session_uuid().await);
        assert_eq!(revision, app.context_state().await.revision);
        assert_eq!(outcome.removed.removed_files, 1);
    }

    #[tokio::test]
    async fn refused_removal_leaves_cache_context_and_identity_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let resident = cache_file(root.path(), "vendor/api.md", "api");
        let app = application(root.path());
        app.test_mutate_context(move |context| context.add_outer_docs_file(&resident))
            .await
            .unwrap()
            .unwrap();
        let identity = app.session_uuid().await;
        let revision = app.context_state().await.revision;
        let operation = app.begin_context_change().unwrap();
        assert!(matches!(
            operation.remove_outer_docs_cache("../vendor".into()).await,
            Err(CacheError::InvalidPath(_))
        ));
        assert!(matches!(
            operation.remove_outer_docs_cache("missing".into()).await,
            Err(CacheError::NotFound(_))
        ));
        assert_eq!(revision, app.context_state().await.revision);
        assert_eq!(identity, app.session_uuid().await);
        assert_eq!(app.outer_docs_cache().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn inventory_is_sorted_read_only_and_available_during_an_operation() {
        let root = tempfile::tempdir().unwrap();
        cache_file(root.path(), "z/b.md", "last");
        cache_file(root.path(), "a/a.md", "first");
        let app = application(root.path());
        let identity = app.session_uuid().await;
        let revision = app.context_state().await.revision;
        let _operation = app.begin_context_change().unwrap();
        let entries = app.outer_docs_cache().await.unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a/a.md", "z/b.md"]
        );
        assert_eq!(revision, app.context_state().await.revision);
        assert_eq!(identity, app.session_uuid().await);
    }
}
