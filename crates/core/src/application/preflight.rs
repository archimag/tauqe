//! Registered-operation preflight gate shared by plan and review batches.
use std::path::Path;
use tokio::sync::watch;
async fn offload<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("blocking task failed: {e}"))
}
/// Verdict of the deterministic pre-flight gate shared by server-driven batches.
///
/// The gate must separate a repository that does not build from an operation
/// that was cancelled while the verification still ran. The first is a project
/// failure the developer has to resolve; the second is the requested
/// termination of the operation, answered with `CANCELLED` so the caller
/// releases the operation slot at once instead of reporting a verification
/// failure of a suite that never ran to completion.
#[derive(Debug)]
pub enum PreflightVerdict {
    /// The repository verified cleanly; the batch may start its first step.
    Passed,
    /// The operation was cancelled before or during the verification.
    Cancelled,
    /// The repository does not build; the report names the failed commands.
    Failed(String),
}

/// Deterministic pre-flight verification shared by server-driven batches.
///
/// Runs the exact same project verification pipeline that the core executes after
/// each autonomous workflow step. When the repository does not build or its tests
/// fail, the batch is blocked synchronously before any LLM request or status change,
/// so a broken tree never triggers a wasted self-healing loop. A repository that
/// declares no verification command passes transparently. The project
/// configuration is validated before the pipeline runs, so a `tauqe.toml`
/// whose model roles or `[project]` command chain are broken is refused
/// synchronously as well.
///
/// `cancel_rx` is the cancellation channel of the operation that already owns the
/// slot: an already-cancelled operation never launches the pipeline, and a cancel
/// delivered during the build/test suite is observed by the substrate, which
/// tears the whole process group of the project command down instead of waiting
/// the suite out.
///
/// The repository index is gated the same way: a lock no invocation may still
/// own is reclaimed, a lock that clears on its own is waited out, and a lock
/// that outlives the gate refuses the batch before its first step opens a
/// transaction.
pub async fn run_preflight_verification(
    repo_root: &Path,
    cancel_rx: watch::Receiver<bool>,
) -> PreflightVerdict {
    // A cancel delivered before the gate starts must not launch the pipeline at
    // all: the operation slot is registered and the client waits for its release.
    if *cancel_rx.borrow() {
        tracing::info!("pre-flight verification skipped: the operation was already cancelled");
        return PreflightVerdict::Cancelled;
    }

    // A locked index blocks every Git transaction the batch is about to open, so
    // the gate runs before the configuration check and before the suite: the
    // first capture of the first step would otherwise fail inside the queue,
    // after a model request was already spent. The gate blocks on its backoff, so
    // it runs off the async executor thread.
    let gate_root = repo_root.to_path_buf();
    let gate_cancel = cancel_rx.clone();
    let gate =
        match offload(move || crate::git::ensure_index_available(&gate_root, Some(&gate_cancel)))
            .await
        {
            Ok(gate) => gate,
            Err(reason) => {
                return PreflightVerdict::Failed(format!(
                    "Pre-flight verification could not inspect the repository index: {reason}"
                ));
            }
        };
    match gate {
        Ok(()) => {}
        Err(crate::git::IndexGateError::Cancelled) => {
            tracing::info!("pre-flight verification skipped: the index gate was cancelled");
            return PreflightVerdict::Cancelled;
        }
        Err(error) => {
            return PreflightVerdict::Failed(format!(
                "Pre-flight verification refused to start: {error}"
            ));
        }
    }

    // The configuration integrity check runs first: a batch must not launch
    // against a `tauqe.toml` whose model roles or command chain are broken,
    // because its steps would fail mid-flight instead of being refused before
    // any LLM request is spent. The check self-offloads its blocking file
    // reads off the async executor thread, and a repository without a
    // configuration file is governed by built-in defaults and passes.
    if let Err(report) = crate::config::validate_project_config_on_disk(repo_root) {
        return PreflightVerdict::Failed(report);
    }

    // A pre-flight gate runs before a turn exists, so it has no change set and
    // the project command conservatively runs its full suite. The command races
    // the cancellation signal inside the substrate, which kills the whole
    // process group and reports an `is_cancelled` result, so no race is
    // re-implemented here.
    let (status, results) =
        crate::toolchain::run_verification_pipeline(repo_root, &[], Some(&cancel_rx)).await;
    // A project that declares no verification lifecycle states its own choice,
    // so the gate passes transparently instead of refusing a batch the project
    // never asked to verify.
    if status.allows_commit() {
        return PreflightVerdict::Passed;
    }
    if results.iter().any(|result| result.is_cancelled) {
        tracing::warn!("pre-flight verification was cancelled by the operation");
        return PreflightVerdict::Cancelled;
    }

    let mut failed_commands = Vec::new();
    for result in results.iter().filter(|r| !r.success) {
        failed_commands.push(format!("`{}`", result.command));
    }
    let report = if failed_commands.is_empty() {
        "Pre-flight verification failed without diagnostic output.".to_string()
    } else {
        format!("Command {} failed.", failed_commands.join(", "))
    };
    PreflightVerdict::Failed(report)
}
