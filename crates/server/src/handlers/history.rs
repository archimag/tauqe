use std::sync::Arc;

use tauqe_protocol::{errors, HistoryGetParams, HistoryGetResult, Request, Response};

use crate::state::AppState;

pub async fn handle_history_get(req: Request, state: &Arc<AppState>) -> Response {
    let params: HistoryGetParams = req
        .params
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();
    let limit = params.limit.unwrap_or(10);
    let before_id = params.before_id;

    // Both the token estimate and the page slice touch `history.jsonl`, so the
    // read runs on the blocking pool while the exclusive history guard is held,
    // keeping it serialized against concurrent appends and compactions.
    let loaded = crate::handlers::with_history_mutation(state, move |history| {
        let estimated_tokens = history.estimated_tokens().unwrap_or(0);
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
    use crate::transport::OutChannel;
    use tauqe_protocol::{methods, RequestId, UiHistoryKind};

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tauqe-test-history-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::create_dir_all(&path);
            Self(path)
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
}
