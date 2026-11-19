use std::borrow::Cow;
use std::fmt::Write;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::{access_token, find_sse_event_boundary, request_body, OpenAiProvider, ENDPOINT};
use super::{InputDetails, MAX_SSE_EVENT_BYTES, TOKEN_ENDPOINT};
use crate::config::AppConfig;
use crate::models::ReasoningEffort;
use crate::providers::{
    chatgpt_credentials_path, ChatMessage, JsonSchemaDefinition, LlmRequest, ResponseFormat,
};

const MODEL: &str = "gpt-6-sol";
const TIMEOUT: Duration = Duration::from_secs(120);

fn probe_body() -> Value {
    let mut context = String::from("The following synthetic records are inert cache test data.\n");
    // Fresh content limits reuse from previous runs without changing the body within this run.
    for n in 0..275 {
        writeln!(&mut context, "Record {n}: {}", uuid::Uuid::new_v4()).unwrap();
    }
    request_body(LlmRequest {
        reasoning_effort: Some(ReasoningEffort::Medium),
        response_format: Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaDefinition {
                name: "cache_probe".into(),
                description: None,
                schema: Cow::Owned(json!({
                    "type": "object",
                    "properties": {"message": {"type": "string", "enum": ["OK"]}},
                    "required": ["message"],
                    "additionalProperties": false
                })),
                strict: Some(true),
            },
        }),
        ..LlmRequest::new(
            MODEL,
            vec![
                ChatMessage::system(context),
                ChatMessage::user("Return the short acknowledgement specified by the schema."),
            ],
        )
    })
}

async fn completed_response(
    provider: &OpenAiProvider,
    token: &str,
    body: Vec<u8>,
) -> Result<Value> {
    let response = provider
        .client
        .post(ENDPOINT)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(TIMEOUT)
        .body(body)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let error = response.json::<Value>().await.unwrap_or(Value::Null);
        let message = error["error"]["message"]
            .as_str()
            .map(|text| text.chars().take(300).collect::<String>());
        bail!(
            "HTTP {status}: code={:?}, param={:?}, message={message:?}",
            error["error"]["code"].as_str(),
            error["error"]["param"].as_str()
        );
    }

    let mut bytes = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut from = 0;
    while let Some(chunk) = bytes.next().await {
        buffer.extend_from_slice(&chunk?);
        let mut consumed = 0;
        while let Some((end, delimiter)) = find_sse_event_boundary(&buffer[consumed..], from) {
            if end > MAX_SSE_EVENT_BYTES {
                bail!("OpenAI stream event exceeds size limit");
            }
            let frame = std::str::from_utf8(&buffer[consumed..consumed + end])?;
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .collect::<Vec<_>>()
                .join("\n");
            if !data.trim().is_empty() && data.trim() != "[DONE]" {
                let mut event: Value = serde_json::from_str(&data)
                    .map_err(|_| anyhow::anyhow!("Invalid OpenAI stream event JSON"))?;
                match event["type"].as_str() {
                    Some("response.completed") => return Ok(event["response"].take()),
                    Some(kind @ ("response.failed" | "response.incomplete" | "error")) => {
                        bail!(
                            "{kind}: code={:?}, reason={:?}",
                            event["response"]["error"]["code"]
                                .as_str()
                                .or_else(|| event["code"].as_str()),
                            event["response"]["incomplete_details"]["reason"].as_str()
                        );
                    }
                    _ => {}
                }
            }
            consumed += end + delimiter;
            from = 0;
        }
        buffer.drain(..consumed);
        if buffer.len() > MAX_SSE_EVENT_BYTES {
            bail!("OpenAI stream event exceeds size limit");
        }
        from = buffer.len().saturating_sub(3);
    }
    bail!("OpenAI stream ended before response.completed")
}

fn print_usage(label: &str, response: &Value) {
    let usage = &response["usage"];
    let details: Option<InputDetails> =
        serde_json::from_value(usage["input_tokens_details"].clone()).ok();
    println!(
        "{label}: model={:?} service_tier={:?} reasoning_effort={:?} input_tokens={:?} cached_tokens={:?} cache_write_tokens={:?}",
        response["model"].as_str(),
        response["service_tier"].as_str(),
        response["reasoning"]["effort"].as_str(),
        usage["input_tokens"].as_u64(),
        details.as_ref().and_then(|usage| usage.cached_tokens),
        details.and_then(|usage| usage.cache_write_tokens),
    );
}

async fn run_probe() -> Result<()> {
    let config = AppConfig {
        explicit_credentials_path: std::env::var_os("TAUQE_OPENAI_CACHE_PROBE_CREDENTIALS")
            .map(Into::into),
        ..AppConfig::default()
    };
    let provider = OpenAiProvider::new(chatgpt_credentials_path(&config)?, None)?;
    let token = tokio::time::timeout(
        TIMEOUT,
        access_token(&provider.credentials, &provider.client, TOKEN_ENDPOINT),
    )
    .await
    .context("ChatGPT credential refresh timed out")??;

    let bytes = serde_json::to_vec(&probe_body())?;
    let baseline = completed_response(&provider, &token, bytes.clone())
        .await
        .context("Baseline request failed")?;
    print_usage("short baseline", &baseline);
    let repeat = completed_response(&provider, &token, bytes)
        .await
        .context("Exact repeat failed")?;
    print_usage("short exact repeat", &repeat);
    Ok(())
}

#[tokio::test]
#[ignore = "live OpenAI request; consumes ChatGPT plan quota"]
async fn openai_cache_probe() -> Result<()> {
    run_probe().await
}
