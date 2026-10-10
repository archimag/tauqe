pub mod config;
pub mod context;
pub mod git;
pub mod history;
pub mod model;
pub mod plan;
pub mod review;

use std::path::Path;
use std::sync::Arc;

use tauqe_core::config::load_config;
use tauqe_core::edits::EditProtocolFactory;
use tauqe_core::history::HistoryManager;
use tauqe_core::workflow::WorkflowFactory;
use tauqe_protocol::{methods, InitializeResult, Request, Response, PROTOCOL_VERSION};

use crate::state::AppState;

pub async fn handle_request(req: Request, state: &Arc<AppState>) -> Response {
    match req.method.as_str() {
        methods::CLIENT_INITIALIZE => handle_client_initialize(req, state).await,

        methods::REPOSITORY_GET_STATE => git::handle_repository_get_state(req, state).await,
        methods::REPOSITORY_INIT => git::handle_repository_init(req, state).await,
        methods::REPOSITORY_LIST_FILES => git::handle_repository_list_files(req, state).await,
        methods::GIT_UNDO => git::handle_git_undo(req, state).await,
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

        methods::HISTORY_GET => history::handle_history_get(req, state).await,

        methods::MODEL_ASK => model::handle_model_ask(req, state).await,
        methods::MODEL_CANCEL => model::handle_model_cancel(req, state).await,
        methods::MODEL_CLEAR_HISTORY => model::handle_model_clear_history(req, state).await,
        methods::MODEL_SELECT => model::handle_model_select(req, state).await,
        methods::MODEL_CURRENT => model::handle_model_current(req, state).await,

        methods::REVIEW_START => review::handle_review_start(req, state).await,
        methods::REVIEW_CANCEL => review::handle_review_cancel(req, state).await,
        methods::REVIEW_LIST => review::handle_review_list(req, state).await,
        methods::REVIEW_GET => review::handle_review_get(req, state).await,
        methods::REVIEW_UPDATE_ITEM => review::handle_review_update_item(req, state).await,
        methods::REVIEW_DELETE => review::handle_review_delete(req, state).await,
        methods::REVIEW_EXECUTE_ITEM => review::handle_review_execute_item(req, state).await,

        methods::PLAN_LIST => plan::handle_plan_list(req, state).await,
        methods::PLAN_GET => plan::handle_plan_get(req, state).await,
        methods::PLAN_SAVE => plan::handle_plan_save(req, state).await,
        methods::PLAN_UPDATE_ITEM => plan::handle_plan_update_item(req, state).await,
        methods::PLAN_DELETE => plan::handle_plan_delete(req, state).await,
        methods::PLAN_SET_ACTIVE => plan::handle_plan_set_active(req, state).await,
        methods::PLAN_EXECUTE_STEP => plan::handle_plan_execute_step(req, state).await,
        methods::PLAN_REFINE => plan::handle_plan_refine(req, state).await,

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

async fn handle_client_initialize(req: Request, state: &Arc<AppState>) -> Response {
    let repo_state = tauqe_core::git::get_repository_state(None);
    let repo_path = Path::new(&repo_state.root);
    let mut cfg = state.config.lock().await;
    if !repo_path.as_os_str().is_empty() {
        let loaded = load_config(Some(repo_path));
        *state.current_selection.lock().await = loaded.default_selection();
        *cfg = loaded;
        {
            let mut ctx = state.context.write().await;
            ctx.set_repo_root(repo_path.to_path_buf());
        }
        let mut hm = HistoryManager::new(repo_path.to_path_buf());
        let sender = state.history_sender.clone();
        hm.set_listener(move |item| {
            let _ = sender.send(item);
        });
        let mut history = state.history.lock().await;
        *history = hm;
    }

    let selection = state.current_selection.lock().await.clone();
    let effective_model = cfg.resolve_model(&selection);

    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION.to_string(),
        server_name: "tauqe-server".to_string(),
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        repository: Some(repo_state),
        model: effective_model,
        workflow: Some(cfg.develop.workflow.clone()),
        edit_protocol: Some(cfg.develop.protocol.clone()),
        available_models: cfg.available_models(),
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
}
