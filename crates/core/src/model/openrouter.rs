use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::watch;
use workbench_protocol::ModelUsageInfo;

use super::gateway::{ChatMessage, StreamEvent};

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
            HeaderValue::from_static("https://github.com/workbench/workbench"),
        );
        headers.insert("X-Title", HeaderValue::from_static("Workbench"));

        let payload = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            reasoning: Some(ReasoningOption { enabled: true }),
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
                        return Ok(());
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
                                                    return Ok(());
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
                            return Ok(());
                        }
                        None => {
                            finished = true;
                        }
                    }
                }
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
        Ok(())
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

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "Ping".to_string(),
        }];

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

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "Hi".to_string(),
        }];

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
}
