//! Registered-operation preflight gate shared by plan and review batches.
use crate::api::{ApplicationEvent, ApplicationObserver, OperationGuard};
use std::path::Path;
use tauqe_core::providers::StreamEvent;
use tauqe_core::stop::HardCancellation;
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

/// Publishes the batch admission check before any model turn exists.
/// Every returned verdict closes the toolchain activity; launch refusal or
/// the first model turn then settles the client's pending launch state.
pub(crate) async fn run_batch_preflight(
    repo_root: &Path,
    operation: &OperationGuard,
    observer: &dyn ApplicationObserver,
) -> PreflightVerdict {
    let id = operation.publication_id();
    observer.publish(
        &id,
        ApplicationEvent::Stream(StreamEvent::ToolchainStarted {
            command: "pre-flight".into(),
        }),
    );
    let verdict = run_preflight_verification(repo_root, operation.hard_cancellation()).await;
    let (success, message) = match &verdict {
        PreflightVerdict::Passed => (true, "Pre-flight gate passed".to_string()),
        PreflightVerdict::Cancelled => (false, "Pre-flight verification cancelled".to_string()),
        PreflightVerdict::Failed(report) => (false, report.clone()),
    };
    observer.publish(
        &id,
        ApplicationEvent::Stream(StreamEvent::ToolchainFinished {
            command: "pre-flight".into(),
            success,
            message: Some(message),
        }),
    );
    verdict
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
    cancel: HardCancellation,
) -> PreflightVerdict {
    // A cancel delivered before the gate starts must not launch the pipeline at
    // all: the operation slot is registered and the client waits for its release.
    if cancel.is_cancelled() {
        tracing::info!("pre-flight verification skipped: the operation was already cancelled");
        return PreflightVerdict::Cancelled;
    }

    // A locked index blocks every Git transaction the batch is about to open, so
    // the gate runs before the configuration check and before the suite: the
    // first capture of the first step would otherwise fail inside the queue,
    // after a model request was already spent. The gate blocks on its backoff, so
    // it runs off the async executor thread.
    let gate_root = repo_root.to_path_buf();
    let gate_cancel = cancel.clone();
    let gate = match offload(move || {
        tauqe_core::git::ensure_index_available(&gate_root, Some(&gate_cancel))
    })
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
        Err(tauqe_core::git::IndexGateError::Cancelled) => {
            tracing::info!("pre-flight verification skipped: the index gate was cancelled");
            return PreflightVerdict::Cancelled;
        }
        Err(error) => {
            return PreflightVerdict::Failed(format!(
                "Pre-flight verification refused to start: {error}"
            ));
        }
    }

    if cancel.is_cancelled() {
        return PreflightVerdict::Cancelled;
    }

    // The configuration integrity check runs first: a batch must not launch
    // against a `tauqe.toml` whose model roles or command chain are broken,
    // because its steps would fail mid-flight instead of being refused before
    // any LLM request is spent. The check self-offloads its blocking file
    // reads off the async executor thread, and a repository without a
    // configuration file is governed by built-in defaults and passes.
    if let Err(report) = tauqe_core::config::validate_project_config_on_disk(repo_root) {
        if cancel.is_cancelled() {
            return PreflightVerdict::Cancelled;
        }
        return PreflightVerdict::Failed(report);
    }

    if cancel.is_cancelled() {
        return PreflightVerdict::Cancelled;
    }

    // A pre-flight gate runs before a turn exists, so it has no change set and
    // the project command conservatively runs its full suite. The command races
    // the cancellation signal inside the substrate, which kills the whole
    // process group and reports an `is_cancelled` result, so no race is
    // re-implemented here.
    let (status, results) =
        tauqe_core::toolchain::run_verification_pipeline(repo_root, &[], Some(&cancel)).await;
    if status == tauqe_core::toolchain::VerificationStatus::Cancelled || cancel.is_cancelled() {
        tracing::warn!("pre-flight verification was cancelled by the operation");
        return PreflightVerdict::Cancelled;
    }
    // A project that declares no verification lifecycle states its own choice,
    // so the gate passes transparently instead of refusing a batch the project
    // never asked to verify.
    if status.allows_commit() {
        return PreflightVerdict::Passed;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::operation::OperationRegistry;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Observer(Mutex<Vec<(String, StreamEvent)>>);

    impl ApplicationObserver for Observer {
        fn publish(&self, id: &str, event: ApplicationEvent) {
            let ApplicationEvent::Stream(event) = event else {
                panic!("preflight must not start a model turn");
            };
            self.0.lock().unwrap().push((id.into(), event));
        }
    }

    #[tokio::test]
    async fn batch_preflight_publishes_a_balanced_activity_for_every_verdict() {
        for case in ["passed", "failed", "cancelled"] {
            let dir = tempfile::tempdir().unwrap();
            tauqe_core::git::state::init_repository(Some(dir.path()), true).unwrap();
            if case == "failed" {
                std::fs::write(dir.path().join("tauqe.toml"), "[invalid").unwrap();
            }
            let registry = Arc::new(OperationRegistry::default());
            let operation = registry.begin().unwrap();
            if case == "cancelled" {
                registry.hard_cancel();
            }
            let observer = Observer::default();
            let verdict = run_batch_preflight(dir.path(), &operation, &observer).await;
            assert!(matches!(
                (case, &verdict),
                ("passed", PreflightVerdict::Passed)
                    | ("failed", PreflightVerdict::Failed(_))
                    | ("cancelled", PreflightVerdict::Cancelled)
            ));
            let events = observer.0.lock().unwrap();
            assert_eq!(events.len(), 2);
            assert!(events
                .iter()
                .all(|(id, _)| id == &operation.publication_id()));
            assert!(
                matches!(&events[0].1, StreamEvent::ToolchainStarted { command } if command == "pre-flight")
            );
            assert!(
                matches!(&events[1].1, StreamEvent::ToolchainFinished { command, success, message } if command == "pre-flight" && *success == (case == "passed") && message.is_some())
            );
        }
    }
}
