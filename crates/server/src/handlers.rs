pub mod architect;
pub mod common;
pub mod config;
pub mod context;
pub mod git;
pub mod history;
pub mod host;
pub mod locale;
pub mod model;
pub mod outer_docs;
pub mod plan;
pub mod review;

use std::path::Path;
use std::sync::Arc;

use crate::state::{AppState, OperationGuard};
use tauqe_core::context::ContextManager;
use tauqe_core::edits::EditProtocolFactory;
use tauqe_core::history::HistoryManager;
use tauqe_core::workflow::WorkflowFactory;
use tauqe_protocol::{
    errors, methods, InitializeResult, Request, RequestId, Response, PROTOCOL_VERSION,
};
use tokio::sync::watch;

pub async fn handle_request(req: Request, state: &Arc<AppState>) -> Response {
    match req.method.as_str() {
        methods::CLIENT_INITIALIZE => handle_client_initialize(req, state).await,

        methods::REPOSITORY_GET_STATE => git::handle_repository_get_state(req, state).await,
        methods::REPOSITORY_INIT => git::handle_repository_init(req, state).await,
        methods::REPOSITORY_LIST_FILES => git::handle_repository_list_files(req, state).await,
        methods::GIT_GET_DIFF => git::handle_git_get_diff(req, state).await,
        methods::GIT_SQUASH_PREVIEW => git::handle_git_squash_preview(req, state).await,
        methods::GIT_SQUASH_GENERATE_MESSAGE => {
            git::handle_git_squash_generate_message(req, state).await
        }
        methods::GIT_SQUASH_APPLY => git::handle_git_squash_apply(req, state).await,

        methods::CONTEXT_GET => context::handle_context_get(req, state).await,
        methods::CONTEXT_ADD => context::handle_context_add(req, state).await,
        methods::CONTEXT_ADD_PATTERN => context::handle_context_add_pattern(req, state).await,
        methods::CONTEXT_REMOVE => context::handle_context_remove(req, state).await,
        methods::CONTEXT_SET_ACCESS => context::handle_context_set_access(req, state).await,
        methods::CONTEXT_CLEAR => context::handle_context_clear(req, state).await,

        methods::OUTER_DOCS_CACHE_LIST => {
            outer_docs::handle_outer_docs_cache_list(req, state).await
        }
        methods::OUTER_DOCS_CACHE_REMOVE => {
            outer_docs::handle_outer_docs_cache_remove(req, state).await
        }

        methods::HISTORY_GET => history::handle_history_get(req, state).await,

        methods::MODEL_ASK => model::handle_model_ask(req, state).await,
        methods::MODEL_CANCEL => model::handle_model_cancel(req, state).await,
        methods::MODEL_CLEAR_HISTORY => model::handle_model_clear_history(req, state).await,
        methods::MODEL_SELECT => model::handle_model_select(req, state).await,
        methods::MODEL_CURRENT => model::handle_model_current(req, state).await,
        methods::MODEL_USAGE_RESET => model::handle_usage_reset(req, state).await,

        methods::DISCUSSION_SET_FOCUS => model::handle_discussion_set_focus(req, state).await,
        methods::DISCUSSION_GET_FOCUS => model::handle_discussion_get_focus(req, state).await,
        methods::DISCUSSION_CLEAR_FOCUS => model::handle_discussion_clear_focus(req, state).await,

        methods::LOCALE_GENERATE => locale::handle_locale_generate(req, state).await,

        methods::REVIEW_START => review::handle_review_start(req, state).await,
        methods::REVIEW_PREVIEW => review::handle_review_preview(req, state).await,
        methods::REVIEW_CANCEL => review::handle_review_cancel(req, state).await,
        methods::REVIEW_LIST => review::handle_review_list(req, state).await,
        methods::REVIEW_GET => review::handle_review_get(req, state).await,
        methods::REVIEW_UPDATE_ITEM => review::handle_review_update_item(req, state).await,
        methods::REVIEW_DELETE => review::handle_review_delete(req, state).await,
        methods::REVIEW_EXECUTE => review::handle_review_execute(req, state).await,
        methods::REVIEW_ANALYZE => review::handle_review_analyze(req, state).await,

        methods::PLAN_LIST => plan::handle_plan_list(req, state).await,
        methods::PLAN_GET => plan::handle_plan_get(req, state).await,
        methods::PLAN_SAVE => plan::handle_plan_save(req, state).await,
        methods::PLAN_UPDATE_ITEM => plan::handle_plan_update_item(req, state).await,
        methods::PLAN_DELETE => plan::handle_plan_delete(req, state).await,
        methods::PLAN_SET_ACTIVE => plan::handle_plan_set_active(req, state).await,
        methods::PLAN_CLEAR_HORIZON => plan::handle_plan_clear_horizon(req, state).await,
        methods::PLAN_EXECUTE => plan::handle_plan_execute(req, state).await,
        methods::PLAN_EXECUTE_PIPELINE => plan::handle_plan_execute_pipeline(req, state).await,
        methods::PLAN_CREATE_PIPELINE => plan::handle_plan_create_pipeline(req, state).await,
        methods::PLAN_REFINE => plan::handle_plan_refine(req, state).await,
        methods::PLAN_REPLAN => plan::handle_plan_replan(req, state).await,

        methods::CONFIG_GET => config::handle_config_get(req, state).await,
        methods::CONFIG_SET => config::handle_config_set(req, state).await,
        methods::CONFIG_RELOAD => config::handle_config_reload(req, state).await,
        methods::CONFIG_CREATE => config::handle_config_create(req, state).await,
        methods::CREDENTIALS_SAVE => config::handle_credentials_save(req, state).await,
        methods::CREDENTIALS_CREATE_STUB => {
            config::handle_credentials_create_stub(req, state).await
        }
        methods::SYSTEM_STATUS => config::handle_system_status(req, state).await,

        _ => Response::err(
            req.id,
            tauqe_protocol::errors::METHOD_NOT_FOUND,
            format!("Method '{}' not found", req.method),
        ),
    }
}

/// Exclusion policy for state-mutating RPCs.
///
/// Two shapes exist, and every mutating handler declares which one it uses:
///
/// - **Slot-owning** ([`begin_exclusive_operation`]): the handler keeps the
///   operation slot for its whole mutation, so no turn or batch can register
///   between the check and the write. Every handler that mutates state a
///   running turn merges back or swaps belongs here: rebinding a manager set
///   (`client/initialize`, `repository/init`), rewriting commit history
///   (`git/squashPreview`, `git/squashApply`), reading the commit range a
///   squash message is generated from (`git/squashGenerateMessage`), and
///   replacing the shared configuration and model selection (`config/set`,
///   `config/reload`,
///   `config/create`, `credentials/save`, `credentials/create_stub`,
///   `model/select`), discarding the interaction history
///   (`model/clearHistory`), and mutating the context a turn reasons over
///   (`context/add`, `context/addPattern`, `context/remove`,
///   `context/setAccess`, `context/clear`). The turns and batches themselves
///   (`model/ask`, `plan/execute`, `plan/executePipeline`, `plan/refine`,
///   `plan/replan`, `review/execute`, `review/analyze`, `locale/generate`)
///   register the slot from the workflow they spawn, so they are mutually
///   exclusive with every request above by construction.
/// - **Slot-observing** ([`reject_if_operation_active`]): the handler performs
///   one bounded write on a single record — `plan/save`, `plan/updateItem`,
///   `plan/clearHorizon`, `plan/delete`, `plan/setActive`, `review/delete`,
///   `review/updateItem`, `model/usageReset`. None of them mutates the context,
///   the history, or the shared model selection a running turn clones, merges
///   back, or swaps when it ends, and each change reaches disk as one atomic
///   write of that record. A request that lands inside the registration window
///   is therefore ordered before the operation's own writes to the same record
///   rather than interleaved with them, and the operation owns the lifecycle of
///   that record: a batch's terminal step status supersedes a client status
///   picked for the step it executed, and `model/usageReset` clears only the
///   lifetime totals, on top of which a turn registering afterwards accumulates
///   its own completed window.
///
/// The distinction matters because a bare `is_operation_active()` observation
/// is not mutual exclusion: a request that registers an operation between the
/// check and the mutation runs concurrently with it. A handler therefore either
/// claims the slot for its whole mutation or holds one of the bounded shapes
/// above; sampling the slot without one of those guarantees is prohibited.
pub(crate) fn reject_if_operation_active(
    req_id: &RequestId,
    state: &Arc<AppState>,
    message: &str,
) -> Option<Response> {
    if state.is_operation_active() {
        Some(Response::err(
            req_id.clone(),
            errors::OPERATION_IN_PROGRESS,
            message,
        ))
    } else {
        None
    }
}

/// Claims the operation slot for a state-mutating request.
///
/// The returned [`OperationGuard`] owns the registration, so the whole mutation
/// — a configuration rebind, a Git history rewrite, a repository
/// re-initialization — is mutually exclusive with any concurrently starting
/// turn or batch instead of merely observing that none was running at the
/// first instruction. The guard clears the slot on drop, so every early return
/// path of the handler releases it.
///
/// The failure keeps the established wire contract: a request arriving while
/// another operation runs is answered with `OPERATION_IN_PROGRESS` and the
/// handler-specific `message`.
pub(crate) fn begin_exclusive_operation(
    req_id: &RequestId,
    state: &Arc<AppState>,
    message: &str,
) -> Result<OperationGuard, Box<Response>> {
    state.try_begin_operation().map_err(|()| {
        let refusal = Response::err(req_id.clone(), errors::OPERATION_IN_PROGRESS, message);
        Box::new(refusal)
    })
}

/// Executes a blocking closure on the Tokio blocking pool.
///
/// Repository handlers perform synchronous git subprocess and file I/O. Running
/// that work directly on an async worker thread starves the runtime, so it is
/// offloaded here. A cancelled or panicked task degrades to an error string
/// instead of crashing the daemon.
pub(crate) async fn offload<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| format!("blocking task failed: {}", err))
}

/// Applies a mutation to the shared context atomically.
///
/// The mutation runs on the blocking pool because it touches the file system
/// (metadata reads, path verification), but the exclusive context write guard
/// is acquired first and moved into the blocking task with the closure, so the
/// whole read-modify-write sequence stays serialized. Cloning the manager under
/// a short read guard and overwriting the slot afterwards would let a
/// concurrent mutation be lost and would reset the monotonic revision.
pub(crate) async fn with_context_mutation<T, F>(
    state: &Arc<AppState>,
    mutate: F,
) -> Result<T, String>
where
    F: FnOnce(&mut ContextManager) -> T + Send + 'static,
    T: Send + 'static,
{
    state.mutate_context(mutate).await
}

/// Applies a mutation to the shared interaction history atomically.
///
/// History persistence is `fsync`-bounded, so the mutation runs on the blocking
/// pool while the exclusive history guard stays held, keeping the whole
/// read-modify-write sequence serialized the same way context mutations are.
pub(crate) async fn with_history_mutation<T, F>(
    state: &Arc<AppState>,
    mutate: F,
) -> Result<T, String>
where
    F: FnOnce(&mut HistoryManager) -> T + Send + 'static,
    T: Send + 'static,
{
    state.mutate_history(mutate).await
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
pub(crate) enum PreflightVerdict {
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
pub(crate) async fn run_preflight_verification(
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

    // The configuration integrity check runs first: a batch must not launch
    // against a `tauqe.toml` whose model roles or command chain are broken,
    // because its steps would fail mid-flight instead of being refused before
    // any LLM request is spent. The check self-offloads its blocking file
    // reads off the async executor thread, and a repository without a
    // configuration file is governed by built-in defaults and passes.
    if let Err(report) = tauqe_core::config::validate_project_config_on_disk(repo_root) {
        return PreflightVerdict::Failed(report);
    }

    // A pre-flight gate runs before a turn exists, so it has no change set and
    // the project command conservatively runs its full suite. The command races
    // the cancellation signal inside the substrate, which kills the whole
    // process group and reports an `is_cancelled` result, so no race is
    // re-implemented here.
    let (status, results) =
        tauqe_core::toolchain::run_verification_pipeline(repo_root, &[], Some(&cancel_rx)).await;
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

/// Rebinds configuration, context, history, and usage telemetry to a
/// repository root.
///
/// `client/initialize` and `repository/init` both resolve the authoritative
/// repository root and then must rebind every long-lived manager to it. The
/// sequence lives here once so the two entry points cannot drift apart. Both
/// call it while owning the operation slot, so a rebind can never interleave
/// with a turn or batch that registered concurrently.
///
/// Each step takes its own short-lived guard: no guard is ever held across
/// another subsystem's `.await`. Rebuilding the history manager rereads the
/// journal, so it is constructed on the blocking pool instead of stalling an
/// async worker thread; attaching the mutation listener stays here, where the
/// client event channel lives.
pub(crate) async fn rebind_to_repository(state: &Arc<AppState>, repo_path: &Path) {
    if repo_path.as_os_str().is_empty() {
        return;
    }

    let loaded = config::reload_config_with_overrides(state, Some(repo_path)).await;
    {
        let mut guard = state.config.lock().await;
        guard.selection = loaded.default_selection();
        guard.config = loaded;
    }

    let repo_buf = repo_path.to_path_buf();
    if let Err(err) = state
        .mutate_context(move |ctx| ctx.set_repo_root(repo_buf))
        .await
    {
        tracing::warn!(
            error = %err,
            "Failed to rebind the context to the initialized repository"
        );
    }

    // The project lifetime totals belong to the repository, so they are
    // re-adopted here. The context must already point at the new root because
    // the telemetry path derives from it.
    state.reload_usage_lifetime().await;

    let history_root = repo_path.to_path_buf();
    let mut history_manager = match offload(move || HistoryManager::new(history_root)).await {
        Ok(history_manager) => history_manager,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "Failed to rebuild the interaction history for the repository"
            );
            return;
        }
    };
    let sender = state.history_sender.clone();
    // Forward every history mutation so a compaction reaches the client as a
    // structural rewrite rather than a trailing append.
    history_manager.set_listener(move |mutation| {
        let _ = sender.send(mutation);
    });
    let mut history = state.history.lock().await;
    *history = history_manager;
}

async fn handle_client_initialize(req: Request, state: &Arc<AppState>) -> Response {
    // Re-initialization rebinds the config, context, usage lifetime, and
    // history, so it claims the operation slot instead of sampling it: a turn
    // registering right after a plain check would have every one of those
    // managers swapped underneath it.
    let _operation = match begin_exclusive_operation(
        &req.id,
        state,
        "Cannot re-initialize the repository while a model operation is in progress",
    ) {
        Ok(guard) => guard,
        Err(resp) => return *resp,
    };

    // Reading the repository state shells out to git, so it is offloaded and
    // resolved from the authoritative shared root instead of the process CWD.
    let repo_root = state.repo_root().await;
    let repo_state = match offload(move || {
        let root = repo_root.as_path();
        tauqe_core::git::get_repository_state(Some(root))
    })
    .await
    {
        Ok(repo_state) => repo_state,
        Err(err) => {
            tracing::warn!(error = %err, "Failed to read repository state during initialize");
            return Response::err(req.id, tauqe_protocol::errors::REPOSITORY_STATE_FAILED, err);
        }
    };
    rebind_to_repository(state, Path::new(&repo_state.root)).await;

    let (effective_model, workflow, edit_protocol, available_models) = {
        let guard = state.config.lock().await;
        (
            guard.config.resolve_model(&guard.selection),
            guard.config.repl.workflow,
            guard.config.repl.protocol,
            guard.config.available_models(),
        )
    };

    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION.to_string(),
        server_name: "tauqe-server".to_string(),
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        repository: Some(repo_state),
        model: effective_model,
        workflow: Some(workflow),
        edit_protocol: Some(edit_protocol),
        available_models,
        available_workflows: WorkflowFactory::available_workflows(),
        available_edit_protocols: EditProtocolFactory::available_protocols(),
    };
    Response::ok_typed(req.id, &result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::OutChannel;
    use tauqe_protocol::RequestId;

    #[tokio::test]
    async fn test_unknown_method_returns_error() {
        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        let req = Request {
            id: RequestId::Number(42),
            method: "nonexistent/method".to_string(),
            params: None,
        };

        let resp = handle_request(req, &state).await;
        assert_eq!(resp.id, RequestId::Number(42));
        assert!(resp.result.is_none());
        assert_eq!(resp.error.unwrap().code, "METHOD_NOT_FOUND");
    }

    #[tokio::test]
    async fn test_config_get_succeeds() {
        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        let req = Request {
            id: RequestId::String("cfg-1".to_string()),
            method: methods::CONFIG_GET.to_string(),
            params: None,
        };

        let resp = handle_request(req, &state).await;
        assert_eq!(resp.id, RequestId::String("cfg-1".to_string()));
        assert!(resp.error.is_none());
        assert!(resp.result.is_some());
    }

    #[tokio::test]
    async fn test_preflight_verification_passes_without_toolchain() {
        let dir = std::env::temp_dir().join(format!(
            "tauqe-test-preflight-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let verdict = run_preflight_verification(&dir, cancel_rx).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(verdict, PreflightVerdict::Passed),
            "got: {verdict:?}"
        );
    }

    #[tokio::test]
    async fn test_preflight_verification_rejects_invalid_config() {
        let dir = std::env::temp_dir().join(format!(
            "tauqe-test-preflight-invalid-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        // Verification is declarative: a declaration that only assigns model
        // roles is complete, so the gate passes it instead of inventing a
        // toolchain the project never declared.
        std::fs::write(
            dir.join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:a/b\"]\n",
        )
        .unwrap();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let verdict = run_preflight_verification(&dir, cancel_rx).await;
        assert!(
            matches!(verdict, PreflightVerdict::Passed),
            "a declaration without verification must pass: {verdict:?}"
        );

        // A declaration the harness could not run predictably is still refused:
        // the file routes its verification through a command nobody declares.
        std::fs::write(
            dir.join("tauqe.toml"),
            concat!(
                "[models]\nprimary = [\"openrouter:a/b\"]\n",
                "\n[project.verify]\ncommand = \"@missing\"\n",
            ),
        )
        .unwrap();
        let (_retry_cancel_tx, retry_cancel_rx) = watch::channel(false);
        let verdict = run_preflight_verification(&dir, retry_cancel_rx).await;
        let _ = std::fs::remove_dir_all(&dir);
        match verdict {
            PreflightVerdict::Failed(report) => {
                assert!(report.contains("missing"), "got: {report}");
            }
            other => panic!("an invalid configuration must be refused: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preflight_verification_accepts_valid_config() {
        let dir = std::env::temp_dir().join(format!(
            "tauqe-test-preflight-valid-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"true\"\n",
        )
        .unwrap();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let verdict = run_preflight_verification(&dir, cancel_rx).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(verdict, PreflightVerdict::Passed),
            "got: {verdict:?}"
        );
    }

    /// A hard cancel delivered while the pre-flight verification runs tears the
    /// verification down instead of waiting the build/test suite out.
    ///
    /// The gate runs after the operation slot is registered, so it must inherit
    /// the operation's cancellation channel: the substrate kills the whole
    /// process group of the project command and reports an `is_cancelled`
    /// result, and the gate answers with its cancelled verdict so the caller
    /// releases the slot at once.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_preflight_verification_is_torn_down_by_a_cancel_signal() {
        let dir = std::env::temp_dir().join(format!(
            "tauqe-test-preflight-cancel-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("tauqe.toml"),
            "[models]\nprimary = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"sh\"\nargs = ['-c', 'printf started > started.marker; sleep 60']\n",
        )
        .unwrap();

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let root = dir.clone();
        let gate = tokio::spawn(async move { run_preflight_verification(&root, cancel_rx).await });

        // Cancel only once the verification command is demonstrably running, so
        // the prompt return proves the teardown instead of a slow start.
        let marker = dir.join("started.marker");
        let mut running = false;
        for _ in 0..200 {
            if marker.exists() {
                running = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(running, "the verification command never started");

        let _ = cancel_tx.send(true);
        let verdict = tokio::time::timeout(std::time::Duration::from_secs(10), gate)
            .await
            .expect("the cancelled gate must return instead of waiting the suite out")
            .expect("the gate task joins");

        assert!(
            matches!(verdict, PreflightVerdict::Cancelled),
            "a cancelled verification must report a cancellation, not a failure: {verdict:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Re-initialization rebinds the configuration, context, usage lifetime,
    /// and history of a live daemon, so it must own the operation slot instead
    /// of sampling it once: a turn registering right after a plain check would
    /// have every one of those managers swapped underneath it.
    #[tokio::test]
    async fn test_client_initialize_is_refused_while_an_operation_is_registered() {
        let dir = std::env::temp_dir().join(format!(
            "tauqe-test-initialize-exclusive-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        tauqe_core::git::init_repository(Some(&dir), true).unwrap();

        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.clone(),
            hist_tx,
            out,
        );

        state
            .mutate_history(|history| {
                history
                    .record_turn("In-flight prompt".to_string(), Vec::new())
                    .map(|_| ())
            })
            .await
            .expect("recording the turn runs on the blocking pool")
            .expect("the turn record must persist");
        let recorded = state
            .history
            .lock()
            .await
            .get_entries()
            .expect("history reads back")
            .len();
        assert!(recorded > 0, "the turn record must be persisted");

        let operation = state.try_begin_operation().expect("operation slot is free");

        let req = Request {
            id: RequestId::Number(9),
            method: methods::CLIENT_INITIALIZE.to_string(),
            params: None,
        };
        let resp = handle_request(req, &state).await;
        assert_eq!(resp.id, RequestId::Number(9));
        assert_eq!(
            resp.error.expect("re-initialization must be refused").code,
            errors::OPERATION_IN_PROGRESS
        );

        assert!(
            state.is_operation_active(),
            "the running operation must keep owning the slot"
        );
        assert_eq!(
            state
                .history
                .lock()
                .await
                .get_entries()
                .expect("history reads back")
                .len(),
            recorded,
            "the running turn's recorded history must survive the refused rebind"
        );

        drop(operation);
        assert!(
            !state.is_operation_active(),
            "releasing the guard must free the slot"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `review/analyze` is wired into the dispatcher: an unresolvable session is
    /// answered with the review error code instead of `METHOD_NOT_FOUND`.
    #[tokio::test]
    async fn test_review_analyze_is_dispatched() {
        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            tauqe_core::config::AppConfig::default(),
            std::path::PathBuf::from("/nonexistent"),
            hist_tx,
            out,
        );

        let req = Request {
            id: RequestId::Number(7),
            method: methods::REVIEW_ANALYZE.to_string(),
            params: Some(serde_json::json!({ "review_id": "missing-session" })),
        };

        let resp = handle_request(req, &state).await;
        assert_eq!(resp.id, RequestId::Number(7));
        assert_eq!(resp.error.unwrap().code, errors::NOT_FOUND);
    }

    /// A repository whose index another process holds cannot run a batch: the
    /// pre-flight gate refuses before the first step opens a transaction, instead
    /// of letting the first capture fail inside the queue.
    #[tokio::test]
    async fn test_preflight_verification_refuses_a_locked_index() {
        let dir = std::env::temp_dir().join(format!(
            "tauqe-test-preflight-locked-index-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        tauqe_core::git::init_repository(Some(&dir), true).unwrap();
        std::fs::write(dir.join(".git").join("index.lock"), "").unwrap();

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let verdict = run_preflight_verification(&dir, cancel_rx).await;

        match verdict {
            PreflightVerdict::Failed(report) => {
                assert!(report.contains("index"), "report: {report}");
            }
            other => panic!("a locked index must be refused: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
