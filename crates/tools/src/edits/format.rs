//! The in-memory formatting hand-off of a staging attempt.
//!
//! A change set is applied to memory first, and the files this attempt staged
//! are then passed through the declared `[project.format]` filter, so the code
//! that reaches the working tree is already formatted and Git never observes an
//! unformatted intermediate.
//!
//! Formatting is never the last word on the staged bytes. The filter rewrites
//! text the admission gate already validated, so every rewritten document is
//! handed back to that same gate before it joins the staged state. A rewritten
//! file the gate rejects keeps the authored content, so the bytes that reach
//! [`StagedEditsState::staged_files`] — and therefore the working tree — are
//! exactly the bytes the validation gate inspected.
//!
//! The filter itself is declared by the project configuration of the repository
//! and runs in [`crate::toolchain`]; this module owns only the hand-off.
//!
//! Staging reads every file it edits, so the state machine performs blocking
//! file I/O while the entry point that drives it is async. The machine is
//! therefore handed to the blocking pool of the runtime instead of running on
//! an executor thread, so a large change set never stalls the tasks that share
//! the runtime with the turn.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauqe_protocol::{ContextState, EditOperation, ProjectConfig};
use tokio::sync::watch;

use super::change_set::{
    stage_and_validate_edits_inner, StageEditsResult, StagedConfigValidator, StagedEditsState,
};
use crate::blocking::{log_join_error, run_blocking, run_blocking_async};

/// Stages edits in memory under an already resolved project configuration and
/// streams every staged file through the project filter.
///
/// Virtual staging has two halves now that formatting is a pure stream: the
/// change set is applied to memory by [`stage_and_validate_edits_inner`], and the
/// content of the files this attempt staged is then passed through the declared
/// `[project.format]` filter on standard input. What lands in
/// [`StageEditsResult::staged_state`] is therefore already formatted, so the code
/// that eventually reaches the working tree is the formatted code and Git never
/// observes an unformatted intermediate.
///
/// `project` is the configuration a bounded turn froze on its first proposal
/// round, so staging never re-reads `tauqe.toml` or re-runs a declared task
/// runner dump. `cancel_rx` is the turn's cancellation signal, so an abandoned
/// attempt stops the filter instead of waiting for it. A repository that declares
/// no filter keeps the authored content, and a filter that fails or times out
/// does the same: a broken formatter never blocks a turn.
pub async fn prepare_staged_edits_with_config(
    repo_root: &Path,
    project: &ProjectConfig,
    context: &ContextState,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> StageEditsResult {
    prepare_staged_edits_with_config_and_validator(
        repo_root,
        project,
        context,
        edits,
        initial_staged,
        None,
        cancel_rx,
    )
    .await
}

/// Stages edits, hands every staged file to `config_validator`, and formats the
/// result in memory.
pub async fn prepare_staged_edits_with_config_and_validator(
    repo_root: &Path,
    project: &ProjectConfig,
    context: &ContextState,
    edits: &[EditOperation],
    initial_staged: Option<&StagedEditsState>,
    config_validator: Option<StagedConfigValidator>,
    cancel_rx: Option<&watch::Receiver<bool>>,
) -> StageEditsResult {
    let inputs = Arc::new(StagingInputs::new(
        repo_root,
        project,
        context,
        edits,
        initial_staged,
    ));
    let mut result = stage_off_the_executor(Arc::clone(&inputs), config_validator).await;
    format_staged_files(repo_root, project, config_validator, &mut result, cancel_rx).await;
    result
}

/// The owned inputs of one staging attempt, moved onto a blocking thread.
///
/// [`stage_and_validate_edits_inner`] borrows its inputs, while an offloaded
/// call has to own everything it touches. The bundle therefore carries the
/// clones of the caller's arguments, and the [`Arc`] around it keeps them
/// available to the synchronous fallback when the offloaded call returns no
/// result.
struct StagingInputs {
    repo_root: PathBuf,
    project: ProjectConfig,
    context: ContextState,
    edits: Vec<EditOperation>,
    initial_staged: Option<StagedEditsState>,
}

impl StagingInputs {
    /// Captures the arguments of one staging attempt.
    fn new(
        repo_root: &Path,
        project: &ProjectConfig,
        context: &ContextState,
        edits: &[EditOperation],
        initial_staged: Option<&StagedEditsState>,
    ) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            project: project.clone(),
            context: context.clone(),
            edits: edits.to_vec(),
            initial_staged: initial_staged.cloned(),
        }
    }

    /// Runs the staging state machine over the captured inputs.
    fn stage(&self, config_validator: Option<StagedConfigValidator>) -> StageEditsResult {
        stage_and_validate_edits_inner(
            &self.repo_root,
            &self.project,
            &self.context,
            &self.edits,
            self.initial_staged.as_ref(),
            config_validator,
        )
    }
}

/// Stages the change set in memory without occupying the calling executor.
///
/// The state machine reads every file it edits and probes the working tree, so
/// running it on an executor thread stalls the runtime that drives the turn —
/// on a current-thread runtime it stalls the only executor thread there is.
/// The attempt is therefore offloaded to the blocking pool, which leaves the
/// executor free to poll cancellation, provider streaming, and RPC traffic
/// while a large change set stages.
///
/// An offload that returns no result — a panicked or cancelled blocking task —
/// is recorded through [`log_join_error`] and the attempt degrades to the
/// synchronous path: the change set still stages, it simply does so on the
/// calling thread.
async fn stage_off_the_executor(
    inputs: Arc<StagingInputs>,
    config_validator: Option<StagedConfigValidator>,
) -> StageEditsResult {
    let offloaded = Arc::clone(&inputs);
    let staged = run_blocking_async(move || offloaded.stage(config_validator)).await;
    match staged {
        Ok(result) => result,
        Err(error) => {
            log_join_error(&error, "stage_and_validate_edits");
            run_blocking(|| inputs.stage(config_validator))
        }
    }
}

/// Streams every file an attempt staged through the declared project filter.
///
/// Formatting has exactly one contour: the staged text of a file enters
/// `[project.format]` on standard input and the rewritten text returns on
/// standard output, so the change set is formatted while it still lives in
/// memory. A repository that declares no filter keeps every authored content,
/// and a cancelled invocation leaves the rest of the change set untouched.
///
/// `config_validator` is the gate the change set passed on admission, and every
/// rewritten document passes it again before it joins the staged state: the
/// filter produces bytes the validator never inspected, so a filter that mangles
/// a repository configuration file would otherwise hand the harness a document
/// it refuses. A rejected rewrite keeps the authored content of that file and
/// records one compact warning, while the attempt itself still succeeds — a
/// filter whose output the gate refuses is a formatter failure for that file,
/// not a change-set failure, so the staged content stays the validated admission
/// and no formatter artifact reaches the patch-retry loop.
async fn format_staged_files(
    repo_root: &Path,
    project: &ProjectConfig,
    config_validator: Option<StagedConfigValidator>,
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
        // The filter rewrote bytes the admission gate already validated, so the
        // rewritten document goes back through that same gate before it becomes
        // the staged content. A rejection keeps the authored content, which the
        // staged state still holds in its validated form.
        let rejected = match config_validator {
            Some(validate) => validate(&path, Some(&content)).err(),
            None => None,
        };
        if let Some(message) = rejected {
            tracing::warn!(
                path = %path,
                reason = %message,
                "in-memory formatter output rejected by the staged configuration gate; keeping the authored content"
            );
            continue;
        }
        result.staged_state.staged_files.insert(path, content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer};

    /// The repository configuration file the filters rewrite.
    const CONFIG_PATH: &str = "tauqe.toml";
    /// The configuration the working tree holds before the attempt.
    const ORIGINAL_CONFIG: &str = "[project]\n";
    /// The configuration the attempt authors in memory.
    const AUTHORED_CONFIG: &str = "# authored configuration\n";

    /// Parses a `[project]` declaration from its JSON form.
    ///
    /// The model is serde-generic, so the JSON form pins the same shape the TOML
    /// reader produces without depending on a TOML parser in this module.
    fn project_config(source: &str) -> ProjectConfig {
        serde_json::from_str(source).expect("the [project] section parses")
    }

    /// Creates a temporary repository the batched ignore probe can inspect.
    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(dir.path())
            .status()
            .expect("git is available");
        assert!(status.success(), "git init failed");
        dir
    }

    /// Builds the context snapshot that holds the staged configuration file.
    fn context_state() -> ContextState {
        ContextState {
            revision: 1,
            items: vec![ContextItem {
                path: CONFIG_PATH.to_string(),
                access: ContextAccess::Editable,
                size_bytes: ORIGINAL_CONFIG.len() as u64,
                estimated_tokens: 1,
                layer: ContextLayer::User,
                insertion_seq: 1,
            }],
            total_estimated_tokens: 1,
        }
    }

    /// Rejects a rewritten document that carries the corruption marker.
    fn rejects_corruption(path: &str, content: Option<&str>) -> Result<(), String> {
        match content {
            Some(content) if content.contains("CORRUPT") => {
                Err(format!("'{path}' carries the corruption marker"))
            }
            _ => Ok(()),
        }
    }

    /// Stages a replacement of the repository configuration file under `project`.
    async fn stage_configuration_replacement(
        repo_root: &Path,
        project: &ProjectConfig,
        validator: StagedConfigValidator,
    ) -> StageEditsResult {
        let edits = [EditOperation::Replace {
            path: CONFIG_PATH.to_string(),
            old_text: ORIGINAL_CONFIG.to_string(),
            new_text: AUTHORED_CONFIG.to_string(),
        }];
        prepare_staged_edits_with_config_and_validator(
            repo_root,
            project,
            &context_state(),
            &edits,
            None,
            Some(validator),
            None,
        )
        .await
    }

    /// A filter whose output the gate refuses never reaches the staged state:
    /// the authored content survives, the attempt still succeeds, and the
    /// corrupted bytes never reach the working tree.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_a_corrupting_formatter_keeps_the_validated_staged_content() {
        let dir = repository();
        std::fs::write(dir.path().join(CONFIG_PATH), ORIGINAL_CONFIG).unwrap();
        let project = project_config(
            r#"{"format": {"command": "sh", "args": ["-c", "cat; printf CORRUPT"]}}"#,
        );

        let result =
            stage_configuration_replacement(dir.path(), &project, rejects_corruption).await;

        assert!(
            result.is_success(),
            "the attempt still succeeds: {:?}",
            result.failed_files
        );
        assert!(result.failed_files.is_empty());
        assert_eq!(result.successful_paths, vec![CONFIG_PATH.to_string()]);
        let staged = result
            .staged_state
            .staged_files
            .get(CONFIG_PATH)
            .expect("the authored content still stages");
        assert_eq!(staged.as_str(), AUTHORED_CONFIG);
        assert!(
            !staged.contains("CORRUPT"),
            "the corrupted formatted bytes reached the staged state: {staged:?}"
        );
        // Staging is memory only, so the working tree keeps the original.
        assert_eq!(
            std::fs::read_to_string(dir.path().join(CONFIG_PATH)).unwrap(),
            ORIGINAL_CONFIG
        );
    }

    /// A filter whose output the gate accepts rewrites the staged content, so
    /// re-gating never disables formatting for a well-behaved project.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_an_accepted_formatter_rewrites_the_staged_content() {
        let dir = repository();
        std::fs::write(dir.path().join(CONFIG_PATH), ORIGINAL_CONFIG).unwrap();
        let project =
            project_config(r#"{"format": {"command": "sh", "args": ["-c", "tr a-z A-Z"]}}"#);

        let result =
            stage_configuration_replacement(dir.path(), &project, rejects_corruption).await;

        assert!(result.is_success());
        assert!(result.failed_files.is_empty());
        assert_eq!(
            result
                .staged_state
                .staged_files
                .get(CONFIG_PATH)
                .map(String::as_str),
            Some("# AUTHORED CONFIGURATION\n")
        );
    }
    /// The thread the offloaded staging call ran on, recorded by the validator
    /// hook of the offload test below.
    static STAGING_THREAD: std::sync::OnceLock<std::thread::ThreadId> = std::sync::OnceLock::new();

    /// Records the thread the staging state machine runs on.
    fn record_staging_thread(_path: &str, _content: Option<&str>) -> Result<(), String> {
        let _ = STAGING_THREAD.set(std::thread::current().id());
        Ok(())
    }

    /// The async entry point is driven from inside a task, so the blocking
    /// reads of the staging state machine must leave the executor thread. On a
    /// current-thread runtime that thread is the only executor thread, so a
    /// staging call that ran there would stall every other task on it.
    #[tokio::test]
    async fn test_staging_is_offloaded_off_the_executor_thread() {
        let dir = repository();
        std::fs::write(dir.path().join(CONFIG_PATH), ORIGINAL_CONFIG).unwrap();
        let project = project_config(r#"{"verify": {"command": "cargo"}}"#);
        let executor_thread = std::thread::current().id();

        let result =
            stage_configuration_replacement(dir.path(), &project, record_staging_thread).await;

        assert!(result.is_success(), "{:?}", result.failed_files);
        let staging_thread = STAGING_THREAD
            .get()
            .expect("the validator hook observed the staging call");
        assert_ne!(
            *staging_thread, executor_thread,
            "staging ran on the executor thread"
        );
    }
}
