//! Repository startup ordering, independent of RPC transport.

use std::path::Path;
use tauqe_core::git::checkpoint::RecoveryOutcome;

/// Recover interrupted writes before migrating or loading session state.
/// Failures remain best effort and are recorded through the tracing facade.
pub(crate) fn prepare(root: &Path) {
    tauqe_core::blocking::run_blocking(|| {
        // Crash recovery resolves any dangling checkpoint or step commit an
        // interrupted workflow left behind, and the resolved shape decides how
        // it is recorded: a completed recovery and a reclaimed artifact are
        // informational, a skipped proof is a warning that names its reason,
        // and a failed pass is an error that names the repository root. A skip
        // is never presented as a completed recovery.
        match tauqe_core::git::checkpoint::recover_interrupted_workflow(root) {
            Ok(RecoveryOutcome::NothingToDo) => {}
            Ok(RecoveryOutcome::Recovered { message }) => {
                tracing::info!(
                    repo_root = %root.display(),
                    "Recovered interrupted workflow: {}",
                    message,
                );
            }
            Ok(RecoveryOutcome::Skipped { reason }) => {
                tracing::warn!(
                    repo_root = %root.display(),
                    "Skipped crash recovery: {}",
                    reason,
                );
            }
            Ok(RecoveryOutcome::Cleaned { artifacts }) => {
                tracing::info!(
                    repo_root = %root.display(),
                    artifacts,
                    "Reclaimed the artifacts of an interrupted workflow",
                );
            }
            Err(error) => {
                tracing::error!(
                    repo_root = %root.display(),
                    "Crash recovery failed: {:#}",
                    error,
                );
            }
        }

        // Keep migration on startup so entity reads remain free of writes.
        // Migration failures are logged by storage and do not prevent opening.
        tauqe_core::review::ReviewStorage::migrate_legacy_if_needed(root);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Application;
    use std::fs;
    use tauqe_core::config::AppConfig;
    use tauqe_core::review::ReviewStorage;
    use tauqe_tools::git::{checkpoint, state, GitHooks};

    const LEGACY_REVIEW: &str = r#"{
        "id": "startup-review",
        "title": "Recovered audit",
        "created_at": 12345,
        "model": "test-model",
        "target_files": [],
        "items": [],
        "raw_markdown": ""
    }"#;

    fn open(root: &Path) -> Application {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        Application::for_repository(AppConfig::default(), root.to_path_buf(), sender)
    }

    fn reviews_dir(root: &Path) -> std::path::PathBuf {
        let path = root.join(".tauqe/reviews");
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restores_documents_before_migration_and_session_loading() {
        let root = tempfile::tempdir().unwrap();
        state::init_repository(Some(root.path()), true, GitHooks::Bypass).unwrap();
        let reviews = reviews_dir(root.path());
        // Legacy unowned backup siblings represent interrupted atomic writes.
        // Startup must restore them before migration and lifetime loading.
        let review_backup = reviews.join("latest.json.tauqe-bak");
        fs::write(&review_backup, LEGACY_REVIEW).unwrap();
        let usage_backup = root.path().join(".tauqe/usage.json.tauqe-bak");
        fs::write(
            &usage_backup,
            r#"{
            "lifetime": {"total_tokens": 150, "cost": 0.125},
            "lifetime_operations": 7
        }"#,
        )
        .unwrap();

        let app = open(root.path());
        let snapshot = app.usage_snapshot().await;
        assert_eq!(snapshot.lifetime.total_tokens, 150);
        assert_eq!(snapshot.lifetime.cost, 0.125);
        assert_eq!(snapshot.lifetime_operations, 7);
        assert_eq!(snapshot.operations, 0);
        assert!(snapshot.session.is_empty());
        assert!(!usage_backup.exists());
        assert!(!review_backup.exists());
        assert!(!reviews.join("latest.json").exists());
        let review = ReviewStorage::load_latest(root.path()).unwrap().unwrap();
        assert_eq!(review.id, "startup-review");
        assert!(reviews.join("startup-review.json").is_file());

        // Clones and reads share the opened session without rerunning startup.
        let later_backup = root.path().join("later.txt.tauqe-bak");
        fs::write(&later_backup, "leave for the next startup").unwrap();
        assert_eq!(app.clone().usage_snapshot().await, snapshot);
        ReviewStorage::list_reviews(root.path()).unwrap();
        assert!(later_backup.exists());
    }

    #[test]
    fn skipped_recovery_preserves_live_transaction_and_still_migrates_reviews() {
        let root = tempfile::tempdir().unwrap();
        state::init_repository(Some(root.path()), true, GitHooks::Bypass).unwrap();
        checkpoint::create_checkpoint(root.path(), GitHooks::Bypass).unwrap();
        let marker = root.path().join(".tauqe/active-transaction.json");
        let marker_before = fs::read(&marker).unwrap();
        let work = root.path().join("in-progress.txt");
        fs::write(&work, "work owned by the running writer").unwrap();
        let reviews = reviews_dir(root.path());
        fs::write(reviews.join("latest.json"), LEGACY_REVIEW).unwrap();

        let _app = open(root.path());
        assert_eq!(fs::read(&marker).unwrap(), marker_before);
        assert_eq!(
            fs::read_to_string(&work).unwrap(),
            "work owned by the running writer"
        );
        assert!(reviews.join("startup-review.json").is_file());
        assert!(!reviews.join("latest.json").exists());
    }

    #[tokio::test]
    async fn unreadable_legacy_review_does_not_prevent_opening_a_non_git_project() {
        let root = tempfile::tempdir().unwrap();
        let reviews = reviews_dir(root.path());
        let legacy = reviews.join("latest.json");
        fs::write(&legacy, "invalid review JSON").unwrap();

        let app = open(root.path());
        assert_eq!(app.repository_root().await, root.path());
        assert!(app.usage_snapshot().await.session.is_empty());
        assert_eq!(fs::read_to_string(legacy).unwrap(), "invalid review JSON");
    }
}
