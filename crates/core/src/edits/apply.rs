//! Application of a validated change set to the working tree.
//!
//! The change set is staged and formatted in memory first, committed atomically
//! through the sibling commit policy of [`crate::atomic_write`], and only then
//! reconciled with the context layer. A caller outside a bounded turn enters
//! through [`apply_edit_proposal`], while a bounded turn applies the
//! configuration it froze through [`apply_edit_proposal_with_config`].

use std::path::Path;

use tauqe_protocol::{EditProposal, ProjectConfig};

use super::atomic::commit_staged_edits;
use super::error::EditError;
use super::format::prepare_staged_edits_with_config;
use crate::context::ContextManager;
use crate::mockups::is_mockups_path;

/// Resolves the effective project configuration and applies `proposal` under it.
///
/// This is the entry point for callers outside a bounded turn, which own no
/// frozen snapshot. A bounded turn applies its change set through
/// [`apply_edit_proposal_with_config`], so the configuration it froze is the one
/// the formatter and the staged validation observe.
pub async fn apply_edit_proposal(
    repo_root: &Path,
    context_manager: &mut ContextManager,
    proposal: &EditProposal,
) -> Result<Vec<String>, EditError> {
    let project = crate::toolchain::effective_project_config(repo_root, None).await;
    apply_edit_proposal_with_config(repo_root, &project, context_manager, proposal).await
}

/// Applies an EditProposal under an already resolved project configuration,
/// after validating and formatting every file in memory.
///
/// If ANY edit fails validation, no files on disk are modified. The commit is
/// atomic: new contents are staged in temporary sibling files and renamed into
/// place, and a mid-commit IO failure rolls the working tree back to its
/// pre-commit state.
pub async fn apply_edit_proposal_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    context_manager: &mut ContextManager,
    proposal: &EditProposal,
) -> Result<Vec<String>, EditError> {
    // The change set is applied and formatted purely in memory first, so the
    // working tree only ever receives code the project filter already accepted.
    let result = prepare_staged_edits_with_config(
        repo_root,
        project,
        context_manager,
        &proposal.edits,
        None,
        None,
    )
    .await;
    if let Some(failed) = result.failed_files.into_iter().next() {
        return Err(failed.error);
    }

    // Writing the validated change set, including creating parent directories,
    // is blocking file I/O. Run it through the blocking helper so a burst of
    // writes never stalls an async worker thread.
    crate::blocking::run_blocking(|| {
        commit_staged_edits(repo_root, &result.staged_state, &result.successful_paths)
    })?;

    // Reconcile the context layer with the committed change set strictly after
    // the disk commit succeeds. The net transition is derived directly from the
    // final staged state instead of replaying individual `Move` renames: every
    // path that no longer exists on disk is dropped, and every staged path is
    // ensured present and editable. Deriving the transition from the net state
    // keeps chained moves (`a -> b -> c`), moves whose source path is recreated,
    // and mixed operation sequences from desynchronizing context from disk.
    //
    // A path already present keeps the layer that owns it, and a path the turn
    // created joins the transient auto layer. The user layer is never widened by
    // a turn: it holds exactly the files the developer added by hand.
    crate::blocking::run_blocking(|| {
        for path in &result.staged_state.deleted_paths {
            let _ = context_manager.remove_file(path);
        }
        for path in result.staged_state.staged_files.keys() {
            if context_manager.contains(path).unwrap_or(false) {
                let _ = context_manager.update_file_metadata(path);
                let _ = context_manager.set_access(path, tauqe_protocol::ContextAccess::Editable);
            } else {
                // A path absent from context is a file the turn created (or a
                // move destination), so it widens the transient auto layer only.
                let _ =
                    context_manager.add_auto_file(path, tauqe_protocol::ContextAccess::Editable);
            }
        }
    });
    // The sandbox never joins the change set a turn reports. The bytes above are
    // already on disk and the created mockup stays visible to the model through
    // the context layer, but `.tauqe/mockups/` must not reach `git add`, the
    // change set delivered to project verification, or the post-hook
    // differential: Git ignores it, so a sandbox path listed as a change would
    // abort the commit of a verified turn.
    Ok(result
        .successful_paths
        .into_iter()
        .filter(|path| !is_mockups_path(path))
        .collect())
}
