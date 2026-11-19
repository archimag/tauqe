//! Server-sent event framing and logging helpers for the OpenRouter transport.

use crate::providers::ModelUsageInfo;

/// Maximum characters of a raw provider response body captured in a log record.
pub(crate) const RESPONSE_BODY_LOG_CHARS: usize = 500;

/// Truncates a provider response body on a Unicode scalar boundary for logging.
pub(crate) fn truncate_response_body(body: &str) -> String {
    let trimmed = body.trim();
    let mut snippet: String = trimmed.chars().take(RESPONSE_BODY_LOG_CHARS).collect();
    if trimmed.chars().count() > RESPONSE_BODY_LOG_CHARS {
        snippet.push('…');
    }
    snippet
}

/// Records the aggregated token and cost metrics of a completed stream.
pub(crate) fn log_stream_completion(model: &str, usage: &Option<ModelUsageInfo>) {
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
