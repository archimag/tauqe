//! Application of a validated change set to the working tree.
//!
//! The change set is staged and formatted in memory first, committed atomically
//! through the sibling commit policy of [`tauqe_tools::atomic_write`], and only then
//! reconciled with the context layer. A caller outside a bounded turn enters
//! through [`apply_edit_proposal`], while a bounded turn applies the
//! configuration it froze through [`apply_edit_proposal_with_config_and_operations`].

use crate::config::project::ProjectConfig;
use std::path::Path;

use crate::edits::EditOperation;
#[cfg(test)]
use crate::edits::EditProposal;

use super::atomic::commit_staged_edits_with_operations;
use super::stage::prepare_edit_batches_with_operations;
use super::EditError;
use crate::context::ContextManager;
use crate::mockups::is_mockups_path;
use tauqe_tools::blocking::StartedOperations;
use tauqe_tools::stop::HardCancellation;

/// Resolves the effective project configuration and applies `proposal` under it.
///
/// This is the entry point for callers outside a bounded turn, which own no
/// frozen snapshot. A bounded turn applies its change set through
/// [`apply_edit_proposal_with_config_and_operations`], so the configuration it froze is the one
/// the formatter and the staged validation observe.
#[cfg(test)]
pub(crate) async fn apply_edit_proposal(
    repo_root: &Path,
    context_manager: &mut ContextManager,
    proposal: &EditProposal,
) -> Result<Vec<String>, EditError> {
    let project = crate::toolchain::effective_project_config(repo_root, None).await;
    apply_edit_proposal_with_config(repo_root, &project, context_manager, &[&proposal.edits]).await
}

/// Test-only standalone entry point; production policies retain their own owner.
#[cfg(test)]
pub(crate) async fn apply_edit_proposal_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    context_manager: &mut ContextManager,
    batches: &[&[EditOperation]],
) -> Result<Vec<String>, EditError> {
    let operations = StartedOperations::default();
    let result = apply_edit_proposal_with_config_and_operations(
        repo_root,
        project,
        context_manager,
        batches,
        None,
        &operations,
    )
    .await;
    operations.close();
    operations.wait().await;
    result
}

/// Applies under an owned operation scope. A cancelled preparation is never
/// downgraded to unformatted bytes, and a lost publication wait leaves the
/// write completion with `operations` for transaction settlement.
pub async fn apply_edit_proposal_with_config_and_operations(
    repo_root: &Path,
    project: &ProjectConfig,
    context_manager: &mut ContextManager,
    batches: &[&[EditOperation]],
    cancel_rx: Option<&HardCancellation>,
    operations: &StartedOperations,
) -> Result<Vec<String>, EditError> {
    let result = prepare_edit_batches_with_operations(
        repo_root,
        project,
        context_manager,
        batches,
        None,
        cancel_rx,
        operations,
    )
    .await
    .map_err(|reason| EditError::Io(String::new(), std::io::Error::other(reason)))?;
    if let Some(failed) = result.failed_files.into_iter().next() {
        return Err(failed.error);
    }
    commit_staged_edits_with_operations(
        repo_root,
        &result.staged_state,
        &result.successful_paths,
        cancel_rx,
        operations,
    )
    .await?;
    crate::blocking::run_blocking(|| {
        for path in &result.staged_state.deleted_paths {
            let _ = context_manager.remove_auto_file(path);
        }
        for path in result.staged_state.staged_files.keys() {
            if context_manager.contains(path).unwrap_or(false) {
                let _ = context_manager.update_file_metadata(path);
            }
            if context_manager.effective_access_for(path)
                != Some(crate::context::ContextAccess::Editable)
            {
                let _ =
                    context_manager.add_auto_file(path, crate::context::ContextAccess::Editable);
            }
        }
    });
    Ok(result
        .successful_paths
        .into_iter()
        .filter(|path| !is_mockups_path(path))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextAccess;
    use crate::edits::EditReplace;
    use crate::stop::StopSource;

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_formatter_cannot_publish_unformatted_bytes() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        crate::git::init_repository(Some(dir.path()), true).unwrap();
        std::fs::write(dir.path().join("a.rs"), "original\n").unwrap();
        let mut context = ContextManager::new(dir.path().to_path_buf());
        context.add_file("a.rs", ContextAccess::Editable).unwrap();
        let project: ProjectConfig = serde_json::from_str(
            r#"{"format":{"command":"sh","args":["-c","cat; touch formatter-started; sleep 30"]}}"#,
        )
        .unwrap();
        let edits = [EditOperation::Replace(EditReplace {
            path: "a.rs".into(),
            old_text: "original\n".into(),
            new_text: "unformatted\n".into(),
        })];
        let stop = StopSource::new();
        let hard = stop.token().hard_cancellation();
        let marker = dir.path().join("formatter-started");
        let canceller = std::thread::spawn(move || {
            for _ in 0..400 {
                if marker.exists() {
                    stop.request_hard();
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            panic!("formatter never started");
        });
        let operations = StartedOperations::default();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            apply_edit_proposal_with_config_and_operations(
                dir.path(),
                &project,
                &mut context,
                &[&edits],
                Some(&hard),
                &operations,
            ),
        )
        .await
        .expect("formatter cancellation must finish");
        canceller.join().unwrap();
        assert!(result.is_err());
        operations.close();
        assert_eq!(operations.wait().await.unconfirmed, 0);
        assert_eq!(
            std::fs::read(dir.path().join("a.rs")).unwrap(),
            b"original\n"
        );
    }
}
