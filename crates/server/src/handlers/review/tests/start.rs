//! `review/start`: streamed synthesis, cancellation, and attempt withdrawal.

use std::sync::Arc;

use super::super::{handle_review_cancel, handle_review_start};
use super::{InterruptedAttemptProvider, TestDir};

use crate::state::AppState;
use crate::transport::OutChannel;
use tauqe_core::providers::mock::{MockLlmProvider, MockResponse};
use tauqe_core::providers::{ChatMessage, LlmProvider, ResponseFormat, StreamEvent};
use tauqe_core::review::ReviewStorage;
use tauqe_protocol::{events, ContextAccess, Request};
use tokio::sync::{mpsc, watch};

#[tokio::test]
async fn test_handle_review_start_defaults_to_reviewer_role() {
    let dir = TestDir::new();
    let target_file = dir.path().join("file.rs");
    std::fs::write(&target_file, "fn foo() {}\n").unwrap();

    let mut config = tauqe_core::config::AppConfig::default();
    config.models.reviewer = Some(vec![tauqe_protocol::ModelRef::openrouter(
        "test/reviewer-model",
    )
    .into()]);
    config.models.primary = vec![tauqe_protocol::ModelRef::openrouter("test/primary-model").into()];

    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(config, dir.path().to_path_buf(), hist_tx, out);

    state
        .context
        .write()
        .await
        .add_file("file.rs", ContextAccess::ReadOnly)
        .unwrap();

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_START.to_string(),
        params: None,
    };
    let res = handle_review_start(req, &state).await;
    let err = res.error.expect("should fail without api key");
    assert_eq!(err.code, "NO_API_KEY");
}

#[tokio::test]
async fn test_review_start_streams_clean_summary_without_json_leakage() {
    let dir = TestDir::new();
    let target_file = dir.path().join("file.rs");
    std::fs::write(&target_file, "fn foo() {}\n").unwrap();

    let payload = r#"{"summary":"The architecture is sound, but locking is racy.","findings":[{"title":"Race condition","severity":"critical","file_path":"file.rs","line_start":1,"line_end":2,"body":"Lock ordering is inconsistent."}]}"#;
    let provider: Arc<dyn LlmProvider> =
        Arc::new(MockLlmProvider::scripted([MockResponse::text(payload)]));

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );
    *state.provider_override.lock().await = Some(provider);

    state
        .context
        .write()
        .await
        .add_file("file.rs", ContextAccess::ReadOnly)
        .unwrap();

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_START.to_string(),
        params: None,
    };
    let res = handle_review_start(req, &state).await;
    assert!(res.error.is_none(), "review start must succeed");

    // Drain streamed events until the review terminates.
    let mut lines = Vec::new();
    while let Ok(Some(line)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
    {
        let terminal =
            line.contains(events::REVIEW_FINISHED) || line.contains(events::REVIEW_ERROR);
        lines.push(line);
        if terminal {
            break;
        }
    }

    // Only cleaned summary deltas reach the client: raw JSON syntax never
    // leaks into the content stream.
    let mut streamed = String::new();
    for line in &lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if val.get("method").and_then(|m| m.as_str()) == Some(events::REVIEW_CONTENT_DELTA) {
                if let Some(delta) = val
                    .get("params")
                    .and_then(|p| p.get("delta"))
                    .and_then(|d| d.as_str())
                {
                    streamed.push_str(delta);
                }
            }
        }
    }
    assert_eq!(streamed, "The architecture is sound, but locking is racy.");
    assert!(!streamed.contains('{'));
    assert!(!streamed.contains('"'));
    assert!(!streamed.contains("summary"));
    assert!(!streamed.contains("findings"));

    // The summary is persisted as the session description and the findings
    // are stored structurally.
    let session = ReviewStorage::load_latest(dir.path())
        .unwrap()
        .expect("review session must be saved");
    assert_eq!(
        session.description.as_deref(),
        Some("The architecture is sound, but locking is racy.")
    );
    assert_eq!(session.items.len(), 1);
    let issues = session.issues();
    assert_eq!(issues[0].issue_type, tauqe_protocol::IssueType::Bug);
}

#[tokio::test]
async fn test_handle_review_cancel_soft_keeps_operation_active() {
    let dir = TestDir::new();
    let (out, _rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );

    let operation = state.try_begin_operation().expect("register operation");
    let soft_rx = operation.soft_stop_receiver();
    let cancel_rx = operation.cancel_receiver();
    assert!(!*soft_rx.borrow());
    assert!(!*cancel_rx.borrow());

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_CANCEL.to_string(),
        params: Some(serde_json::json!({ "mode": "soft" })),
    };
    let res = handle_review_cancel(req, &state).await;
    assert!(res.error.is_none());

    assert!(*soft_rx.borrow());
    assert!(!*cancel_rx.borrow());
    assert!(state.is_operation_active());
}

/// A provider that reports cancellation exclusively through an explicit
/// `StreamEvent::Cancelled`, without the turn's cancel signal ever being
/// raised. This reproduces the race the review handler must tolerate:
/// cancellation reported by the stream event alone.
struct StreamCancellingProvider;

#[async_trait::async_trait]
impl LlmProvider for StreamCancellingProvider {
    fn kind(&self) -> tauqe_protocol::ProviderKind {
        tauqe_protocol::ProviderKind::OpenRouter
    }

    async fn stream_chat(
        &self,
        _model: &str,
        _messages: Vec<ChatMessage>,
        _response_format: Option<ResponseFormat>,
        tx: mpsc::Sender<StreamEvent>,
        _cancel_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let _ = tx.send(StreamEvent::Cancelled).await;
        Ok(())
    }
}

#[tokio::test]
async fn test_review_start_surfaces_stream_cancelled_event() {
    let dir = TestDir::new();
    let target_file = dir.path().join("file.rs");
    std::fs::write(&target_file, "fn foo() {}\n").unwrap();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );
    *state.provider_override.lock().await = Some(Arc::new(StreamCancellingProvider));

    state
        .context
        .write()
        .await
        .add_file("file.rs", ContextAccess::ReadOnly)
        .unwrap();

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_START.to_string(),
        params: None,
    };
    let res = handle_review_start(req, &state).await;
    assert!(
        res.error.is_none(),
        "review start must acknowledge the operation"
    );

    // An explicit `Cancelled` stream event must surface as `review/cancelled`,
    // never as a truncated-parse error or an empty-review error.
    let mut saw_cancelled = false;
    let mut saw_error = false;
    while let Ok(Some(line)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
    {
        if line.contains(events::REVIEW_CANCELLED) {
            saw_cancelled = true;
        }
        if line.contains(events::REVIEW_ERROR) {
            saw_error = true;
        }
        if saw_cancelled || saw_error {
            break;
        }
    }

    assert!(
        saw_cancelled,
        "an explicit Cancelled event must surface as review/cancelled"
    );
    assert!(
        !saw_error,
        "cancellation must not be misreported as a review error"
    );
}

/// A mid-stream transport break must withdraw the aborted review synthesis
/// attempt instead of splicing the retried summary onto its prefix.
#[tokio::test]
async fn test_review_start_withdraws_the_aborted_attempt_before_the_retry() {
    let dir = TestDir::new();
    std::fs::write(dir.path().join("file.rs"), "fn foo() {}\n").unwrap();

    let provider = InterruptedAttemptProvider::new(
        r#"{"summary":"The architecture is sound"#,
        r#"{"summary":"The architecture is sound, but locking is racy.","findings":[]}"#,
    )
    .retrying();

    let (out, mut rx) = OutChannel::new();
    let (hist_tx, _hist_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = AppState::new(
        tauqe_core::config::AppConfig::default(),
        dir.path().to_path_buf(),
        hist_tx,
        out,
    );
    *state.provider_override.lock().await = Some(provider);

    state
        .context
        .write()
        .await
        .add_file("file.rs", ContextAccess::ReadOnly)
        .unwrap();

    let req = Request {
        id: tauqe_protocol::RequestId::Number(1),
        method: tauqe_protocol::methods::REVIEW_START.to_string(),
        params: None,
    };
    let res = handle_review_start(req, &state).await;
    assert!(
        res.error.is_none(),
        "review start must acknowledge the operation"
    );

    // The client mirrors the daemon: a withdrawn attempt is dropped, so the
    // summary it renders is exactly the retried payload and never the aborted
    // prefix with the retried document concatenated behind it.
    let mut summary = String::new();
    let mut resets = 0usize;
    let mut finished = false;
    while let Ok(Some(line)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match value.get("method").and_then(|m| m.as_str()) {
            Some(events::REVIEW_CONTENT_DELTA) => {
                if let Some(delta) = value.pointer("/params/delta").and_then(|d| d.as_str()) {
                    summary.push_str(delta);
                }
            }
            Some(events::MODEL_STREAM_RESET) => {
                resets += 1;
                summary.clear();
            }
            Some(events::REVIEW_FINISHED) => {
                finished = true;
                break;
            }
            Some(events::REVIEW_ERROR) => break,
            _ => {}
        }
    }

    assert!(
        finished,
        "the retried payload must parse instead of failing as a corrupted document"
    );
    assert_eq!(
        resets, 1,
        "the withdrawn attempt must be announced exactly once"
    );
    assert_eq!(summary, "The architecture is sound, but locking is racy.");

    let session = ReviewStorage::load_latest(dir.path())
        .unwrap()
        .expect("review session must be saved");
    assert_eq!(
        session.description.as_deref(),
        Some("The architecture is sound, but locking is racy.")
    );
}
