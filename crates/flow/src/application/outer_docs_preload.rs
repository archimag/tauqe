//! Materialize declared step documentation and admit it to resident context.

use super::context::ContextSession;
use std::path::Path;
use tauqe_core::context::ContextState;
use tauqe_core::outer_docs::{OuterDocsRegistry, MAX_DOCUMENT_BYTES};

use tauqe_core::stop::HardCancellation;

#[derive(Debug)]
pub struct OuterDocsPreload {
    pub diagnostics: Vec<String>,
    pub context_update: Option<ContextState>,
}

/// Typed refusal of one declared external document.
///
/// A trust decision belongs to the developer: no round of the run can widen a
/// declaration the repository cannot reach, so the refusal travels outward with
/// the exact cause and the `[[workspace]]` block that would authorize the source.
#[derive(Debug)]
pub struct OuterDocsRefusal {
    /// Declared entry the refusal addresses.
    pub id: String,
    /// Exact cause reported by the developer's trust declaration.
    pub reason: String,
    /// Ready-to-paste `[[workspace]]` declaration that would authorize the source.
    pub remedy: Option<String>,
}

impl OuterDocsRefusal {
    /// Diagnostic the step reports to the developer instead of starting its turn.
    pub fn to_message(&self) -> String {
        let mut message = format!(
            "The step was not started: the declared external document `{id}` is refused by the developer's trust declaration. {reason}",
            id = self.id,
            reason = self.reason
        );
        match &self.remedy {
            Some(remedy) => {
                message.push_str("\n\nAuthorize the source by adding this declaration to `$XDG_CONFIG_HOME/tauqe/refs-trust.toml`:\n\n");
                message.push_str(remedy.trim_end());
            }
            None => message.push_str(
                "\n\nThe declared source cannot be fetched at all, so correct the `[[outer_docs]]` entry of `tauqe.toml` instead of widening the trust declaration.",
            ),
        }
        message
    }
}

impl ContextSession {
    /// Trust refusals stop the step; delivery and admission failures remain
    /// diagnostics. The frozen registry belongs to the running turn.
    pub(crate) async fn preload_declared_outer_docs(
        &self,
        repo_root: &Path,
        registry: &OuterDocsRegistry,
        declared: &[String],
        cancel: &HardCancellation,
    ) -> Result<OuterDocsPreload, OuterDocsRefusal> {
        let mut unavailable: Vec<String> = Vec::new();
        let mut mounted: Vec<String> = Vec::new();
        let mut requested: Vec<&str> = Vec::new();

        for id in declared {
            let id = id.trim();
            if id.is_empty() || requested.contains(&id) {
                continue;
            }
            requested.push(id);
            match registry
                .materialize_declared(repo_root, id, MAX_DOCUMENT_BYTES, Some(cancel))
                .await
            {
                Ok(cache_path) => {
                    if !mounted.contains(&cache_path) {
                        mounted.push(cache_path);
                    }
                }
                Err(error) if error.trust().is_some() => {
                    return Err(OuterDocsRefusal {
                        id: id.to_string(),
                        reason: error.reason(),
                        remedy: error.remedy(),
                    });
                }
                Err(error) => unavailable.push(format!("- `{id}`: {}", error.reason())),
            }
        }

        if mounted.is_empty() {
            return Ok(OuterDocsPreload {
                diagnostics: unavailable,
                context_update: None,
            });
        }

        let applied = self.admit_outer_documents(mounted).await;

        let mut context_update = None;
        match applied {
            Ok(admission) => {
                let outcome = admission.outcome;
                if !outcome.added.is_empty() {
                    context_update = Some(admission.state);
                }
                for rejection in &outcome.outer_docs_rejected {
                    unavailable.push(format!("- `{}`: {}", rejection.path, rejection.reason));
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "the declared external documents could not be mounted"
                );
                unavailable.push(format!(
                    "- the declared external documents could not be mounted: {err}"
                ));
            }
        }

        Ok(OuterDocsPreload {
            diagnostics: unavailable,
            context_update,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use tauqe_core::stop::StopSource;
    #[test]
    fn test_a_trust_refusal_reports_the_declaration_that_authorizes_the_source() {
        let refusal = OuterDocsRefusal {
            id: "vendor/api.md".to_string(),
            reason: "the workspace has no entry in `refs-trust.toml`".to_string(),
            remedy: Some("[[workspace]]\nroot = \"/work/project\"".to_string()),
        };

        let message = refusal.to_message();

        assert!(message.contains("vendor/api.md"), "{message}");
        assert!(message.contains("no entry"), "{message}");
        assert!(message.contains("refs-trust.toml"), "{message}");
        assert!(message.contains("[[workspace]]"), "{message}");
    }

    #[test]
    fn test_a_refusal_without_a_declaration_names_the_entry_to_correct() {
        let refusal = OuterDocsRefusal {
            id: "vendor/api.md".to_string(),
            reason: "the source `file:///etc/passwd` is not an address the registry can fetch"
                .to_string(),
            remedy: None,
        };

        let message = refusal.to_message();

        assert!(message.contains("tauqe.toml"), "{message}");
        assert!(!message.contains("[[workspace]]"), "{message}");
    }
    fn application(root: &Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(
            tauqe_core::config::AppConfig::default(),
            root.to_path_buf(),
            sender,
        )
    }

    fn registry() -> OuterDocsRegistry {
        OuterDocsRegistry::new(&[
            crate::application::outer_doc_entry("vendor", "API", "https://example.test/guide.md"),
            crate::application::outer_doc_entry(
                "refused",
                "Refused",
                "https://user:password@example.test/private.md",
            ),
        ])
    }

    fn cached_document(root: &Path) -> std::path::PathBuf {
        let path = root
            .join(tauqe_core::outer_docs::OUTER_DOCS_CACHE_DIR)
            .join("vendor/guide.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "API reference").unwrap();
        path
    }

    #[tokio::test]
    async fn preload_deduplicates_declared_documents_and_keeps_delivery_failures_as_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        cached_document(root.path());
        let app = application(root.path());
        let identity = app.session_uuid().await;
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        let declared = vec![
            " ".into(),
            " vendor ".into(),
            "vendor".into(),
            "missing".into(),
            " missing ".into(),
        ];
        let preload = app
            .context_session()
            .preload_declared_outer_docs(root.path(), &registry(), &declared, &cancel)
            .await
            .unwrap();
        assert_eq!(preload.diagnostics.len(), 1);
        assert!(preload.diagnostics[0].contains("`missing`"));
        let snapshot = preload.context_update.unwrap();
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].path, "vendor/guide.md");
        assert_eq!(
            snapshot.items[0].layer,
            tauqe_core::context::ContextLayer::OuterDocs
        );
        assert_eq!(
            snapshot.items[0].access,
            tauqe_core::context::ContextAccess::ReadOnly
        );
        assert_eq!(snapshot.revision, app.context_state().await.revision);
        assert_eq!(identity, app.session_uuid().await);
        let repeated = app
            .context_session()
            .preload_declared_outer_docs(root.path(), &registry(), &["vendor".into()], &cancel)
            .await
            .unwrap();
        assert!(repeated.context_update.is_none());
        assert!(repeated.diagnostics.is_empty());
    }

    #[tokio::test]
    async fn preload_admits_cached_snapshot_without_a_size_gate() {
        let root = tempfile::tempdir().unwrap();
        let cached = cached_document(root.path());
        let app = application(root.path());
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        let preload = app
            .context_session()
            .preload_declared_outer_docs(root.path(), &registry(), &["vendor".into()], &cancel)
            .await
            .unwrap();
        assert!(preload.diagnostics.is_empty());
        assert_eq!(
            preload.context_update.unwrap().items[0].path,
            "vendor/guide.md"
        );
        assert!(cached.exists());
    }

    #[tokio::test]
    async fn trust_refusal_stops_before_admitting_already_materialized_candidates() {
        let root = tempfile::tempdir().unwrap();
        let cached = cached_document(root.path());
        let app = application(root.path());
        let identity = app.session_uuid().await;
        let revision = app.context_state().await.revision;
        let source = StopSource::new();
        let cancel = source.token().hard_cancellation();
        let refusal = app
            .context_session()
            .preload_declared_outer_docs(
                root.path(),
                &registry(),
                &["vendor".into(), "refused".into()],
                &cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(refusal.id, "refused");
        assert!(refusal.reason.contains("credentials"));
        assert!(refusal.to_message().contains("The step was not started"));
        assert!(app.context_state().await.items.is_empty());
        assert_eq!(revision, app.context_state().await.revision);
        assert_eq!(identity, app.session_uuid().await);
        assert!(cached.exists());
    }
}
