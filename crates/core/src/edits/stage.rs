//! The engine's staging adapter.
//!
//! The virtual-staging state machine itself is deterministic substrate and lives
//! in `tauqe_tools::edits`: it works from a protocol context snapshot and a
//! resolved project configuration and writes nothing on its own. This module is
//! the adapter the engine keeps, because two inputs are cognitive:
//!
//! - the live [`ContextManager`] is captured into the `ContextState` snapshot the
//!   substrate reads, so staging never borrows a mutable engine;
//! - `tauqe.toml` governs the environment a change set is verified in, so the
//!   in-memory validation of a staged configuration file stays with the crate
//!   that owns the harness configuration and reaches the substrate as a hook.

use std::path::Path;

use tauqe_protocol::{EditOperation, ProjectConfig};
use tokio::sync::watch;

use super::{StageEditsResult, StagedEditsState};
use crate::context::ContextManager;
use crate::escape::single_line;

/// Resolves the project configuration a single staging attempt observes.
///
/// A bounded turn freezes its configuration on the first proposal round and
/// passes it in. Callers outside a turn read only the `[project]` section: a task
/// runner dump contributes nothing to a staging attempt that has no declared
/// filter of its own.
fn project_config(repo_root: &Path) -> ProjectConfig {
    crate::config::load_section::<ProjectConfig>(repo_root, "project")
}

/// Validates the staged content of a repository configuration file.
///
/// The harness must never commit a configuration it could not load on its next
/// start, so a change set that rewrites the repository configuration is checked
/// while it is still purely in memory. Both accepted file names are guarded, so a
/// repository configured through `.tauqe.toml` receives exactly the same
/// treatment as one configured through `tauqe.toml`. The validation has two
/// levels: the TOML syntax is parsed first, and the resulting declaration is then
/// validated semantically, so dropped model roles or a missing verification
/// command are rejected as well.
fn validate_project_config(path: &str, content: Option<&str>) -> Result<(), String> {
    if !crate::config::is_repo_config_path(path) {
        return Ok(());
    }
    let Some(content) = content else {
        return Ok(());
    };

    // Level 1 — syntactic validation of the staged bytes.
    let parsed: crate::config::AppConfig = toml::from_str(content).map_err(|err| {
        format!(
            "the file is not valid TOML: {}",
            single_line(&err.to_string())
        )
    })?;

    // Level 2 — semantic validation of the mandatory structures the harness
    // depends on: the model roles must resolve and verification must stay
    // declared.
    match parsed.validate() {
        Ok(()) => Ok(()),
        Err(err) => Err(single_line(&err.to_string())),
    }
}

/// Validates `edits` against the context the engine currently holds.
///
/// Returns the staged in-memory state without changing disk or the live context,
/// so a caller can inspect the outcome before anything is applied.
pub fn stage_and_validate_edits(
    repo_root: &Path,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
) -> StageEditsResult {
    // Resolving the project configuration reads `tauqe.toml` from disk, which is
    // blocking work, so the whole attempt runs off the async executor thread.
    crate::blocking::run_blocking(|| {
        let context = context_manager.get_state();
        let project = project_config(repo_root);
        tauqe_tools::edits::stage_and_validate_edits_with(
            repo_root,
            &project,
            &context,
            edits,
            initial_staged,
            Some(validate_project_config),
        )
    })
}

/// Resolves the effective project configuration and stages `edits` under it.
///
/// This is the entry point for callers outside a bounded turn, which own no frozen
/// snapshot: it reads `tauqe.toml` and, when a task runner is declared, runs the
/// runner's dump before the change set is staged. A bounded turn holds the
/// configuration it froze on its first proposal round and calls
/// [`prepare_staged_edits_with_config`], so the dump runs once for the whole turn
/// instead of once per staging attempt.
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
/// `project` is the configuration a bounded turn froze on its first proposal
/// round, so staging never re-reads `tauqe.toml` or re-runs a declared task
/// runner dump. `cancel_rx` is the turn's cancellation signal, so an abandoned
/// attempt stops the filter instead of waiting for it.
pub async fn prepare_staged_edits_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    context_manager: &ContextManager,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> StageEditsResult {
    let context = context_manager.get_state();
    tauqe_tools::edits::prepare_staged_edits_with_config_and_validator(
        repo_root,
        project,
        &context,
        edits,
        initial_staged,
        Some(validate_project_config),
        cancel_rx,
    )
    .await
}
