use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tauqe_protocol::{ModelUsageInfo, ProviderKind};

use super::types::{ChatMessage, ReasoningEffort, ResponseFormat, StreamEvent};
use super::LlmProvider;

/// Maximum time to establish a connection to OpenRouter.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Maximum idle time between reads on an open stream. Bounds a stalled
/// provider so a hung connection cannot block the turn indefinitely.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// Maximum characters of a raw provider response body captured in a log record.
const RESPONSE_BODY_LOG_CHARS: usize = 500;

pub struct OpenRouterProvider {
    api_key: String,
    base_url: String,
    http_client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderPreferences>,
}

#[derive(Debug, Serialize)]
struct ProviderPreferences {
    require_parameters: bool,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct ReasoningOption {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<ReasoningEffort>,
}

#[derive(Debug, Deserialize)]
struct StreamingChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<StreamingChoice>,
    #[serde(default)]
    usage: Option<OpenRouterUsage>,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    total_cost: Option<f64>,
    #[serde(default)]
    error: Option<OpenRouterError>,
}

#[derive(Debug, Deserialize)]
struct GenerationResponse {
    data: Option<GenerationData>,
}

#[derive(Debug, Deserialize)]
struct GenerationData {
    #[serde(default)]
    total_cost: Option<f64>,
    #[serde(default)]
    tokens_prompt: Option<u32>,
    #[serde(default)]
    tokens_completion: Option<u32>,
    #[serde(default)]
    native_tokens_prompt: Option<u32>,
    #[serde(default)]
    native_tokens_completion: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct GenerationStats {
    pub total_cost: Option<f64>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct StreamingChoice {
    #[serde(default)]
    delta: Option<StreamingDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    error: Option<OpenRouterError>,
}

#[derive(Debug, Deserialize)]
struct StreamingDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenRouterUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    total_cost: Option<f64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Deserialize, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, Default)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterError {
    message: String,
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

    async fn execute_stream(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        response_format: Option<ResponseFormat>,
        reasoning_effort: Option<ReasoningEffort>,
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

        let uses_json_schema = matches!(&response_format, Some(ResponseFormat::JsonSchema { .. }));

        tracing::info!(
            model = %model,
            effort = ?reasoning_effort,
            uses_json_schema,
            messages_count = messages.len(),
            "openrouter request dispatched"
        );

        let payload = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            stream: true,
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
            _ = async {
                let _ = cancel_rx.wait_for(|cancelled| *cancelled).await;
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

        while !finished {
            if tx.is_closed() {
                return Ok(());
            }
            tokio::select! {
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow() {
                        let _ = tx.send(StreamEvent::Cancelled).await;
                        return Ok(());
                    }
                }
                chunk_res = byte_stream.next() => {
                    match chunk_res {
                        Some(Ok(chunk)) => {
                            buffer.extend_from_slice(&chunk);

                            while let Some((pos, delim_len)) = find_sse_event_boundary(&buffer) {
                                let event_bytes = buffer[..pos].to_vec();
                                buffer.drain(..pos + delim_len);
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

/// Truncates a provider response body on a Unicode scalar boundary for logging.
fn truncate_response_body(body: &str) -> String {
    let trimmed = body.trim();
    let mut snippet: String = trimmed.chars().take(RESPONSE_BODY_LOG_CHARS).collect();
    if trimmed.chars().count() > RESPONSE_BODY_LOG_CHARS {
        snippet.push('…');
    }
    snippet
}

/// Records the aggregated token and cost metrics of a completed stream.
fn log_stream_completion(model: &str, usage: &Option<ModelUsageInfo>) {
    let usage = usage.clone().unwrap_or_default();
    tracing::info!(
        model = %model,
        prompt_tokens = usage.prompt_tokens,
        completion_tokens = usage.completion_tokens,
        total_tokens = usage.total_tokens,
        reasoning_tokens = ?usage.reasoning_tokens,
        cached_tokens = ?usage.cached_tokens,
        cost = ?usage.cost,
        "openrouter stream completed"
    );
}

fn find_sse_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let pos_lf = find_subslice(buf, b"\n\n").map(|p| (p, 2));
    let pos_crlf = find_subslice(buf, b"\r\n\r\n").map(|p| (p, 4));

    match (pos_lf, pos_crlf) {
        (Some((p1, l1)), Some((p2, l2))) => {
            if p1 <= p2 {
                Some((p1, l1))
            } else {
                Some((p2, l2))
            }
        }
        (Some(res), None) => Some(res),
        (None, Some(res)) => Some(res),
        (None, None) => None,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
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
        self.execute_stream(model, messages, response_format, None, tx, cancel_rx)
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
            tx,
            cancel_rx,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
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
                .stream_chat(
                    "test-model",
                    vec![],
                    response_format,
                    tx,
                    cancel_rx,
                )
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

    #[test]
    fn test_find_sse_event_boundary() {
        assert_eq!(find_sse_event_boundary(b""), None);
        assert_eq!(find_sse_event_boundary(b"abc"), None);
        assert_eq!(find_sse_event_boundary(b"abc\n\ndef"), Some((3, 2)));
        assert_eq!(find_sse_event_boundary(b"abc\r\n\r\ndef"), Some((3, 4)));
        assert_eq!(find_sse_event_boundary(b"abc\r\n\r\ndef\n\n"), Some((3, 4)));
        assert_eq!(find_sse_event_boundary(b"abc\n\ndef\r\n\r\n"), Some((3, 2)));
    }

    #[test]
    fn test_multibyte_utf8_split_across_chunks() {
        let mut buffer = Vec::new();
        let text = "data: {\"choices\":[{\"delta\":{\"content\":\"Привет\"}}]}\n\n";
        let bytes = text.as_bytes();
        let mid = bytes.len() / 2;
        buffer.extend_from_slice(&bytes[..mid]);
        assert_eq!(find_sse_event_boundary(&buffer), None);
        buffer.extend_from_slice(&bytes[mid..]);
        let (pos, len) = find_sse_event_boundary(&buffer).expect("found boundary");
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
        assert!(res.unwrap_err().to_string().contains("without [DONE] marker"));

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

    #[test]
    fn test_truncate_response_body_is_utf8_safe() {
        let body = "ошибка ".repeat(200);
        let truncated = truncate_response_body(&body);
        assert!(truncated.chars().count() <= RESPONSE_BODY_LOG_CHARS + 1);
        assert!(truncated.ends_with('…'));
    }
}
