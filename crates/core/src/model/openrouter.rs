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
    http_client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningOption>,
}

#[derive(Debug, Serialize)]
struct ReasoningOption {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct StreamingChunk {
    #[serde(default)]
    choices: Vec<StreamingChoice>,
    #[serde(default)]
    usage: Option<OpenRouterUsage>,
    #[serde(default)]
    error: Option<OpenRouterError>,
}

#[derive(Debug, Deserialize)]
struct StreamingChoice {
    #[serde(default)]
    delta: Option<StreamingDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
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
            http_client: reqwest::Client::new(),
        }
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
            reasoning: Some(ReasoningOption { enabled: true }),
        };

        let response = self
            .http_client
            .post("https://openrouter.ai/api/v1/chat/completions")
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
                                                if let Some(err) = chunk.error {
                                                    let _ = tx.send(StreamEvent::Error(err.message)).await;
                                                    return Ok(());
                                                }

                                                if let Some(usage) = chunk.usage {
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
                                                        cost: usage.cost,
                                                    };
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

                                                    if let Some(finish_reason) = choice.finish_reason {
                                                        if finish_reason == "stop" || finish_reason == "length" {
                                                            finished = true;
                                                            break;
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

        let _ = tx.send(StreamEvent::Done).await;
        Ok(())
    }
}
