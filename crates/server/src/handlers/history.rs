use std::sync::Arc;

use tauqe_protocol::{HistoryGetParams, HistoryGetResult, Request, Response, ResponseError};

use crate::state::AppState;

pub async fn handle_history_get(req: Request, state: &Arc<AppState>) -> Response {
    let params: HistoryGetParams = req
        .params
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();
    let limit = params.limit.unwrap_or(10);
    let history = state.history.lock().await;
    match history.get_ui_slice(limit, params.before_id) {
        Ok((items, has_more, total_count)) => {
            let result = HistoryGetResult {
                items,
                has_more,
                total_count,
            };
            Response {
                id: req.id,
                result: serde_json::to_value(result).ok(),
                error: None,
            }
        }
        Err(err) => Response {
            id: req.id,
            result: None,
            error: Some(ResponseError {
                code: "HISTORY_GET_FAILED".to_string(),
                message: err.to_string(),
                data: None,
            }),
        },
    }
}
