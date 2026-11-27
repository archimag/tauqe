pub mod architect;
pub mod common;
pub mod config;
pub mod context;
pub(crate) mod edit_cycle;
pub mod git;
pub mod history;
pub mod locale;
pub(crate) mod login;
pub mod model;
pub mod outer_docs;
pub mod plan;
pub mod review;

use crate::contracts::Project;
use std::sync::Arc;

use crate::state::AppState;
use tauqe_protocol::{errors, methods, InitializeResult, Request, Response, PROTOCOL_VERSION};

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
        methods::REVIEW_UPDATE_GROUP_STEP => {
            review::handle_review_update_group_step(req, state).await
        }
        methods::REVIEW_DELETE => review::handle_review_delete(req, state).await,
        methods::REVIEW_EXECUTE => review::handle_review_execute(req, state).await,
        methods::REVIEW_ANALYZE => review::handle_review_analyze(req, state).await,
        methods::REVIEW_REFINE_GROUP => review::handle_review_refine_group(req, state).await,

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
        methods::CHATGPT_SIGN_OUT => match state.application.sign_out_chatgpt().await {
            Ok(revoked) => Response::ok(req.id, serde_json::json!(revoked)),
            Err(err) => Response::err(req.id, errors::CREDENTIALS_SAVE_FAILED, err.to_string()),
        },
        methods::CHATGPT_SIGN_IN_CANCEL => {
            match serde_json::from_value::<tauqe_protocol::config::ChatGptSignInId>(
                req.params.unwrap_or_default(),
            ) {
                Ok(params) => {
                    state.login.dismiss(&params.id);
                    Response::ok(req.id, serde_json::Value::Null)
                }
                Err(_) => Response::err(req.id, errors::INVALID_PARAMS, "Missing sign-in ID"),
            }
        }
        methods::SYSTEM_STATUS => config::handle_system_status(req, state).await,

        _ => Response::err(
            req.id,
            tauqe_protocol::errors::METHOD_NOT_FOUND,
            format!("Method '{}' not found", req.method),
        ),
    }
}

#[cfg(test)]
pub(crate) use tauqe_flow::api::{run_preflight_verification, PreflightVerdict};

async fn handle_client_initialize(req: Request, state: &Arc<AppState>) -> Response {
    let observer = Arc::new(crate::publication::Publisher {
        out: state.out.clone(),
    });
    let initialized = match state.application.initialize_client(observer).await {
        Ok(initialized) => initialized,
        Err(tauqe_flow::api::InitializationError::Busy(_)) => {
            return Response::err(
                req.id,
                errors::OPERATION_IN_PROGRESS,
                "Cannot re-initialize the repository while a model operation is in progress",
            )
        }
        Err(error) => {
            return Response::err(req.id, errors::REPOSITORY_STATE_FAILED, error.to_string())
        }
    };
    let config = initialized.configuration;
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION.to_string(),
        server_name: "tauqe-server".to_string(),
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        repository: Some(git::repository_dto(initialized.repository)),
        model: config.model.project(),
        workflow: Some(config.workflow.project()),
        edit_protocol: Some(tauqe_protocol::EditProtocolKind::Structured),
        available_models: config
            .available_models
            .into_iter()
            .map(Project::project)
            .collect(),
        available_workflows: config
            .available_workflows
            .into_iter()
            .map(Project::project)
            .collect(),
        available_edit_protocols: initialized
            .available_edit_protocols
            .into_iter()
            .map(Project::project)
            .collect(),
    };
    Response::ok_typed(req.id, &result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
    use crate::transport::OutChannel;
    use tauqe_core::stop::{HardCancellation, StopSource};
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
        let dir = scratch_root("preflight");
        let verdict = run_preflight_verification(&dir, HardCancellation::never()).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(verdict, PreflightVerdict::Passed),
            "got: {verdict:?}"
        );
    }

    #[tokio::test]
    async fn test_preflight_verification_rejects_invalid_config() {
        let dir = scratch_root("preflight-invalid");
        // Verification is declarative: a declaration that only assigns model
        // roles is complete, so the gate passes it instead of inventing a
        // toolchain the project never declared.
        std::fs::write(
            dir.join("tauqe.toml"),
            "[models]\nrepl = [\"openrouter:a/b\"]\n",
        )
        .unwrap();
        let verdict = run_preflight_verification(&dir, HardCancellation::never()).await;
        assert!(
            matches!(verdict, PreflightVerdict::Passed),
            "a declaration without verification must pass: {verdict:?}"
        );

        // A declaration the harness could not run predictably is still refused:
        // the file routes its verification through a command nobody declares.
        std::fs::write(
            dir.join("tauqe.toml"),
            concat!(
                "[models]\nrepl = [\"openrouter:a/b\"]\n",
                "\n[project.verify]\ncommand = \"@missing\"\n",
            ),
        )
        .unwrap();
        let verdict = run_preflight_verification(&dir, HardCancellation::never()).await;
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
        let dir = scratch_root("preflight-valid");
        std::fs::write(
            dir.join("tauqe.toml"),
            "[models]\nrepl = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"true\"\n",
        )
        .unwrap();
        let verdict = run_preflight_verification(&dir, HardCancellation::never()).await;
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
        let dir = scratch_root("preflight-cancel");
        std::fs::write(
            dir.join("tauqe.toml"),
            "[models]\nrepl = [\"openrouter:a/b\"]\n\n[project.verify]\ncommand = \"sh\"\nargs = ['-c', 'printf started > started.marker; sleep 60']\n",
        )
        .unwrap();

        let stop = StopSource::new();
        let hard = stop.token().hard_cancellation();
        let root = dir.clone();
        let gate = tokio::spawn(async move { run_preflight_verification(&root, hard).await });

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

        stop.request_hard();
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
        let dir = scratch_root("initialize-exclusive");
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
            .application
            .record_history_turn("In-flight prompt".into(), Vec::new())
            .await
            .expect("the turn record must persist");
        let recorded = state
            .application
            .test_mutate_history(|history| history.get_entries())
            .await
            .unwrap()
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
                .application
                .test_mutate_history(|history| history.get_entries())
                .await
                .unwrap()
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
        let dir = scratch_root("preflight-locked-index");
        tauqe_core::git::init_repository(Some(&dir), true).unwrap();
        std::fs::write(dir.join(".git").join("index.lock"), "").unwrap();

        let verdict = run_preflight_verification(&dir, HardCancellation::never()).await;

        match verdict {
            PreflightVerdict::Failed(report) => {
                assert!(report.contains("index"), "report: {report}");
            }
            other => panic!("a locked index must be refused: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
