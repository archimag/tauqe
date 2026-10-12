//! Parser and types for the `<success>` and `<error>` terminal outcome tags.

use super::{next_control_tag, strip_tags};
use crate::edits::protocol::xml::extract_attribute;

/// Universal terminal outcome emitted by the model at the conclusion of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutcome {
    Success { summary: Option<String> },
    Error { message: String },
}

impl TerminalOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn summary(&self) -> Option<&str> {
        match self {
            Self::Success { summary } => summary.as_deref(),
            Self::Error { .. } => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Success { .. } => None,
            Self::Error { message } => Some(message.as_str()),
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

/// Extracts `<success ...>` completion signal from model output if present.
pub fn extract_success(text: &str) -> Option<Option<String>> {
    let mut last_summary = None;
    let mut found = false;
    let mut pos = 0;
    while let Some((start, end)) = next_success_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let mut summary = extract_attribute(header, "summary")
            .or_else(|| extract_attribute(header, "note"))
            .or_else(|| extract_attribute(header, "message"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if summary.is_none() && !is_self_closing && header_end < tag.len() {
            let after_header = &tag[header_end + 1..];
            let body = if let Some(close_idx) = after_header.rfind("</") {
                after_header[..close_idx].trim()
            } else {
                after_header.trim()
            };
            if !body.is_empty() {
                summary = Some(body.to_string());
            }
        }
        last_summary = summary;
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
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let mut msg = extract_attribute(header, "message")
            .or_else(|| extract_attribute(header, "reason"))
            .or_else(|| extract_attribute(header, "details"))
            .or_else(|| extract_attribute(header, "note"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if msg.is_none() && !is_self_closing && header_end < tag.len() {
            let after_header = &tag[header_end + 1..];
            let body = if let Some(close_idx) = after_header.rfind("</") {
                after_header[..close_idx].trim()
            } else {
                after_header.trim()
            };
            if !body.is_empty() {
                msg = Some(body.to_string());
            }
        }
        last_error = Some(msg.unwrap_or_else(|| "Execution error".to_string()));
        pos = abs_end;
    }
    last_error
}

/// Extracts the final terminal outcome (`<success>` or `<error>`) from model output.
pub fn extract_terminal_outcome(text: &str) -> Option<TerminalOutcome> {
    let mut outcomes: Vec<(usize, TerminalOutcome)> = Vec::new();

    let mut pos = 0;
    while let Some((start, end)) = next_success_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let mut summary = extract_attribute(header, "summary")
            .or_else(|| extract_attribute(header, "note"))
            .or_else(|| extract_attribute(header, "message"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if summary.is_none() && !is_self_closing && header_end < tag.len() {
            let after_header = &tag[header_end + 1..];
            let body = if let Some(close_idx) = after_header.rfind("</") {
                after_header[..close_idx].trim()
            } else {
                after_header.trim()
            };
            if !body.is_empty() {
                summary = Some(body.to_string());
            }
        }
        outcomes.push((abs_start, TerminalOutcome::Success { summary }));
        pos = abs_end;
    }

    pos = 0;
    while let Some((start, end)) = next_error_tag(&text[pos..]) {
        let abs_start = pos + start;
        let abs_end = pos + end;
        let tag = &text[abs_start..abs_end];
        let header_end = tag.find('>').unwrap_or(tag.len());
        let header = &tag[..header_end];
        let is_self_closing = header.trim_end().ends_with('/');

        let mut msg = extract_attribute(header, "message")
            .or_else(|| extract_attribute(header, "reason"))
            .or_else(|| extract_attribute(header, "details"))
            .or_else(|| extract_attribute(header, "note"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if msg.is_none() && !is_self_closing && header_end < tag.len() {
            let after_header = &tag[header_end + 1..];
            let body = if let Some(close_idx) = after_header.rfind("</") {
                after_header[..close_idx].trim()
            } else {
                after_header.trim()
            };
            if !body.is_empty() {
                msg = Some(body.to_string());
            }
        }
        outcomes.push((
            abs_start,
            TerminalOutcome::Error {
                message: msg.unwrap_or_else(|| "Execution error".to_string()),
            },
        ));
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

/// Removes all `<success>` and `<error>` outcome tags from text.
pub fn strip_terminal_outcome_tags(text: &str) -> String {
    strip_error_tags(&strip_success_tags(text))
}
