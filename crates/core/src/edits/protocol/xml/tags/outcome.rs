//! Parser and types for the `<success>`, `<error>`, `<blocked>`, and `<skip>` terminal outcome tags.

use crate::edits::protocol::xml::engine::{self, XmlTagMatch, XmlTagSpec};

/// Universal terminal outcome emitted by the model at the conclusion of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutcome {
    Success {
        summary: Option<String>,
    },
    Error {
        message: String,
    },
    Blocked {
        reason: String,
        details: Option<String>,
    },
    Skip {
        reason: String,
    },
}

impl TerminalOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    pub fn summary(&self) -> Option<&str> {
        match self {
            Self::Success { summary } => summary.as_deref(),
            _ => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Error { message } => Some(message.as_str()),
            _ => None,
        }
    }

    pub fn blocked_reason(&self) -> Option<&str> {
        match self {
            Self::Blocked { reason, .. } => Some(reason.as_str()),
            _ => None,
        }
    }

    pub fn blocked_details(&self) -> Option<&str> {
        match self {
            Self::Blocked { details, .. } => details.as_deref(),
            _ => None,
        }
    }
}

/// Specification of the `<success>` control tag from the XML tag registry.
pub(crate) fn success_tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("success")
}

/// Specification of the `<error>` control tag from the XML tag registry.
pub(crate) fn error_tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("error")
}

/// Specification of the `<blocked>` control tag from the XML tag registry.
pub(crate) fn blocked_tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("blocked")
}

/// Specification of the `<skip>` tag from the XML tag registry.
pub(crate) fn skip_tag_spec() -> Option<&'static XmlTagSpec> {
    engine::tag_spec("skip")
}

const SUCCESS_SUMMARY_ATTRS: &[&str] = &["summary", "note", "message"];
const ERROR_MESSAGE_ATTRS: &[&str] = &["message", "reason", "details", "note"];
const BLOCKED_REASON_ATTRS: &[&str] = &["reason", "message", "violated_assumption"];
const BLOCKED_DETAILS_ATTRS: &[&str] = &["details", "note", "suggested_action"];
const SKIP_REASON_ATTRS: &[&str] = &["reason", "message", "note"];

/// Parses a single `<blocked ...>` match into a reason and optional details,
/// preferring explicit attributes over the paired body text.
fn parse_blocked_match(found: &XmlTagMatch<'_>) -> (String, Option<String>) {
    let mut reason = found
        .attr_any(BLOCKED_REASON_ATTRS)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let mut details = found
        .attr_any(BLOCKED_DETAILS_ATTRS)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if let Some(body) = found.body {
        if reason.is_none() {
            reason = Some(body.to_string());
        } else if details.is_none() {
            details = Some(body.to_string());
        }
    }

    (
        reason.unwrap_or_else(|| "Execution blocked".to_string()),
        details,
    )
}

/// Parses a single `<skip ...>` match into its reason.
///
/// The `reason` attribute supplies the primary diagnostic and the paired body
/// text carries the supporting explanation. Both are combined so a recorded
/// skip note preserves the full reasoning instead of dropping the body.
fn parse_skip_match(found: &XmlTagMatch<'_>) -> String {
    let reason = found
        .attr_any(SKIP_REASON_ATTRS)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let body = found
        .body
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    match (reason, body) {
        (Some(reason), Some(body)) if reason == body => reason,
        (Some(reason), Some(body)) => format!("{}: {}", reason, body),
        (Some(reason), None) => reason,
        (None, Some(body)) => body,
        (None, None) => "Step skipped without a stated reason".to_string(),
    }
}

/// Extracts `<success ...>` completion signal from model output if present.
pub fn extract_success(text: &str) -> Option<Option<String>> {
    let spec = success_tag_spec()?;
    let mut last_summary = None;
    let mut found = false;
    for tag in engine::iter_tags(text, spec) {
        last_summary = tag.attr_or_body(SUCCESS_SUMMARY_ATTRS);
        found = true;
    }
    found.then_some(last_summary)
}

/// Extracts `<error ...>` blocker/failure signal from model output if present.
pub fn extract_error(text: &str) -> Option<String> {
    let spec = error_tag_spec()?;
    let mut last_error = None;
    for tag in engine::iter_tags(text, spec) {
        last_error = Some(
            tag.attr_or_body(ERROR_MESSAGE_ATTRS)
                .unwrap_or_else(|| "Execution error".to_string()),
        );
    }
    last_error
}

/// Extracts `<blocked ...>` blocker/assumption failure signal from model output if present.
pub fn extract_blocked(text: &str) -> Option<(String, Option<String>)> {
    let spec = blocked_tag_spec()?;
    let mut last_blocked = None;
    for tag in engine::iter_tags(text, spec) {
        last_blocked = Some(parse_blocked_match(&tag));
    }
    last_blocked
}

/// Extracts a `<skip ...>` reason from model output if present.
pub fn extract_skip(text: &str) -> Option<String> {
    let spec = skip_tag_spec()?;
    let mut last_reason = None;
    for tag in engine::iter_tags(text, spec) {
        last_reason = Some(parse_skip_match(&tag));
    }
    last_reason
}

/// Extracts the final terminal outcome (`<success>`, `<error>`, `<blocked>`, or
/// `<skip>`) from model output.
pub fn extract_terminal_outcome(text: &str) -> Option<TerminalOutcome> {
    let mut outcomes: Vec<(usize, TerminalOutcome)> = Vec::new();

    if let Some(spec) = success_tag_spec() {
        for tag in engine::iter_tags(text, spec) {
            outcomes.push((
                tag.start,
                TerminalOutcome::Success {
                    summary: tag.attr_or_body(SUCCESS_SUMMARY_ATTRS),
                },
            ));
        }
    }

    if let Some(spec) = error_tag_spec() {
        for tag in engine::iter_tags(text, spec) {
            outcomes.push((
                tag.start,
                TerminalOutcome::Error {
                    message: tag
                        .attr_or_body(ERROR_MESSAGE_ATTRS)
                        .unwrap_or_else(|| "Execution error".to_string()),
                },
            ));
        }
    }

    if let Some(spec) = blocked_tag_spec() {
        for tag in engine::iter_tags(text, spec) {
            let (reason, details) = parse_blocked_match(&tag);
            outcomes.push((tag.start, TerminalOutcome::Blocked { reason, details }));
        }
    }

    if let Some(spec) = skip_tag_spec() {
        for tag in engine::iter_tags(text, spec) {
            outcomes.push((
                tag.start,
                TerminalOutcome::Skip {
                    reason: parse_skip_match(&tag),
                },
            ));
        }
    }

    outcomes.sort_by_key(|(start, _)| *start);
    outcomes.pop().map(|(_, outcome)| outcome)
}

fn strip_outcome_tags(text: &str, spec: Option<&'static XmlTagSpec>) -> String {
    match spec {
        Some(spec) => engine::strip_tag_by_spec(text, spec),
        None => text.to_string(),
    }
}

/// Removes all `<success>` tags and content from text.
pub fn strip_success_tags(text: &str) -> String {
    strip_outcome_tags(text, success_tag_spec())
}

/// Removes all `<error>` tags and content from text.
pub fn strip_error_tags(text: &str) -> String {
    strip_outcome_tags(text, error_tag_spec())
}

/// Removes all `<blocked>` tags and content from text.
pub fn strip_blocked_tags(text: &str) -> String {
    strip_outcome_tags(text, blocked_tag_spec())
}

/// Removes all `<skip>` tags and content from text.
pub fn strip_skip_tags(text: &str) -> String {
    strip_outcome_tags(text, skip_tag_spec())
}

/// Removes the terminal outcome tags from text.
///
/// `<skip>` is deliberately preserved: the review contour reads the tag back
/// from the cleaned assistant text to record why a finding was skipped, so the
/// tag must survive conversational cleaning. Callers that need it gone apply
/// [`strip_skip_tags`] explicitly.
pub fn strip_terminal_outcome_tags(text: &str) -> String {
    strip_blocked_tags(&strip_error_tags(&strip_success_tags(text)))
}
