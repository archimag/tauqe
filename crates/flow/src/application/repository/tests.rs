use super::*;
use tauqe_core::config::AppConfig;
fn application(root: &std::path::Path) -> Application {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    Application::for_repository(AppConfig::default(), root.to_path_buf(), events)
}
#[tokio::test]
async fn invalid_revisions_are_refused_before_git_queries() {
    let root = tempfile::tempdir().unwrap();
    let app = application(root.path());
    let operation = app.try_begin_operation().unwrap();
    for revision in ["--help", "", "-p"] {
        assert!(matches!(
            app.repository_diff(Some(revision.into()), None).await,
            Err(RepositoryError::Invalid(_))
        ));
        assert!(matches!(
            app.squash_preview_for_operation(&operation, Some(revision.into()))
                .await,
            Err(RepositoryError::Invalid(_))
        ));
    }
    let other = Application::default();
    let foreign = other.try_begin_operation().unwrap();
    assert!(matches!(
        app.squash_preview_for_operation(&foreign, None).await,
        Err(RepositoryError::Failed(_))
    ));
}
#[tokio::test]
async fn diff_and_preview_keep_commit_scope_and_base_precedence() {
    let root = tempfile::tempdir().unwrap();
    tauqe_core::git::init_repository(Some(root.path()), true).unwrap();
    let base = tauqe_core::git::run_git(Some(root.path()), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    std::fs::write(root.path().join("file.txt"), "First\n").unwrap();
    tauqe_core::git::cmd::stage_files(root.path(), &["file.txt".into()]).unwrap();
    tauqe_core::git::cmd::commit_with_developer_author(root.path(), "Add file", false).unwrap();
    let app = application(root.path());
    let operation = app.try_begin_operation().unwrap();
    let preview = app
        .squash_preview_for_operation(&operation, Some(base.clone()))
        .await
        .unwrap();
    assert_eq!(preview.base_ref, base);
    assert_eq!(preview.commits.len(), 1);
    assert!(preview.files.iter().any(|file| file.path == "file.txt"));
    let full = app
        .repository_diff(Some("HEAD".into()), None)
        .await
        .unwrap();
    let filtered = app
        .repository_diff(Some("HEAD".into()), Some("not-this-file".into()))
        .await
        .unwrap();
    assert_eq!(full, filtered);
    assert!(full.contains("file.txt"));
    let implicit = app
        .squash_preview_for_operation(&operation, None)
        .await
        .unwrap();
    assert_eq!(
        implicit.base_ref,
        implicit
            .session_base
            .or(implicit.upstream_base)
            .unwrap_or_else(|| "HEAD~1".into())
    );
}
