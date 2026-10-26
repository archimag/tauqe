use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use tauqe_protocol::{ModelUsageInfo, ProviderKind};
use tokio::sync::{mpsc, watch};

use super::retry::JSON_TRUNCATION_MARKER;
use super::types::{ChatMessage, ReasoningEffort, ResponseFormat, StreamEvent};
use super::LlmProvider;

use self::sse::{
    find_sse_event_boundary, log_stream_completion, truncate_response_body, MAX_SSE_EVENT_BYTES,
};
use self::types::{
    ChatCompletionRequest, GenerationResponse, ProviderPreferences, ReasoningOption, StreamOptions,
    StreamingChunk,
};

mod sse;
pub mod types;

pub use self::types::GenerationStats;

/// Maximum time to establish a connection to OpenRouter.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Maximum idle time between reads on an open stream. Bounds a stalled
/// provider so a hung connection cannot block the turn indefinitely.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub struct OpenRouterProvider {
    api_key: String,
    base_url: String,
    http_client: reqwest::Client,
}

impl OpenRouterProvider {
    pub fn new(api_key: String) -> Self {
        let http_client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .unwrap_or_else(|err| {
                tracing::warn!("Failed to build HTTP client with timeouts: {}", err);
                reqwest::Client::new()
            });
        Self {
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            http_client,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub async fn get_generation_stats(
        &self,
        generation_id: &str,
    ) -> Result<Option<GenerationStats>> {
        let url = format!(
            "{}/generation?id={}",
            self.base_url.trim_end_matches('/'),
            generation_id
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key.trim()))?,
        );

        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
            }

            let resp = match self
                .http_client
                .get(&url)
                .headers(headers.clone())
                .send()
                .await
            {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!("OpenRouter generation stats request failed: {}", err);
                    continue;
                }
            };

            let status = resp.status();
            if status.is_client_error() {
                return Ok(None);
            }

            if status.is_success() {
                if let Ok(gen_resp) = resp.json::<GenerationResponse>().await {
                    if let Some(data) = gen_resp.data {
                        if data.total_cost.is_some() {
                            return Ok(Some(GenerationStats {
                                total_cost: data.total_cost,
                                prompt_tokens: data.tokens_prompt.or(data.native_tokens_prompt),
                                completion_tokens: data
                                    .tokens_completion
                                    .or(data.native_tokens_completion),
                            }));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_stream(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        reasoning_effort: Option<ReasoningEffort>,
        session_id: Option<&str>,
        tx: mpsc::Sender<StreamEvent>,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key.trim()))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "HTTP-Referer",
            HeaderValue::from_static("https://github.com/tauqe/tauqe"),
        );
        headers.insert("X-Title", HeaderValue::from_static("Tauqe"));

        if *cancel_rx.borrow() {
            let _ = tx.send(StreamEvent::Cancelled).await;
            return Ok(());
        }

        // A structured response is one JSON document, so a generation stopped at
        // the token limit leaves it unparseable.
        let expects_json = matches!(
            &response_format,
            Some(ResponseFormat::JsonObject | ResponseFormat::JsonSchema { .. })
        );
        let uses_json_schema = matches!(&response_format, Some(ResponseFormat::JsonSchema { .. }));

        tracing::info!(
            model = %model,
            effort = ?reasoning_effort,
            session_id = session_id.unwrap_or("none"),
            uses_json_schema,
            messages_count = messages.len(),
            "openrouter request dispatched"
        );

        let payload = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            stream: true,
            session_id: session_id.map(str::to_string),
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            reasoning: Some(ReasoningOption {
                enabled: true,
                effort: reasoning_effort,
            }),
            provider: if uses_json_schema {
                Some(ProviderPreferences {
                    require_parameters: true,
                })
            } else {
                None
            },
            response_format,
        };

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = tokio::select! {
            // The `wait_for` future yields a `watch::Ref` guard, which is not
            // `Send`. Scope it inside an async block that drops the guard
            // before the branch body awaits, so the whole turn future stays
            // `Send`.
            //
            // A dropped cancellation sender surfaces as `Err`: no signal can
            // ever arrive again, so the branch parks instead of firing and
            // the request proceeds without a cancellation branch, applying
            // the same closure policy as `retry::wait_with_countdown`.
            _ = async {
                if cancel_rx.wait_for(|cancelled| *cancelled).await.is_err() {
                    std::future::pending::<()>().await;
                }
            } => {
                let _ = tx.send(StreamEvent::Cancelled).await;
                return Ok(());
            }
            res = self
                .http_client
                .post(&url)
                .headers(headers)
                .json(&payload)
                .send() => {
                if let Err(err) = &res {
                    tracing::warn!(
                        is_timeout = err.is_timeout(),
                        is_connect = err.is_connect(),
                        error = %err,
                        "openrouter request failed"
                    );
                }
                res.context("Failed to connect to OpenRouter API")?
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let err_text = response.text().await.unwrap_or_default();
            tracing::error!(
                status_code = status.as_u16(),
                response = %truncate_response_body(&err_text),
                "openrouter http error"
            );
            bail!("OpenRouter HTTP error {}: {}", status, err_text);
        }

        let mut byte_stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        let mut finished = false;
        let mut got_done_marker = false;
        let mut generation_id: Option<String> = None;
        let mut latest_usage: Option<ModelUsageInfo> = None;
        // Set once the cancellation sender is dropped: no further signal
        // can ever arrive, so the cancellation branch is disabled for the
        // rest of the stream instead of re-polling a future that stays
        // permanently ready. This mirrors the closure policy of
        // `retry::wait_with_countdown`.
        let mut cancel_closed = false;

        while !finished {
            if tx.is_closed() {
                return Ok(());
            }
            tokio::select! {
                changed = cancel_rx.changed(), if !cancel_closed => {
                    match changed {
                        Ok(()) => {
                            if *cancel_rx.borrow() {
                                let _ = tx.send(StreamEvent::Cancelled).await;
                                return Ok(());
                            }
                        }
                        // The cancellation sender is gone, so the branch is
                        // disabled instead of spinning on a permanently ready
                        // future.
                        Err(_) => cancel_closed = true,
                    }
                }
                chunk_res = byte_stream.next() => {
                    match chunk_res {
                        Some(Ok(chunk)) => {
                            let pending = buffer.len();
                            if pending + chunk.len() > MAX_SSE_EVENT_BYTES {
                                let msg = format!(
                                    "OpenRouter SSE event exceeded {} bytes without an event terminator",
                                    MAX_SSE_EVENT_BYTES
                                );
                                tracing::error!(
                                    pending_bytes = pending,
                                    chunk_bytes = chunk.len(),
                                    limit_bytes = MAX_SSE_EVENT_BYTES,
                                    "openrouter sse buffer cap exceeded"
                                );
                                let _ = tx.send(StreamEvent::Error(msg.clone())).await;
                                bail!("{}", msg);
                            }

                            buffer.extend_from_slice(&chunk);

                            // Resume scanning at the chunk seam. The longest
                            // delimiter is `\r\n\r\n` (4 bytes), so reaching
                            // back 3 bytes is enough to catch a boundary split
                            // across two chunks.
                            let mut search_from = pending.saturating_sub(3);

                            while let Some((pos, delim_len)) =
                                find_sse_event_boundary(&buffer, search_from)
                            {
                                let event_bytes = buffer[..pos].to_vec();
                                buffer.drain(..pos + delim_len);
                                search_from = 0;

                                let event_block = String::from_utf8_lossy(&event_bytes);

                                for line in event_block.lines() {
                                    let line = line.trim();
                                    if let Some(payload_str) = line.strip_prefix("data:") {
                                        let payload_str = payload_str.trim();
                                        if payload_str == "[DONE]" {
                                            got_done_marker = true;
                                            finished = true;
                                            break;
                                        }
                                        if payload_str.is_empty() {
                                            continue;
                                        }

                                        let chunk_result =
                                            serde_json::from_str::<StreamingChunk>(payload_str);
                                        if let Err(err) = &chunk_result {
                                            tracing::warn!(error = %err, "openrouter sse parse error");
                                        }
                                        if let Ok(chunk) = chunk_result {
                                            if let Some(id) = chunk.id {
                                                if !id.is_empty() {
                                                    generation_id = Some(id);
                                                }
                                            }

                                            if let Some(err) = chunk.error {
                                                tracing::error!(error = %err.message, "openrouter stream error");
                                                let _ = tx.send(StreamEvent::Error(err.message.clone())).await;
                                                bail!("OpenRouter streaming error: {}", err.message);
                                            }

                                            if let Some(usage) = chunk.usage {
                                                let cost = usage
                                                    .cost
                                                    .or(usage.total_cost)
                                                    .or(chunk.cost)
                                                    .or(chunk.total_cost);
                                                let usage_info = ModelUsageInfo {
                                                    prompt_tokens: usage.prompt_tokens,
                                                    completion_tokens: usage.completion_tokens,
                                                    total_tokens: usage.total_tokens,
                                                    reasoning_tokens: usage
                                                        .completion_tokens_details
                                                        .and_then(|d| d.reasoning_tokens),
                                                    cached_tokens: usage
                                                        .prompt_tokens_details
                                                        .and_then(|d| d.cached_tokens),
                                                    cost,
                                                };
                                                latest_usage = Some(usage_info.clone());
                                                let _ = tx.send(StreamEvent::Usage(usage_info)).await;
                                            } else if chunk.cost.is_some() || chunk.total_cost.is_some() {
                                                let cost = chunk.cost.or(chunk.total_cost);
                                                let usage_info = ModelUsageInfo {
                                                    prompt_tokens: 0,
                                                    completion_tokens: 0,
                                                    total_tokens: 0,
                                                    reasoning_tokens: None,
                                                    cached_tokens: None,
                                                    cost,
                                                };
                                                latest_usage = Some(usage_info.clone());
                                                let _ = tx.send(StreamEvent::Usage(usage_info)).await;
                                            }

                                            for choice in chunk.choices {
                                                if let Some(err) = choice.error {
                                                    tracing::error!(error = %err.message, "openrouter stream choice error");
                                                    let _ = tx.send(StreamEvent::Error(err.message.clone())).await;
                                                    bail!("OpenRouter streaming choice error: {}", err.message);
                                                }
                                                if choice.finish_reason.as_deref() == Some("error") {
                                                    tracing::error!(
                                                        finish_reason = "error",
                                                        "openrouter generation error"
                                                    );
                                                    let msg = "Generation ended with finish_reason: error".to_string();
                                                    let _ = tx
                                                        .send(StreamEvent::Error(msg.clone()))
                                                        .await;
                                                    bail!("OpenRouter error: {}", msg);
                                                }
                                                if choice.finish_reason.as_deref() == Some("length")
                                                    && expects_json
                                                {
                                                    tracing::error!(
                                                        finish_reason = "length",
                                                        "openrouter structured output truncated"
                                                    );
                                                    let msg = format!(
                                                        "OpenRouter structured JSON response {}",
                                                        JSON_TRUNCATION_MARKER
                                                    );
                                                    let _ = tx
                                                        .send(StreamEvent::Error(msg.clone()))
                                                        .await;
                                                    bail!("{}", msg);
                                                }

                                                if let Some(delta) = choice.delta {
                                                    if let Some(content) = delta.content {
                                                        if !content.is_empty() {
                                                            let _ = tx.send(StreamEvent::TextDelta(content)).await;
                                                        }
                                                    }
                                                    if let Some(reasoning) = delta.reasoning {
                                                        if !reasoning.is_empty() {
                                                            let _ = tx.send(StreamEvent::ReasoningDelta(reasoning)).await;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some(Err(err)) => {
                            tracing::warn!(error = %err, "openrouter stream read error");
                            let _ = tx.send(StreamEvent::Error(err.to_string())).await;
                            bail!("OpenRouter stream read error: {}", err);
                        }
                        None => {
                            finished = true;
                        }
                    }
                }
            }
        }

        if tx.is_closed() {
            return Ok(());
        }

        if !got_done_marker {
            tracing::warn!("openrouter stream ended prematurely without [DONE] marker");
            let msg = "OpenRouter stream ended prematurely without [DONE] marker".to_string();
            let _ = tx.send(StreamEvent::Error(msg.clone())).await;
            bail!("{}", msg);
        }

        if let Some(gen_id) = generation_id {
            let needs_cost = latest_usage.as_ref().is_none_or(|u| u.cost.is_none());
            if needs_cost {
                if let Ok(Some(stats)) = self.get_generation_stats(&gen_id).await {
                    let mut usage = latest_usage.clone().unwrap_or_default();
                    if usage.cost.is_none() {
                        usage.cost = stats.total_cost;
                    }
                    if usage.prompt_tokens == 0 {
                        if let Some(pt) = stats.prompt_tokens {
                            usage.prompt_tokens = pt;
                        }
                    }
                    if usage.completion_tokens == 0 {
                        if let Some(ct) = stats.completion_tokens {
                            usage.completion_tokens = ct;
                        }
                    }
                    if usage.total_tokens == 0 {
                        usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
                    }
                    latest_usage = Some(usage.clone());
                    let _ = tx.send(StreamEvent::Usage(usage)).await;
                }
            }
        }

        log_stream_completion(model, &latest_usage);

        let _ = tx.send(StreamEvent::Done).await;
        Ok(())
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    async fn stream_chat(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        self.execute_stream(model, messages, response_format, None, None, tx, cancel_rx)
            .await
    }

    async fn stream_chat_with_effort(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        reasoning_effort: Option<ReasoningEffort>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        self.execute_stream(
            model,
            messages,
            response_format,
            reasoning_effort,
            None,
            tx,
            cancel_rx,
        )
        .await
    }

    async fn stream_chat_with_session(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        reasoning_effort: Option<ReasoningEffort>,
        session_id: Option<&str>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        self.execute_stream(
            model,
            messages,
            response_format,
            reasoning_effort,
            session_id,
            tx,
            cancel_rx,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::sse::{
        find_sse_event_boundary, truncate_response_body, MAX_SSE_EVENT_BYTES,
        RESPONSE_BODY_LOG_CHARS,
    };
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_stream_chat_success() {
        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello, \"}}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"world!\"}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n\
                        data: [DONE]\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("Authorization", "Bearer test-api-key"))
            .and(header("content-type", "application/json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let messages = vec![ChatMessage::user("Ping")];

        let provider_task = tokio::spawn(async move {
            provider
                .stream_text("test-model", messages, tx, cancel_rx)
                .await
        });

        let mut text_chunks = Vec::new();
        let mut received_usage = None;
        let mut got_done = false;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta(delta) => text_chunks.push(delta),
                StreamEvent::Usage(usage) => received_usage = Some(usage),
                StreamEvent::Done => got_done = true,
                _ => {}
            }
        }

        let res = provider_task.await.expect("task join failed");
        assert!(res.is_ok());

        assert_eq!(text_chunks.concat(), "Hello, world!");
        assert!(got_done);

        let usage = received_usage.expect("Usage event expected");
        assert_eq!(usage.prompt_tokens, 5);
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(usage.total_tokens, 7);
    }

    #[tokio::test]
    async fn test_stream_chat_fetches_generation_cost() {
        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"id\":\"gen-test-xyz\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
                        data: {\"id\":\"gen-test-xyz\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n\
                        data: [DONE]\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let gen_stats_body = r#"{"data":{"id":"gen-test-xyz","total_cost":0.00045,"tokens_prompt":10,"tokens_completion":5}}"#;

        Mock::given(method("GET"))
            .and(path("/generation"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(gen_stats_body),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let messages = vec![ChatMessage::user("Hi")];

        let provider_task = tokio::spawn(async move {
            provider
                .stream_text("test-model", messages, tx, cancel_rx)
                .await
        });

        let mut received_usage = None;
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Usage(usage) = event {
                received_usage = Some(usage);
            }
        }

        let res = provider_task.await.expect("task join failed");
        assert!(res.is_ok());

        let usage = received_usage.expect("Usage event with cost expected");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.cost, Some(0.00045));
    }

    #[tokio::test]
    async fn test_stream_chat_json_schema_requires_parameters() {
        use super::super::types::JsonSchemaDefinition;

        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"{}\"}}]}\n\ndata: [DONE]\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let response_format = Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name: "model_result".to_string(),
                description: None,
                schema: serde_json::json!({"type": "object"}),
                strict: Some(true),
            },
        });

        let provider_task = tokio::spawn(async move {
            provider
                .stream_chat("test-model", vec![], response_format, tx, cancel_rx)
                .await
        });

        while let Some(_event) = rx.recv().await {}
        assert!(provider_task.await.expect("task join failed").is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let req_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(req_json["provider"]["require_parameters"], true);
        assert_eq!(req_json["response_format"]["type"], "json_schema");
        assert_eq!(req_json["reasoning"]["enabled"], true);
        assert!(req_json.get("tools").is_none());
        assert!(req_json.get("tool_choice").is_none());
        assert!(req_json.get("parallel_tool_calls").is_none());
    }

    #[tokio::test]
    async fn test_stream_chat_with_effort_translates_reasoning_effort() {
        use super::super::types::JsonSchemaDefinition;

        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let response_format = Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name: "model_result".to_string(),
                description: None,
                schema: serde_json::json!({"type": "object"}),
                strict: Some(true),
            },
        });

        let provider_task = tokio::spawn(async move {
            provider
                .stream_chat_with_effort(
                    "test-model",
                    vec![],
                    response_format,
                    Some(ReasoningEffort::XHigh),
                    tx,
                    cancel_rx,
                )
                .await
        });

        let mut got_done = false;
        while let Some(event) = rx.recv().await {
            if matches!(event, StreamEvent::Done) {
                got_done = true;
            }
        }
        assert!(provider_task.await.expect("task join failed").is_ok());
        assert!(got_done, "stream should terminate with a Done event");

        let requests = mock_server.received_requests().await.unwrap();
        let req_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(req_json["reasoning"]["enabled"], true);
        assert_eq!(req_json["reasoning"]["effort"], "xhigh");
        assert_eq!(req_json["provider"]["require_parameters"], true);
    }

    async fn mount_stream_mock(mock_server: &MockServer) {
        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(mock_server)
            .await;
    }

    #[tokio::test]
    async fn test_stream_chat_serializes_session_id() {
        let mock_server = MockServer::start().await;
        mount_stream_mock(&mock_server).await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        provider
            .stream_chat_with_session(
                "test-model",
                vec![],
                None,
                None,
                Some("tauqe-plan-test-123"),
                tx,
                cancel_rx,
            )
            .await
            .expect("session stream should succeed");
        while rx.recv().await.is_some() {}

        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["session_id"], "tauqe-plan-test-123");
    }

    #[tokio::test]
    async fn test_stream_chat_omits_session_id_when_none() {
        let mock_server = MockServer::start().await;
        mount_stream_mock(&mock_server).await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await
            .expect("plain stream should succeed");
        while rx.recv().await.is_some() {}

        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(body.get("session_id").is_none());
    }

    #[test]
    fn test_find_sse_event_boundary() {
        assert_eq!(find_sse_event_boundary(b"", 0), None);
        assert_eq!(find_sse_event_boundary(b"abc", 0), None);
        assert_eq!(find_sse_event_boundary(b"abc\n\ndef", 0), Some((3, 2)));
        assert_eq!(find_sse_event_boundary(b"abc\r\n\r\ndef", 0), Some((3, 4)));
        assert_eq!(
            find_sse_event_boundary(b"abc\r\n\r\ndef\n\n", 0),
            Some((3, 4))
        );
        assert_eq!(
            find_sse_event_boundary(b"abc\n\ndef\r\n\r\n", 0),
            Some((3, 2))
        );
        assert_eq!(find_sse_event_boundary(b"abc\n\ndef", 1), Some((3, 2)));
        assert_eq!(find_sse_event_boundary(b"abc\n\ndef", 4), None);
        assert_eq!(find_sse_event_boundary(b"abc\n\ndef", 99), None);
    }

    #[test]
    fn test_multibyte_utf8_split_across_chunks() {
        let mut buffer = Vec::new();
        let text = "data: {\"choices\":[{\"delta\":{\"content\":\"Привет\"}}]}\n\n";
        let bytes = text.as_bytes();
        let mid = bytes.len() / 2;
        buffer.extend_from_slice(&bytes[..mid]);
        assert_eq!(find_sse_event_boundary(&buffer, 0), None);
        buffer.extend_from_slice(&bytes[mid..]);
        let resume_from = mid.saturating_sub(3);
        let (pos, len) = find_sse_event_boundary(&buffer, resume_from).expect("found boundary");
        assert_eq!(pos, bytes.len() - 2);
        assert_eq!(len, 2);
        let event_str = String::from_utf8(buffer[..pos].to_vec()).expect("valid utf-8");
        assert!(event_str.contains("Привет"));
    }

    #[tokio::test]
    async fn test_stream_chat_crlf_and_multibyte_utf8() {
        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Привет, \"}}]}\r\n\r\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"мир!\"}}]}\r\n\r\n\
                        data: [DONE]\r\n\r\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let messages = vec![ChatMessage::user("Ping")];

        let provider_task = tokio::spawn(async move {
            provider
                .stream_text("test-model", messages, tx, cancel_rx)
                .await
        });

        let mut text_chunks = Vec::new();
        while let Some(event) = rx.recv().await {
            if let StreamEvent::TextDelta(delta) = event {
                text_chunks.push(delta);
            }
        }

        let res = provider_task.await.expect("task join failed");
        assert!(res.is_ok());
        assert_eq!(text_chunks.concat(), "Привет, мир!");
    }

    #[tokio::test]
    async fn test_stream_chat_chunk_error_returns_err() {
        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"error\":{\"message\":\"Rate limit reached\"}}\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let res = provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await;

        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("Rate limit reached"));

        let mut saw_error_event = false;
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Error(msg) = event {
                if msg.contains("Rate limit reached") {
                    saw_error_event = true;
                }
            }
        }
        assert!(saw_error_event);
    }

    #[tokio::test]
    async fn test_stream_chat_premature_eof_without_done_returns_err() {
        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Truncated\"}}]}\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let res = provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await;

        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .to_string()
            .contains("without [DONE] marker"));

        let mut saw_error_event = false;
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Error(msg) = event {
                if msg.contains("without [DONE] marker") {
                    saw_error_event = true;
                }
            }
        }
        assert!(saw_error_event);
    }

    #[tokio::test]
    async fn test_stream_chat_cancels_pending_http_request() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(60))
                    .set_body_string("data: [DONE]\n\n"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let provider_task = tokio::spawn(async move {
            provider
                .stream_text("test-model", vec![], tx, cancel_rx)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel_tx.send(true).expect("failed to send cancel signal");

        let res = tokio::time::timeout(std::time::Duration::from_secs(2), provider_task)
            .await
            .expect("cancellation should resolve promptly")
            .expect("task join failed");

        assert!(res.is_ok());

        let event = rx.recv().await.expect("expected stream event");
        assert!(matches!(event, StreamEvent::Cancelled));
    }

    #[tokio::test]
    async fn test_stream_chat_pre_cancelled() {
        let mock_server = MockServer::start().await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(true);

        let res = provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await;

        assert!(res.is_ok());

        let event = rx.recv().await.expect("expected stream event");
        assert!(matches!(event, StreamEvent::Cancelled));
        assert_eq!(mock_server.received_requests().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_stream_chat_dropped_cancel_sender_before_headers_proceeds() {
        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        // A dropped cancellation sender must disable the wait branch
        // instead of cancelling the turn spuriously.
        drop(cancel_tx);

        let res = provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await;
        assert!(res.is_ok());

        let mut text = String::new();
        let mut saw_cancelled = false;
        let mut got_done = false;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta(delta) => text.push_str(&delta),
                StreamEvent::Cancelled => saw_cancelled = true,
                StreamEvent::Done => got_done = true,
                _ => {}
            }
        }
        assert_eq!(text, "ok");
        assert!(got_done);
        assert!(
            !saw_cancelled,
            "a dropped cancellation sender must not cancel the turn"
        );
    }

    #[tokio::test]
    async fn test_stream_chat_dropped_cancel_sender_mid_stream_completes() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        // A minimal SSE server: it answers one event, then withholds the
        // terminator long enough for the test to drop the cancellation
        // sender while the stream is open, and only then finishes the body.
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            // Drain the request head and JSON body so the connection is
            // quiet before the response is streamed.
            let head_separator = &b"\r\n\r\n"[..];
            let head_end = loop {
                let read = socket.read(&mut chunk).await.expect("read");
                assert!(read > 0, "request ended before the head separator");
                request.extend_from_slice(&chunk[..read]);
                if let Some(pos) = request.windows(4).position(|w| w == head_separator) {
                    break pos;
                }
            };
            let head_text = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
            let content_length: usize = head_text
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            let body_start = head_end + 4;
            while request.len() < body_start + content_length {
                let read = socket.read(&mut chunk).await.expect("read");
                assert!(read > 0, "request ended before the body was drained");
                request.extend_from_slice(&chunk[..read]);
            }

            let event = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n";
            let done = "data: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                event.len() + done.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
            socket.write_all(event.as_bytes()).await.expect("write");
            socket.flush().await.expect("flush");
            // Withhold the terminator while the test drops the sender.
            tokio::time::sleep(Duration::from_millis(400)).await;
            socket.write_all(done.as_bytes()).await.expect("write");
            socket.flush().await.expect("flush");
        });

        let provider = OpenRouterProvider::new("test-key".to_string());
        let provider = provider.with_base_url(format!("http://{addr}"));

        let (tx, mut rx) = mpsc::channel(10);
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let provider_task = tokio::spawn(async move {
            provider
                .stream_text("test-model", vec![], tx, cancel_rx)
                .await
        });

        // Let the request reach the SSE loop and consume the first event
        // before the cancellation sender is dropped mid-stream.
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(cancel_tx);

        let res = tokio::time::timeout(Duration::from_secs(5), provider_task)
            .await
            .expect("a dropped cancellation sender must not hot-spin the stream loop")
            .expect("task join failed");
        assert!(res.is_ok(), "the stream must complete: {:?}", res.err());

        let mut text = String::new();
        let mut saw_cancelled = false;
        let mut got_done = false;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta(delta) => text.push_str(&delta),
                StreamEvent::Cancelled => saw_cancelled = true,
                StreamEvent::Done => got_done = true,
                _ => {}
            }
        }
        assert_eq!(text, "Hi");
        assert!(got_done);
        assert!(
            !saw_cancelled,
            "a dropped cancellation sender must not cancel the turn"
        );
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_stream_chat_http_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized API key"))
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("invalid-key".to_string()).with_base_url(mock_server.uri());

        let (tx, _rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let res = provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await;

        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(err_msg.contains("OpenRouter HTTP error 401"));
        assert!(err_msg.contains("Unauthorized API key"));
    }

    #[tokio::test]
    async fn test_stream_chat_with_response_format() {
        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"message\\\":\\\"hi\\\"}\"}}]}\n\ndata: [DONE]\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("Authorization", "Bearer test-api-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let response_format = Some(ResponseFormat::JsonObject);
        let provider_task = tokio::spawn(async move {
            provider
                .stream_chat("test-model", vec![], response_format, tx, cancel_rx)
                .await
        });

        while let Some(_event) = rx.recv().await {}

        let res = provider_task.await.expect("task join failed");
        assert!(res.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let req_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            req_json
                .get("response_format")
                .unwrap()
                .get("type")
                .unwrap(),
            "json_object"
        );
    }

    #[tokio::test]
    async fn test_structured_output_truncated_by_length_is_transient() {
        use super::super::types::JsonSchemaDefinition;

        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"message\\\":\\\"par\"},\"finish_reason\":\"length\"}]}\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let response_format = Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name: "model_result".to_string(),
                description: None,
                schema: serde_json::json!({"type": "object"}),
                strict: Some(true),
            },
        });

        let res = provider
            .stream_chat("test-model", vec![], response_format, tx, cancel_rx)
            .await;

        assert!(res.is_err());
        let message = res.unwrap_err().to_string();
        assert!(
            message.contains(JSON_TRUNCATION_MARKER),
            "unexpected error: {message}"
        );

        let mut saw_error = false;
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Error(error) = event {
                assert!(error.contains(JSON_TRUNCATION_MARKER));
                saw_error = true;
            }
        }
        assert!(saw_error, "the truncation must surface as a stream error");
    }

    #[tokio::test]
    async fn test_plain_text_length_finish_reason_is_not_an_error() {
        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"truncated answer\"},\"finish_reason\":\"length\"}]}\n\n\
                        data: [DONE]\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await
            .expect("a text answer may legitimately end at the token limit");

        let mut text = String::new();
        let mut got_done = false;
        let mut errors = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta(delta) => text.push_str(&delta),
                StreamEvent::Done => got_done = true,
                StreamEvent::Error(error) => errors.push(error),
                _ => {}
            }
        }
        assert_eq!(text, "truncated answer");
        assert!(got_done);
        assert!(errors.is_empty(), "unexpected stream errors: {errors:?}");
    }

    #[tokio::test]
    async fn test_stream_chat_finish_reason_error_returns_err() {
        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"error\"}]}\n\n";

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let res = provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await;

        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .to_string()
            .contains("finish_reason: error"));

        let mut saw_error_event = false;
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Error(msg) = event {
                if msg.contains("finish_reason: error") {
                    saw_error_event = true;
                }
            }
        }
        assert!(saw_error_event);
    }

    #[tokio::test]
    async fn test_stream_chat_rate_limit_http_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, _rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let res = provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await;

        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(err_msg.contains("429"));
        assert!(err_msg.contains("Too Many Requests"));
    }

    #[tokio::test]
    async fn test_stream_chat_aborts_oversized_unterminated_event() {
        let mock_server = MockServer::start().await;

        let payload = "x".repeat(MAX_SSE_EVENT_BYTES + 1);
        let sse_body = format!("data: {}", payload);

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body.into_bytes(), "text/event-stream"),
            )
            .mount(&mock_server)
            .await;

        let provider =
            OpenRouterProvider::new("test-api-key".to_string()).with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let res = provider
            .stream_text("test-model", vec![], tx, cancel_rx)
            .await;

        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("terminator"));

        let mut saw_error_event = false;
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Error(msg) = event {
                assert!(msg.contains("terminator"));
                saw_error_event = true;
            }
        }
        assert!(
            saw_error_event,
            "oversized unterminated event must surface a stream error"
        );
    }

    #[test]
    fn test_truncate_response_body_is_utf8_safe() {
        let body = "ошибка ".repeat(200);
        let truncated = truncate_response_body(&body);
        assert!(truncated.chars().count() <= RESPONSE_BODY_LOG_CHARS + 1);
        assert!(truncated.ends_with('…'));
    }
}
