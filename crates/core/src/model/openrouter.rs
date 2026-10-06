use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::ModelUsageInfo;

use super::gateway::{ChatMessage, FunctionCall, ResponseFormat, StreamEvent, ToolCall, ToolDefinition};

pub struct OpenRouterClient {
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
    tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderPreferences>,
}

/// OpenRouter provider routing preferences.
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
    #[serde(default)]
    tool_calls: Option<Vec<StreamingToolCallDelta>>,
}

#[derive(Debug, Deserialize, Clone)]
struct StreamingToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    tool_type: Option<String>,
    #[serde(default)]
    function: Option<StreamingFunctionDelta>,
}

#[derive(Debug, Deserialize, Clone)]
struct StreamingFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
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

impl OpenRouterClient {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
            http_client: reqwest::Client::new(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Queries OpenRouter generation stats for the given generation ID.
    pub async fn get_generation_stats(&self, generation_id: &str) -> Result<Option<GenerationStats>> {
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

        // OpenRouter may take a brief moment to calculate generation stats after streaming.
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
                Err(_) => continue,
            };

            let status = resp.status();
            if status.is_client_error() {
                // Non-existent generation or unsupported provider endpoint
                return Ok(None);
            }

            if status.is_success() {
                if let Ok(gen_resp) = resp.json::<GenerationResponse>().await {
                    if let Some(data) = gen_resp.data {
                        if data.total_cost.is_some() {
                            return Ok(Some(GenerationStats {
                                total_cost: data.total_cost,
                                prompt_tokens: data.tokens_prompt.or(data.native_tokens_prompt),
                                completion_tokens: data.tokens_completion.or(data.native_tokens_completion),
                            }));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    pub async fn stream_chat(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        tx: mpsc::Sender<StreamEvent>,
        cancel_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        let _ = self
            .stream_chat_with_tools(model, messages, None, None, tx, cancel_rx)
            .await?;
        Ok(())
    }

    pub async fn stream_chat_with_tools(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        tools: Option<Vec<ToolDefinition>>,
        response_format: Option<ResponseFormat>,
        tx: mpsc::Sender<StreamEvent>,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Result<Vec<ToolCall>> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key.trim()))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "HTTP-Referer",
            HeaderValue::from_static("https://github.com/workbench/workbench"),
        );
        headers.insert("X-Title", HeaderValue::from_static("Workbench"));

        // Structured Outputs (json_schema) and function calling are mutually exclusive:
        // when a schema is requested, tools are never sent, and routing is restricted
        // to providers that support every requested parameter.
        let uses_json_schema = matches!(&response_format, Some(ResponseFormat::JsonSchema { .. }));
        let has_tools = !uses_json_schema && tools.as_ref().map_or(false, |t| !t.is_empty());
        let payload = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            // Avoid over-constraining provider routing under require_parameters.
            reasoning: if uses_json_schema {
                None
            } else {
                Some(ReasoningOption { enabled: true })
            },
            provider: if uses_json_schema {
                Some(ProviderPreferences {
                    require_parameters: true,
                })
            } else {
                None
            },
            response_format,
            tools: if has_tools { tools } else { None },
            tool_choice: if has_tools {
                Some(serde_json::Value::String("auto".to_string()))
            } else {
                None
            },
            parallel_tool_calls: if has_tools { Some(true) } else { None },
        };

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = self
            .http_client
            .post(&url)
            .headers(headers)
            .json(&payload)
            .send()
            .await
            .context("Failed to connect to OpenRouter API")?;

        if !response.status().is_success() {
            let status = response.status();
            let err_text = response.text().await.unwrap_or_default();
            bail!("OpenRouter HTTP error {}: {}", status, err_text);
        }

        struct InProgressCall {
            id: Option<String>,
            tool_type: String,
            name: Option<String>,
            arguments: String,
        }

        let mut in_progress_calls: std::collections::BTreeMap<usize, InProgressCall> =
            std::collections::BTreeMap::new();
        let mut byte_stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut finished = false;
        let mut generation_id: Option<String> = None;
        let mut latest_usage: Option<ModelUsageInfo> = None;

        while !finished {
            tokio::select! {
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow() {
                        let _ = tx.send(StreamEvent::Cancelled).await;
                        return Ok(Vec::new());
                    }
                }
                chunk_res = byte_stream.next() => {
                    match chunk_res {
                        Some(Ok(chunk)) => {
                            let text = String::from_utf8_lossy(&chunk);
                            buffer.push_str(&text);

                            while let Some(pos) = buffer.find("\n\n") {
                                let event_block = buffer[..pos].to_string();
                                buffer.drain(..pos + 2);

                                for line in event_block.lines() {
                                    let line = line.trim();
                                    if let Some(payload_str) = line.strip_prefix("data:") {
                                        let payload_str = payload_str.trim();
                                        if payload_str == "[DONE]" {
                                            finished = true;
                                            break;
                                        }
                                        if payload_str.is_empty() {
                                            continue;
                                        }

                                        match serde_json::from_str::<StreamingChunk>(payload_str) {
                                            Ok(chunk) => {
                                                if let Some(id) = chunk.id {
                                                    if !id.is_empty() {
                                                        generation_id = Some(id);
                                                    }
                                                }

                                                if let Some(err) = chunk.error {
                                                    let _ = tx.send(StreamEvent::Error(err.message)).await;
                                                    return Ok(Vec::new());
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
                                                        let _ = tx.send(StreamEvent::Error(err.message)).await;
                                                        return Ok(Vec::new());
                                                    }
                                                    if choice.finish_reason.as_deref() == Some("error") {
                                                        let _ = tx
                                                            .send(StreamEvent::Error(
                                                                "Generation ended with finish_reason: error".to_string(),
                                                            ))
                                                            .await;
                                                        return Ok(Vec::new());
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
                                                        if let Some(tool_call_deltas) = delta.tool_calls {
                                                            for tc in tool_call_deltas {
                                                                let entry = in_progress_calls
                                                                    .entry(tc.index)
                                                                    .or_insert_with(|| InProgressCall {
                                                                        id: None,
                                                                        tool_type: tc
                                                                            .tool_type
                                                                            .clone()
                                                                            .unwrap_or_else(|| "function".to_string()),
                                                                        name: None,
                                                                        arguments: String::new(),
                                                                    });

                                                                if let Some(id) = tc.id {
                                                                    if !id.is_empty() {
                                                                        entry.id = Some(id);
                                                                    }
                                                                }
                                                                if let Some(tt) = tc.tool_type {
                                                                    if !tt.is_empty() {
                                                                        entry.tool_type = tt;
                                                                    }
                                                                }

                                                                if let Some(func) = tc.function {
                                                                    if let Some(name) = func.name {
                                                                        if !name.is_empty() {
                                                                            match &mut entry.name {
                                                                                Some(existing) => existing.push_str(&name),
                                                                                None => entry.name = Some(name),
                                                                            }
                                                                        }
                                                                    }
                                                                    if let Some(args) = func.arguments {
                                                                        if !args.is_empty() {
                                                                            entry.arguments.push_str(&args);
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            Err(_) => {
                                                // ignore malformed chunks
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some(Err(err)) => {
                            let _ = tx.send(StreamEvent::Error(err.to_string())).await;
                            return Ok(Vec::new());
                        }
                        None => {
                            finished = true;
                        }
                    }
                }
            }
        }

        let mut completed_calls = Vec::new();
        for (idx, in_progress) in in_progress_calls {
            let name = in_progress.name.unwrap_or_default();
            if !name.is_empty() {
                let call = ToolCall {
                    id: in_progress.id.unwrap_or_else(|| format!("call_{}", idx)),
                    tool_type: in_progress.tool_type,
                    function: FunctionCall {
                        name,
                        arguments: in_progress.arguments,
                    },
                };
                completed_calls.push(call);
            }
        }

        // If cost was not present in the SSE stream, fetch actual generation stats from OpenRouter
        if let Some(gen_id) = generation_id {
            let needs_cost = latest_usage.as_ref().map_or(true, |u| u.cost.is_none());
            if needs_cost {
                if let Ok(Some(stats)) = self.get_generation_stats(&gen_id).await {
                    let mut usage = latest_usage.unwrap_or_default();
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
                    let _ = tx.send(StreamEvent::Usage(usage)).await;
                }
            }
        }

        let _ = tx.send(StreamEvent::Done).await;
        Ok(completed_calls)
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

        let client = OpenRouterClient::new("test-api-key".to_string())
            .with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let messages = vec![ChatMessage::user("Ping")];

        let client_task = tokio::spawn(async move {
            client.stream_chat("test-model", messages, tx, cancel_rx).await
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

        let res = client_task.await.expect("task join failed");
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

        let client = OpenRouterClient::new("test-api-key".to_string())
            .with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let messages = vec![ChatMessage::user("Hi")];

        let client_task = tokio::spawn(async move {
            client.stream_chat("test-model", messages, tx, cancel_rx).await
        });

        let mut received_usage = None;
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Usage(usage) = event {
                received_usage = Some(usage);
            }
        }

        let res = client_task.await.expect("task join failed");
        assert!(res.is_ok());

        let usage = received_usage.expect("Usage event with cost expected");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.cost, Some(0.00045));
    }

    #[tokio::test]
    async fn test_stream_chat_with_tools_emits_tool_calls() {
        use super::super::gateway::{FunctionDefinition, ToolDefinition};

        let mock_server = MockServer::start().await;

        let sse_body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"edit_file\",\"arguments\":\"{\\\"path\\\":\\\"main.rs\\\"\"}}]}}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\",\\\"old_text\\\":\\\"a\\\",\\\"new_text\\\":\\\"b\\\"}\"}}]}}]}\n\n\
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

        let client = OpenRouterClient::new("test-api-key".to_string())
            .with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(20);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let tools = vec![ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "edit_file".to_string(),
                description: "edit".to_string(),
                parameters: serde_json::json!({}),
            },
        }];

        let client_task = tokio::spawn(async move {
            client
                .stream_chat_with_tools("test-model", vec![], Some(tools), None, tx, cancel_rx)
                .await
        });

        while let Some(_event) = rx.recv().await {}

        let res = client_task.await.expect("task join failed");
        let completed_calls = res.expect("stream_chat_with_tools should succeed");

        assert_eq!(completed_calls.len(), 1);
        assert_eq!(completed_calls[0].id, "call_abc");
        assert_eq!(completed_calls[0].function.name, "edit_file");
        assert_eq!(
            completed_calls[0].function.arguments,
            "{\"path\":\"main.rs\",\"old_text\":\"a\",\"new_text\":\"b\"}"
        );
    }

    #[tokio::test]
    async fn test_stream_chat_json_schema_requires_parameters_and_drops_tools() {
        use super::super::gateway::{FunctionDefinition, JsonSchemaDefinition, ToolDefinition};

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

        let client = OpenRouterClient::new("test-api-key".to_string())
            .with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let tools = vec![ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "edit_file".to_string(),
                description: "edit".to_string(),
                parameters: serde_json::json!({}),
            },
        }];
        let response_format = Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name: "model_result".to_string(),
                description: None,
                schema: serde_json::json!({"type": "object"}),
                strict: Some(true),
            },
        });

        let client_task = tokio::spawn(async move {
            client
                .stream_chat_with_tools("test-model", vec![], Some(tools), response_format, tx, cancel_rx)
                .await
        });

        while let Some(_event) = rx.recv().await {}
        assert!(client_task.await.expect("task join failed").is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        let req_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(req_json["provider"]["require_parameters"], true);
        assert_eq!(req_json["response_format"]["type"], "json_schema");
        assert!(req_json.get("tools").is_none());
        assert!(req_json.get("tool_choice").is_none());
        assert!(req_json.get("parallel_tool_calls").is_none());
    }

    #[tokio::test]
    async fn test_stream_chat_http_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized API key"))
            .mount(&mock_server)
            .await;

        let client = OpenRouterClient::new("invalid-key".to_string())
            .with_base_url(mock_server.uri());

        let (tx, _rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let res = client
            .stream_chat("test-model", vec![], tx, cancel_rx)
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

        let client = OpenRouterClient::new("test-api-key".to_string())
            .with_base_url(mock_server.uri());

        let (tx, mut rx) = mpsc::channel(10);
        let (_cancel_tx, cancel_rx) = watch::channel(false);

        let response_format = Some(ResponseFormat::JsonObject);
        let client_task = tokio::spawn(async move {
            client
                .stream_chat_with_tools("test-model", vec![], None, response_format, tx, cancel_rx)
                .await
        });

        while let Some(_event) = rx.recv().await {}

        let res = client_task.await.expect("task join failed");
        assert!(res.is_ok());

        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let req_json: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            req_json.get("response_format").unwrap().get("type").unwrap(),
            "json_object"
        );
    }
}
