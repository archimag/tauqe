//! The in-memory formatting hand-off of a staging attempt.
//!
//! A change set is applied to memory first, and the files this attempt staged
//! are then passed through the declared `[project.format]` filter, so the code
//! that reaches the working tree is already formatted and Git never observes an
//! unformatted intermediate.

use std::path::Path;

use tauqe_protocol::{EditOperation, ProjectConfig};
use tokio::sync::watch;

use super::change_set::{stage_and_validate_edits_inner, StageEditsResult, StagedEditsState};
use crate::context::ContextManager;

/// Resolves the effective project configuration and stages `edits` under it.
///
/// This is the entry point for callers outside a bounded turn, which own no
/// frozen snapshot: it reads `tauqe.toml` and, when a task runner is declared,
/// runs the runner's dump before the change set is staged. A bounded turn holds
/// the configuration it froze on its first proposal round and calls
/// [`prepare_staged_edits_with_config`], so the dump runs once for the whole
/// turn instead of once per staging attempt.
pub async fn prepare_staged_edits(
    repo_root: &Path,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
) -> StageEditsResult {
    let project = crate::toolchain::effective_project_config(repo_root, None).await;
    prepare_staged_edits_with_config(
        repo_root,
        &project,
        context_manager,
        edits,
        initial_staged,
        None,
    )
    .await
}

/// Stages edits in memory under an already resolved project configuration and
/// streams every staged file through the project filter.
///
/// Virtual staging has two halves now that formatting is a pure stream: the
/// change set is applied to memory by `stage_and_validate_edits`, and the
/// content of the files this attempt staged is then passed through the declared
/// `[project.format]` filter on standard input. What lands in
/// [`StageEditsResult::staged_state`] is therefore already formatted, so the
/// code that eventually reaches the working tree is the formatted code and Git
/// never observes an unformatted intermediate.
///
/// `project` is the configuration a bounded turn froze on its first proposal
/// round, so staging never re-reads `tauqe.toml` or re-runs a declared task
/// runner dump. `cancel_rx` is the turn's cancellation signal, so an abandoned
/// attempt stops the filter instead of waiting for it. A repository that
/// declares no filter keeps the authored content, and a filter that fails or
/// times out does the same: a broken formatter never blocks a turn.
pub async fn prepare_staged_edits_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> StageEditsResult {
    let mut result =
        stage_and_validate_edits_inner(repo_root, context_manager, edits, initial_staged);
    format_staged_files(repo_root, project, &mut result, cancel_rx).await;
    result
}

/// Streams every file an attempt staged through the declared project filter.
///
/// Formatting has exactly one contour: the staged text of a file enters
/// `[project.format]` on standard input and the rewritten text returns on
/// standard output, so the change set is formatted while it still lives in
/// memory. A repository that declares no filter keeps every authored content,
/// and a cancelled invocation leaves the rest of the change set untouched.
async fn format_staged_files(
    repo_root: &Path,
    project: &ProjectConfig,
    result: &mut StageEditsResult,
    cancel_rx: Option<&watch::Receiver<bool>>,
) {
    let mut formatted: Vec<(String, String)> = Vec::new();
    for path in &result.successful_paths {
        let Some(content) = result.staged_state.staged_files.get(path).cloned() else {
            // A staged deletion carries no content to rewrite.
            continue;
        };
        let Some(outcome) =
            crate::toolchain::format_with_project_config(repo_root, project, &content, cancel_rx)
                .await
        else {
            // No in-memory formatter is declared, so nothing is rewritten.
            return;
        };
        if outcome.is_cancelled {
            return;
        }
        formatted.push((path.clone(), outcome.content));
    }
    for (path, content) in formatted {
        result.staged_state.staged_files.insert(path, content);
    }
}
