use tauqe_protocol::{DiscussionContextRequest, DiscussionPlanUpdate, DiscussionResponse};
use tracing::{info, warn};

use super::json::extract_streamed_message;
use super::EditStreamFilter;
use crate::providers::StreamEvent;

/// Streaming filter for Structured Output in discussion and planning modes.
///
/// It streams the top-level `"message"` field on the fly as `StreamEvent::TextDelta`
/// while buffering the payload to parse structured commands (`DiscussionResponse`)
/// at the end of the turn.
#[derive(Debug, Default)]
pub struct DiscussionStreamFilter {
    passthrough: Option<bool>,
    buffer: String,
    message_streamed_bytes: usize,
    message_finished: bool,
    response: Option<DiscussionResponse>,
}

impl DiscussionStreamFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes a delta chunk and yields any unescaped `TextDelta` stream events.
    pub fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();

        // 0. Detect passthrough if provider returned plain text (not JSON)
        if self.passthrough.is_none() {
            match self.buffer.chars().find(|c| !c.is_whitespace()) {
                None => return events,
                Some(c) => {
                    let plain = c != '{' && c != '`';
                    self.passthrough = Some(plain);
                    if plain {
                        events.push(StreamEvent::TextDelta(self.buffer.clone()));
                        return events;
                    }
                }
            }
        } else if self.passthrough == Some(true) {
            if !chunk.is_empty() {
                events.push(StreamEvent::TextDelta(chunk.to_string()));
            }
            return events;
        }

        // If buffer starts with ``` but no { appears after reasonable length, switch to passthrough
        if self.passthrough == Some(false) && self.buffer.find('{').is_none() {
            if self.buffer.len() > 64 {
                self.passthrough = Some(true);
                events.push(StreamEvent::TextDelta(self.buffer.clone()));
                return events;
            }
            return events;
        }

        // 1. Stream message field deltas on the fly
        if !self.message_finished {
            let (delta, new_streamed, finished) =
                extract_streamed_message(&self.buffer, self.message_streamed_bytes);
            if let Some(d) = delta {
                if !d.is_empty() {
                    events.push(StreamEvent::TextDelta(d));
                }
            }
            self.message_streamed_bytes = new_streamed;
            self.message_finished = finished;
        }

        events
    }

    /// Flushes any remaining streamed content and deserializes the structured response.
    pub fn finish_internal(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if self.passthrough == Some(true) {
            self.response = Some(Self::parse_response(&self.buffer));
            return events;
        }

        if !self.message_finished {
            let (delta, new_streamed, finished) =
                extract_streamed_message(&self.buffer, self.message_streamed_bytes);
            if let Some(d) = delta {
                if !d.is_empty() {
                    events.push(StreamEvent::TextDelta(d));
                }
            }
            self.message_streamed_bytes = new_streamed;
            self.message_finished = finished;
        }

        // Fallback: if nothing was streamed as message, and buffer is non-empty
        if self.message_streamed_bytes == 0 && !self.buffer.trim().is_empty() {
            let parsed = Self::parse_response(&self.buffer);
            if !parsed.message.is_empty() {
                events.push(StreamEvent::TextDelta(parsed.message.clone()));
            } else {
                events.push(StreamEvent::TextDelta(self.buffer.clone()));
            }
            self.response = Some(parsed);
        } else {
            self.response = Some(Self::parse_response(&self.buffer));
        }

        events
    }

    /// Consumes the filter, flushing remaining text deltas and returning the parsed response.
    pub fn finish_with_response(mut self) -> (Vec<StreamEvent>, DiscussionResponse) {
        let events = self.finish_internal();
        let resp = self
            .response
            .take()
            .unwrap_or_else(|| Self::parse_response(&self.buffer));
        (events, resp)
    }

    /// Returns the parsed structured response if available.
    pub fn response(&self) -> Option<&DiscussionResponse> {
        self.response.as_ref()
    }

    /// Robustly parses a raw model output into `DiscussionResponse` with resilient fallbacks.
    pub fn parse_response(raw: &str) -> DiscussionResponse {
        let clean = extract_json_str(raw);
        if let Ok(mut resp) = serde_json::from_str::<DiscussionResponse>(clean) {
            Self::sanitize_plan_update(&mut resp);
            return resp;
        }

        // Try finding JSON object in raw if markdown wrappers or text surrounds it
        if let Some(start) = raw.find('{') {
            if let Some(end) = raw.rfind('}') {
                if end > start {
                    let candidate = &raw[start..=end];
                    if let Ok(mut resp) = serde_json::from_str::<DiscussionResponse>(candidate) {
                        Self::sanitize_plan_update(&mut resp);
                        return resp;
                    }
                }
            }
        }

        // Partial JSON fallback: try to parse as generic Value
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(clean) {
            warn!("DiscussionResponse schema mismatch, attempting partial extraction");
            let message = val
                .get("message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| clean.to_string());

            let plan_update = val
                .get("plan_update")
                .filter(|v| !v.is_null())
                .and_then(|v| serde_json::from_value::<DiscussionPlanUpdate>(v.clone()).ok())
                .filter(|u| u.id.is_some() || !u.items.is_empty() || u.title.is_some());

            let context_requests = val
                .get("context_requests")
                .filter(|v| !v.is_null())
                .and_then(|v| {
                    serde_json::from_value::<Vec<DiscussionContextRequest>>(v.clone()).ok()
                })
                .unwrap_or_default();

            let user_language = val
                .get("user_language")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            return DiscussionResponse {
                message,
                plan_update,
                context_requests,
                user_language,
            };
        }

        // Non-JSON plain text fallback
        info!("Discussion response parsed as plain text fallback");
        DiscussionResponse::new(raw.trim())
    }

    fn sanitize_plan_update(resp: &mut DiscussionResponse) {
        if let Some(ref update) = resp.plan_update {
            if update.id.is_none() && update.items.is_empty() && update.title.is_none() {
                resp.plan_update = None;
            }
        }
    }
}

impl EditStreamFilter for DiscussionStreamFilter {
    fn push_chunk(&mut self, chunk: &str) -> Vec<StreamEvent> {
        DiscussionStreamFilter::push_chunk(self, chunk)
    }

    fn finish(mut self: Box<Self>) -> Vec<StreamEvent> {
        self.finish_internal()
    }
}

fn extract_json_str(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(idx) = trimmed.find("```json") {
        let after = &trimmed[idx + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    } else if let Some(idx) = trimmed.find("```") {
        let after = &trimmed[idx + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::{ContextAccess, DiscussionPlanAction, PlanItem, PlanItemStatus};

    #[test]
    fn test_discussion_stream_filter_character_by_character() {
        let mut filter = DiscussionStreamFilter::new();
        let json = r#"{"message":"Hello developer! How can I help?","plan_update":null,"context_requests":[],"user_language":"Russian"}"#;

        let mut streamed = String::new();
        for c in json.chars() {
            let s = c.to_string();
            for ev in filter.push_chunk(&s) {
                if let StreamEvent::TextDelta(d) = ev {
                    streamed.push_str(&d);
                }
            }
        }

        let (final_events, response) = filter.finish_with_response();
        for ev in final_events {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }

        assert_eq!(streamed, "Hello developer! How can I help?");
        assert_eq!(response.message, "Hello developer! How can I help?");
        assert_eq!(response.user_language.as_deref(), Some("Russian"));
        assert!(response.plan_update.is_none());
        assert!(response.context_requests.is_empty());
    }

    #[test]
    fn test_discussion_stream_filter_escaped_characters() {
        let mut filter = DiscussionStreamFilter::new();
        let json = r#"{"message":"Line 1\nLine 2 with \"quotes\" and \\slash and \u0041","plan_update":null,"context_requests":[],"user_language":null}"#;

        let mut streamed = String::new();
        for chunk in json.as_bytes().chunks(5) {
            let s = std::str::from_utf8(chunk).unwrap();
            for ev in filter.push_chunk(s) {
                if let StreamEvent::TextDelta(d) = ev {
                    streamed.push_str(&d);
                }
            }
        }

        let (final_events, response) = filter.finish_with_response();
        for ev in final_events {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }

        let expected = "Line 1\nLine 2 with \"quotes\" and \\slash and A";
        assert_eq!(streamed, expected);
        assert_eq!(response.message, expected);
    }

    #[test]
    fn test_discussion_stream_filter_with_plan_update() {
        let mut filter = DiscussionStreamFilter::new();
        let json = serde_json::json!({
            "message": "Updating plan with new steps",
            "plan_update": {
                "action": "update",
                "id": "test-plan",
                "title": "Test Plan",
                "description": "Plan description",
                "items": [
                    {
                        "id": "1",
                        "title": "First step",
                        "details": "Details of first step",
                        "status": "todo",
                        "children": []
                    }
                ]
            },
            "context_requests": [
                {
                    "path": "crates/core/src/lib.rs",
                    "access": "read_only"
                }
            ],
            "user_language": "Russian"
        })
        .to_string();

        let events = filter.push_chunk(&json);
        let mut streamed: String = events
            .into_iter()
            .filter_map(|e| {
                if let StreamEvent::TextDelta(d) = e {
                    Some(d)
                } else {
                    None
                }
            })
            .collect();

        let (final_events, response) = filter.finish_with_response();
        for ev in final_events {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }

        assert_eq!(streamed, "Updating plan with new steps");
        assert_eq!(response.message, "Updating plan with new steps");
        assert_eq!(response.user_language.as_deref(), Some("Russian"));

        let plan_update = response.plan_update.expect("plan_update must be present");
        assert_eq!(plan_update.action, DiscussionPlanAction::Update);
        assert_eq!(plan_update.id.as_deref(), Some("test-plan"));
        assert_eq!(plan_update.items.len(), 1);
        assert_eq!(
            plan_update.items[0],
            PlanItem {
                id: "1".to_string(),
                title: "First step".to_string(),
                details: Some("Details of first step".to_string()),
                status: PlanItemStatus::Todo,
                children: vec![],
            }
        );

        assert_eq!(response.context_requests.len(), 1);
        assert_eq!(
            response.context_requests[0],
            DiscussionContextRequest {
                path: "crates/core/src/lib.rs".to_string(),
                access: ContextAccess::ReadOnly,
            }
        );
    }

    #[test]
    fn test_discussion_stream_filter_markdown_fence() {
        let mut filter = DiscussionStreamFilter::new();
        let raw = "```json\n{\"message\": \"Fenced response message\", \"plan_update\": null, \"context_requests\": [], \"user_language\": null}\n```";

        let mut streamed = String::new();
        for ev in filter.push_chunk(raw) {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }
        let (final_events, response) = filter.finish_with_response();
        for ev in final_events {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }

        assert_eq!(streamed, "Fenced response message");
        assert_eq!(response.message, "Fenced response message");
    }

    #[test]
    fn test_discussion_stream_filter_plain_text_fallback() {
        let mut filter = DiscussionStreamFilter::new();
        let plain = "This is a direct plain text response from a non-schema provider.";

        let mut streamed = String::new();
        for ev in filter.push_chunk(plain) {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }
        let (final_events, response) = filter.finish_with_response();
        for ev in final_events {
            if let StreamEvent::TextDelta(d) = ev {
                streamed.push_str(&d);
            }
        }

        assert_eq!(streamed, plain);
        assert_eq!(response.message, plain);
        assert!(response.plan_update.is_none());
    }

    #[test]
    fn test_discussion_stream_filter_partial_json_fallback() {
        let raw = r#"{"message":"Partial valid message","plan_update":{"broken":true}}"#;
        let resp = DiscussionStreamFilter::parse_response(raw);
        assert_eq!(resp.message, "Partial valid message");
        assert!(resp.plan_update.is_none());
    }
}
