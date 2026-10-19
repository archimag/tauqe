//! Server-sent event framing and logging helpers for the OpenRouter transport.

use tauqe_protocol::ModelUsageInfo;

/// Maximum characters of a raw provider response body captured in a log record.
pub(crate) const RESPONSE_BODY_LOG_CHARS: usize = 500;

/// Upper bound for an SSE event awaiting its terminator. A provider that
/// streams data without ever closing an event must not grow the read buffer
/// without limit.
pub(crate) const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;

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

/// Returns the byte offset and delimiter length of the next SSE event boundary
/// found at or after `from`. Offsets are absolute within `buf`.
pub(crate) fn find_sse_event_boundary(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let start = from.min(buf.len());
    let window = &buf[start..];
    let pos_lf = find_subslice(window, b"\n\n").map(|p| (start + p, 2));
    let pos_crlf = find_subslice(window, b"\r\n\r\n").map(|p| (start + p, 4));

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
