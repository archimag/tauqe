//! Parser and types for the `<success>` and `<error>` terminal outcome tags.

use super::{next_control_tag, strip_tags};
use crate::edits::protocol::xml::extract_attribute;

/// Universal terminal outcome emitted by the model at the conclusion of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutcome {
    Success { summary: Option<String> },
    Error { message: String },
    Blocked { reason: String, details: Option<String> },
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

/// Locates the next complete `<success .../>` or `<success>...</success>` tag.
pub(crate) fn next_success_tag(text: &str) -> Option<(usize, usize)> {
    next_control_tag(text, "<success")
}

/// Locates the next complete `<error .../>` or `<error>...</error>` tag.
pub(crate) fn next_error_tag(text: &str) -> Option<(usize, usize)> {
    next_control_tag(text, "<error")
}

/// Locates the next complete `<blocked .../>` or `<blocked>...</blocked>` tag.
pub(crate) fn next_blocked_tag(text: &str) -> Option<(usize, usize)> {
    next_control_tag(text, "<blocked")
}

/// Splits a control tag into its header and optional body content.
fn split_tag_parts(tag: &str) -> (&str, Option<&str>) {
    let header_end = tag.find('>').unwrap_or(tag.len());
    let header = &tag[..header_end];
    let is_self_closing = header.trim_end().ends_with('/');
    let body = if is_self_closing || header_end >= tag.len() {
        None
    } else {
        let after_header = &tag[header_end + 1..];
        let body = if let Some(close_idx) = after_header.rfind("</") {
            after_header[..close_idx].trim()
        } else {
            after_header.trim()
        };
        if body.is_empty() {
            None
        } else {
            Some(body)
        }
    };
    (header, body)
}

/// Parses a single `<success ...>` tag, preferring attributes over body text.
fn parse_success_tag(tag: &str) -> Option<String> {
    let (header, body) = split_tag_parts(tag);
    extract_attribute(header, "summary")
        .or_else(|| extract_attribute(header, "note"))
        .or_else(|| extract_attribute(header, "message"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| body.map(|s| s.to_string()))
}

/// Parses a single `<error ...>` tag, preferring attributes over body text.
fn parse_error_tag(tag: &str) -> Option<String> {
    let (header, body) = split_tag_parts(tag);
    extract_attribute(header, "message")
        .or_else(|| extract_attribute(header, "reason"))
        .or_else(|| extract_attribute(header, "details"))
        .or_else(|| extract_attribute(header, "note"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| body.map(|s| s.to_string()))
}

/// Parses a single `<blocked ...>` tag into a reason and optional details.
fn parse_blocked_tag(tag: &str) -> (String, Option<String>) {
    let (header, body) = split_tag_parts(tag);

    let mut reason = extract_attribute(header, "reason")
        .or_else(|| extract_attribute(header, "message"))
        .or_else(|| extract_attribute(header, "violated_assumption"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut details = extract_attribute(header, "details")
        .or_else(|| extract_attribute(header, "note"))
        .or_else(|| extract_attribute(header, "suggested_action"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(body) = body {
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

/// Extracts `<success ...>` completion signal from model output if present.
pub fn extract_success(text: &str) -> Option<Option<String>> {
    let mut last_summary = None;
    let mut found = false;
    let mut pos = 0;
    while let Some((start, end)) = next_success_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        last_summary = parse_success_tag(&text[abs_start..abs_end]);
        found = true;
        pos = abs_end;
    }
    if found {
        Some(last_summary)
    } else {
        None
    }
}

/// Extracts `<error ...>` blocker/failure signal from model output if present.
pub fn extract_error(text: &str) -> Option<String> {
    let mut last_error = None;
    let mut pos = 0;
    while let Some((start, end)) = next_error_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        last_error = Some(
            parse_error_tag(&text[abs_start..abs_end])
                .unwrap_or_else(|| "Execution error".to_string()),
        );
        pos = abs_end;
    }
    last_error
}

/// Extracts `<blocked ...>` blocker/assumption failure signal from model output if present.
pub fn extract_blocked(text: &str) -> Option<(String, Option<String>)> {
    let mut last_blocked = None;
    let mut pos = 0;
    while let Some((start, end)) = next_blocked_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        last_blocked = Some(parse_blocked_tag(&text[abs_start..abs_end]));
        pos = abs_end;
    }
    last_blocked
}

/// Extracts the final terminal outcome (`<success>` or `<error>`) from model output.
pub fn extract_terminal_outcome(text: &str) -> Option<TerminalOutcome> {
    let mut outcomes: Vec<(usize, TerminalOutcome)> = Vec::new();

    let mut pos = 0;
    while let Some((start, end)) = next_success_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let summary = parse_success_tag(&text[abs_start..abs_end]);
        outcomes.push((abs_start, TerminalOutcome::Success { summary }));
        pos = abs_end;
    }

    pos = 0;
    while let Some((start, end)) = next_error_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let message = parse_error_tag(&text[abs_start..abs_end])
            .unwrap_or_else(|| "Execution error".to_string());
        outcomes.push((abs_start, TerminalOutcome::Error { message }));
        pos = abs_end;
    }

    pos = 0;
    while let Some((start, end)) = next_blocked_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let (reason, details) = parse_blocked_tag(&text[abs_start..abs_end]);
        outcomes.push((abs_start, TerminalOutcome::Blocked { reason, details }));
        pos = abs_end;
    }

    outcomes.sort_by_key(|(start, _)| *start);
    outcomes.pop().map(|(_, outcome)| outcome)
}

/// Removes all `<success>` tags and content from text.
pub fn strip_success_tags(text: &str) -> String {
    strip_tags(text, next_success_tag)
}

/// Removes all `<error>` tags and content from text.
pub fn strip_error_tags(text: &str) -> String {
    strip_tags(text, next_error_tag)
}

/// Removes all `<blocked>` tags and content from text.
pub fn strip_blocked_tags(text: &str) -> String {
    strip_tags(text, next_blocked_tag)
}

/// Removes all `<success>`, `<error>`, and `<blocked>` outcome tags from text.
pub fn strip_terminal_outcome_tags(text: &str) -> String {
    strip_blocked_tags(&strip_error_tags(&strip_success_tags(text)))
}
