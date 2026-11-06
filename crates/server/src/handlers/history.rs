use std::sync::Arc;

use tauqe_protocol::{errors, HistoryGetParams, HistoryGetResult, Request, Response};

use crate::state::AppState;

pub async fn handle_history_get(req: Request, state: &Arc<AppState>) -> Response {
    // A present payload must decode through the typed DTO: silently defaulting
    // an unparsable one would page the journal from the default window instead
    // of reporting the mistake.
    let params: HistoryGetParams = match req.params {
        Some(payload) => match serde_json::from_value(payload) {
            Ok(params) => params,
            Err(err) => {
                return Response::err(
                    req.id,
                    errors::INVALID_PARAMS,
                    format!("Invalid history get parameters: {}", err),
                );
            }
        },
        None => HistoryGetParams::default(),
    };
    let limit = params.limit.unwrap_or(10);
    let before_id = params.before_id;

    // The page slice reads `history.jsonl`, so it runs on the blocking pool
    // while the exclusive history guard is held, keeping it serialized against
    // the mutations served through the same guard. The token estimate is
    // maintained by the manager as it appends, so pricing the page costs no
    // extra journal read here. A running turn works on its own clone of the
    // manager and therefore never holds this guard, so the page reports the
    // journal as it stands — including the entries the turn has already
    // appended — instead of waiting for the bounded turn to finish. Compaction
    // and rollback replace the journal atomically, so a concurrent read sees
    // either the previous or the next journal, never a partial one.
    let loaded = crate::handlers::with_history_mutation(state, move |history| {
        let estimated_tokens = history.estimated_tokens();
        history
            .get_ui_slice(limit, before_id)
            .map(|(items, has_more, total_count)| (items, has_more, total_count, estimated_tokens))
            .map_err(|err| err.to_string())
    })
    .await;

    match loaded {
        Ok(Ok((items, has_more, total_count, estimated_tokens))) => {
            let result = HistoryGetResult {
                items,
                has_more,
                total_count,
                estimated_tokens,
            };
            Response::ok_typed(req.id, &result)
        }
        Ok(Err(err)) => Response::err(req.id, errors::HISTORY_GET_FAILED, err),
        Err(err) => Response::err(req.id, errors::HISTORY_GET_FAILED, err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::common::test_support::scratch_root;
    use crate::transport::OutChannel;
    use tauqe_protocol::{methods, RequestId, UiHistoryKind};

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            Self(scratch_root("history"))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_state(dir: &TestDir) -> Arc<AppState> {
        let (out, _rx) = OutChannel::new();
        let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
        AppState::new(
            tauqe_core::config::AppConfig::default(),
            dir.path().to_path_buf(),
            hist_tx,
            out,
        )
    }

    fn get_req(id: u64, limit: Option<usize>, before_id: Option<u64>) -> Request {
        let mut params = serde_json::Map::new();
        if let Some(limit) = limit {
            params.insert("limit".to_string(), serde_json::json!(limit));
        }
        if let Some(before_id) = before_id {
            params.insert("before_id".to_string(), serde_json::json!(before_id));
        }
        Request {
            id: RequestId::Number(id),
            method: methods::HISTORY_GET.to_string(),
            params: Some(serde_json::Value::Object(params)),
        }
    }

    /// Builds a session of eight entries, then compacts the summarized prefix
    /// while retaining the last two turns as the tail. The resulting on-disk
    /// session is a synthesized Summary followed by tail entries with ids
    /// `[5, 6, 7, 8]`; the Summary receives id `4`.
    async fn compacted_state(dir: &TestDir) -> Arc<AppState> {
        let state = test_state(dir);
        {
            let mut history = state.history.lock().await;
            for i in 1..=4 {
                history.record_turn(format!("Task {}", i), vec![]).unwrap();
                history
                    .record_response(format!("Done {}", i), None, None, vec![])
                    .unwrap();
            }
            let (_, tail) = history.prepare_compaction(2).unwrap().unwrap();
            history
                .apply_compaction("Earlier tasks summarized.", tail)
                .unwrap();
        }
        state
    }

    fn parse_result(res: &Response) -> HistoryGetResult {
        let value = res.result.clone().expect("response must carry a result");
        serde_json::from_value(value).expect("history/get result must deserialize")
    }

    #[tokio::test]
    async fn test_history_get_returns_summary_first_after_compaction() {
        let dir = TestDir::new();
        let state = compacted_state(&dir).await;

        let res = handle_history_get(get_req(1, Some(10), None), &state).await;
        assert!(res.error.is_none(), "unexpected error: {:?}", res.error);
        let result = parse_result(&res);

        // One synthesized Summary plus the four retained tail entries.
        assert_eq!(result.total_count, 5);
        assert!(!result.has_more);
        assert_eq!(result.items.len(), 5);

        // The Summary sits strictly at index zero, ahead of the retained tail.
        let summary = &result.items[0];
        assert_eq!(summary.kind, UiHistoryKind::System);
        assert_eq!(summary.text, "Earlier tasks summarized.");

        assert!(
            result.estimated_tokens > 0,
            "a compacted session must report a non-zero token footprint"
        );
    }

    #[tokio::test]
    async fn test_history_get_pagination_reaches_summary_at_history_start() {
        let dir = TestDir::new();
        let state = compacted_state(&dir).await;

        // The newest page never reaches the Summary.
        let res = handle_history_get(get_req(1, Some(2), None), &state).await;
        let result = parse_result(&res);
        assert_eq!(result.total_count, 5);
        assert!(result.has_more);
        let newest_ids: Vec<u64> = result.items.iter().map(|i| i.id).collect();
        assert_eq!(newest_ids, vec![7, 8]);
        assert!(
            result.items.iter().all(|i| i.kind != UiHistoryKind::System),
            "the newest page must not surface the Summary"
        );

        // Walking back to the beginning surfaces the Summary as the first item.
        let res = handle_history_get(get_req(2, Some(2), Some(5)), &state).await;
        let result = parse_result(&res);
        assert!(!result.has_more);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].kind, UiHistoryKind::System);
        assert_eq!(result.items[0].text, "Earlier tasks summarized.");

        // An intermediate page stays within the retained tail and omits the
        // Summary entirely.
        let res = handle_history_get(get_req(3, Some(2), Some(7)), &state).await;
        let result = parse_result(&res);
        assert!(result.has_more);
        let middle_ids: Vec<u64> = result.items.iter().map(|i| i.id).collect();
        assert_eq!(middle_ids, vec![5, 6]);
        assert!(
            result.items.iter().all(|i| i.kind != UiHistoryKind::System),
            "an intermediate page must not surface the Summary"
        );
    }

    #[tokio::test]
    async fn test_history_get_ids_are_monotonically_increasing_after_compaction() {
        let dir = TestDir::new();
        let state = compacted_state(&dir).await;

        let res = handle_history_get(get_req(1, Some(100), None), &state).await;
        let result = parse_result(&res);

        let ids: Vec<u64> = result.items.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![4, 5, 6, 7, 8]);
        for pair in ids.windows(2) {
            assert!(
                pair[0] < pair[1],
                "ids must increase monotonically, got {:?}",
                ids
            );
        }
    }
    /// A malformed pagination field must be refused instead of silently paging
    /// with defaults: the client asked for a window it will never receive.
    #[tokio::test]
    async fn test_history_get_rejects_malformed_payloads() {
        let dir = TestDir::new();
        let state = test_state(&dir);
        {
            let mut history = state.history.lock().await;
            history.record_turn("Task".to_string(), Vec::new()).unwrap();
        }

        for payload in [
            serde_json::json!({ "limit": "ten" }),
            serde_json::json!({ "limit": -1 }),
            serde_json::json!({ "before_id": "later" }),
        ] {
            let req = Request {
                id: RequestId::Number(9),
                method: methods::HISTORY_GET.to_string(),
                params: Some(payload.clone()),
            };
            let res = handle_history_get(req, &state).await;
            let error = res
                .error
                .unwrap_or_else(|| panic!("the malformed payload {payload} was accepted"));
            assert_eq!(error.code, errors::INVALID_PARAMS, "{payload}");
        }

        // An absent payload keeps its documented meaning: the newest page.
        let res = handle_history_get(
            Request {
                id: RequestId::Number(10),
                method: methods::HISTORY_GET.to_string(),
                params: None,
            },
            &state,
        )
        .await;
        assert!(res.error.is_none(), "unexpected error: {:?}", res.error);
        let result = parse_result(&res);
        assert_eq!(result.total_count, 1);
        assert_eq!(result.items.len(), 1);
        assert!(!result.has_more);
    }
}
